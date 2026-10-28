// RING3-MIGRATION-REFERENCE START: capability-broker exception: rootd owns job
// placement and kill policy. Ring0 keeps the job tree, group admission and the
// bounded group-kill mechanism.
//! `SYS_RUSTOS_JOB`: create, inspect and kill groups of processes.
//!
//! - **Owner:** `kernel-ps` owns the job tree; this file is its syscall edge.
//! - **Boundary:** root-supervisor authority only in this phase. Per-job handle
//!   rights replace the gate when job policy lands.
//! - **Failure:** a stale job is `ESRCH`, a killing job is `EBUSY`; a job holding
//!   the caller is `EPERM` so a supervisor cannot kill itself.
//! - **Forbidden:** killing the root job, or any caller-visible partial effect on
//!   an invalid request.
use super::*;

use rustos_user_abi::syscall::{
    IPC_SERVICE_CAP_ROOT_SUPERVISOR, JOB_ABI_VERSION, JOB_OP_CREATE_CHILD, JOB_OP_GET_INFO,
    JOB_OP_KILL, JOB_OP_PROCESS_JOB, JOB_STATE_KILLING, JOB_STATE_LIVE, RustosJobArgs,
};

/// Members ended per round of one kill call, and rounds per call. A call ends
/// at most `KILL_ROUNDS * KILL_PIDS_PER_ROUND` processes and reports the rest,
/// so a large job never holds one syscall for unbounded time.
const KILL_PIDS_PER_ROUND: usize = 16;
const KILL_ROUNDS: usize = 8;

fn job_error_errno(error: multitask::JobError) -> i64 {
    match error {
        multitask::JobError::Stale => LINUX_ESRCH,
        multitask::JobError::Killing => LINUX_EBUSY,
        multitask::JobError::TooDeep | multitask::JobError::Full => LINUX_EAGAIN,
        multitask::JobError::Root => LINUX_EPERM,
    }
}

/// A request carries its inputs only: every output field must arrive zero, so a
/// caller cannot mistake stale memory for a reply.
fn job_args_are_valid(args: &RustosJobArgs) -> bool {
    let outputs_zero = args.result_job == 0
        && args.state == 0
        && args.depth == 0
        && args.processes == 0
        && args.subtree_processes == 0
        && args.children == 0
        && args.kills == 0
        && args.remaining == 0;
    args.abi_version == JOB_ABI_VERSION && args.flags == 0 && outputs_zero
}

fn job_state_code(state: multitask::JobState) -> u32 {
    match state {
        multitask::JobState::Live => JOB_STATE_LIVE,
        multitask::JobState::Killing => JOB_STATE_KILLING,
    }
}

/// Ends the members of `job`'s subtree with the single-finalizer termination
/// and returns how many are still alive after the bounded step.
fn kill_job(job: multitask::JobId) -> Result<u64, i64> {
    if let Some(caller) = multitask::current_user_process_id() {
        if multitask::job_subtree_contains_process(job, caller) {
            return Err(LINUX_EPERM);
        }
    }
    multitask::job_kill_begin(job).map_err(job_error_errno)?;
    let mut pids = [0_u64; KILL_PIDS_PER_ROUND];
    for _ in 0..KILL_ROUNDS {
        let count = multitask::job_collect_members(job, &mut pids);
        if count == 0 {
            return Ok(0);
        }
        for &pid in &pids[..count] {
            let _ = terminate_process_once_and_finalize(pid);
        }
    }
    // Count what is left without ending anything further.
    let mut probe = [0_u64; KILL_PIDS_PER_ROUND];
    let mut remaining = 0_u64;
    let live = multitask::job_collect_members(job, &mut probe);
    remaining += live as u64;
    if live == probe.len() {
        // The probe saturated; report at least the subtree size from the info.
        if let Ok(info) = multitask::job_info(job) {
            remaining = remaining.max(info.subtree_processes);
        }
    }
    Ok(remaining)
}

fn run_job_op(args: &mut RustosJobArgs) -> Result<(), i64> {
    let job = multitask::JobId::from_raw(args.job);
    match args.op {
        JOB_OP_GET_INFO => {
            let info = multitask::job_info(job).map_err(job_error_errno)?;
            args.result_job = info.parent.map_or(0, multitask::JobId::to_raw);
            args.state = job_state_code(info.state);
            args.depth = u32::from(info.depth);
            args.processes = info.processes;
            args.subtree_processes = info.subtree_processes;
            args.children = info.children;
            args.kills = info.kills;
            Ok(())
        }
        JOB_OP_CREATE_CHILD => {
            args.result_job = multitask::job_create_child(job)
                .map_err(job_error_errno)?
                .to_raw();
            Ok(())
        }
        JOB_OP_KILL => {
            args.remaining = kill_job(job)?;
            Ok(())
        }
        JOB_OP_PROCESS_JOB => {
            args.result_job = multitask::process_job(args.pid)
                .ok_or(LINUX_ESRCH)?
                .to_raw();
            Ok(())
        }
        _ => Err(LINUX_EINVAL),
    }
}

pub(super) fn syscall_linux_rustos_job(args_ptr: u64) -> u64 {
    if !ipc_ops::current_process_has_service_capability(IPC_SERVICE_CAP_ROOT_SUPERVISOR) {
        return linux_errno(LINUX_EPERM);
    }
    let mut args = match usermem::read_current_user_struct::<RustosJobArgs>(args_ptr) {
        Ok(args) => args,
        Err(err) => return linux_errno(address_space_error_to_linux_errno(err)),
    };
    if !job_args_are_valid(&args) {
        return linux_errno(LINUX_EINVAL);
    }
    // A job op reads `job` or `pid`; the other must be zero so a typo cannot
    // silently address the root job.
    let selector_ok = match args.op {
        JOB_OP_PROCESS_JOB => args.job == 0 && args.pid != 0,
        JOB_OP_GET_INFO | JOB_OP_CREATE_CHILD | JOB_OP_KILL => args.pid == 0,
        _ => false,
    };
    if !selector_ok {
        return linux_errno(LINUX_EINVAL);
    }
    if let Err(errno) = run_job_op(&mut args) {
        return linux_errno(errno);
    }
    match usermem::write_current_user_struct(args_ptr, &args) {
        Ok(()) => 0,
        Err(err) => linux_errno(address_space_error_to_linux_errno(err)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(op: u16) -> RustosJobArgs {
        RustosJobArgs {
            abi_version: JOB_ABI_VERSION,
            op,
            ..RustosJobArgs::default()
        }
    }

    #[test]
    fn a_job_request_is_exact_version_with_zero_flags_and_zero_outputs() {
        assert!(job_args_are_valid(&request(JOB_OP_GET_INFO)));
        for bad in [
            RustosJobArgs { abi_version: 0, ..request(JOB_OP_GET_INFO) },
            RustosJobArgs { flags: 1, ..request(JOB_OP_GET_INFO) },
            RustosJobArgs { result_job: 1, ..request(JOB_OP_GET_INFO) },
            RustosJobArgs { state: 1, ..request(JOB_OP_GET_INFO) },
            RustosJobArgs { remaining: 1, ..request(JOB_OP_KILL) },
            RustosJobArgs { kills: 1, ..request(JOB_OP_KILL) },
        ] {
            assert!(!job_args_are_valid(&bad));
        }
    }

    #[test]
    fn job_errors_map_to_distinct_errnos() {
        assert_eq!(job_error_errno(multitask::JobError::Stale), LINUX_ESRCH);
        assert_eq!(job_error_errno(multitask::JobError::Killing), LINUX_EBUSY);
        assert_eq!(job_error_errno(multitask::JobError::Full), LINUX_EAGAIN);
        assert_eq!(job_error_errno(multitask::JobError::Root), LINUX_EPERM);
    }

    #[test]
    fn job_states_have_stable_wire_codes() {
        assert_eq!(job_state_code(multitask::JobState::Live), JOB_STATE_LIVE);
        assert_eq!(job_state_code(multitask::JobState::Killing), JOB_STATE_KILLING);
    }

    #[test]
    fn the_root_job_wire_id_matches_the_kernel_root() {
        assert_eq!(multitask::ROOT_JOB.to_raw(), rustos_user_abi::syscall::JOB_ID_ROOT);
    }
}
// RING3-MIGRATION-REFERENCE END: rootd-owned job broker.
