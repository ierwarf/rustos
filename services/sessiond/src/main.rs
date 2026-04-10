use std::collections::{BTreeMap, BTreeSet};
use std::thread;
use std::time::{Duration, Instant};

use runtime_control::{
    decode_c_string, load_autostart_program_entries, load_startup_entries, RuntimeClient,
    StartupMode, DEFAULT_APPLICATIONS_DIR, DEFAULT_AUTOSTART_DIR,
};
const POLL_INTERVAL: Duration = Duration::from_millis(100);
const LAUNCH_SETTLE_DELAY: Duration = Duration::from_millis(250);
const LAUNCH_START_TIMEOUT: Duration = Duration::from_secs(60);
const RETRY_BACKOFF: Duration = Duration::from_secs(1);

#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd)]
struct LaunchEntry {
    desktop_file_id: String,
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
            diag_client::diag_error!("sessiond", "failed to open runtime device: errno={}", err);
            return;
        }
    };
    let launch_entries = load_launch_entries();
    diag_client::diag_info!(
        "sessiond",
        "loaded {} desktop/session entries",
        launch_entries.len()
    );
    let mut launched_once = BTreeSet::new();
    let mut pending_launch = None::<PendingLaunch>;
    let mut retry_after = BTreeMap::<String, Instant>::new();

    loop {
        let running = match runtime.snapshot_running_programs() {
            Ok(running) => running,
            Err(err) => {
                diag_client::diag_error!("sessiond", "snapshot running failed: errno={}", err);
                thread::sleep(POLL_INTERVAL);
                continue;
            }
        };

        let running_execs = running
            .iter()
            .map(|program| decode_c_string(&program.desktop_file_id))
            .collect::<BTreeSet<_>>();

        if let Some(pending) = pending_launch.as_ref() {
            if running_execs.contains(pending.desktop_file_id.as_str()) {
                retry_after.remove(pending.desktop_file_id.as_str());
                if pending.requested_at.elapsed() >= LAUNCH_SETTLE_DELAY {
                    diag_client::diag_info!(
                        "sessiond",
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

            diag_client::diag_warn!(
                "sessiond",
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
            if retry_after
                .get(entry.desktop_file_id.as_str())
                .is_some_and(|deadline| Instant::now() < *deadline)
            {
                continue;
            }
            if entry.restart {
                if running_execs.contains(entry.desktop_file_id.as_str()) {
                    continue;
                }
                diag_client::diag_info!(
                    "sessiond",
                    "ensuring desktop service {}",
                    entry.desktop_file_id
                );
            } else if launched_once.contains(entry.desktop_file_id.as_str()) {
                continue;
            } else {
                diag_client::diag_info!(
                    "sessiond",
                    "launching desktop app {}",
                    entry.desktop_file_id
                );
            }

            match runtime.request_launch_program_new_session(entry.desktop_file_id.as_str()) {
                Ok(()) => {
                    if !entry.restart {
                        launched_once.insert(entry.desktop_file_id.clone());
                    }
                    retry_after.remove(entry.desktop_file_id.as_str());
                    diag_client::diag_info!(
                        "sessiond",
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
                    diag_client::diag_error!(
                        "sessiond",
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
        diag_client::diag_warn!(
            "sessiond",
            "application desktop dir unavailable: {DEFAULT_APPLICATIONS_DIR}"
        );
        return Vec::new();
    };

    entries
        .into_iter()
        .filter(|entry| entry.mode == StartupMode::Desktop)
        .map(|entry| LaunchEntry {
            restart: entry.exec.starts_with("services/"),
            desktop_file_id: entry.desktop_file_id,
        })
        .collect()
}

fn load_autostart_entries() -> Vec<LaunchEntry> {
    let Ok(entries) = load_autostart_program_entries(DEFAULT_AUTOSTART_DIR) else {
        diag_client::diag_warn!(
            "sessiond",
            "autostart directory unavailable: {DEFAULT_AUTOSTART_DIR}"
        );
        return Vec::new();
    };
    entries
        .into_iter()
        .map(|entry| LaunchEntry {
            restart: entry.exec.starts_with("services/"),
            desktop_file_id: entry.desktop_file_id,
        })
        .collect()
}
