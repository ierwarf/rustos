use std::collections::{BTreeMap, BTreeSet};
use std::thread;
use std::time::{Duration, Instant};

use runtime_control::protocol::RUNTIME_WATCH_MAX_WAIT_MS;
use runtime_control::{
    decode_c_string, load_autostart_program_entries, load_startup_entries, DesktopProgramEntry,
    RuntimeClient, StartupMode, DEFAULT_APPLICATIONS_DIR, DEFAULT_AUTOSTART_DIR,
    RUNNING_PROGRAMS_DIGEST_UNKNOWN,
};
/// Cadence while a launch is in flight. Settling and timeout are both judged
/// against this loop, so it has to stay tight whenever something is pending.
const POLL_INTERVAL: Duration = Duration::from_millis(8);
/// Longest an idle session asks runtimed to hold the reply while the running
/// set is unchanged.
///
/// An idle session has one question - "did a program start or exit" - and used
/// to ask it on a timer, which meant a full `snapshot_running_programs` round
/// trip per interval for the life of the session to be told "no" almost every
/// time. Runtimed now answers that question on the edge, so this is only how
/// often the watch re-arms while genuinely nothing happens, and a `restart`
/// entry whose service dies is noticed in one broker pass instead of within a
/// polling interval.
const IDLE_WATCH_WAIT: Duration = Duration::from_millis(RUNTIME_WATCH_MAX_WAIT_MS as u64);
const LAUNCH_SETTLE_DELAY: Duration = Duration::from_millis(40);
const LAUNCH_START_TIMEOUT: Duration = Duration::from_secs(20);
const RETRY_BACKOFF: Duration = Duration::from_millis(200);
/// Launch requests per once-only app before sessiond stops asking. A request
/// that was accepted but never produced a running program is not a launch.
const MAX_LAUNCH_ATTEMPTS: u32 = 3;

#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd)]
struct LaunchEntry {
    package_id: String,
    desktop_file_id: String,
    runtime_deps: Vec<String>,
    restart: bool,
}

#[derive(Clone, Debug)]
struct PendingLaunch {
    desktop_file_id: String,
    package_id: String,
    restart: bool,
    requested_at: Instant,
    /// The program was observed running at least once. If it then vanishes
    /// before settling it exited early; waiting out the launch timeout would
    /// only delay the retry by 20 s.
    first_seen_running: Option<Instant>,
}

impl PendingLaunch {
    fn observe_running(&mut self, now: Instant) -> bool {
        let first_seen = *self.first_seen_running.get_or_insert(now);
        now.saturating_duration_since(first_seen) >= LAUNCH_SETTLE_DELAY
    }
}

fn main() {
    let runtime = match RuntimeClient::open_default() {
        Ok(client) => client,
        Err(err) => {
            observability_client::error!(
                "sessiond",
                service,
                "failed to open runtime device: errno={}",
                err
            );
            return;
        }
    };
    let launch_entries = load_launch_entries();
    let package_by_desktop_id = launch_entries
        .iter()
        .map(|entry| (entry.desktop_file_id.clone(), entry.package_id.clone()))
        .collect::<BTreeMap<_, _>>();
    observability_client::info!(
        "sessiond",
        service,
        "loaded {} desktop/session entries",
        launch_entries.len()
    );
    // Packages that were observed running and settled; dependency authority.
    let mut launched_once_packages = BTreeSet::new();
    let mut launch_attempts = BTreeMap::<String, u32>::new();
    let mut abandoned_packages = BTreeSet::<String>::new();
    let mut pending_launch = None::<PendingLaunch>;
    let mut retry_after = BTreeMap::<String, Instant>::new();
    // Consecutive early exits per desktop id; widens the retry delay so a
    // crash-looping `restart` service is not relaunched every 200 ms.
    let mut early_exits = BTreeMap::<String, u32>::new();
    let mut observed_digest = RUNNING_PROGRAMS_DIGEST_UNKNOWN;

    loop {
        let observed = observe_running_programs(
            &runtime,
            pending_launch.is_some(),
            &mut observed_digest,
            &retry_after,
        );
        let running = match observed {
            Ok(running) => running,
            Err(err) => {
                observability_client::error!(
                    "sessiond",
                    service,
                    "snapshot running failed: errno={}",
                    err
                );
                // A failing runtime channel must not turn into an 8 ms
                // error-log spin; only a pending launch needs the tight cadence.
                thread::sleep(if pending_launch.is_some() {
                    POLL_INTERVAL
                } else {
                    RETRY_BACKOFF
                });
                continue;
            }
        };

        let running_desktop_ids = running
            .iter()
            .map(|program| decode_c_string(&program.desktop_file_id))
            .collect::<BTreeSet<_>>();
        let running_packages = running_desktop_ids
            .iter()
            .map(|desktop_id| {
                package_by_desktop_id
                    .get(desktop_id)
                    .cloned()
                    .unwrap_or_else(|| package_id_from_desktop_id(desktop_id))
            })
            .collect::<BTreeSet<_>>();

        if let Some(pending) = pending_launch.as_mut() {
            if running_desktop_ids.contains(pending.desktop_file_id.as_str()) {
                let settled = pending.observe_running(Instant::now());
                retry_after.remove(pending.desktop_file_id.as_str());
                if settled {
                    observability_client::info!(
                        "sessiond",
                        service,
                        "launch settled for {}",
                        pending.desktop_file_id
                    );
                    if !pending.restart {
                        launched_once_packages.insert(pending.package_id.clone());
                    }
                    launch_attempts.remove(pending.package_id.as_str());
                    early_exits.remove(pending.desktop_file_id.as_str());
                    pending_launch = None;
                }
                thread::sleep(POLL_INTERVAL);
                continue;
            }

            let exited_early = pending.first_seen_running.is_some();
            if !exited_early && pending.requested_at.elapsed() < LAUNCH_START_TIMEOUT {
                thread::sleep(POLL_INTERVAL);
                continue;
            }

            if exited_early {
                observability_client::warn!(
                    "sessiond",
                    service,
                    "{} exited before its launch settled",
                    pending.desktop_file_id
                );
            } else {
                observability_client::warn!(
                    "sessiond",
                    service,
                    "launch timed out waiting for {}",
                    pending.desktop_file_id
                );
            }
            let retry_delay = if exited_early {
                let exits = early_exits.entry(pending.desktop_file_id.clone()).or_insert(0);
                *exits = exits.saturating_add(1);
                RETRY_BACKOFF * (1_u32 << (*exits - 1).min(6))
            } else {
                RETRY_BACKOFF
            };
            retry_after.insert(
                pending.desktop_file_id.clone(),
                Instant::now() + retry_delay,
            );
            if !pending.restart
                && launch_attempts
                    .get(pending.package_id.as_str())
                    .is_some_and(|attempts| *attempts >= MAX_LAUNCH_ATTEMPTS)
            {
                // Dependents must not treat a program that never ran as
                // launched, so this is not recorded in `launched_once_packages`.
                observability_client::error!(
                    "sessiond",
                    service,
                    "giving up on {} after {} launch attempts",
                    pending.desktop_file_id,
                    MAX_LAUNCH_ATTEMPTS
                );
                abandoned_packages.insert(pending.package_id.clone());
            }
            pending_launch = None;
        }

        for entry in &launch_entries {
            if !runtime_deps_satisfied(
                &entry.runtime_deps,
                &running_packages,
                &launched_once_packages,
            ) {
                continue;
            }
            if retry_after
                .get(entry.desktop_file_id.as_str())
                .is_some_and(|deadline| Instant::now() < *deadline)
            {
                continue;
            }
            if entry.restart {
                if running_packages.contains(entry.package_id.as_str()) {
                    continue;
                }
                observability_client::info!(
                    "sessiond",
                    service,
                    "ensuring desktop service {}",
                    entry.desktop_file_id
                );
            } else if launched_once_packages.contains(entry.package_id.as_str())
                || abandoned_packages.contains(entry.package_id.as_str())
            {
                continue;
            } else {
                observability_client::info!(
                    "sessiond",
                    service,
                    "launching desktop app {}",
                    entry.desktop_file_id
                );
            }

            match runtime.request_launch_program_new_session(entry.desktop_file_id.as_str()) {
                Ok(()) => {
                    if !entry.restart {
                        *launch_attempts.entry(entry.package_id.clone()).or_insert(0) += 1;
                    }
                    retry_after.remove(entry.desktop_file_id.as_str());
                    observability_client::info!(
                        "sessiond",
                        service,
                        "launch request queued for {}",
                        entry.desktop_file_id
                    );
                    pending_launch = Some(PendingLaunch {
                        desktop_file_id: entry.desktop_file_id.clone(),
                        package_id: entry.package_id.clone(),
                        restart: entry.restart,
                        requested_at: Instant::now(),
                        first_seen_running: None,
                    });
                    break;
                }
                Err(err) => {
                    retry_after.insert(
                        entry.desktop_file_id.clone(),
                        Instant::now() + RETRY_BACKOFF,
                    );
                    observability_client::error!(
                        "sessiond",
                        service,
                        "launch {} failed: errno={err}",
                        entry.desktop_file_id
                    );
                }
            }
        }

        // A pending launch owns the tight cadence. An idle session does not
        // sleep at all: it has already spent this pass parked inside the watch
        // above, so sleeping here would only add latency to the edge it is
        // waiting for.
        if pending_launch.is_some() {
            thread::sleep(POLL_INTERVAL);
        }
    }
}

/// Observe the running set the way this pass needs it.
///
/// While a launch is in flight the loop is judging settle and timeout deadlines
/// of its own, so it takes an immediate snapshot and keeps its tight cadence.
/// With nothing pending it has no deadline of its own to keep and parks on the
/// change edge instead, bounded by the soonest launch retry so a backed-off
/// entry is not held past its own deadline.
fn observe_running_programs(
    runtime: &RuntimeClient,
    launch_pending: bool,
    observed_digest: &mut u64,
    retry_after: &BTreeMap<String, Instant>,
) -> Result<Vec<runtime_control::RuntimeRunningProgram>, i32> {
    if launch_pending {
        // The set is about to change underneath us, so nothing learned here is
        // worth carrying into the next watch.
        *observed_digest = RUNNING_PROGRAMS_DIGEST_UNKNOWN;
        return runtime.snapshot_running_programs();
    }

    let (running, digest) =
        runtime.watch_running_programs(*observed_digest, idle_watch_wait(retry_after))?;
    *observed_digest = digest;
    Ok(running)
}

fn idle_watch_wait(retry_after: &BTreeMap<String, Instant>) -> Duration {
    let now = Instant::now();
    retry_after
        .values()
        .map(|deadline| deadline.saturating_duration_since(now))
        // A deadline already past has had its pass: the launch loop ran before
        // this call. Letting it contribute zero here would park for no time at
        // all and spin whenever an entry is retained across a pass that skipped
        // it - a dep-blocked entry, or a `restart` package something else
        // started. The floor bounds the same case to the pending cadence.
        .filter(|remaining| !remaining.is_zero())
        .min()
        .unwrap_or(IDLE_WATCH_WAIT)
        .clamp(POLL_INTERVAL, IDLE_WATCH_WAIT)
}

fn load_launch_entries() -> Vec<LaunchEntry> {
    let mut entries = BTreeSet::new();

    for entry in load_startup_registry_desktop_entries() {
        entries.insert(entry);
    }
    for entry in load_autostart_entries() {
        entries.insert(entry);
    }

    let mut launch_entries = entries.into_iter().collect::<Vec<_>>();
    launch_entries.sort_by(|lhs, rhs| {
        rhs.restart
            .cmp(&lhs.restart)
            .then_with(|| lhs.desktop_file_id.cmp(&rhs.desktop_file_id))
    });
    launch_entries
}

fn load_startup_registry_desktop_entries() -> Vec<LaunchEntry> {
    let Ok(entries) = load_startup_entries(DEFAULT_APPLICATIONS_DIR) else {
        observability_client::warn!(
            "sessiond",
            service,
            "application desktop dir unavailable: {DEFAULT_APPLICATIONS_DIR}"
        );
        return Vec::new();
    };

    entries
        .into_iter()
        .filter(|entry| entry.mode == StartupMode::Desktop)
        .map(|entry| LaunchEntry {
            restart: entry.exec.starts_with("services/"),
            package_id: entry.package_id,
            desktop_file_id: entry.desktop_file_id,
            runtime_deps: entry.runtime_deps,
        })
        .collect()
}

fn load_autostart_entries() -> Vec<LaunchEntry> {
    let Ok(entries) = load_autostart_program_entries(DEFAULT_AUTOSTART_DIR) else {
        observability_client::warn!(
            "sessiond",
            service,
            "autostart directory unavailable: {DEFAULT_AUTOSTART_DIR}"
        );
        return Vec::new();
    };
    entries.into_iter().map(desktop_launch_entry).collect()
}

fn desktop_launch_entry(entry: DesktopProgramEntry) -> LaunchEntry {
    LaunchEntry {
        restart: entry.exec.starts_with("services/"),
        package_id: entry.package_id,
        desktop_file_id: entry.desktop_file_id,
        runtime_deps: entry.runtime_deps,
    }
}

fn runtime_deps_satisfied(
    deps: &[String],
    running_packages: &BTreeSet<String>,
    launched_once_packages: &BTreeSet<String>,
) -> bool {
    deps.iter()
        .all(|dep| running_packages.contains(dep) || launched_once_packages.contains(dep))
}

fn package_id_from_desktop_id(desktop_id: &str) -> String {
    desktop_id
        .strip_suffix(".desktop")
        .unwrap_or(desktop_id)
        .to_string()
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::time::{Duration, Instant};

    use super::{idle_watch_wait, PendingLaunch, IDLE_WATCH_WAIT, LAUNCH_SETTLE_DELAY, POLL_INTERVAL};

    #[test]
    fn delayed_first_observation_does_not_settle_a_new_process() {
        let requested_at = Instant::now();
        let mut pending = PendingLaunch {
            desktop_file_id: String::from("app.desktop"),
            package_id: String::from("app"),
            restart: false,
            requested_at,
            first_seen_running: None,
        };
        let first_seen = requested_at + Duration::from_secs(1);
        assert!(!pending.observe_running(first_seen));
        assert!(!pending.observe_running(first_seen + LAUNCH_SETTLE_DELAY - Duration::from_millis(1)));
        assert!(pending.observe_running(first_seen + LAUNCH_SETTLE_DELAY));
    }

    #[test]
    fn an_idle_session_parks_without_slowing_launch_settling() {
        // Settling is judged by this loop, so the pending cadence has to stay
        // fast enough to observe the settle delay rather than overshoot it.
        assert!(POLL_INTERVAL < LAUNCH_SETTLE_DELAY);
        // And the idle wait has to be a real park, or the round trip it was
        // meant to remove is still being paid on a timer.
        assert!(IDLE_WATCH_WAIT >= POLL_INTERVAL * 8);
        assert_eq!(idle_watch_wait(&BTreeMap::new()), IDLE_WATCH_WAIT);
    }

    #[test]
    fn a_pending_retry_shortens_the_park_without_ever_reaching_zero() {
        let now = Instant::now();
        let mut retry_after = BTreeMap::new();
        retry_after.insert(
            String::from("late.desktop"),
            now + Duration::from_millis(120),
        );
        let wait = idle_watch_wait(&retry_after);
        assert!(wait <= Duration::from_millis(120));
        assert!(wait >= POLL_INTERVAL);

        // An entry whose deadline has already passed had its attempt in the
        // pass that just ran. It must not collapse the next park to nothing.
        let mut stale = BTreeMap::new();
        stale.insert(String::from("stuck.desktop"), now - Duration::from_secs(1));
        assert_eq!(idle_watch_wait(&stale), IDLE_WATCH_WAIT);
    }
}
