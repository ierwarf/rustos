use std::collections::{BTreeMap, BTreeSet};
use std::thread;
use std::time::{Duration, Instant};

use runtime_control::{
    decode_c_string, load_autostart_program_entries, load_startup_entries, DesktopProgramEntry,
    RuntimeClient, StartupMode, DEFAULT_APPLICATIONS_DIR, DEFAULT_AUTOSTART_DIR,
};
const POLL_INTERVAL: Duration = Duration::from_millis(100);
const LAUNCH_SETTLE_DELAY: Duration = Duration::from_millis(250);
const LAUNCH_START_TIMEOUT: Duration = Duration::from_secs(60);
const RETRY_BACKOFF: Duration = Duration::from_secs(1);

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
    requested_at: Instant,
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
    let mut launched_once_packages = BTreeSet::new();
    let mut pending_launch = None::<PendingLaunch>;
    let mut retry_after = BTreeMap::<String, Instant>::new();

    loop {
        let running = match runtime.snapshot_running_programs() {
            Ok(running) => running,
            Err(err) => {
                observability_client::error!(
                    "sessiond",
                    service,
                    "snapshot running failed: errno={}",
                    err
                );
                thread::sleep(POLL_INTERVAL);
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

        if let Some(pending) = pending_launch.as_ref() {
            if running_desktop_ids.contains(pending.desktop_file_id.as_str()) {
                retry_after.remove(pending.desktop_file_id.as_str());
                if pending.requested_at.elapsed() >= LAUNCH_SETTLE_DELAY {
                    observability_client::info!(
                        "sessiond",
                        service,
                        "launch settled for {}",
                        pending.desktop_file_id
                    );
                    pending_launch = None;
                }
                thread::sleep(POLL_INTERVAL);
                continue;
            }

            if pending.requested_at.elapsed() < LAUNCH_START_TIMEOUT {
                thread::sleep(POLL_INTERVAL);
                continue;
            }

            observability_client::warn!(
                "sessiond",
                service,
                "launch timed out waiting for {}",
                pending.desktop_file_id
            );
            retry_after.insert(
                pending.desktop_file_id.clone(),
                Instant::now() + RETRY_BACKOFF,
            );
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
            } else if launched_once_packages.contains(entry.package_id.as_str()) {
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
                        launched_once_packages.insert(entry.package_id.clone());
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
                        requested_at: Instant::now(),
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

        thread::sleep(POLL_INTERVAL);
    }
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
