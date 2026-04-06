use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::thread;
use std::time::{Duration, Instant};

use runtime_control::{
    decode_c_string, load_startup_entries, RuntimeClient, StartupMode,
    DEFAULT_STARTUP_REGISTRY_PATH,
};

const AUTOSTART_DIR: &str = "/etc/xdg/autostart";
const POLL_INTERVAL: Duration = Duration::from_millis(100);
const LAUNCH_SETTLE_DELAY: Duration = Duration::from_millis(250);
const LAUNCH_START_TIMEOUT: Duration = Duration::from_secs(60);
const RETRY_BACKOFF: Duration = Duration::from_secs(1);

#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd)]
struct LaunchEntry {
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
            eprintln!("sessiond: failed to open runtime device: errno={err}");
            return;
        }
    };
    let launch_entries = load_launch_entries();
    eprintln!(
        "sessiond: loaded {} desktop/session entries",
        launch_entries.len()
    );
    let mut launched_once = BTreeSet::new();
    let mut pending_launch = None::<PendingLaunch>;
    let mut retry_after = BTreeMap::<String, Instant>::new();

    loop {
        let programs = match runtime.snapshot_programs() {
            Ok(programs) => programs,
            Err(err) => {
                eprintln!("sessiond: snapshot programs failed: errno={err}");
                thread::sleep(POLL_INTERVAL);
                continue;
            }
        };
        let running = match runtime.snapshot_running_programs() {
            Ok(running) => running,
            Err(err) => {
                eprintln!("sessiond: snapshot running failed: errno={err}");
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
                    eprintln!("sessiond: launch settled for {}", pending.exec);
                    pending_launch = None;
                }
                thread::sleep(POLL_INTERVAL);
                continue;
            }

            if pending.requested_at.elapsed() < LAUNCH_START_TIMEOUT {
                thread::sleep(POLL_INTERVAL);
                continue;
            }

            eprintln!("sessiond: launch timed out waiting for {}", pending.exec);
            retry_after.insert(pending.exec.clone(), Instant::now() + RETRY_BACKOFF);
            pending_launch = None;
        }

        for entry in &launch_entries {
            if retry_after
                .get(entry.exec.as_str())
                .is_some_and(|deadline| Instant::now() < *deadline)
            {
                continue;
            }
            let Some(program_id) = exec_to_id.get(entry.exec.as_str()).copied() else {
                continue;
            };

            if entry.restart {
                if running_execs.contains(entry.exec.as_str()) {
                    continue;
                }
                eprintln!(
                    "sessiond: ensuring desktop service {} (program_id={})",
                    entry.exec, program_id
                );
            } else if launched_once.contains(entry.exec.as_str()) {
                continue;
            } else {
                eprintln!(
                    "sessiond: launching desktop app {} (program_id={})",
                    entry.exec, program_id
                );
            }

            match runtime.request_launch_new_session(program_id) {
                Ok(()) => {
                    if !entry.restart {
                        launched_once.insert(entry.exec.clone());
                    }
                    retry_after.remove(entry.exec.as_str());
                    eprintln!("sessiond: launch request queued for {}", entry.exec);
                    pending_launch = Some(PendingLaunch {
                        exec: entry.exec.clone(),
                        requested_at: Instant::now(),
                    });
                    break;
                }
                Err(err) => {
                    retry_after.insert(entry.exec.clone(), Instant::now() + RETRY_BACKOFF);
                    eprintln!("sessiond: launch {} failed: errno={err}", entry.exec);
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
            .then_with(|| lhs.exec.cmp(&rhs.exec))
    });
    launch_entries
}

fn load_startup_registry_desktop_entries() -> Vec<LaunchEntry> {
    let Ok(entries) = load_startup_entries(DEFAULT_STARTUP_REGISTRY_PATH) else {
        eprintln!(
            "sessiond: startup registry unavailable: {}",
            DEFAULT_STARTUP_REGISTRY_PATH
        );
        return Vec::new();
    };

    entries
        .into_iter()
        .filter(|entry| entry.mode == StartupMode::Desktop)
        .map(|entry| LaunchEntry {
            restart: entry.exec.starts_with("services/"),
            exec: entry.exec,
        })
        .collect()
}

fn load_autostart_entries() -> Vec<LaunchEntry> {
    let Ok(read_dir) = fs::read_dir(AUTOSTART_DIR) else {
        eprintln!(
            "sessiond: autostart directory unavailable: {}",
            AUTOSTART_DIR
        );
        return Vec::new();
    };

    let mut paths = read_dir
        .filter_map(|entry| entry.ok().map(|entry| entry.path()))
        .filter(|path| path.extension().and_then(|ext| ext.to_str()) == Some("desktop"))
        .collect::<Vec<_>>();
    paths.sort();

    let mut entries = Vec::new();
    for path in paths {
        let Ok(contents) = fs::read_to_string(&path) else {
            continue;
        };
        if let Some(exec) = parse_autostart_exec(&contents) {
            entries.push(LaunchEntry {
                restart: exec.starts_with("services/"),
                exec,
            });
        }
    }
    entries
}

fn parse_autostart_exec(contents: &str) -> Option<String> {
    let mut in_desktop_entry = false;
    let mut entry_type = None::<&str>;
    let mut hidden = false;
    let mut no_display = false;
    let mut enabled = true;
    let mut only_show_in = None::<&str>;
    let mut not_show_in = None::<&str>;
    let mut exec = None::<String>;

    for raw_line in contents.lines() {
        let line = raw_line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if line.starts_with('[') && line.ends_with(']') {
            in_desktop_entry = line == "[Desktop Entry]";
            continue;
        }
        if !in_desktop_entry {
            continue;
        }

        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let value = value.trim();
        match key.trim() {
            "Type" => entry_type = Some(value),
            "Exec" => exec = parse_exec_target(value).map(str::to_string),
            "Hidden" => hidden = parse_bool(value),
            "NoDisplay" => no_display = parse_bool(value),
            "X-GNOME-Autostart-enabled" => enabled = parse_bool(value),
            "OnlyShowIn" => only_show_in = Some(value),
            "NotShowIn" => not_show_in = Some(value),
            _ => {}
        }
    }

    if !matches!(entry_type, None | Some("Application")) || hidden || no_display || !enabled {
        return None;
    }
    if let Some(value) = only_show_in {
        if !desktop_list_contains(value, "RustOS") {
            return None;
        }
    }
    if let Some(value) = not_show_in {
        if desktop_list_contains(value, "RustOS") {
            return None;
        }
    }

    exec
}

fn parse_exec_target(value: &str) -> Option<&str> {
    let value = value.trim_start();
    if value.is_empty() {
        return None;
    }

    let bytes = value.as_bytes();
    if matches!(bytes.first(), Some(b'"') | Some(b'\'')) {
        let quote = bytes[0];
        let end = bytes[1..]
            .iter()
            .position(|candidate| *candidate == quote)
            .map(|index| index + 1)?;
        return Some(&value[1..end]);
    }

    let end = value.find(char::is_whitespace).unwrap_or(value.len());
    Some(&value[..end])
}

fn parse_bool(value: &str) -> bool {
    matches!(value, "1" | "true" | "True" | "yes" | "Yes")
}

fn desktop_list_contains(value: &str, entry: &str) -> bool {
    value
        .split(';')
        .map(str::trim)
        .any(|candidate| !candidate.is_empty() && candidate == entry)
}
