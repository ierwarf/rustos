use std::collections::{BTreeMap, BTreeSet};
use std::ffi::CString;
use std::io::Write;
use std::thread;
use std::time::{Duration, Instant};

use runtime_control::{load_startup_entries, StartupEntry, StartupMode, DEFAULT_APPLICATIONS_DIR};

const INITD_EXEC_PATH: &str = "services/initd/initd.elf";
const RUNTIMED_EXEC_PATH: &str = "services/runtimed/runtimed.elf";
const STORAGED_EXEC_PATH: &str = "services/storaged/storaged.elf";
const POLL_INTERVAL: Duration = Duration::from_millis(50);
const RETRY_BACKOFF: Duration = Duration::from_secs(1);
const DEFAULT_PATH_ENV: &str = "PATH=/bin:/usr/bin:/usr/local/bin";
const DEFAULT_HOME_ENV: &str = "HOME=/home/user";
const XDG_RUNTIME_DIR: &str = "XDG_RUNTIME_DIR=/run/user/1000";
const WAYLAND_DISPLAY: &str = "WAYLAND_DISPLAY=wayland-0";
const BOOT_TRACE_ENABLED: bool = true;
const SYS_RUSTOS_SPAWN_EXEC: libc::c_long = 0x5255_0002;

#[derive(Clone, Debug, Eq, PartialEq)]
struct StartupLaunchEntry {
    package_id: String,
    exec: String,
    runtime_deps: Vec<String>,
    restart: bool,
}

#[derive(Clone, Debug)]
struct RunningService {
    package_id: String,
    exec: String,
    restart: bool,
}

fn main() {
    boot_line("initd: main enter");
    let load_started = Instant::now();
    boot_line("initd: load entries begin");
    let startup_entries = load_init_entries();
    boot_line(
        format!(
            "initd: load entries done count={} elapsed_ms={}",
            startup_entries.len(),
            load_started.elapsed().as_millis()
        )
        .as_str(),
    );
    observability_client::info!("initd", service, "init services={}", startup_entries.len());

    let mut running = BTreeMap::<i32, RunningService>::new();
    let mut launched_once_packages = BTreeSet::new();
    let mut retry_after = BTreeMap::<String, Instant>::new();

    loop {
        reap_children(&mut running, &mut retry_after);

        let running_packages = running
            .values()
            .map(|service| service.package_id.clone())
            .collect::<BTreeSet<_>>();
        let now = Instant::now();
        for entry in startup_entries.clone() {
            if !runtime_deps_satisfied(
                &entry.runtime_deps,
                &running_packages,
                &launched_once_packages,
            ) {
                continue;
            }

            if retry_after
                .get(entry.exec.as_str())
                .is_some_and(|deadline| now < *deadline)
            {
                continue;
            }

            if entry.restart {
                if running_packages.contains(&entry.package_id) {
                    continue;
                }
            } else if running_packages.contains(&entry.package_id)
                || launched_once_packages.contains(entry.package_id.as_str())
            {
                continue;
            }

            match spawn_exec(entry.exec.as_str()) {
                Ok(pid) => {
                    running.insert(
                        pid,
                        RunningService {
                            package_id: entry.package_id.clone(),
                            exec: entry.exec.clone(),
                            restart: entry.restart,
                        },
                    );
                    retry_after.remove(entry.exec.as_str());
                    if !entry.restart {
                        launched_once_packages.insert(entry.package_id);
                    }
                    thread::yield_now();
                }
                Err(err) => {
                    observability_client::error!(
                        "initd",
                        service,
                        "launch {} failed: errno={err}",
                        entry.exec
                    );
                    retry_after.insert(entry.exec, Instant::now() + RETRY_BACKOFF);
                }
            }
        }

        thread::sleep(POLL_INTERVAL);
    }
}

fn load_init_entries() -> Vec<StartupLaunchEntry> {
    let Ok(entries) = load_startup_entries(DEFAULT_APPLICATIONS_DIR) else {
        observability_client::warn!(
            "initd",
            service,
            "application desktop dir unavailable: {DEFAULT_APPLICATIONS_DIR}"
        );
        return Vec::new();
    };

    let mut launch_entries = entries
        .into_iter()
        .filter(|entry| entry.mode == StartupMode::Init)
        .filter(|entry| entry.exec != INITD_EXEC_PATH)
        .map(startup_launch_entry)
        .collect::<Vec<_>>();
    launch_entries.sort_by(|lhs, rhs| {
        rhs.restart
            .cmp(&lhs.restart)
            .then_with(|| {
                init_exec_priority(lhs.exec.as_str()).cmp(&init_exec_priority(rhs.exec.as_str()))
            })
            .then_with(|| lhs.exec.cmp(&rhs.exec))
    });
    launch_entries
}

fn startup_launch_entry(entry: StartupEntry) -> StartupLaunchEntry {
    StartupLaunchEntry {
        package_id: entry.package_id,
        restart: entry.exec.starts_with("services/"),
        exec: entry.exec,
        runtime_deps: entry.runtime_deps,
    }
}

fn init_exec_priority(exec: &str) -> u8 {
    match exec {
        RUNTIMED_EXEC_PATH => 0,
        STORAGED_EXEC_PATH => 1,
        _ => 2,
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

fn reap_children(
    running: &mut BTreeMap<i32, RunningService>,
    retry_after: &mut BTreeMap<String, Instant>,
) {
    loop {
        let mut status = 0_i32;
        let pid = unsafe {
            libc::syscall(
                libc::SYS_wait4 as libc::c_long,
                -1_i32,
                &mut status as *mut i32,
                libc::WNOHANG,
                std::ptr::null_mut::<libc::rusage>(),
            ) as i32
        };
        if pid > 0 {
            if let Some(service) = running.remove(&pid) {
                if service.restart {
                    retry_after.insert(service.exec, Instant::now() + RETRY_BACKOFF);
                }
            }
            continue;
        }
        if pid == 0 || (pid == -1 && last_errno() == libc::ECHILD) {
            break;
        }
        break;
    }
}

fn spawn_exec(exec_path: &str) -> Result<i32, i32> {
    boot_line(&format!("initd: spawn begin exec={exec_path}"));
    let path = CString::new(exec_path).unwrap_or_else(|_| CString::new("/").unwrap());
    let argv = [path.as_ptr(), std::ptr::null()];
    let env = [
        CString::new(DEFAULT_PATH_ENV).unwrap(),
        CString::new(DEFAULT_HOME_ENV).unwrap(),
        CString::new(XDG_RUNTIME_DIR).unwrap(),
        CString::new(WAYLAND_DISPLAY).unwrap(),
    ];
    let mut envp = env.iter().map(|value| value.as_ptr()).collect::<Vec<_>>();
    envp.push(std::ptr::null());
    let pid = unsafe {
        libc::syscall(
            SYS_RUSTOS_SPAWN_EXEC,
            path.as_ptr(),
            argv.as_ptr(),
            envp.as_ptr(),
            1_u64,
            0_u64,
            50_u64,
        ) as i32
    };
    if pid < 0 {
        return Err(last_errno());
    }
    boot_line(&format!("initd: spawn returned exec={exec_path} pid={pid}"));
    Ok(pid)
}

fn last_errno() -> i32 {
    std::io::Error::last_os_error()
        .raw_os_error()
        .unwrap_or(libc::EIO)
}

fn boot_line(message: &str) {
    if !BOOT_TRACE_ENABLED {
        return;
    }
    let _ = std::io::stderr().write_all(message.as_bytes());
    let _ = std::io::stderr().write_all(b"\n");
}
