use super::*;

/// Bounded wait while retiring live sibling threads before tearing down
/// process-wide descriptors and epoll instances.
const SIBLING_QUIESCE_TIMEOUT_NS: u64 = 100_000_000;

pub(super) fn should_record_process_exit(exit_group: bool, thread_count: Option<usize>) -> bool {
    exit_group || thread_count == Some(1)
}

/// Fast lock-free check for whether the calling thread's process is already
/// sealed as exiting.
///
/// The O(1) published live identity proves `!exiting && !exec_in_progress`;
/// only when publication is revoked (exiting or mid-exec) does this consult
/// `is_user_process_exiting` so an exec transition is never mistaken for exit.
pub(super) fn current_process_is_exiting() -> bool {
    if multitask::current_user_ipc_identity().is_some() {
        return false;
    }
    multitask::current_user_process_id().is_some_and(multitask::is_user_process_exiting)
}

/// Retires the calling thread of an already-exiting process without returning a
/// spurious teardown error (`EIO`, `EFAULT`, `EBADF`, ...) to userspace and
/// without launching a competing group-exit that could preempt the winning
/// terminator mid-cleanup.
pub(super) fn retire_exiting_sibling_thread() -> ! {
    cleanup_linux_thread_exit();
    // If the group terminator already marked this task Retired, yielding
    // switches it off-CPU immediately instead of waiting in halt.
    multitask::yield_now();
    multitask::exit_current_user_task()
}

/// Retires the calling thread if its process is already exiting.
pub(super) fn retire_if_current_process_exiting() {
    if current_process_is_exiting() {
        retire_exiting_sibling_thread();
    }
}

/// Decides whether a caller that lost `mark_user_process_exiting_once` must
/// immediately retire only itself instead of falling through to
/// `exit_current_user_process`.
///
/// A losing thread of an already-exiting process must NOT fall through to
/// `exit_current_user_process` (whose group-exit worker could retire the
/// winning terminator while it is still running `finish_linux_process_termination`).
/// When `exec_in_progress` caused the election to return `false`
/// (`process_is_exiting == false`), or for a non-member caller, we must NOT
/// self-retire here.
pub(super) fn should_retire_losing_exit_caller(
    caller_pid: Option<u64>,
    target_pid: u64,
    process_is_exiting: bool,
) -> bool {
    caller_pid == Some(target_pid) && process_is_exiting
}

/// Retires every sibling thread of `process_id` BEFORE closing descriptors and
/// retiring kernel epoll instances.
///
/// Retiring epolls wakes tasks blocked in `epoll_wait`; if siblings were still
/// live when `release_all_service_handle_refs` ran, woken siblings would race
/// synchronous VFS closes for tens of milliseconds, hit `EpollError::NotFound -> EIO`
/// or hidden `PROCESS_IDENTITY -> EFAULT`, and trap in userspace before
/// `exit_current_user_process` ever ran.
///
/// `process_id` is already sealed as exiting (`object.exiting == true`), so no
/// new thread can attach while this loop drains existing siblings.
fn quiesce_sibling_threads_before_teardown(process_id: u64) {
    let caller_is_member = multitask::current_user_process_id() == Some(process_id);
    if !caller_is_member {
        // External / kernel-cleanup caller (e.g. sealed multi-threaded fault):
        // join or start the durable group exit so live siblings are retired
        // before process-wide handles are torn down.
        if !multitask::live_user_thread_ids(process_id).is_empty() {
            let _ = multitask::terminate_user_process(process_id);
        }
        return;
    }
    if multitask::current_user_process_thread_count() == Some(1) {
        return;
    }
    let self_tid = multitask::current_user_thread_id();
    let mut deadline_ns: Option<u64> = None;
    let mut terminated_any = false;
    loop {
        let siblings: Vec<u64> = multitask::live_user_thread_ids(process_id)
            .into_iter()
            .filter(|&tid| Some(tid) != self_tid)
            .collect();
        if siblings.is_empty() {
            break;
        }
        for tid in siblings {
            if multitask::terminate_user_task(tid) {
                terminated_any = true;
            }
        }
        let still_live = multitask::live_user_thread_ids(process_id)
            .into_iter()
            .any(|tid| Some(tid) != self_tid);
        if !still_live {
            break;
        }
        let deadline = *deadline_ns.get_or_insert_with(|| {
            crate::arch::rtc::monotonic_nanos().saturating_add(SIBLING_QUIESCE_TIMEOUT_NS)
        });
        if crate::arch::rtc::monotonic_nanos() >= deadline {
            break;
        }
        multitask::yield_now();
    }
    if terminated_any {
        // Give remote CPUs that just received reschedule IPIs a turn to switch
        // retired siblings off-CPU before handle and epoll teardown begins.
        multitask::yield_now();
    }
}

pub(crate) fn record_linux_process_termination(process_id: u64, wait_status: i32) {
    if multitask::mark_user_process_exiting_once(process_id) != Some(true) {
        if should_retire_losing_exit_caller(
            multitask::current_user_process_id(),
            process_id,
            multitask::is_user_process_exiting(process_id),
        ) {
            retire_exiting_sibling_thread();
        }
        return;
    }
    finish_linux_process_termination(process_id, wait_status);
}

/// Everything after the once-per-process exiting election: sibling retirement,
/// service-handle and endpoint teardown, wait status, parent notification,
/// lifecycle record.
fn finish_linux_process_termination(process_id: u64, wait_status: i32) {
    quiesce_sibling_threads_before_teardown(process_id);
    debug_ops::record_process_termination_diagnostic(process_id, wait_status);
    release_all_service_handle_refs(process_id);
    ipc_ops::cleanup_service_endpoints_for_process(process_id);
    cleanup_proc_broker_state_for_process(process_id);
    let parent = multitask::parent_process_id_of(process_id).unwrap_or(0);
    // Queue the lifecycle EXIT before the zombie becomes visible to `wait4`:
    // service policy (vfsd advisory locks) must observe the exit no later than
    // any waiter, or a parent that returns from `wait4` can still see the dead
    // child's locks.
    offload_ops::record_process_exit(process_id, parent, wait_status);
    let _ = multitask::note_process_exit_status(process_id, wait_status);
    if parent != 0 {
        multitask::queue_linux_process_sigchld(
            parent,
            rustos_user_abi::syscall::PROCD_SIGCHLD_EVENT_EXIT,
        );
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn losing_exit_caller_self_retires_only_for_own_exiting_process() {
        // Sibling of an already-exiting process must self-retire rather than
        // starting a competing group-exit against the winning terminator.
        assert!(should_retire_losing_exit_caller(Some(42), 42, true));
        // Concurrent exec_in_progress returned false without setting exiting:
        // must NOT self-retire; fall through to group_exit::exit_current.
        assert!(!should_retire_losing_exit_caller(Some(42), 42, false));
        // External / kernel-cleanup caller or different PID must never self-retire.
        assert!(!should_retire_losing_exit_caller(None, 42, true));
        assert!(!should_retire_losing_exit_caller(Some(99), 42, true));
    }

    #[test]
    fn current_process_is_exiting_is_false_without_active_user_task() {
        assert!(!current_process_is_exiting());
    }
}
