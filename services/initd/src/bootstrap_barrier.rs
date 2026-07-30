//! Pure dependency predicates for initd's overlapped foundation-service boot.
//!
//! - **Owner:** `initd` owns exact post-init child admission.
//! - **Boundary:** A spawned child and an endpoint published by another PID
//!   are both untrusted dependency claims.
//! - **Lifecycle:** Activate independent children, observe exact endpoint
//!   ownership, then open runtimed/storaged consumer barriers.
//! - **Concurrency:** Predicates inspect one supervisor-owned snapshot; no
//!   thread or syscall is hidden in this module.
//! - **Failure:** Partial, foreign, or stale admission keeps the barrier shut.
//! - **Forbidden:** Activation alone never becomes dependency authority.
//! - **Evidence:** `post-init-bootstrap-barrier`.

use std::collections::BTreeMap;

use rustos_user_abi::syscall::{IPC_SERVICE_DEVMGRD, IPC_SERVICE_INPUTD, IPC_SERVICE_NETD};

use super::{
    rootd_exec_for_service_id, RunningService, DEVMGRD_EXEC_PATH, INPUTD_EXEC_PATH, NETD_EXEC_PATH,
    RUNTIMED_EXEC_PATH, STORAGED_EXEC_PATH,
};

pub(super) const RUNTIMED_BOOTSTRAP_SERVICES: [u64; 3] =
    [IPC_SERVICE_NETD, IPC_SERVICE_DEVMGRD, IPC_SERVICE_INPUTD];

pub(super) fn endpoint_admission_may_overlap(exec: &str) -> bool {
    matches!(exec, NETD_EXEC_PATH | DEVMGRD_EXEC_PATH | INPUTD_EXEC_PATH)
}

pub(super) fn consumer_requires_bootstrap_barrier(exec: &str) -> bool {
    matches!(exec, RUNTIMED_EXEC_PATH | STORAGED_EXEC_PATH)
}

pub(super) fn bootstrap_endpoint_admissions_complete(
    running: &BTreeMap<i32, RunningService>,
) -> bool {
    RUNTIMED_BOOTSTRAP_SERVICES.into_iter().all(|service_id| {
        rootd_exec_for_service_id(service_id).is_some_and(|exec| {
            running
                .values()
                .any(|service| service.exec == exec && service.endpoint_ready)
        })
    })
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::{
        bootstrap_endpoint_admissions_complete, consumer_requires_bootstrap_barrier,
        endpoint_admission_may_overlap,
    };
    use crate::{
        RunningService, DEVMGRD_EXEC_PATH, INPUTD_EXEC_PATH, NETD_EXEC_PATH, RUNTIMED_EXEC_PATH,
        STORAGED_EXEC_PATH,
    };

    #[test]
    fn independent_bootstrap_activation_overlaps_only_before_consumer_barriers() {
        for exec in [NETD_EXEC_PATH, DEVMGRD_EXEC_PATH, INPUTD_EXEC_PATH] {
            assert!(endpoint_admission_may_overlap(exec));
            assert!(!consumer_requires_bootstrap_barrier(exec));
        }
        assert!(!endpoint_admission_may_overlap(RUNTIMED_EXEC_PATH));
        assert!(consumer_requires_bootstrap_barrier(RUNTIMED_EXEC_PATH));
        assert!(consumer_requires_bootstrap_barrier(STORAGED_EXEC_PATH));
    }

    #[test]
    fn dependency_packages_exclude_spawned_but_unadmitted_endpoints() {
        let mut running = BTreeMap::new();
        running.insert(
            41,
            RunningService {
                package_id: "netd".into(),
                exec: NETD_EXEC_PATH.into(),
                restart: true,
                endpoint_ready: false,
            },
        );
        running.insert(
            42,
            RunningService {
                package_id: "devmgrd".into(),
                exec: DEVMGRD_EXEC_PATH.into(),
                restart: true,
                endpoint_ready: true,
            },
        );
        let spawned = running
            .values()
            .map(|service| service.package_id.as_str())
            .collect::<Vec<_>>();
        let admitted = running
            .values()
            .filter(|service| service.endpoint_ready)
            .map(|service| service.package_id.as_str())
            .collect::<Vec<_>>();
        assert_eq!(spawned, ["netd", "devmgrd"]);
        assert_eq!(admitted, ["devmgrd"]);
    }

    #[test]
    fn bootstrap_barrier_requires_every_exact_endpoint_admission() {
        let mut running = BTreeMap::new();
        for (pid, (package, exec)) in [
            (41, ("netd", NETD_EXEC_PATH)),
            (42, ("devmgrd", DEVMGRD_EXEC_PATH)),
            (43, ("inputd", INPUTD_EXEC_PATH)),
        ] {
            running.insert(
                pid,
                RunningService {
                    package_id: package.into(),
                    exec: exec.into(),
                    restart: true,
                    endpoint_ready: true,
                },
            );
        }
        assert!(bootstrap_endpoint_admissions_complete(&running));
        running.get_mut(&42).expect("devmgrd").endpoint_ready = false;
        assert!(!bootstrap_endpoint_admissions_complete(&running));
    }
}
