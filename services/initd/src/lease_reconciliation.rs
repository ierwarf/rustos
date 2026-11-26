//! Rootd lease reconciliation and bounded read-only query recovery.
//!
//! Initd owns supervision policy. An IPC capacity/overlap refusal is not proof
//! that a lease exited: only a fresh authenticated rootd snapshot may change
//! local service ownership. Failed queries grant nothing and retain one fixed
//! five-second operation deadline across the ordinary 100 ms polling turns.
//!
//! A *slow* rootd is not a *dead* rootd. During startup the five-second
//! deadline stays fatal (the boot has its own bounded budget). After startup a
//! query that exhausts its deadline is a degraded-supervision event: initd keeps
//! every local lease, backs off (100 ms doubling to a 5 s cap) and asks again.
//! Only evidence that rootd or the lease is really gone (peer death, identity
//! or authority refusal, a malformed snapshot) escalates to a fatal exit.
//! Exiting on a slow reply would orphan every service initd reported exactly
//! when the machine is starved, which is when supervision matters most.

use std::collections::{BTreeMap, BTreeSet};
use std::time::{Duration, Instant};

use rustos_user_abi::performance::IPC_BOOT_CONTROL_HARD_LIMIT_MS;

use super::{
    boot_line, fail_closed, reclaim_rootd_post_init_lease, record_launch_failure,
    rootd_post_init_lease_query, rootd_service_id_for_exec, startup_timeout_ms,
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
    slow: BTreeMap<u64, SlowQuery>,
}

/// Post-startup bookkeeping for a service whose lease query keeps timing out.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct SlowQuery {
    streak: u32,
    retry_at: Instant,
}

const SLOW_QUERY_BACKOFF_BASE: Duration = Duration::from_millis(100);
const SLOW_QUERY_BACKOFF_CAP: Duration = Duration::from_secs(5);

fn slow_query_backoff(streak: u32) -> Duration {
    let shift = streak.saturating_sub(1).min(16);
    SLOW_QUERY_BACKOFF_BASE
        .saturating_mul(1_u32 << shift)
        .min(SLOW_QUERY_BACKOFF_CAP)
}

/// Transport-level refusals that mean "no timely answer", not "wrong answer".
pub(super) fn is_slow_peer_errno(errno: i32) -> bool {
    matches!(
        errno,
        libc::ETIMEDOUT | libc::EAGAIN | libc::EBUSY | libc::ENOSPC | libc::EINTR
    )
}

struct QueryAttempt {
    deadline: Instant,
    timeout_ms: u64,
}

impl LeaseQueryRecovery {
    #[cfg(test)]
    fn begin(&self, service_id: u64, now: Instant) -> Result<QueryAttempt, i32> {
        self.begin_parent(service_id, now, None)
    }

    fn begin_parent(
        &self,
        service_id: u64,
        now: Instant,
        parent: Option<Instant>,
    ) -> Result<QueryAttempt, i32> {
        let deadline = self
            .deadlines
            .get(&service_id)
            .copied()
            .unwrap_or_else(|| now + Duration::from_millis(IPC_BOOT_CONTROL_HARD_LIMIT_MS));
        let deadline = parent.map_or(deadline, |end| end.min(deadline));
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

    /// True while a previously timed-out service is still backing off.
    fn backing_off(&self, service_id: u64, now: Instant) -> bool {
        self.slow
            .get(&service_id)
            .is_some_and(|slow| now < slow.retry_at)
    }

    /// Records one expired query deadline after startup and returns the new
    /// consecutive-timeout streak. Never fatal; the next attempt gets a fresh
    /// five-second deadline once the backoff elapses.
    fn note_slow(&mut self, service_id: u64, now: Instant) -> u32 {
        self.deadlines.remove(&service_id);
        let streak = self
            .slow
            .get(&service_id)
            .map_or(1, |slow| slow.streak.saturating_add(1));
        self.slow.insert(
            service_id,
            SlowQuery {
                streak,
                retry_at: now + slow_query_backoff(streak),
            },
        );
        streak
    }

    fn complete(
        &mut self,
        service_id: u64,
        attempt: QueryAttempt,
        now: Instant,
        result: Result<(u64, u16), i32>,
    ) -> Result<Option<(u64, u16)>, i32> {
        if now >= attempt.deadline {
            self.deadlines.remove(&service_id);
            return Err(libc::ETIMEDOUT);
        }
        match result {
            Ok(snapshot) => {
                self.deadlines.remove(&service_id);
                self.slow.remove(&service_id);
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

/// After startup completes a slow peer degrades supervision instead of
/// ending it; during startup the boot's own bounded budget stays authoritative.
fn degrade_on_slow_peer(parent_deadline: Option<Instant>, errno: i32) -> bool {
    parent_deadline.is_none() && is_slow_peer_errno(errno)
}

/// Logs a slow-rootd episode at streaks 1, 2, 4, 8, ... (allocation only when
/// logging) and arms the backoff. Never exits.
fn note_slow_rootd(queries: &mut LeaseQueryRecovery, service_id: u64, exec: &str) {
    let streak = queries.note_slow(service_id, Instant::now());
    if streak.is_power_of_two() {
        boot_line(&format!(
            "initd: rootd slow service_id={service_id} exec={exec} timeouts={streak} degraded=1 retry_ms={}",
            slow_query_backoff(streak).as_millis()
        ));
    }
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
        parent_deadline: Option<Instant>,
        initially_admitted: &mut BTreeSet<String>,
    ) -> bool {
        let mut running_changed = false;
        let recovery = &mut self.leases;
        for entry in startup_entries {
            let Some(service_id) = rootd_service_id_for_exec(entry.exec.as_str()) else {
                continue;
            };
            let query_failed = |errno: i32| -> ! {
                if parent_deadline.is_some() {
                    fail_closed(&format!("initd: fatal startup failure v=1 phase=lease-query exec={} service_id={service_id} errno={errno}", entry.exec));
                }
                fail_closed(&format!(
                "initd: fatal rootd post-init lease query failed service_id={service_id} errno={errno}"
            ))
            };
            if parent_deadline.is_none() && self.queries.backing_off(service_id, Instant::now()) {
                continue;
            }
            let attempt = match self
                .queries
                .begin_parent(service_id, Instant::now(), parent_deadline)
            {
                Ok(attempt) => attempt,
                Err(errno) if degrade_on_slow_peer(parent_deadline, errno) => {
                    note_slow_rootd(&mut self.queries, service_id, entry.exec.as_str());
                    continue;
                }
                Err(errno) => query_failed(errno),
            };
            let result = rootd_post_init_lease_query(service_id, attempt.timeout_ms);
            let snapshot = match self
                .queries
                .complete(service_id, attempt, Instant::now(), result)
            {
                Ok(snapshot) => snapshot,
                Err(errno) if degrade_on_slow_peer(parent_deadline, errno) => {
                    note_slow_rootd(&mut self.queries, service_id, entry.exec.as_str());
                    continue;
                }
                Err(errno) => query_failed(errno),
            };
            let Some((pid, state)) = snapshot
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
                    match startup_timeout_ms(parent_deadline, 1, Instant::now()).and_then(|timeout| wait_reported_service_endpoint_with_timeout(entry.exec.as_str(), pid, timeout)) {
                    Ok(()) => {
                        startup_timeout_ms(parent_deadline, 1, Instant::now()).unwrap_or_else(|errno| query_failed(errno));
                        initially_admitted.insert(entry.package_id.clone());
                        running.insert(
                            pid,
                            RunningService {
                                package_id: entry.package_id.clone(),
                                exec: entry.exec.clone(),
                                restart: entry.restart,
                                endpoint_ready: true,
                            },
                        );
                        running_changed = true;
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
                        match reclaim_rootd_post_init_lease(service_id, pid as u64, parent_deadline) {
                            Ok(()) => {
                                recovery.remove(&service_id);
                            }
                            // Slow rootd: keep the recovery entry and retry on the next turn.
                            Err(errno) if degrade_on_slow_peer(parent_deadline, errno) => {
                                note_slow_rootd(&mut self.queries, service_id, entry.exec.as_str());
                            }
                            Err(reclaim_errno) => fail_closed(&format!(
                                "initd: fatal rootd stale lease reclaim failed service_id={service_id} pid={pid} errno={reclaim_errno}"
                            )),
                        }
                    }
                    Err(errno) => fail_closed(&format!(
                        "initd: fatal rootd lease endpoint probe failed service_id={service_id} pid={pid} errno={errno}"
                    )),
                }
                }
                ROOTD_LEASE_STATE_EXITED => {
                    recovery.remove(&service_id);
                    if let Some(service) = running.remove(&i32::try_from(pid).unwrap_or(-1)) {
                        running_changed = true;
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
                        match reclaim_rootd_post_init_lease(service_id, pid, parent_deadline) {
                            Ok(()) => {}
                            Err(errno) if degrade_on_slow_peer(parent_deadline, errno) => {
                                note_slow_rootd(&mut self.queries, service_id, entry.exec.as_str());
                            }
                            Err(errno) => fail_closed(&format!(
                                "initd: fatal rootd exited lease reclaim failed service_id={service_id} pid={pid} errno={errno}"
                            )),
                        }
                    }
                }
                _ => {
                    recovery.remove(&service_id);
                    if pid != 0 {
                        match reclaim_rootd_post_init_lease(service_id, pid, parent_deadline) {
                            Ok(()) => {}
                            Err(errno) if degrade_on_slow_peer(parent_deadline, errno) => {
                                note_slow_rootd(&mut self.queries, service_id, entry.exec.as_str());
                            }
                            Err(errno) => fail_closed(&format!(
                                "initd: fatal rootd terminal lease reclaim failed service_id={service_id} pid={pid} errno={errno}"
                            )),
                        }
                    }
                }
            }
        }
        running_changed
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parent_deadline_clips_query_retries_without_reminting() {
        let now = Instant::now();
        let parent = now + Duration::from_millis(200);
        let mut recovery = LeaseQueryRecovery::default();
        let first = recovery.begin_parent(4, now, Some(parent)).unwrap();
        assert_eq!(first.timeout_ms, 200);
        assert_eq!(
            recovery.complete(4, first, now, Err(libc::EAGAIN)),
            Ok(None)
        );
        let later = now + Duration::from_millis(150);
        assert_eq!(
            recovery
                .begin_parent(4, later, Some(parent))
                .unwrap()
                .timeout_ms,
            50
        );
        assert!(matches!(
            recovery.begin_parent(4, parent, Some(parent)),
            Err(libc::ETIMEDOUT)
        ));
    }

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

    #[test]
    fn slow_backoff_doubles_from_100ms_and_caps_at_five_seconds() {
        let ms = |streak| slow_query_backoff(streak).as_millis();
        assert_eq!(
            [ms(1), ms(2), ms(3), ms(4), ms(6), ms(7), ms(100)],
            [100, 200, 400, 800, 3200, 5000, 5000]
        );
    }

    #[test]
    fn expired_query_deadline_after_startup_degrades_then_retries_with_a_fresh_deadline() {
        let now = Instant::now();
        let mut recovery = LeaseQueryRecovery::default();
        // The rootd call burns its whole five-second deadline (the evidence's
        // errno=110): `complete` reports ETIMEDOUT ...
        let first = recovery.begin(4, now).unwrap();
        let end = first.deadline;
        let err = recovery
            .complete(4, first, end, Err(libc::ETIMEDOUT))
            .unwrap_err();
        assert_eq!(err, libc::ETIMEDOUT);
        // ... which after startup is a degradation, not an exit.
        assert!(degrade_on_slow_peer(None, err));
        assert_eq!(recovery.note_slow(4, end), 1);
        assert!(recovery.backing_off(4, end + Duration::from_millis(50)));
        assert!(!recovery.backing_off(4, end + Duration::from_millis(100)));
        // The retry is a brand-new bounded call, not an already-expired one.
        let retry_at = end + Duration::from_millis(100);
        let retry = recovery.begin(4, retry_at).unwrap();
        assert_eq!(retry.timeout_ms, IPC_BOOT_CONTROL_HARD_LIMIT_MS);
        // A second consecutive timeout backs off longer.
        assert_eq!(recovery.note_slow(4, retry_at + Duration::from_secs(5)), 2);
        // A fresh snapshot ends the episode.
        let again = recovery.begin(4, retry_at + Duration::from_secs(6)).unwrap();
        assert!(recovery
            .complete(4, again, retry_at + Duration::from_secs(6), Ok((73, ROOTD_LEASE_STATE_RUNNING)))
            .unwrap()
            .is_some());
        assert!(recovery.slow.is_empty());
    }

    #[test]
    fn startup_deadline_and_real_peer_death_stay_fatal() {
        let startup = Some(Instant::now() + Duration::from_secs(1));
        assert!(!degrade_on_slow_peer(startup, libc::ETIMEDOUT));
        for errno in [libc::EPIPE, libc::ESRCH, libc::EPERM, libc::EACCES, libc::EINVAL, libc::EPROTO] {
            assert!(!degrade_on_slow_peer(None, errno), "errno {errno} is not slowness");
        }
        for errno in [libc::ETIMEDOUT, libc::EAGAIN, libc::EBUSY, libc::ENOSPC, libc::EINTR] {
            assert!(degrade_on_slow_peer(None, errno));
        }
    }

    #[test]
    fn one_slow_service_does_not_back_off_the_others() {
        let now = Instant::now();
        let mut recovery = LeaseQueryRecovery::default();
        recovery.note_slow(4, now);
        assert!(recovery.backing_off(4, now));
        assert!(!recovery.backing_off(8, now));
    }
}
