use std::collections::{BTreeMap, BTreeSet};
use std::ffi::CString;
use std::io::Write;
use std::mem::size_of;
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use runtime_control::{
    load_runtime_default_env, load_startup_entries, RuntimeEnvScope, StartupEntry, StartupMode,
    DEFAULT_APPLICATIONS_DIR, DEFAULT_RUNTIME_ENV_REGISTRY_PATH,
};
use rustos_user_abi::syscall::{
    LoaderSpawnRequest, LoaderSpawnResponse, IPC_SERVICE_LINUX_SYSCALLD, IPC_SERVICE_LOADERD,
    IPC_SERVICE_VFSD, LOADER_OP_SPAWN_EXEC, LOADER_REQUEST_ABI_VERSION, LOADER_SPAWN_ARG_BYTES,
    LOADER_SPAWN_ENV_BYTES, LOADER_SPAWN_EXEC_PATH_CAPACITY, SYS_RUSTOS_IPC_CALL,
    SYS_RUSTOS_IPC_LOOKUP_SERVICE_ENDPOINT,
};

const INITD_EXEC_PATH: &str = "services/initd/initd.elf";
const SYSCALLD_EXEC_PATH: &str = "services/syscalld/syscalld.elf";
const VFSD_EXEC_PATH: &str = "services/vfsd/vfsd.elf";
const NETD_EXEC_PATH: &str = "services/netd/netd.elf";
const DEVMGRD_EXEC_PATH: &str = "services/devmgrd/devmgrd.elf";
const DRIVERD_EXEC_PATH: &str = "services/driverd/driverd.elf";
const LOADERD_EXEC_PATH: &str = "services/loaderd/loaderd.elf";
const RUNTIMED_EXEC_PATH: &str = "services/runtimed/runtimed.elf";
const STORAGED_EXEC_PATH: &str = "services/storaged/storaged.elf";
const INPUTD_EXEC_PATH: &str = "services/inputd/inputd.elf";
const POLL_INTERVAL: Duration = Duration::from_millis(50);
const RETRY_BACKOFF: Duration = Duration::from_secs(1);
static LOADER_ENDPOINT_CACHE: AtomicU64 = AtomicU64::new(0);

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
        let mut launched_this_round = false;
        for entry in startup_entries.clone() {
            if !runtime_deps_satisfied(
                &entry.runtime_deps,
                &running_packages,
                &launched_once_packages,
            ) {
                continue;
            }
            if !launch_gate_satisfied(entry.exec.as_str()) {
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
                    observability_client::info!(
                        "initd",
                        service,
                        "launch {} pid={pid}",
                        entry.exec
                    );
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
                    launched_this_round = true;
                    break;
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

        if launched_this_round {
            thread::yield_now();
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
        SYSCALLD_EXEC_PATH => 0,
        VFSD_EXEC_PATH => 1,
        LOADERD_EXEC_PATH => 2,
        NETD_EXEC_PATH => 3,
        DEVMGRD_EXEC_PATH => 4,
        DRIVERD_EXEC_PATH => 5,
        STORAGED_EXEC_PATH => 6,
        INPUTD_EXEC_PATH => 7,
        RUNTIMED_EXEC_PATH => 8,
        _ => 9,
    }
}

fn launch_gate_satisfied(exec: &str) -> bool {
    match exec {
        SYSCALLD_EXEC_PATH | VFSD_EXEC_PATH => true,
        LOADERD_EXEC_PATH => {
            service_ready(IPC_SERVICE_LINUX_SYSCALLD) && service_ready(IPC_SERVICE_VFSD)
        }
        NETD_EXEC_PATH | DEVMGRD_EXEC_PATH | DRIVERD_EXEC_PATH | STORAGED_EXEC_PATH
        | INPUTD_EXEC_PATH => foundation_policy_services_ready(),
        _ => foundation_policy_services_ready(),
    }
}

fn foundation_policy_services_ready() -> bool {
    [
        IPC_SERVICE_LINUX_SYSCALLD,
        IPC_SERVICE_VFSD,
        IPC_SERVICE_LOADERD,
    ]
    .into_iter()
    .all(service_ready)
}

fn service_ready(service_id: u64) -> bool {
    let result = unsafe {
        libc::syscall(
            SYS_RUSTOS_IPC_LOOKUP_SERVICE_ENDPOINT as libc::c_long,
            service_id,
        ) as i64
    };
    result > 0
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
                observability_client::warn!(
                    "initd",
                    service,
                    "service exited package={} exec={} pid={} status={}",
                    service.package_id,
                    service.exec,
                    pid,
                    status
                );
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
    if service_ready(IPC_SERVICE_LOADERD) {
        return spawn_exec_via_loaderd(exec_path);
    }

    Err(libc::EAGAIN)
}

fn spawn_exec_via_loaderd(exec_path: &str) -> Result<i32, i32> {
    let argv = [CString::new(exec_path).map_err(|_| libc::EINVAL)?];
    let env = load_runtime_default_env(DEFAULT_RUNTIME_ENV_REGISTRY_PATH, RuntimeEnvScope::Init)
        .unwrap_or_default()
        .into_iter()
        .filter_map(|value| CString::new(value).ok())
        .collect::<Vec<_>>();
    let request = build_loader_spawn_request(exec_path, &argv, &env)?;
    let endpoint = lookup_loader_endpoint()?;
    let mut response = LoaderSpawnResponse::default();
    let call = unsafe {
        libc::syscall(
            SYS_RUSTOS_IPC_CALL as libc::c_long,
            endpoint,
            (&request as *const LoaderSpawnRequest) as u64,
            size_of::<LoaderSpawnRequest>() as u64,
            (&mut response as *mut LoaderSpawnResponse) as u64,
            size_of::<LoaderSpawnResponse>() as u64,
        ) as i64
    };
    if call < 0 {
        LOADER_ENDPOINT_CACHE.store(0, Ordering::Relaxed);
        return Err((-call) as i32);
    }
    if call as usize != size_of::<LoaderSpawnResponse>()
        || response.version != LOADER_REQUEST_ABI_VERSION
        || response.op != LOADER_OP_SPAWN_EXEC
    {
        return Err(libc::EINVAL);
    }
    if response.status != 0 {
        return Err(response.status);
    }
    let pid = i32::try_from(response.pid).map_err(|_| libc::EOVERFLOW)?;
    boot_line(&format!(
        "initd: loader spawn returned exec={exec_path} pid={pid}"
    ));
    Ok(pid)
}

fn build_loader_spawn_request(
    exec_path: &str,
    argv: &[CString],
    env: &[CString],
) -> Result<LoaderSpawnRequest, i32> {
    let exec_bytes = exec_path.as_bytes();
    if exec_bytes.is_empty()
        || exec_bytes.len() > LOADER_SPAWN_EXEC_PATH_CAPACITY
        || exec_bytes.contains(&0)
    {
        return Err(libc::EINVAL);
    }
    let mut request = LoaderSpawnRequest {
        version: LOADER_REQUEST_ABI_VERSION,
        op: LOADER_OP_SPAWN_EXEC,
        flags: 1,
        console_session: 0,
        weight_micros: exec_weight_micros(exec_path),
        exec_path_len: exec_bytes.len() as u32,
        argv_count: u16::try_from(argv.len()).map_err(|_| libc::E2BIG)?,
        env_count: u16::try_from(env.len()).map_err(|_| libc::E2BIG)?,
        ..LoaderSpawnRequest::default()
    };
    request.exec_path[..exec_bytes.len()].copy_from_slice(exec_bytes);
    request.argv_bytes_len =
        copy_cstring_blob(argv, &mut request.argv_bytes, LOADER_SPAWN_ARG_BYTES)?;
    request.env_bytes_len = copy_cstring_blob(env, &mut request.env_bytes, LOADER_SPAWN_ENV_BYTES)?;
    Ok(request)
}

fn copy_cstring_blob(values: &[CString], dest: &mut [u8], capacity: usize) -> Result<u32, i32> {
    let mut offset = 0usize;
    for value in values {
        let bytes = value.as_bytes();
        let next = offset
            .checked_add(bytes.len())
            .and_then(|value| value.checked_add(1))
            .ok_or(libc::E2BIG)?;
        if next > capacity || next > dest.len() {
            return Err(libc::E2BIG);
        }
        dest[offset..offset + bytes.len()].copy_from_slice(bytes);
        dest[offset + bytes.len()] = 0;
        offset = next;
    }
    u32::try_from(offset).map_err(|_| libc::E2BIG)
}

fn lookup_loader_endpoint() -> Result<u64, i32> {
    let cached = LOADER_ENDPOINT_CACHE.load(Ordering::Relaxed);
    if cached != 0 {
        return Ok(cached);
    }
    let endpoint = unsafe {
        libc::syscall(
            SYS_RUSTOS_IPC_LOOKUP_SERVICE_ENDPOINT as libc::c_long,
            IPC_SERVICE_LOADERD,
        ) as i64
    };
    if endpoint < 0 {
        return Err((-endpoint) as i32);
    }
    let endpoint = endpoint as u64;
    if endpoint != 0 {
        LOADER_ENDPOINT_CACHE.store(endpoint, Ordering::Relaxed);
    }
    Ok(endpoint)
}

fn exec_weight_micros(exec_path: &str) -> u64 {
    let _ = exec_path;
    50
}

fn last_errno() -> i32 {
    std::io::Error::last_os_error()
        .raw_os_error()
        .unwrap_or(libc::EIO)
}

fn boot_line(message: &str) {
    if option_env!("RUSTOS_LOGGING_BOOT_TRACE_ENABLED") == Some("true") {
        let _ = std::io::stderr().write_all(message.as_bytes());
        let _ = std::io::stderr().write_all(b"\n");
        return;
    }
    unsafe {
        let _ = libc::syscall(0x5255_0001 as libc::c_long, message.as_ptr(), message.len());
        let _ = libc::syscall(0x5255_0001 as libc::c_long, b"\n".as_ptr(), 1_usize);
    }
}
