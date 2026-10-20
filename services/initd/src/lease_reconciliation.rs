//! Rootd lease reconciliation and bounded read-only query recovery.
//!
//! Initd owns supervision policy. An IPC capacity/overlap refusal is not proof
//! that a lease exited: only a fresh authenticated rootd snapshot may change
//! local service ownership. Failed queries grant nothing and retain one fixed
//! five-second operation deadline across the ordinary 100 ms polling turns.

use std::collections::{BTreeMap, BTreeSet};
use std::time::{Duration, Instant};

use rustos_user_abi::performance::IPC_BOOT_CONTROL_HARD_LIMIT_MS;

use super::{
    boot_line, fail_closed, reclaim_rootd_post_init_lease, record_launch_failure,
    rootd_post_init_lease_query, rootd_service_id_for_exec,
    wait_reported_service_endpoint_with_timeout, RetryState, RunningService, StartupLaunchEntry,
    ROOTD_LEASE_RECOVERY_TIMEOUT, ROOTD_LEASE_STATE_EMPTY, ROOTD_LEASE_STATE_EXITED,
    ROOTD_LEASE_STATE_RUNNING,
};

#[derive(Clone, Copy, Debug)]
struct RootdLeaseRecovery {
    pid: i32,
    deadline: Instant,
}

/// Supervisor-loop state: per-service stale-lease adoption deadlines plus the
/// bounded query-retry budget.
#[derive(Default)]
pub(super) struct RootdLeaseReconciler {
    leases: BTreeMap<u64, RootdLeaseRecovery>,
    queries: LeaseQueryRecovery,
}

#[derive(Default)]
struct LeaseQueryRecovery {
    deadlines: BTreeMap<u64, Instant>,
}

struct QueryAttempt {
    deadline: Instant,
    timeout_ms: u64,
}

impl LeaseQueryRecovery {
    fn begin(&self, service_id: u64, now: Instant) -> Result<QueryAttempt, i32> {
        let deadline = self
            .deadlines
            .get(&service_id)
            .copied()
            .unwrap_or_else(|| now + Duration::from_millis(IPC_BOOT_CONTROL_HARD_LIMIT_MS));
        let remaining = deadline.saturating_duration_since(now).as_millis();
        // A zero-millisecond wire timeout is invalid. Refuse rather than round
        // up and widen the original deadline or start a new five-second call.
        if remaining == 0 {
            return Err(libc::ETIMEDOUT);
        }
        Ok(QueryAttempt {
            deadline,
            timeout_ms: u64::try_from(remaining)
                .unwrap_or(IPC_BOOT_CONTROL_HARD_LIMIT_MS)
                .min(IPC_BOOT_CONTROL_HARD_LIMIT_MS),
        })
    }

    fn complete(
        &mut self,
        service_id: u64,
        attempt: QueryAttempt,
        now: Instant,
        result: Result<(u64, u16), i32>,
    ) -> Result<Option<(u64, u16)>, i32> {
        match result {
            Ok(snapshot) => {
                self.deadlines.remove(&service_id);
                Ok(Some(snapshot))
            }
            Err(errno) if query_refusal_is_transient(errno) && now < attempt.deadline => {
                self.deadlines.insert(service_id, attempt.deadline);
                Ok(None)
            }
            Err(errno) => {
                self.deadlines.remove(&service_id);
                Err(errno)
            }
        }
    }
}

/// Capacity, overlap and interruption refusals say nothing about the lease;
/// identity, authority and peer-death failures stay fatal.
fn query_refusal_is_transient(errno: i32) -> bool {
    matches!(
        errno,
        libc::ENOSPC | libc::EAGAIN | libc::EBUSY | libc::EINTR | libc::ETIMEDOUT
    )
}

impl RootdLeaseReconciler {
    /// A rootd lease is still awaiting adoption or reclaim for this service.
    pub(super) fn recovering(&self, service_id: u64) -> bool {
        self.leases.contains_key(&service_id)
    }

    pub(super) fn reconcile(
        &mut self,
        startup_entries: &[StartupLaunchEntry],
        running: &mut BTreeMap<i32, RunningService>,
        launched_once_packages: &mut BTreeSet<String>,
        retry_state: &mut BTreeMap<String, RetryState>,
        now: Instant,
    ) {
        let recovery = &mut self.leases;
        for entry in startup_entries {
            let Some(service_id) = rootd_service_id_for_exec(entry.exec.as_str()) else {
                continue;
            };
            let query_failed = |errno: i32| -> ! {
                fail_closed(&format!(
                "initd: fatal rootd post-init lease query failed service_id={service_id} errno={errno}"
            ))
            };
            let attempt = self
                .queries
                .begin(service_id, now)
                .unwrap_or_else(|errno| query_failed(errno));
            let result = rootd_post_init_lease_query(service_id, attempt.timeout_ms);
            let Some((pid, state)) = self
                .queries
                .complete(service_id, attempt, Instant::now(), result)
                .unwrap_or_else(|errno| query_failed(errno))
            else {
                // Transient refusal: keep every local lease and retry on the next
                // reconciliation turn within the first query's deadline.
                continue;
            };
            match state {
                ROOTD_LEASE_STATE_EMPTY => {
                    recovery.remove(&service_id);
                }
                ROOTD_LEASE_STATE_RUNNING => {
                    let pid = i32::try_from(pid).unwrap_or_else(|_| {
                        fail_closed(&format!(
                        "initd: fatal rootd lease pid overflow service_id={service_id} pid={pid}"
                    ))
                    });
                    if running.contains_key(&pid) {
                        recovery.remove(&service_id);
                        continue;
                    }
                    let state = recovery.entry(service_id).or_insert(RootdLeaseRecovery {
                        pid,
                        deadline: now + ROOTD_LEASE_RECOVERY_TIMEOUT,
                    });
                    if state.pid != pid {
                        *state = RootdLeaseRecovery {
                            pid,
                            deadline: now + ROOTD_LEASE_RECOVERY_TIMEOUT,
                        };
                    }
                    match wait_reported_service_endpoint_with_timeout(entry.exec.as_str(), pid, 1) {
                    Ok(()) => {
                        running.insert(
                            pid,
                            RunningService {
                                package_id: entry.package_id.clone(),
                                exec: entry.exec.clone(),
                                restart: entry.restart,
                                endpoint_ready: true,
                            },
                        );
                        if !entry.restart {
                            launched_once_packages.insert(entry.package_id.clone());
                        }
                        recovery.remove(&service_id);
                        boot_line(&format!(
                            "initd: adopted rootd lease exec={} pid={pid}",
                            entry.exec
                        ));
                    }
                    Err(errno) if errno == libc::ETIMEDOUT && now < state.deadline => {}
                    Err(errno) if errno == libc::ETIMEDOUT || errno == libc::ESRCH => {
                        reclaim_rootd_post_init_lease(service_id, pid as u64).unwrap_or_else(
                            |reclaim_errno| {
                                fail_closed(&format!(
                                    "initd: fatal rootd stale lease reclaim failed service_id={service_id} pid={pid} errno={reclaim_errno}"
                                ))
                            },
                        );
                        recovery.remove(&service_id);
                    }
                    Err(errno) => fail_closed(&format!(
                        "initd: fatal rootd lease endpoint probe failed service_id={service_id} pid={pid} errno={errno}"
                    )),
                }
                }
                ROOTD_LEASE_STATE_EXITED => {
                    recovery.remove(&service_id);
                    if let Some(service) = running.remove(&i32::try_from(pid).unwrap_or(-1)) {
                        if service.restart {
                            record_launch_failure(
                                retry_state,
                                service.exec.as_str(),
                                "rootd-exit",
                                0,
                            );
                        }
                    }
                    if pid != 0 {
                        reclaim_rootd_post_init_lease(service_id, pid).unwrap_or_else(|errno| {
                        fail_closed(&format!(
                            "initd: fatal rootd exited lease reclaim failed service_id={service_id} pid={pid} errno={errno}"
                        ))
                    });
                    }
                }
                _ => {
                    recovery.remove(&service_id);
                    if pid != 0 {
                        reclaim_rootd_post_init_lease(service_id, pid).unwrap_or_else(|errno| {
                        fail_closed(&format!(
                            "initd: fatal rootd terminal lease reclaim failed service_id={service_id} pid={pid} errno={errno}"
                        ))
                    });
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transient_query_refusal_preserves_state_and_one_original_deadline() {
        let now = Instant::now();
        let mut recovery = LeaseQueryRecovery::default();
        let first = recovery.begin(4, now).unwrap();
        let deadline = first.deadline;
        assert_eq!(first.timeout_ms, IPC_BOOT_CONTROL_HARD_LIMIT_MS);
        assert_eq!(
            recovery.complete(4, first, now, Err(libc::ENOSPC)),
            Ok(None)
        );
        let later = now + Duration::from_millis(100);
        let retry = recovery.begin(4, later).unwrap();
        assert_eq!(retry.deadline, deadline);
        assert_eq!(retry.timeout_ms, IPC_BOOT_CONTROL_HARD_LIMIT_MS - 100);
        assert_eq!(
            recovery.complete(4, retry, later, Err(libc::EAGAIN)),
            Ok(None)
        );
        assert_eq!(recovery.begin(4, deadline).err(), Some(libc::ETIMEDOUT));
    }

    #[test]
    fn only_fresh_success_clears_query_failure_and_supplies_a_lease_snapshot() {
        let now = Instant::now();
        let mut recovery = LeaseQueryRecovery::default();
        let first = recovery.begin(4, now).unwrap();
        assert_eq!(recovery.complete(4, first, now, Err(libc::EINTR)), Ok(None));
        let retry = recovery.begin(4, now + Duration::from_millis(200)).unwrap();
        let snapshot = (73, ROOTD_LEASE_STATE_RUNNING);
        assert_eq!(
            recovery.complete(4, retry, now, Ok(snapshot)),
            Ok(Some(snapshot))
        );
        assert!(recovery.deadlines.is_empty());
    }

    #[test]
    fn permanent_errors_and_expired_transport_deadlines_remain_fatal() {
        let now = Instant::now();
        for errno in [
            libc::EINVAL,
            libc::EPERM,
            libc::EACCES,
            libc::EPIPE,
            libc::ESRCH,
        ] {
            let mut recovery = LeaseQueryRecovery::default();
            let first = recovery.begin(4, now).unwrap();
            assert_eq!(recovery.complete(4, first, now, Err(errno)), Err(errno));
            assert!(recovery.deadlines.is_empty());
        }
        let mut recovery = LeaseQueryRecovery::default();
        let first = recovery.begin(4, now).unwrap();
        let end = first.deadline;
        assert_eq!(
            recovery.complete(4, first, end, Err(libc::ETIMEDOUT)),
            Err(libc::ETIMEDOUT)
        );
    }

    #[test]
    fn unavailable_service_cannot_reset_another_services_recovery_budget() {
        let now = Instant::now();
        let mut recovery = LeaseQueryRecovery::default();
        let first = recovery.begin(4, now).unwrap();
        let deadline = first.deadline;
        assert_eq!(
            recovery.complete(4, first, now, Err(libc::ENOSPC)),
            Ok(None)
        );
        let other = recovery.begin(8, now + Duration::from_millis(250)).unwrap();
        assert_eq!(
            recovery.complete(8, other, now, Ok((74, ROOTD_LEASE_STATE_RUNNING))),
            Ok(Some((74, ROOTD_LEASE_STATE_RUNNING)))
        );
        assert_eq!(recovery.begin(4, now).unwrap().deadline, deadline);
    }
}
