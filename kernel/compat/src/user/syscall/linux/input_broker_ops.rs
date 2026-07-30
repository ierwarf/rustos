//! Capability-gated input transport ingestion and watchdog wait brokerage.
//!
//! - **Owner:** `inputd` owns decode/session/read policy; Compat admits the
//!   exact input-service owner to bounded ring0 transport.
//! - **Boundary:** Service identity, ring records, counts, and readiness tokens
//!   are untrusted until owner and shape checks pass.
//! - **Lifecycle:** Acquire consumer lease, drain bounded records, publish
//!   readiness, arm watchdog, wake/revoke, and withdraw on owner exit.
//! - **Concurrency:** Check/arm/commit is atomic with scheduler state; MSI-X
//!   leaves only wake and never decode.
//! - **Failure:** Timeout, malformed record, owner exit, session reset, queue
//!   full, and transport revoke cannot retain a stale consumer.
//! - **Forbidden:** No input policy in ring0, polling, native USB/PS2 fallback,
//!   or foreign service drain.
//! - **Evidence:** `input-delivery-lifecycle`.
// RING3-MIGRATION-REFERENCE START: inputd should own input stats, ingress
// admission, and event coalescing policy. Ring0 keeps bounded input ingest and
// current-process user-copy substrate.
use super::*;

use rustos_user_abi::syscall::{
    INPUTD_INGEST_MAX_EVENTS, INPUTD_IPC_ABI_VERSION, IPC_SERVICE_CAP_INPUT_POLICY,
    InputDvmRecordWire, InputIngestBrokerArgs, InputStatsBrokerArgs, InputStatsWire,
};

/// Interrupt delivery is the primary wake path. This bounded watchdog is a
/// second, independent failure detector for a lost/coalesced MSI-X edge or a
/// producer/service restart race; it does not consume or interpret records.
/// At 100 ms it adds at most ten idle wakeups per second while keeping the
/// 2,048-slot ring far from exhaustion under the admitted 256 frame/s ceiling.
const INPUT_INGESTION_WATCHDOG_MS: u64 = 100;

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

    let snapshot = kernel_io_manager::api::input::transport::debug_snapshot();
    let stats = InputStatsWire {
        pointer_packet_submits: snapshot.records_copied,
        read_calls: snapshot.broker_calls,
        read_events: snapshot.records_copied,
        lock_active: 0,
        lock_last_seq: 0,
        queued: snapshot.queued as u64,
        dropped_discrete: 0,
        dropped_lossy: snapshot.revoke_count,
        flags: 0,
        reserved0: 0,
        readiness_generation: 0,
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
        || args.out_records_ptr == 0
        || args.out_count_ptr == 0
    {
        return linux_errno(LINUX_EINVAL);
    }

    let capacity = usize::try_from(args.out_capacity).unwrap_or(usize::MAX);
    if capacity > INPUTD_INGEST_MAX_EVENTS {
        return linux_errno(LINUX_EINVAL);
    }
    // The MSI-X leaf only wakes waiters. This capability-gated broker copies
    // fixed records; inputd performs every semantic and lifecycle check.
    let mut records = alloc::vec![InputDvmRecordWire::default(); capacity];
    let count = kernel_io_manager::api::input::service_dvm_input_pending(&mut records);
    let byte_len = match count.checked_mul(core::mem::size_of::<InputDvmRecordWire>()) {
        Some(len) => len,
        None => return linux_errno(LINUX_EINVAL),
    };
    if byte_len != 0 {
        // `InputDvmRecordWire` is a repr(C), fully initialized wire object.
        // Copy the admitted batch through usermem once: per-record page-table
        // validation made ingress cost proportional to event count and could
        // not sustain the already-bounded DVM producer.
        let bytes = unsafe { core::slice::from_raw_parts(records.as_ptr().cast::<u8>(), byte_len) };
        if let Err(err) = usermem::write_current_user_bytes(args.out_records_ptr, bytes) {
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
        if kernel_io_manager::api::input::transport::has_pending_records() {
            return 0;
        }
        let Some(task_id) = multitask::current_task_id() else {
            return linux_errno(LINUX_EINVAL);
        };
        if !multitask::arm_block_current_task() {
            return linux_errno(LINUX_EINVAL);
        }
        if !kernel_io_manager::api::input::transport::arm_inputd_ingestion_waiter(task_id) {
            let _ = multitask::cancel_block_current_task();
            return linux_errno(LINUX_EBUSY);
        }
        // This capability check plus the successfully armed single waiter is
        // the input-ring consumer admission point. Publishing readiness from
        // an application's poll path creates a boot cycle: L0 waits for the
        // flag before producing, while no app is required to poll during
        // bootstrap. Fail closed if the DVM aperture cannot be admitted.
        if !kernel_io_manager::api::input::mark_dvm_policy_consumer_ready() {
            kernel_io_manager::api::input::transport::disarm_inputd_ingestion_waiter(task_id);
            let _ = multitask::cancel_block_current_task();
            return linux_errno(LINUX_ENODEV);
        }
        if !kernel_io_manager::api::input::transport::arm_consumer_wake() {
            kernel_io_manager::api::input::transport::disarm_inputd_ingestion_waiter(task_id);
            let _ = multitask::cancel_block_current_task();
            return linux_errno(LINUX_ENODEV);
        }
        // Close the interrupt→wait registration race. A producer that commits
        // after the first check must either wake this task or be observed here.
        if kernel_io_manager::api::input::transport::has_pending_records() {
            kernel_io_manager::api::input::transport::disarm_inputd_ingestion_waiter(task_id);
            let _ = multitask::cancel_block_current_task();
            continue;
        }
        let ticks_per_second = crate::arch::rtc::ticks_per_second().max(1);
        let watchdog_ticks = INPUT_INGESTION_WATCHDOG_MS
            .saturating_mul(ticks_per_second)
            .div_ceil(1000)
            .max(1);
        let watchdog_deadline = crate::arch::rtc::ticks().saturating_add(watchdog_ticks);
        if !crate::arch::rtc::arm_sleep_waiter_until_tick(task_id, watchdog_deadline) {
            kernel_io_manager::api::input::transport::disarm_inputd_ingestion_waiter(task_id);
            let _ = multitask::cancel_block_current_task();
            return linux_errno(LINUX_EBUSY);
        }
        match multitask::commit_block_current_task_and_yield() {
            Some(true) => {}
            Some(false) => {}
            None => {
                kernel_io_manager::api::input::transport::disarm_inputd_ingestion_waiter(task_id);
                crate::arch::rtc::disarm_sleep_waiter(task_id);
                return linux_errno(LINUX_EINVAL);
            }
        }
        kernel_io_manager::api::input::transport::disarm_inputd_ingestion_waiter(task_id);
        crate::arch::rtc::disarm_sleep_waiter(task_id);
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

    #[test]
    fn ingestion_watchdog_is_bounded_below_ring_exhaustion_time() {
        assert!((1..=100).contains(&INPUT_INGESTION_WATCHDOG_MS));
    }
}
