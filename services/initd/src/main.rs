use std::collections::{BTreeMap, BTreeSet};
use std::thread;
use std::time::{Duration, Instant};

use runtime_control::{
    decode_c_string, load_startup_entries, RuntimeClient, StartupMode,
    DEFAULT_STARTUP_REGISTRY_PATH,
};

const INITD_EXEC_PATH: &str = "services/initd/initd.elf";
const POLL_INTERVAL: Duration = Duration::from_millis(100);
const LAUNCH_SETTLE_DELAY: Duration = Duration::from_millis(250);
const LAUNCH_START_TIMEOUT: Duration = Duration::from_secs(60);
const RETRY_BACKOFF: Duration = Duration::from_secs(1);

#[derive(Clone, Debug, Eq, PartialEq)]
struct StartupLaunchEntry {
    mode: StartupMode,
    exec: String,
    restart: bool,
}

#[derive(Clone, Debug)]
struct PendingLaunch {
    exec: String,
    requested_at: Instant,
}

fn main() {
    let runtime = match RuntimeClient::open_default() {
        Ok(client) => client,
        Err(err) => {
            eprintln!("initd: failed to open runtime device: errno={err}");
            return;
        }
    };
    let startup_entries = load_session_entries();
    eprintln!(
        "initd: loaded {} startup service entries",
        startup_entries.len()
    );
    let mut launched_once = BTreeSet::new();
    let mut pending_launch = None::<PendingLaunch>;
    let mut retry_after = BTreeMap::<String, Instant>::new();

    loop {
        let programs = match runtime.snapshot_programs() {
            Ok(programs) => programs,
            Err(err) => {
                eprintln!("initd: snapshot programs failed: errno={err}");
                thread::sleep(POLL_INTERVAL);
                continue;
            }
        };
        let running = match runtime.snapshot_running_programs() {
            Ok(running) => running,
            Err(err) => {
                eprintln!("initd: snapshot running failed: errno={err}");
                thread::sleep(POLL_INTERVAL);
                continue;
            }
        };

        let exec_to_id = programs
            .iter()
            .map(|program| (decode_c_string(&program.exec_path), program.program_id))
            .collect::<BTreeMap<_, _>>();
        let id_to_exec = exec_to_id
            .iter()
            .map(|(exec, program_id)| (*program_id, exec.clone()))
            .collect::<BTreeMap<_, _>>();
        let mut running_execs = BTreeSet::new();
        for program in &running {
            if let Some(exec) = id_to_exec.get(&program.program_id) {
                running_execs.insert(exec.clone());
            }
        }

        if let Some(pending) = pending_launch.as_ref() {
            if running_execs.contains(pending.exec.as_str()) {
                retry_after.remove(pending.exec.as_str());
                if pending.requested_at.elapsed() >= LAUNCH_SETTLE_DELAY {
                    eprintln!("initd: launch settled for {}", pending.exec);
                    pending_launch = None;
                }
                thread::sleep(POLL_INTERVAL);
                continue;
            }

            if pending.requested_at.elapsed() < LAUNCH_START_TIMEOUT {
                thread::sleep(POLL_INTERVAL);
                continue;
            }

            eprintln!("initd: launch timed out waiting for {}", pending.exec);
            retry_after.insert(pending.exec.clone(), Instant::now() + RETRY_BACKOFF);
            pending_launch = None;
        }

        for entry in &startup_entries {
            if retry_after
                .get(entry.exec.as_str())
                .is_some_and(|deadline| Instant::now() < *deadline)
            {
                continue;
            }
            let Some(program_id) = exec_to_id.get(entry.exec.as_str()).copied() else {
                continue;
            };
            let mode_label = match entry.mode {
                StartupMode::Init => "init",
                StartupMode::Session => "session",
                StartupMode::Desktop => "desktop",
            };

            if entry.restart {
                if running_execs.contains(entry.exec.as_str()) {
                    continue;
                }
                eprintln!(
                    "initd: ensuring {} service {} (program_id={})",
                    mode_label, entry.exec, program_id
                );
            } else if launched_once.contains(entry.exec.as_str()) {
                continue;
            } else {
                eprintln!(
                    "initd: launching one-shot {} target {} (program_id={})",
                    mode_label, entry.exec, program_id
                );
            }

            match runtime.request_launch_new_session(program_id) {
                Ok(()) => {
                    if !entry.restart {
                        launched_once.insert(entry.exec.clone());
                    }
                    retry_after.remove(entry.exec.as_str());
                    eprintln!("initd: launch request queued for {}", entry.exec);
                    pending_launch = Some(PendingLaunch {
                        exec: entry.exec.clone(),
                        requested_at: Instant::now(),
                    });
                    break;
                }
                Err(err) => {
                    retry_after.insert(entry.exec.clone(), Instant::now() + RETRY_BACKOFF);
                    eprintln!("initd: launch {} failed: errno={err}", entry.exec);
                }
            }
        }

        thread::sleep(POLL_INTERVAL);
    }
}

fn load_session_entries() -> Vec<StartupLaunchEntry> {
    let Ok(entries) = load_startup_entries(DEFAULT_STARTUP_REGISTRY_PATH) else {
        eprintln!(
            "initd: startup registry unavailable: {}",
            DEFAULT_STARTUP_REGISTRY_PATH
        );
        return Vec::new();
    };

    let mut launch_entries = entries
        .into_iter()
        .filter(|entry| matches!(entry.mode, StartupMode::Init | StartupMode::Session))
        .filter(|entry| entry.exec != INITD_EXEC_PATH)
        .map(|entry| StartupLaunchEntry {
            mode: entry.mode,
            restart: entry.exec.starts_with("services/"),
            exec: entry.exec,
        })
        .collect::<Vec<_>>();
    launch_entries.sort_by(|lhs, rhs| {
        lhs.mode
            .cmp(&rhs.mode)
            .then_with(|| rhs.restart.cmp(&lhs.restart))
            .then_with(|| lhs.exec.cmp(&rhs.exec))
    });
    launch_entries
}
