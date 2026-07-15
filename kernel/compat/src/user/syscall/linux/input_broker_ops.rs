// RING3-MIGRATION-REFERENCE START: inputd should own input stats, ingress
// admission, and event coalescing policy. Ring0 keeps bounded input ingest and
// current-process user-copy substrate.
use super::*;

use rustos_user_abi::syscall::{
    INPUT_STATS_FLAG_PENDING_COALESCED, INPUT_STATS_FLAG_PENDING_POINTER_POSITION,
    INPUTD_INGEST_MAX_EVENTS, INPUTD_IPC_ABI_VERSION, IPC_SERVICE_CAP_INPUT_POLICY,
    InputIngestBrokerArgs, InputIngressWire, InputStatsBrokerArgs, InputStatsWire,
};

#[inline]
fn input_broker_abi_is_current(abi_version: u16) -> bool {
    abi_version == INPUTD_IPC_ABI_VERSION
}

pub(super) fn syscall_linux_rustos_input_stats_broker(args_ptr: u64) -> u64 {
    if !ipc_ops::current_process_has_service_capability(IPC_SERVICE_CAP_INPUT_POLICY) {
        return linux_errno(LINUX_EPERM);
    }

    let args = match usermem::read_current_user_struct::<InputStatsBrokerArgs>(args_ptr) {
        Ok(args) => args,
        Err(err) => return linux_errno(address_space_error_to_linux_errno(err)),
    };
    if !input_broker_abi_is_current(args.abi_version)
        || args.reserved0 != 0
        || args.reserved1 != 0
        || args.out_stats_ptr == 0
    {
        return linux_errno(LINUX_EINVAL);
    }

    kernel_io_manager::api::input::service_dvm_input_pending();
    let snapshot = kernel_io_manager::api::input::event_queue::debug_snapshot();
    let stats = InputStatsWire {
        pointer_packet_submits: snapshot.pointer_packet_submits,
        read_calls: snapshot.read_calls,
        read_events: snapshot.read_events,
        lock_active: snapshot.lock_active,
        lock_last_seq: snapshot.lock_last_seq,
        queued: snapshot.queued as u64,
        dropped_discrete: snapshot.dropped_discrete,
        dropped_lossy: snapshot.dropped_lossy,
        flags: (snapshot.pending_coalesced as u32 * INPUT_STATS_FLAG_PENDING_COALESCED)
            | (snapshot.pending_pointer_position as u32
                * INPUT_STATS_FLAG_PENDING_POINTER_POSITION),
        reserved0: 0,
    };

    match usermem::write_current_user_struct(args.out_stats_ptr, &stats) {
        Ok(()) => 0,
        Err(err) => linux_errno(address_space_error_to_linux_errno(err)),
    }
}
// RING3-MIGRATION-REFERENCE END: inputd-owned input broker policy.

pub(super) fn syscall_linux_rustos_input_ingest_broker(args_ptr: u64) -> u64 {
    if !ipc_ops::current_process_has_service_capability(IPC_SERVICE_CAP_INPUT_POLICY) {
        return linux_errno(LINUX_EPERM);
    }

    let args = match usermem::read_current_user_struct::<InputIngestBrokerArgs>(args_ptr) {
        Ok(args) => args,
        Err(err) => return linux_errno(address_space_error_to_linux_errno(err)),
    };
    if !input_broker_abi_is_current(args.abi_version)
        || args.reserved0 != 0
        || args.reserved1 != 0
        || args.reserved2 != 0
        || args.out_events_ptr == 0
        || args.out_count_ptr == 0
    {
        return linux_errno(LINUX_EINVAL);
    }

    let capacity = usize::try_from(args.out_capacity).unwrap_or(usize::MAX);
    if capacity > INPUTD_INGEST_MAX_EVENTS {
        return linux_errno(LINUX_EINVAL);
    }
    // The MSI-X leaf only wakes waiters. Drain in this capability-gated broker
    // before transferring ingress so decoder ownership remains task-context-only.
    kernel_io_manager::api::input::service_dvm_input_pending();
    let mut ingress = alloc::vec![InputIngressWire::default(); capacity];
    let count = kernel_io_manager::api::input::event_queue::drain_ingress(&mut ingress);
    for (index, wire) in ingress.iter().take(count).enumerate() {
        let offset = index
            .checked_mul(core::mem::size_of::<InputIngressWire>())
            .and_then(|offset| u64::try_from(offset).ok());
        let Some(offset) = offset else {
            return linux_errno(LINUX_EINVAL);
        };
        if let Err(err) = usermem::write_current_user_struct(args.out_events_ptr + offset, wire) {
            return linux_errno(address_space_error_to_linux_errno(err));
        }
    }
    match usermem::write_current_user_u32(args.out_count_ptr, count as u32) {
        Ok(()) => count as u64,
        Err(err) => linux_errno(address_space_error_to_linux_errno(err)),
    }
}

/// Sleep the inputd-owned ingestion worker until either a decoded ingress
/// record or a raw DVM ring record is available. The MSI-X leaf only wakes the
/// waiter; this capability-gated task-context turn remains the sole consumer
/// and therefore cannot move input policy back into ring0.
pub(super) fn syscall_linux_rustos_input_wait_broker() -> u64 {
    if !ipc_ops::current_process_has_service_capability(IPC_SERVICE_CAP_INPUT_POLICY) {
        return linux_errno(LINUX_EPERM);
    }

    loop {
        if kernel_io_manager::api::input::event_queue::has_pending_input_events() {
            return 0;
        }
        let Some(task_id) = multitask::current_task_id() else {
            return linux_errno(LINUX_EINVAL);
        };
        if !multitask::arm_block_current_task() {
            return linux_errno(LINUX_EINVAL);
        }
        if !kernel_io_manager::api::input::event_queue::arm_inputd_ingestion_waiter(task_id) {
            let _ = multitask::cancel_block_current_task();
            return linux_errno(LINUX_EBUSY);
        }
        // This capability check plus the successfully armed single waiter is
        // the input-ring consumer admission point. Publishing readiness from
        // an application's poll path creates a boot cycle: L0 waits for the
        // flag before producing, while no app is required to poll during
        // bootstrap. Fail closed if the DVM aperture cannot be admitted.
        if !kernel_io_manager::api::input::mark_dvm_policy_consumer_ready() {
            kernel_io_manager::api::input::event_queue::disarm_inputd_ingestion_waiter(task_id);
            let _ = multitask::cancel_block_current_task();
            return linux_errno(LINUX_ENODEV);
        }
        // Close the interrupt→wait registration race. A producer that commits
        // after the first check must either wake this task or be observed here.
        if kernel_io_manager::api::input::event_queue::has_pending_input_events() {
            kernel_io_manager::api::input::event_queue::disarm_inputd_ingestion_waiter(task_id);
            let _ = multitask::cancel_block_current_task();
            continue;
        }
        match multitask::commit_block_current_task() {
            Some(true) => multitask::yield_now(),
            Some(false) => {}
            None => {
                kernel_io_manager::api::input::event_queue::disarm_inputd_ingestion_waiter(task_id);
                return linux_errno(LINUX_EINVAL);
            }
        }
        kernel_io_manager::api::input::event_queue::disarm_inputd_ingestion_waiter(task_id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn input_brokers_reject_stale_wire_versions() {
        assert!(input_broker_abi_is_current(INPUTD_IPC_ABI_VERSION));
        assert!(!input_broker_abi_is_current(
            INPUTD_IPC_ABI_VERSION.saturating_sub(1)
        ));
    }
}
