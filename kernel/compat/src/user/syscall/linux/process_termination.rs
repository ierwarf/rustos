use super::*;

pub(super) fn should_record_process_exit(exit_group: bool, thread_count: Option<usize>) -> bool {
    exit_group || thread_count == Some(1)
}

pub(crate) fn record_linux_process_termination(process_id: u64, wait_status: i32) {
    if multitask::mark_user_process_exiting_once(process_id) != Some(true) {
        return;
    }
    finish_linux_process_termination(process_id, wait_status);
}

/// Everything after the once-per-process exiting election: service-handle and
/// endpoint teardown, wait status, parent notification, lifecycle record.
fn finish_linux_process_termination(process_id: u64, wait_status: i32) {
    debug_ops::record_process_termination_diagnostic(process_id, wait_status);
    release_all_service_handle_refs(process_id);
    ipc_ops::cleanup_service_endpoints_for_process(process_id);
    cleanup_proc_broker_state_for_process(process_id);
    let _ = multitask::note_process_exit_status(process_id, wait_status);
    let parent = multitask::parent_process_id_of(process_id).unwrap_or(0);
    if parent != 0 {
        multitask::queue_linux_process_sigchld(
            parent,
            rustos_user_abi::syscall::PROCD_SIGCHLD_EVENT_EXIT,
        );
    }
    offload_ops::record_process_exit(process_id, parent, wait_status);
}

/// `sealed`: the faulting task already won the scheduler's first-wins terminal
/// seal (the process was marked exiting at fault time), so the exiting election
/// must not be repeated; the seal is the once-per-process gate.
pub(crate) fn record_linux_process_fault_termination(process_id: u64, vector: u8, sealed: bool) {
    let status = linux_fault_wait_status(vector);
    if sealed {
        finish_linux_process_termination(process_id, status);
    } else {
        record_linux_process_termination(process_id, status);
    }
}

pub(super) fn linux_fault_wait_status(vector: u8) -> i32 {
    // Linux reports the terminating signal in the low seven wait-status
    // bits. Match its x86 exception classes for the traps RustOS retires
    // directly instead of delivering a catchable signal frame.
    match vector {
        0 | 9 | 16 | 19 => 8, // SIGFPE
        1 | 3 => 5,           // SIGTRAP
        6 => 4,               // SIGILL
        11 | 12 | 17 => 7,    // SIGBUS
        _ => 11,              // SIGSEGV
    }
}
