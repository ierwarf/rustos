use std::collections::{BTreeMap, BTreeSet};
use std::ffi::CString;
use std::io::Write;
use std::thread;
use std::time::{Duration, Instant};

use runtime_control::{load_startup_entries, StartupMode, DEFAULT_APPLICATIONS_DIR};

const INITD_EXEC_PATH: &str = "services/initd/initd.elf";
const RUNTIMED_EXEC_PATH: &str = "services/runtimed/runtimed.elf";
const STORAGED_EXEC_PATH: &str = "services/storaged/storaged.elf";
const DEBUGD_EXEC_PATH: &str = "services/debugd/debugd.elf";
const POLL_INTERVAL: Duration = Duration::from_millis(50);
const RETRY_BACKOFF: Duration = Duration::from_secs(1);
const NONCRITICAL_INIT_DELAY: Duration = Duration::from_secs(20);
const DEBUGD_DEFER_AFTER_RUNTIMED: Duration = Duration::from_secs(30);
const DEFAULT_PATH_ENV: &str = "PATH=/bin:/usr/bin:/usr/local/bin";
const DEFAULT_HOME_ENV: &str = "HOME=/home/user";
const XDG_RUNTIME_DIR: &str = "XDG_RUNTIME_DIR=/run/user/1000";
const WAYLAND_DISPLAY: &str = "WAYLAND_DISPLAY=wayland-0";
const BOOT_TRACE_ENABLED: bool = true;
const SYS_RUSTOS_SPAWN_EXEC: libc::c_long = 0x5255_0002;

#[derive(Clone, Debug, Eq, PartialEq)]
struct StartupLaunchEntry {
    exec: String,
    restart: bool,
}

#[derive(Clone, Debug)]
struct RunningService {
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
    diag_client::diag_info!("initd", "init services={}", startup_entries.len());

    let mut running = BTreeMap::<i32, RunningService>::new();
    let mut launched_once = BTreeSet::new();
    let mut retry_after = BTreeMap::<String, Instant>::new();
    let mut noncritical_after = None::<Instant>;

    loop {
        reap_children(&mut running, &mut retry_after);

        let running_execs = running
            .values()
            .map(|service| service.exec.clone())
            .collect::<BTreeSet<_>>();
        let now = Instant::now();
        if runtimed_started(&running_execs, &launched_once) && noncritical_after.is_none() {
            noncritical_after = Some(now + NONCRITICAL_INIT_DELAY);
        }
        let noncritical_ready = noncritical_after.is_some_and(|deadline| now >= deadline);
        let debugd_ready = noncritical_after
            .map(|deadline| deadline + DEBUGD_DEFER_AFTER_RUNTIMED)
            .is_some_and(|deadline| now >= deadline);

        for entry in startup_entries.clone() {
            if entry.exec != RUNTIMED_EXEC_PATH && !noncritical_ready {
                continue;
            }
            if entry.exec == DEBUGD_EXEC_PATH && !debugd_ready {
                continue;
            }

            if retry_after
                .get(entry.exec.as_str())
                .is_some_and(|deadline| now < *deadline)
            {
                continue;
            }

            if entry.restart {
                if running_execs.contains(&entry.exec) {
                    continue;
                }
            } else if running_execs.contains(&entry.exec)
                || launched_once.contains(entry.exec.as_str())
            {
                continue;
            }

            match spawn_exec(entry.exec.as_str()) {
                Ok(pid) => {
                    running.insert(
                        pid,
                        RunningService {
                            exec: entry.exec.clone(),
                            restart: entry.restart,
                        },
                    );
                    retry_after.remove(entry.exec.as_str());
                    if !entry.restart {
                        launched_once.insert(entry.exec);
                    }
                    thread::yield_now();
                }
                Err(err) => {
                    diag_client::diag_error!("initd", "launch {} failed: errno={err}", entry.exec);
                    retry_after.insert(entry.exec, Instant::now() + RETRY_BACKOFF);
                }
            }
        }

        thread::sleep(POLL_INTERVAL);
    }
}

fn runtimed_started(running_execs: &BTreeSet<String>, launched_once: &BTreeSet<String>) -> bool {
    running_execs.contains(RUNTIMED_EXEC_PATH) || launched_once.contains(RUNTIMED_EXEC_PATH)
}

fn load_init_entries() -> Vec<StartupLaunchEntry> {
    let Ok(entries) = load_startup_entries(DEFAULT_APPLICATIONS_DIR) else {
        diag_client::diag_warn!(
            "initd",
            "application desktop dir unavailable: {DEFAULT_APPLICATIONS_DIR}"
        );
        return Vec::new();
    };

    let mut launch_entries = entries
        .into_iter()
        .filter(|entry| entry.mode == StartupMode::Init)
        .filter(|entry| entry.exec != INITD_EXEC_PATH)
        .map(|entry| StartupLaunchEntry {
            restart: entry.exec.starts_with("services/"),
            exec: entry.exec,
        })
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

fn init_exec_priority(exec: &str) -> u8 {
    match exec {
        RUNTIMED_EXEC_PATH => 0,
        STORAGED_EXEC_PATH => 1,
        DEBUGD_EXEC_PATH => 2,
        _ => 3,
    }
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
