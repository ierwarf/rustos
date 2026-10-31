//! Live negative JobPolicy gate on slow rootd control operations.
//!
//! This is not a positive grant and never replaces rootd's exact live
//! lease/subject authentication. It runs *after* that authentication, only
//! on checkpoint or lease-reclaim control requests; regular service lookup,
//! IPC transport, scheduler and fault/TLB paths are untouched.
//!
//! Checking the current JobId and effective policy on each request avoids
//! accepting a cached pre-revocation answer after parent JobPolicy tightens.
use rustos_component_model::policy_ops::SlowBrokerOperation;
use rustos_user_abi::syscall::RustosJobPolicy;

const EACCES: i32 = 13;
const EPROTO: i32 = 71;

fn admit_effective(
    expected_job: u64,
    actual_job: u64,
    effective: RustosJobPolicy,
    operation: SlowBrokerOperation,
) -> Result<(), i32> {
    if expected_job == 0 || actual_job != expected_job {
        return Err(EPROTO);
    }
    if !effective.allows(operation as u16) {
        return Err(EACCES);
    }
    Ok(())
}

/// Call only after the authenticated sender has been matched to the exact
/// live service namespace. An error never authorizes a legacy fallback.
#[cfg(not(test))]
pub(super) fn authorize_job_slow_op(
    sender_pid: u64,
    operation: SlowBrokerOperation,
) -> Result<(), i32> {
    use rustos_user_abi::syscall::{JOB_BOOTSTRAP_ROOT_FD, JOB_RIGHT_INSPECT};
    if sender_pid == 0 {
        return Err(EACCES);
    }
    let id = super::jobs::process_job_in(JOB_BOOTSTRAP_ROOT_FD, sender_pid)
        .map_err(|_| EACCES)?;
    let handle = super::jobs::open_subtree(JOB_BOOTSTRAP_ROOT_FD, id, JOB_RIGHT_INSPECT)
        .map_err(|_| EACCES)?;
    let answer = super::job_policy::get(handle);
    super::jobs::close(handle);
    let info = answer.map_err(|_| EACCES)?;
    admit_effective(id, info.id, info.effective, operation)
}

// The host harness has no live kernel descriptor authority. Test admission
// algebra with explicit inputs below; never issue real syscalls in host tests.
#[cfg(test)]
pub(super) fn authorize_job_slow_op(
    _sender_pid: u64,
    _operation: SlowBrokerOperation,
) -> Result<(), i32> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn each_live_slow_control_gate_deny_is_authoritative() {
        let operations = [
            SlowBrokerOperation::WriteRecoveryCheckpoint,
            SlowBrokerOperation::CompactRecoveryCheckpoint,
            SlowBrokerOperation::ReadRecoveryCheckpoint,
            SlowBrokerOperation::ReclaimServiceLease,
        ];
        for operation in operations {
            let denied = RustosJobPolicy::ALL
                .with_operation(operation as u16, false)
                .unwrap();
            assert_eq!(admit_effective(41, 41, denied, operation), Err(EACCES));
            assert_eq!(admit_effective(41, 41, RustosJobPolicy::ALL, operation), Ok(()));
            assert_eq!(admit_effective(41, 42, RustosJobPolicy::ALL, operation), Err(EPROTO));
            assert_eq!(admit_effective(0, 0, RustosJobPolicy::ALL, operation), Err(EPROTO));
        }
    }

    #[test]
    fn independent_checkpoint_policy_bits_do_not_alias() {
        let allowed = RustosJobPolicy::EMPTY
            .with_operation(SlowBrokerOperation::ReadRecoveryCheckpoint as u16, true)
            .unwrap();
        assert!(admit_effective(
            8, 8, allowed, SlowBrokerOperation::ReadRecoveryCheckpoint
        ).is_ok());
        assert_eq!(admit_effective(
            8, 8, allowed, SlowBrokerOperation::WriteRecoveryCheckpoint
        ), Err(EACCES));
        assert_eq!(admit_effective(
            8, 8, allowed, SlowBrokerOperation::ReclaimServiceLease
        ), Err(EACCES));
    }
}
