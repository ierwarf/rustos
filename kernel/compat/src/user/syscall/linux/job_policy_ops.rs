//! Typed Job authority configures qualitative ceilings; userspace chooses the
//! masks. This mechanism grants no broker role and leaves Job ABI v4 unchanged.
//!
//! - Owner: compat validates the wire/typed descriptor; kernel-ps owns ceilings.
//! - Boundary: GET requires INSPECT, SET requires SET_POLICY, never SET_LIMITS.
//! - Lifecycle: resolve the descriptor generation again under PROCESS_TABLE.
//! - Concurrency: scalar SET refreshes descendants before returning; copyout
//!   failure does not undo it, and the four-word mask is not an atomic read.
//! - Failure: closed-version/header checks precede mutation and output is
//!   prevalidated; stale authority and user-copy failures return Linux errno.
//! - Forbidden: no positive grants, inferred role, or untyped JobId authority.
//! - Evidence: job-policy-lifecycle/publication, native wire/rights witnesses
//!   and opt-in rootd boot controller with framed Current-cache checks.
use super::*;
use kernel_object::api::handle::JobHandleRights;
use rustos_user_abi::syscall::{
    JOB_POLICY_ABI_VERSION, JOB_POLICY_OP_GET, JOB_POLICY_OP_SET,
    RustosJobPolicyArgs,
};

fn policy_args_are_valid(args: &RustosJobPolicyArgs) -> bool {
    args.abi_version == JOB_POLICY_ABI_VERSION
        && args.flags == 0
        && args.reserved == 0
        && args.job == 0
        && args.effective.is_empty()
        && match args.op {
            JOB_POLICY_OP_GET => args.own.is_empty(),
            JOB_POLICY_OP_SET => true,
            _ => false,
        }
}

fn required_policy_right(op: u16) -> Result<JobHandleRights, i64> {
    match op {
        JOB_POLICY_OP_GET => Ok(JobHandleRights::INSPECT),
        JOB_POLICY_OP_SET => Ok(JobHandleRights::SET_POLICY),
        _ => Err(LINUX_EINVAL),
    }
}

fn run_policy_op(args: &mut RustosJobPolicyArgs, caller: u64) -> Result<(), i64> {
    let required = required_policy_right(args.op)?;
    let job = multitask::with_process_state_by_pid(caller, |state| {
        state.handles().get_entry(args.job_handle).ok_or(LINUX_EBADF)
            .and_then(|entry| super::job_ops::job_authority_with_right(entry, required))
            .map(|(job, _)| job)
    }).ok_or(LINUX_ESRCH)??;
    // Typed descriptors carry generation, not a lifetime pin. Resolve it again
    // under the same table lock that serializes kill/recycling and policy writes.
    let info = match args.op {
        JOB_POLICY_OP_GET => multitask::job_policy_info(job),
        JOB_POLICY_OP_SET => multitask::job_set_policy(job, args.own),
        _ => return Err(LINUX_EINVAL),
    }.map_err(super::job_ops::job_error_errno)?;
    args.job = job.to_raw();
    args.own = info.own;
    args.effective = info.effective;
    Ok(())
}

/// Test-build-only end-to-end witness for the actual installed Current adapter.
/// The quiescent rootd boot probe changes its own highest ceiling bit and then
/// observes GET completion. No default boot validation or broker grant is added.
fn check_current_cache_if_enabled(args: &RustosJobPolicyArgs, caller: u64) -> Result<(), i64> {
    if option_env!("RUSTOS_ROOTD_POLICY_PROBE") != Some("1") || args.op != JOB_POLICY_OP_GET {
        return Ok(());
    }
    if multitask::process_job(caller).map(multitask::JobId::to_raw) != Some(args.job) {
        return Ok(());
    }
    let operation = rustos_user_abi::syscall::JOB_POLICY_OPERATIONS as u16 - 1;
    let actual = multitask::current_user_job_policy_allows(operation);
    if actual != args.effective.allows(operation) {
        return Err(LINUX_EIO);
    }
    debug::record_milestone(
        debug::LogCategory::Compat, "job-policy-current-cache-checked", args.job, u64::from(actual),
    );
    Ok(())
}

pub(super) fn syscall_linux_rustos_job_policy(args_ptr: u64) -> u64 {
    let mut args = match usermem::read_current_user_struct::<RustosJobPolicyArgs>(args_ptr) {
        Ok(args) => args,
        Err(error) => return linux_errno(address_space_error_to_linux_errno(error)),
    };
    if !policy_args_are_valid(&args) {
        return linux_errno(LINUX_EINVAL);
    }
    let Some(caller) = multitask::current_user_process_id() else {
        return linux_errno(LINUX_ESRCH);
    };
    if let Err(error) = usermem::validate_current_user_write_buffer(
        args_ptr, size_of::<RustosJobPolicyArgs>(),
    ) {
        return linux_errno(address_space_error_to_linux_errno(error));
    }
    if let Err(errno) = run_policy_op(&mut args, caller) {
        return linux_errno(errno);
    }
    if let Err(errno) = check_current_cache_if_enabled(&args, caller) {
        return linux_errno(errno);
    }
    // SET commits scalar ceiling state, not new descriptor authority. A later
    // copyout fault is reported but does not roll back the completed mutation.
    if let Err(error) = usermem::write_current_user_struct(args_ptr, &args) {
        return linux_errno(address_space_error_to_linux_errno(error));
    }
    0
}

#[cfg(test)]
mod tests {
    use super::*;
    use kernel_object::api::handle::HandleRights;
    use rustos_user_abi::syscall::RustosJobPolicy;

    fn request(op: u16) -> RustosJobPolicyArgs {
        RustosJobPolicyArgs { abi_version: JOB_POLICY_ABI_VERSION, op, job_handle: 3,
            ..RustosJobPolicyArgs::default() }
    }

    #[test]
    fn policy_requests_are_versioned_closed_and_cannot_supply_output_authority() {
        let get = request(JOB_POLICY_OP_GET);
        assert!(policy_args_are_valid(&get));
        let set = RustosJobPolicyArgs { own: RustosJobPolicy::ALL, ..request(JOB_POLICY_OP_SET) };
        assert!(policy_args_are_valid(&set));
        for invalid in [
            RustosJobPolicyArgs { abi_version: 0, ..get },
            RustosJobPolicyArgs { abi_version: JOB_POLICY_ABI_VERSION + 1, ..get },
            RustosJobPolicyArgs { op: 0, ..get },
            RustosJobPolicyArgs { op: u16::MAX, ..get },
            RustosJobPolicyArgs { flags: 1, ..get },
            RustosJobPolicyArgs { reserved: 1, ..get },
            RustosJobPolicyArgs { job: 1, ..get },
            RustosJobPolicyArgs { own: RustosJobPolicy::ALL, ..get },
            RustosJobPolicyArgs { effective: RustosJobPolicy::ALL, ..set },
        ] {
            assert!(!policy_args_are_valid(&invalid));
        }
    }

    #[test]
    fn policy_rights_are_typed_and_do_not_alias_limit_or_process_creation_rights() {
        assert_eq!(required_policy_right(JOB_POLICY_OP_GET), Ok(JobHandleRights::INSPECT));
        assert_eq!(required_policy_right(JOB_POLICY_OP_SET), Ok(JobHandleRights::SET_POLICY));
        assert_eq!(required_policy_right(u16::MAX), Err(LINUX_EINVAL));
        let table = multitask::HandleTable::new();
        assert_eq!(super::super::job_ops::job_authority_with_right(
            table.get_entry(0).unwrap(), JobHandleRights::SET_POLICY,
        ), Err(LINUX_EBADF));
        for rights in [JobHandleRights::SET_LIMITS, JobHandleRights::CREATE_PROCESS,
                       JobHandleRights::INSPECT, JobHandleRights::empty()] {
            let entry = multitask::HandleEntry::new_with_rights(
                multitask::KernelHandle::Job(multitask::ROOT_JOB), HandleRights::Job(rights), 0, 0,
            );
            assert_eq!(super::super::job_ops::job_authority_with_right(
                &entry, JobHandleRights::SET_POLICY,
            ), Err(LINUX_EPERM));
        }
        let entry = multitask::HandleEntry::new_with_rights(
            multitask::KernelHandle::Job(multitask::ROOT_JOB),
            HandleRights::Job(JobHandleRights::SET_POLICY), 0, 0,
        );
        assert_eq!(super::super::job_ops::job_authority_with_right(
            &entry, JobHandleRights::SET_POLICY,
        ), Ok((multitask::ROOT_JOB, JobHandleRights::SET_POLICY)));
        assert_eq!(super::super::job_ops::job_authority_with_right(
            &entry, JobHandleRights::INSPECT,
        ), Err(LINUX_EPERM));
    }
}
