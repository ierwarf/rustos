// RING3-MIGRATION-REFERENCE START: capability-broker exception: procd/rootd
// own lifecycle event routing and restart policy. Ring0 keeps capability-gated
// exit-event collection substrate.
use super::*;

use rustos_user_abi::syscall::{
    IPC_SERVICE_CAP_PROCESS_POLICY, IPC_SERVICE_CAP_ROOT_SUPERVISOR, LifecycleDrainBrokerArgs,
    RustosRootdTerminateBrokerArgs,
};

/// A supervisor may defer a restart but must not turn the timer substrate into
/// an unbounded privileged sleep primitive. The backoff value is published by
/// rootd and deliberately fits below this cap.
const ROOTD_WAIT_MAX_MILLIS: u64 = 1_000;

fn rootd_wait_delay_is_valid(millis: u64) -> bool {
    (1..=ROOTD_WAIT_MAX_MILLIS).contains(&millis)
}

fn rootd_terminate_args_are_valid(args: &RustosRootdTerminateBrokerArgs) -> bool {
    args.abi_version == 1 && args.reserved0 == 0 && args.flags == 0 && args.target_pid != 0
}

pub(super) fn syscall_linux_rustos_lifecycle_drain_broker(args_ptr: u64) -> u64 {
    let is_process_policy =
        ipc_ops::current_process_has_service_capability(IPC_SERVICE_CAP_PROCESS_POLICY);
    let is_root_supervisor =
        ipc_ops::current_process_has_service_capability(IPC_SERVICE_CAP_ROOT_SUPERVISOR);
    if !is_process_policy && !is_root_supervisor {
        return linux_errno(LINUX_EPERM);
    }
    let args = match usermem::read_current_user_struct::<LifecycleDrainBrokerArgs>(args_ptr) {
        Ok(args) => args,
        Err(err) => return linux_errno(address_space_error_to_linux_errno(err)),
    };
    match offload_ops::drain_lifecycle_events(&args) {
        Ok(count) => count,
        Err(errno) => linux_errno(errno),
    }
}

/// Sleep the calling root supervisor for one bounded restart-backoff interval.
/// The kernel does not select a delay, retain retry state, or restart a
/// process: those remain rootd policy. Keeping this timer primitive here
/// avoids depending on syscalld while syscalld itself is being recovered.
pub(super) fn syscall_linux_rustos_rootd_wait_broker(millis: u64) -> u64 {
    if !ipc_ops::current_process_has_service_capability(IPC_SERVICE_CAP_ROOT_SUPERVISOR) {
        return linux_errno(LINUX_EPERM);
    }
    if !rootd_wait_delay_is_valid(millis) {
        return linux_errno(LINUX_EINVAL);
    }
    crate::arch::rtc::sleep(millis);
    0
}

/// Finalize one rootd-selected service process.  Rootd decides whether a
/// lease is stale; this substrate only verifies root-supervisor authority and
/// performs the complete process teardown in one non-yielding syscall turn.
pub(super) fn syscall_linux_rustos_rootd_terminate_broker(args_ptr: u64) -> u64 {
    if !ipc_ops::current_process_has_service_capability(IPC_SERVICE_CAP_ROOT_SUPERVISOR) {
        return linux_errno(LINUX_EPERM);
    }
    let args = match usermem::read_current_user_struct::<RustosRootdTerminateBrokerArgs>(args_ptr) {
        Ok(args) => args,
        Err(err) => return linux_errno(address_space_error_to_linux_errno(err)),
    };
    if !rootd_terminate_args_are_valid(&args) {
        return linux_errno(LINUX_EINVAL);
    }
    if multitask::current_user_process_id() == Some(args.target_pid) {
        return linux_errno(LINUX_EPERM);
    }
    let parent_pid = multitask::parent_process_id_of(args.target_pid).unwrap_or(0);
    // Retire every sibling first.  No target thread can publish new authority
    // between this point and the cleanup below.
    if !multitask::terminate_user_process(args.target_pid) {
        return linux_errno(LINUX_ESRCH);
    }
    if !multitask::mark_user_process_exiting(args.target_pid) {
        return linux_errno(LINUX_ESRCH);
    }
    ipc_ops::cleanup_service_endpoints_for_process(args.target_pid);
    let _ = super::super::cleanup_proc_broker_state_for_process(args.target_pid);
    // A fixed SIGKILL status prevents rootd policy from forging application
    // exit codes while still giving waiters and lifecycle consumers a terminal
    // result they can reason about.
    let exit_status = linux_abi::SIGKILL as i32;
    let _ = multitask::note_process_exit_status(args.target_pid, exit_status);
    offload_ops::record_process_exit(args.target_pid, parent_pid, exit_status);
    if parent_pid != 0 {
        multitask::queue_linux_signal(parent_pid, parent_pid, linux_abi::SIGCHLD as u64);
    }
    0
}

#[cfg(test)]
mod tests {
    use super::{rootd_terminate_args_are_valid, rootd_wait_delay_is_valid};
    use rustos_user_abi::syscall::RustosRootdTerminateBrokerArgs;

    #[test]
    fn rootd_wait_delay_is_strictly_bounded() {
        assert!(!rootd_wait_delay_is_valid(0));
        assert!(rootd_wait_delay_is_valid(250));
        assert!(rootd_wait_delay_is_valid(1_000));
        assert!(!rootd_wait_delay_is_valid(1_001));
    }

    #[test]
    fn rootd_terminate_args_require_a_v1_nonzero_target_without_flags() {
        let valid = RustosRootdTerminateBrokerArgs {
            abi_version: 1,
            target_pid: 7,
            ..RustosRootdTerminateBrokerArgs::default()
        };
        assert!(rootd_terminate_args_are_valid(&valid));
        assert!(!rootd_terminate_args_are_valid(
            &RustosRootdTerminateBrokerArgs::default()
        ));
        assert!(!rootd_terminate_args_are_valid(
            &RustosRootdTerminateBrokerArgs { flags: 1, ..valid }
        ));
    }
}
// RING3-MIGRATION-REFERENCE END: procd/rootd-owned lifecycle broker substrate exception.
