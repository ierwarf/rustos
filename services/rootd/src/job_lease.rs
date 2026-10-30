//! Rootd-owned service-job descriptors. A lease retains supervisor authority;
//! only an attenuated duplicate is lent to the loader. Retirement keeps the
//! descriptor while a bounded kill still has work, then closes it exactly once.
use super::*;

pub(super) fn ensure(lease: &mut Lease) -> Result<u64, i64> {
    if let Some(handle) = lease.job_handle { return Ok(handle); }
    let handle = jobs::create_child(rustos_user_abi::syscall::JOB_BOOTSTRAP_ROOT_FD)?;
    lease.job_handle = Some(handle);
    Ok(handle)
}

pub(super) fn retire(lease: &mut Lease) -> Result<(), i64> {
    retire_with(lease, jobs::kill, jobs::close)
}

fn retire_with(
    lease: &mut Lease,
    mut kill: impl FnMut(u64) -> Result<u64, i64>,
    mut close: impl FnMut(u64),
) -> Result<(), i64> {
    // The application realm is separate from the service subtree. Retiring
    // only the loader process would strand its apps and descriptor custody.
    if let Some(handle) = lease.application_realm {
        match kill(handle) {
            Ok(0) | Err(3) => {
                lease.application_realm = None;
                lease.application_realm_configured = false;
                close(handle);
            }
            Ok(_) => return Err(11),
            Err(errno) => return Err(errno),
        }
    }
    let Some(handle) = lease.job_handle else { return Ok(()); };
    match kill(handle) {
        Ok(0) | Err(3) => {
            lease.job_handle = None;
            close(handle);
            Ok(())
        }
        Ok(_) => Err(11),
        Err(errno) => Err(errno),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn boot_direct_service_retains_job_and_checks_published_membership() {
        // Source wiring witness; acceptance needs the signed boot milestone.
        let main = include_str!("main.rs");
        let tracked = main.split_once("fn spawn_tracked_with_attempts(").unwrap().1;
        assert!(tracked.find("job_lease::ensure(lease)?").unwrap()
            < tracked.find("spawn_exec(lease.exec_path, lease.weight_micros, job_handle)").unwrap());
        let direct = main.split_once("fn spawn_exec(").unwrap().1;
        let expected = direct.find("jobs::info(job_handle)?.id").unwrap();
        let args = direct.find("jobs::bootstrap_args(path, weight_micros, job_handle)").unwrap();
        let syscall = direct.find("syscall1(SYS_RUSTOS_SPAWN_EXEC, &args").unwrap();
        let check = direct.find("jobs::process_job(result as u64) != Ok(expected_job)").unwrap();
        let milestone = direct.find("rootd: direct job placement ok").unwrap();
        assert!(expected < args && args < syscall && syscall < check && check < milestone);
    }

    #[test]
    fn loader_service_lease_retains_job_authority_and_checks_published_membership() {
        // Source wiring witness, not a boot or production user-copy test.
        let main = include_str!("main.rs");
        let initial = main.split_once("fn spawn_initd_via_loaderd(").unwrap().1;
        assert!(initial.find("job_lease::ensure(&mut leases[INITD_LEASE_INDEX])").unwrap()
            < initial.find("spawn_exec_via_loaderd_cooperative(").unwrap());
        let failure = initial.split_once("Err(_) => {").unwrap().1;
        assert!(failure.find("job_lease::retire(&mut leases[INITD_LEASE_INDEX])").unwrap()
            < failure.find("attempts = attempts.saturating_add(1)").unwrap());
        let restart = main.split_once("fn restart_lease(").unwrap().1;
        assert!(restart.find("job_lease::retire(&mut leases[index])").unwrap()
            < restart.find("job_lease::ensure(&mut leases[index])").unwrap());
        let blocking = main.split_once("fn spawn_exec_via_loaderd_blocking(").unwrap().1;
        let expected = blocking.find("jobs::info(job_handle)?.id").unwrap();
        let send = blocking.find("jobs::call_loader(endpoint as u64, job_handle, request, response)").unwrap();
        let verify = blocking.find("jobs::process_job(response.pid as u64) != Ok(expected_job)").unwrap();
        assert!(expected < send && send < verify && verify < blocking.find("Ok(response.pid as u64)").unwrap());
    }

    #[test]
    fn bounded_job_retirement_retains_authority_until_empty_then_closes_once() {
        let mut lease = lease(BOOTSTRAP_MANIFEST[0]);
        lease.job_handle = Some(9);
        let mut closed = 0;
        assert_eq!(retire_with(&mut lease, |fd| { assert_eq!(fd, 9); Ok(128) }, |_| closed += 1), Err(11));
        assert_eq!((lease.job_handle, closed), (Some(9), 0));
        assert_eq!(retire_with(&mut lease, |_| Ok(0), |fd| { assert_eq!(fd, 9); closed += 1 }), Ok(()));
        assert_eq!((lease.job_handle, closed), (None, 1));
        assert_eq!(retire_with(&mut lease, |_| panic!("already retired"), |_| panic!("double close")), Ok(()));
    }

    #[test]
    fn loader_replacement_drains_apps_before_releasing_the_service_job() {
        let mut lease = lease(BOOTSTRAP_MANIFEST[0]);
        lease.job_handle = Some(9);
        lease.application_realm = Some(10);
        lease.application_realm_configured = true;
        let mut closed = std::vec::Vec::new();
        assert_eq!(retire_with(&mut lease, |fd| { assert_eq!(fd, 10); Ok(1) }, |fd| closed.push(fd)), Err(11));
        assert_eq!((lease.job_handle, lease.application_realm), (Some(9), Some(10)));
        assert!(lease.application_realm_configured);
        assert!(closed.is_empty());
        let mut killed = std::vec::Vec::new();
        assert_eq!(retire_with(&mut lease, |fd| { killed.push(fd); Ok(0) }, |fd| closed.push(fd)), Ok(()));
        assert_eq!(killed, [10, 9]);
        assert_eq!(closed, [10, 9]);
        assert_eq!((lease.job_handle, lease.application_realm), (None, None));
        assert!(!lease.application_realm_configured);
    }

    #[test]
    fn stale_job_is_closed_but_denied_kill_keeps_the_supervisors_descriptor() {
        let mut lease = lease(BOOTSTRAP_MANIFEST[0]);
        lease.job_handle = Some(9);
        assert_eq!(retire_with(&mut lease, |_| Err(1), |_| panic!("denial lost authority")), Err(1));
        assert_eq!(lease.job_handle, Some(9));
        let mut closed = 0;
        assert_eq!(retire_with(&mut lease, |_| Err(3), |_| closed += 1), Ok(()));
        assert_eq!((lease.job_handle, closed), (None, 1));
    }
}
