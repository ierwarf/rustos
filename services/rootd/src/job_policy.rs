//! Typed client for qualitative Job ceilings, never positive broker grants.
//!
//! - Owner: rootd chooses masks using retained typed Job authority.
//! - Boundary: fixed-width native ABI replies must echo the closed request;
//!   effective masks are subsets of own masks, not independent positive grants.
//! - Lifecycle: no descriptor is allocated or transferred by these operations.
//! - Concurrency: kernel serializes SET; a failed copyout need not undo SET.
//! - Failure: Linux errno is preserved; malformed success envelopes are EPROTO.
//! - Forbidden: never infer broker roles or widen authority from a reply.
//! - Evidence: native roundtrip/envelope/status tests and job-policy-lifecycle.
use rustos_user_abi::syscall::{
    JOB_POLICY_ABI_VERSION, JOB_POLICY_OP_GET, JOB_POLICY_OP_SET, SYS_RUSTOS_JOB_POLICY,
    RustosJobPolicy, RustosJobPolicyArgs,
};

const EPROTO: i64 = 71;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct Info {
    pub id: u64,
    pub own: RustosJobPolicy,
    pub effective: RustosJobPolicy,
}

fn call_with(
    op: u16, handle: u64, own: RustosJobPolicy,
    invoke: impl FnOnce(&mut RustosJobPolicyArgs) -> i64,
) -> Result<Info, i64> {
    let mut args = RustosJobPolicyArgs {
        abi_version: JOB_POLICY_ABI_VERSION, op, job_handle: handle, own,
        ..RustosJobPolicyArgs::default()
    };
    let status = invoke(&mut args);
    if (-4095..=-1).contains(&status) { return Err(-status); }
    if status != 0 || args.abi_version != JOB_POLICY_ABI_VERSION || args.op != op
        || args.job_handle != handle || args.flags != 0 || args.reserved != 0
        || args.job == 0 || !args.effective.is_subset_of(args.own)
        || (op == JOB_POLICY_OP_SET && args.own != own)
    {
        return Err(EPROTO);
    }
    Ok(Info { id: args.job, own: args.own, effective: args.effective })
}

fn call(op: u16, handle: u64, own: RustosJobPolicy) -> Result<Info, i64> {
    call_with(op, handle, own, |args| {
        super::syscall1(SYS_RUSTOS_JOB_POLICY, (args as *mut RustosJobPolicyArgs) as u64)
    })
}

pub(super) fn get(handle: u64) -> Result<Info, i64> {
    call(JOB_POLICY_OP_GET, handle, RustosJobPolicy::EMPTY)
}

pub(super) fn set(handle: u64, own: RustosJobPolicy) -> Result<Info, i64> {
    call(JOB_POLICY_OP_SET, handle, own)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reply() -> RustosJobPolicyArgs {
        RustosJobPolicyArgs {
            abi_version: JOB_POLICY_ABI_VERSION, op: JOB_POLICY_OP_GET, job_handle: 9, job: 1,
            own: RustosJobPolicy::ALL, effective: RustosJobPolicy::ALL,
            ..RustosJobPolicyArgs::default()
        }
    }

    #[test]
    fn policy_client_uses_closed_requests_and_preserves_clamped_effective_masks() {
        let effective = RustosJobPolicy { words: [1, 2, 4, 8] };
        let get = call_with(JOB_POLICY_OP_GET, 9, RustosJobPolicy::EMPTY, |args| {
            assert_eq!(args.abi_version, JOB_POLICY_ABI_VERSION);
            assert_eq!(args.op, JOB_POLICY_OP_GET);
            assert_eq!(args.job_handle, 9);
            assert_eq!((args.flags, args.reserved, args.job), (0, 0, 0));
            assert!(args.own.is_empty() && args.effective.is_empty());
            args.job = 1;
            args.own = RustosJobPolicy::ALL;
            args.effective = effective;
            0
        }).unwrap();
        assert_eq!(get, Info { id: 1, own: RustosJobPolicy::ALL, effective });
        let set = call_with(JOB_POLICY_OP_SET, 9, RustosJobPolicy::ALL, |args| {
            assert_eq!(args.own, RustosJobPolicy::ALL);
            assert!(args.effective.is_empty());
            args.job = 1;
            args.effective = RustosJobPolicy::EMPTY;
            0
        }).unwrap();
        assert_eq!(set.own, RustosJobPolicy::ALL);
        assert!(set.effective.is_empty(), "parent denial is not the requested own ceiling");
    }

    #[test]
    fn policy_client_refuses_wrong_identity_envelope_or_non_subset_reply() {
        let valid = reply();
        for invalid in [
            RustosJobPolicyArgs { abi_version: JOB_POLICY_ABI_VERSION + 1, ..valid },
            RustosJobPolicyArgs { op: JOB_POLICY_OP_SET, ..valid },
            RustosJobPolicyArgs { job_handle: 10, ..valid },
            RustosJobPolicyArgs { flags: 1, ..valid },
            RustosJobPolicyArgs { reserved: 1, ..valid },
            RustosJobPolicyArgs { job: 0, ..valid },
            RustosJobPolicyArgs { own: RustosJobPolicy::EMPTY, ..valid },
        ] {
            assert_eq!(call_with(JOB_POLICY_OP_GET, 9, RustosJobPolicy::EMPTY,
                                |args| { *args = invalid; 0 }), Err(EPROTO));
        }
        assert_eq!(call_with(JOB_POLICY_OP_SET, 9, RustosJobPolicy::ALL, |args| {
            args.job = 1;
            args.own = RustosJobPolicy::EMPTY;
            0
        }), Err(EPROTO));
    }

    #[test]
    fn policy_client_distinguishes_linux_errno_from_malformed_success_status() {
        assert_eq!(call_with(JOB_POLICY_OP_GET, 9, RustosJobPolicy::EMPTY, |_| -1), Err(1));
        for status in [1, i64::MIN, -4096] {
            assert_eq!(call_with(JOB_POLICY_OP_GET, 9, RustosJobPolicy::EMPTY,
                                |args| { *args = reply(); status }), Err(EPROTO));
        }
        assert_eq!(call_with(JOB_POLICY_OP_GET, 9, RustosJobPolicy::EMPTY, |_| 0), Err(EPROTO));
    }
}
