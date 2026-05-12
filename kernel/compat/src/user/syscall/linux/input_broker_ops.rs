use super::*;

use rustos_user_abi::syscall::{
    INPUT_STATS_FLAG_PENDING_COALESCED, INPUT_STATS_FLAG_PENDING_POINTER_POSITION,
    IPC_SERVICE_CAP_INPUT_POLICY, InputStatsBrokerArgs, InputStatsWire,
};

pub(super) fn syscall_linux_rustos_input_stats_broker(args_ptr: u64) -> u64 {
    if !ipc_ops::current_process_has_service_capability(IPC_SERVICE_CAP_INPUT_POLICY) {
        return linux_errno(LINUX_EPERM);
    }

    let args = match usermem::read_current_user_struct::<InputStatsBrokerArgs>(args_ptr) {
        Ok(args) => args,
        Err(err) => return linux_errno(address_space_error_to_linux_errno(err)),
    };
    if args.abi_version != 1
        || args.reserved0 != 0
        || args.reserved1 != 0
        || args.out_stats_ptr == 0
    {
        return linux_errno(LINUX_EINVAL);
    }

    let snapshot = kernel_io_manager::api::input::event_queue::debug_snapshot();
    let stats = InputStatsWire {
        pointer_packet_submits: snapshot.pointer_packet_submits,
        pointer_absolute_submits: snapshot.pointer_absolute_submits,
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
