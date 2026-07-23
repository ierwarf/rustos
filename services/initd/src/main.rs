use std::collections::{BTreeMap, BTreeSet};
use std::ffi::CString;
use std::io::Write;
use std::mem::size_of;
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use runtime_control::{
    load_runtime_default_env, load_startup_entries, RuntimeEnvScope, StartupEntry, StartupMode,
    DEFAULT_RUNTIME_ENV_REGISTRY_PATH, DEFAULT_STARTUP_REGISTRY_PATH,
};
use rustos_user_abi::syscall::{
    CommercialMaxProtocolRequest, CommercialMaxProtocolResponse, LoaderSpawnRequest,
    LoaderSpawnResponse, RustosIpcWaitServiceEndpointArgs, COMMERCIAL_MAX_PROTOCOL_ABI_VERSION,
    COMMERCIAL_MAX_PROTOCOL_ROOTD_SUPERVISOR, COMMERCIAL_MAX_ROOTD_OP_POST_INIT_LEASE_QUERY,
    COMMERCIAL_MAX_ROOTD_OP_POST_INIT_LEASE_RECLAIM, COMMERCIAL_MAX_ROOTD_OP_READINESS_SIGNAL,
    IPC_SERVICE_DEVMGRD, IPC_SERVICE_INITD, IPC_SERVICE_INPUTD, IPC_SERVICE_LINUX_SYSCALLD,
    IPC_SERVICE_LOADERD, IPC_SERVICE_NETD, IPC_SERVICE_ROOTD, IPC_SERVICE_SESSIOND,
    IPC_SERVICE_STORAGED, IPC_SERVICE_VFSD, IPC_WAIT_SERVICE_ENDPOINT_ABI_VERSION,
    IPC_WAIT_SERVICE_ENDPOINT_MAX_TIMEOUT_MS, LOADER_OP_ACTIVATE, LOADER_OP_SPAWN_EXEC,
    LOADER_REQUEST_ABI_VERSION, LOADER_SPAWN_ARG_BYTES, LOADER_SPAWN_ENV_BYTES,
    LOADER_SPAWN_EXEC_PATH_CAPACITY, LOADER_SPAWN_FLAG_DEFER_START, ROOTD_LEASE_STATE_EMPTY,
    ROOTD_LEASE_STATE_EXITED, ROOTD_LEASE_STATE_RUNNING, SYS_RUSTOS_IPC_CALL,
    SYS_RUSTOS_IPC_ENDPOINT_CREATE, SYS_RUSTOS_IPC_LOOKUP_SERVICE_ENDPOINT,
    SYS_RUSTOS_IPC_REGISTER_SERVICE_ENDPOINT, SYS_RUSTOS_IPC_WAIT_SERVICE_ENDPOINT,
    TASK_WEIGHT_INTERACTIVE_FLAG,
};

const INITD_EXEC_PATH: &str = "services/initd/initd.elf";
const SYSCALLD_EXEC_PATH: &str = "services/syscalld/syscalld.elf";
const VFSD_EXEC_PATH: &str = "services/vfsd/vfsd.elf";
const NETD_EXEC_PATH: &str = "services/netd/netd.elf";
const DEVMGRD_EXEC_PATH: &str = "services/devmgrd/devmgrd.elf";
const LOADERD_EXEC_PATH: &str = "services/loaderd/loaderd.elf";
const RUNTIMED_EXEC_PATH: &str = "services/runtimed/runtimed.elf";
const STORAGED_EXEC_PATH: &str = "services/storaged/storaged.elf";
const INPUTD_EXEC_PATH: &str = "services/inputd/inputd.elf";
const POLL_INTERVAL: Duration = Duration::from_millis(2);
const RETRY_BACKOFF: Duration = Duration::from_millis(50);
const ROOTD_LEASE_RECONCILIATION_INTERVAL: Duration = Duration::from_millis(100);
const ROOTD_LEASE_RECOVERY_TIMEOUT: Duration =
    Duration::from_millis(IPC_WAIT_SERVICE_ENDPOINT_MAX_TIMEOUT_MS);
const INITD_LAUNCH_MAX_FAILURES: u32 = 32;
const ROOTD_LEASE_REPORT_MAX_ATTEMPTS: u32 = 16;
const DEFAULT_INIT_TASK_WEIGHT_MICROS: u64 = 1_000;
const EARLY_POLICY_TASK_WEIGHT_MICROS: u64 = 4_000;
const DISPLAY_CRITICAL_TASK_WEIGHT_MICROS: u64 = 2_000;
// Keep secondary services on the boot path. Guest Instant can be unavailable
// during early bring-up, so time-based deferral can leave storaged deferred
// indefinitely under KVM.
const SECONDARY_SERVICE_DEFER_AFTER_RUNTIMED: Duration = Duration::ZERO;
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

#[derive(Clone, Debug)]
struct RetryState {
    next_attempt: Instant,
    failures: u32,
}

#[derive(Clone, Copy, Debug)]
struct RootdLeaseRecovery {
    pid: i32,
    deadline: Instant,
}

fn main() {
    boot_line("initd: main enter");
    if let Err(errno) = publish_init_identity() {
        boot_line(format!("initd: identity publication failed errno={errno}").as_str());
        std::process::exit(1);
    }
    boot_line("initd: identity endpoint registered");
    let load_started = Instant::now();
    boot_line("initd: load entries begin");
    let startup_entries = load_init_entries();
    let init_env = load_init_env();
    boot_line(
        format!(
            "initd: load entries done count={} env={} elapsed_ms={}",
            startup_entries.len(),
            init_env.len(),
            load_started.elapsed().as_millis()
        )
        .as_str(),
    );
    observability_client::info!("initd", service, "init services={}", startup_entries.len());
    boot_line("initd: supervisor loop enter");

    let mut running = BTreeMap::<i32, RunningService>::new();
    let mut launched_once_packages = BTreeSet::new();
    let mut retry_state = BTreeMap::<String, RetryState>::new();
    let mut rootd_lease_recovery = BTreeMap::<u64, RootdLeaseRecovery>::new();
    let mut defer_secondary_services_until = None::<Instant>;
    let mut next_rootd_lease_reconciliation = Instant::now();
    let mut initial_lease_reconciliation = true;

    loop {
        reap_children(&mut running, &mut retry_state);

        let now = Instant::now();
        if now >= next_rootd_lease_reconciliation {
            if initial_lease_reconciliation {
                boot_line("initd: initial lease reconciliation begin");
            }
            reconcile_rootd_post_init_leases(
                &startup_entries,
                &mut running,
                &mut launched_once_packages,
                &mut retry_state,
                &mut rootd_lease_recovery,
                now,
            );
            if initial_lease_reconciliation {
                boot_line(&format!(
                    "initd: foundation endpoints syscalld={} vfsd={} loaderd={}",
                    service_ready_status(IPC_SERVICE_LINUX_SYSCALLD),
                    service_ready_status(IPC_SERVICE_VFSD),
                    service_ready_status(IPC_SERVICE_LOADERD),
                ));
                boot_line("initd: initial lease reconciliation done");
                initial_lease_reconciliation = false;
            }
            next_rootd_lease_reconciliation = now + ROOTD_LEASE_RECONCILIATION_INTERVAL;
        }

        let mut running_packages = running
            .values()
            .map(|service| service.package_id.clone())
            .collect::<BTreeSet<_>>();
        let now = Instant::now();
        let mut launched_this_round = false;
        for entry in startup_entries.iter() {
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
            if secondary_service_deferred(entry.exec.as_str(), now, defer_secondary_services_until)
            {
                continue;
            }

            if retry_state
                .get(entry.exec.as_str())
                .is_some_and(|state| now < state.next_attempt)
            {
                continue;
            }

            if rootd_service_id_for_exec(entry.exec.as_str())
                .is_some_and(|service_id| rootd_lease_recovery.contains_key(&service_id))
            {
                // rootd has an older admitted PID whose exact endpoint is
                // still within the bounded adoption window.  Do not create a
                // duplicate service or overwrite its authority lease.
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

            match spawn_exec(entry.exec.as_str(), &init_env) {
                Ok(pid) => {
                    running.insert(
                        pid,
                        RunningService {
                            package_id: entry.package_id.clone(),
                            exec: entry.exec.clone(),
                            restart: entry.restart,
                        },
                    );
                    running_packages.insert(entry.package_id.clone());
                    retry_state.remove(entry.exec.as_str());
                    if let Err(err) = report_rootd_service_lease(entry.exec.as_str(), pid) {
                        fail_closed_after_child_cleanup(
                            pid,
                            &format!(
                            "initd: fatal rootd lease report failed exec={} pid={pid} errno={err}",
                            entry.exec
                        ),
                        );
                    }
                    boot_line(&format!(
                        "initd: rootd lease report ok exec={} pid={pid}",
                        entry.exec
                    ));
                    if let Err(err) = activate_spawned_service(pid) {
                        fail_closed_after_child_cleanup(
                            pid,
                            &format!(
                            "initd: fatal service activation failed exec={} pid={pid} errno={err}",
                            entry.exec
                        ),
                        );
                    }
                    if let Err(err) = wait_reported_service_endpoint(entry.exec.as_str(), pid) {
                        fail_closed_after_child_cleanup(
                            pid,
                            &format!(
                            "initd: fatal service endpoint not ready exec={} pid={pid} errno={err}",
                            entry.exec
                        ),
                        );
                    }
                    boot_line(&format!(
                        "initd: service endpoint ready exec={} pid={pid}",
                        entry.exec
                    ));
                    if !entry.restart {
                        launched_once_packages.insert(entry.package_id.clone());
                    }
                    if entry.exec == RUNTIMED_EXEC_PATH
                        && !SECONDARY_SERVICE_DEFER_AFTER_RUNTIMED.is_zero()
                    {
                        defer_secondary_services_until =
                            Some(Instant::now() + SECONDARY_SERVICE_DEFER_AFTER_RUNTIMED);
                    }
                    launched_this_round = true;
                    thread::yield_now();
                    continue;
                }
                Err(err) => {
                    observability_client::error!(
                        "initd",
                        service,
                        "launch {} failed: errno={err}",
                        entry.exec
                    );
                    record_launch_failure(&mut retry_state, entry.exec.as_str(), "spawn", err);
                }
            }
        }

        if launched_this_round {
            thread::yield_now();
            continue;
        }
        thread::sleep(POLL_INTERVAL);
    }
}

// ZERO_TRUST_IDENTITY_ONLY_ENDPOINT: this publication has no receive or call
// surface. Its only consumer is the kernel's exact live-owner validator.
/// Publish a request-less service endpoint solely as a restart-sensitive
/// identity object. Privileged brokers validate its kernel-owned publication;
/// no caller is granted a request path to this endpoint.
fn publish_init_identity() -> Result<u64, i32> {
    let endpoint = unsafe { libc::syscall(SYS_RUSTOS_IPC_ENDPOINT_CREATE as libc::c_long) as i64 };
    if endpoint <= 0 {
        return Err(if endpoint < 0 {
            (-endpoint) as i32
        } else {
            libc::EIO
        });
    }
    let registered = unsafe {
        libc::syscall(
            SYS_RUSTOS_IPC_REGISTER_SERVICE_ENDPOINT as libc::c_long,
            IPC_SERVICE_INITD,
            endpoint as u64,
        ) as i64
    };
    if registered < 0 {
        return Err((-registered) as i32);
    }
    Ok(endpoint as u64)
}

fn secondary_service_deferred(exec: &str, now: Instant, deadline: Option<Instant>) -> bool {
    if SECONDARY_SERVICE_DEFER_AFTER_RUNTIMED.is_zero() {
        return false;
    }
    match exec {
        STORAGED_EXEC_PATH => deadline.is_none_or(|defer_until| now < defer_until),
        _ => false,
    }
}

fn load_init_entries() -> Vec<StartupLaunchEntry> {
    let entries = match load_startup_entries(DEFAULT_STARTUP_REGISTRY_PATH) {
        Ok(entries) => entries,
        Err(err) => {
            observability_client::warn!(
                "initd",
                service,
                "startup registry unavailable path={} kind={:?} errno={:?}",
                DEFAULT_STARTUP_REGISTRY_PATH,
                err.kind(),
                err.raw_os_error()
            );
            return Vec::new();
        }
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
    boot_line(
        format!(
            "initd: launch order={}",
            launch_entries
                .iter()
                .map(|entry| entry.exec.as_str())
                .collect::<Vec<_>>()
                .join(",")
        )
        .as_str(),
    );
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

fn load_init_env() -> Vec<CString> {
    match load_runtime_default_env(DEFAULT_RUNTIME_ENV_REGISTRY_PATH, RuntimeEnvScope::Init) {
        Ok(values) => values,
        Err(err) => {
            observability_client::warn!(
                "initd",
                service,
                "runtime env unavailable path={} kind={:?} errno={:?}",
                DEFAULT_RUNTIME_ENV_REGISTRY_PATH,
                err.kind(),
                err.raw_os_error()
            );
            Vec::new()
        }
    }
    .into_iter()
    .filter_map(|value| CString::new(value).ok())
    .collect()
}

fn init_exec_priority(exec: &str) -> u8 {
    match exec {
        SYSCALLD_EXEC_PATH => 0,
        VFSD_EXEC_PATH => 1,
        LOADERD_EXEC_PATH => 2,
        NETD_EXEC_PATH => 3,
        DEVMGRD_EXEC_PATH => 4,
        INPUTD_EXEC_PATH => 5,
        STORAGED_EXEC_PATH => 6,
        RUNTIMED_EXEC_PATH => 7,
        _ => 8,
    }
}

fn launch_gate_satisfied(exec: &str) -> bool {
    match exec {
        SYSCALLD_EXEC_PATH | VFSD_EXEC_PATH => true,
        LOADERD_EXEC_PATH => {
            service_ready(IPC_SERVICE_LINUX_SYSCALLD) && service_ready(IPC_SERVICE_VFSD)
        }
        RUNTIMED_EXEC_PATH => {
            foundation_policy_services_ready()
                && service_ready(IPC_SERVICE_NETD)
                && service_ready(rustos_user_abi::syscall::IPC_SERVICE_DEVMGRD)
                && service_ready(rustos_user_abi::syscall::IPC_SERVICE_INPUTD)
                && service_ready(IPC_SERVICE_STORAGED)
        }
        STORAGED_EXEC_PATH => {
            foundation_policy_services_ready()
                && service_ready(IPC_SERVICE_NETD)
                && service_ready(rustos_user_abi::syscall::IPC_SERVICE_DEVMGRD)
                && service_ready(rustos_user_abi::syscall::IPC_SERVICE_INPUTD)
        }
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

fn service_ready_status(service_id: u64) -> i64 {
    unsafe {
        libc::syscall(
            SYS_RUSTOS_IPC_LOOKUP_SERVICE_ENDPOINT as libc::c_long,
            service_id,
        ) as i64
    }
}

fn service_ready(service_id: u64) -> bool {
    let status = service_ready_status(service_id);
    match classify_service_ready_status(status) {
        Ok(ready) => ready,
        Err(errno) => fail_closed(&format!(
            "initd: fatal service readiness contract failure service_id={service_id} status={status} errno={errno}"
        )),
    }
}

fn classify_service_ready_status(status: i64) -> Result<bool, i32> {
    if status > 0 {
        return Ok(true);
    }
    if status == -(libc::ENOSYS as i64) {
        return Ok(false);
    }
    if status < 0 {
        return Err(i32::try_from(-status).unwrap_or(libc::EOVERFLOW));
    }
    Err(libc::EPROTO)
}

fn report_rootd_service_lease(exec_path: &str, pid: i32) -> Result<(), i32> {
    let Some(service_id) = rootd_service_id_for_exec(exec_path) else {
        return Ok(());
    };
    if pid <= 0 {
        observability_client::error!(
            "initd",
            service,
            "rootd lease report skipped exec={} invalid_pid={pid}",
            exec_path
        );
        return Err(libc::EINVAL);
    }
    let mut attempt = 0_u32;
    loop {
        attempt += 1;
        if attempt == 1 {
            if let Some(service_id) = rootd_service_id_for_exec(exec_path) {
                boot_line(&format!(
                    "initd: rootd lease report begin exec={} pid={pid} service_id={service_id}",
                    exec_path
                ));
            }
        }
        match report_rootd_service_lease_inner(service_id, exec_path, pid as u64) {
            Ok(()) => {
                boot_line(&format!(
                    "initd: rootd lease report success exec={} pid={pid} attempt={attempt}",
                    exec_path
                ));
                return Ok(());
            }
            Err(errno) if attempt < ROOTD_LEASE_REPORT_MAX_ATTEMPTS => {
                observability_client::warn!(
                    "initd",
                    service,
                    "rootd lease report retry exec={} pid={pid} errno={errno} attempt={attempt}",
                    exec_path
                );
                thread::yield_now();
            }
            Err(errno) => {
                observability_client::error!(
                    "initd",
                    service,
                    "rootd lease report failed exec={} pid={pid} errno={errno} attempts={attempt}",
                    exec_path
                );
                return Err(errno);
            }
        }
    }
}

fn reconcile_rootd_post_init_leases(
    startup_entries: &[StartupLaunchEntry],
    running: &mut BTreeMap<i32, RunningService>,
    launched_once_packages: &mut BTreeSet<String>,
    retry_state: &mut BTreeMap<String, RetryState>,
    recovery: &mut BTreeMap<u64, RootdLeaseRecovery>,
    now: Instant,
) {
    for entry in startup_entries {
        let Some(service_id) = rootd_service_id_for_exec(entry.exec.as_str()) else {
            continue;
        };
        let (pid, state) = rootd_post_init_lease_query(service_id).unwrap_or_else(|errno| {
            fail_closed(&format!(
                "initd: fatal rootd post-init lease query failed service_id={service_id} errno={errno}"
            ))
        });
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
                        record_launch_failure(retry_state, service.exec.as_str(), "rootd-exit", 0);
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

fn wait_reported_service_endpoint(exec_path: &str, pid: i32) -> Result<(), i32> {
    wait_reported_service_endpoint_with_timeout(
        exec_path,
        pid,
        IPC_WAIT_SERVICE_ENDPOINT_MAX_TIMEOUT_MS,
    )
}

fn wait_reported_service_endpoint_with_timeout(
    exec_path: &str,
    pid: i32,
    timeout_ms: u64,
) -> Result<(), i32> {
    let Some(service_id) = rootd_service_id_for_exec(exec_path) else {
        return Ok(());
    };
    let args = RustosIpcWaitServiceEndpointArgs {
        abi_version: IPC_WAIT_SERVICE_ENDPOINT_ABI_VERSION,
        service_id,
        expected_pid: u64::try_from(pid).map_err(|_| libc::EINVAL)?,
        timeout_ms,
        ..RustosIpcWaitServiceEndpointArgs::default()
    };
    let status = unsafe {
        libc::syscall(
            SYS_RUSTOS_IPC_WAIT_SERVICE_ENDPOINT as libc::c_long,
            (&args as *const RustosIpcWaitServiceEndpointArgs) as u64,
        ) as i64
    };
    if status < 0 {
        return Err((-status) as i32);
    }
    boot_line(&format!(
        "initd: endpoint ready event exec={exec_path} service_id={service_id} pid={pid}"
    ));
    Ok(())
}

fn rootd_post_init_lease_query(service_id: u64) -> Result<(u64, u16), i32> {
    let response =
        call_rootd_supervisor(COMMERCIAL_MAX_ROOTD_OP_POST_INIT_LEASE_QUERY, service_id, 0)?;
    u16::try_from(response.value1)
        .map(|state| (response.value0, state))
        .map_err(|_| libc::EINVAL)
}

fn reclaim_rootd_post_init_lease(service_id: u64, pid: u64) -> Result<(), i32> {
    if pid == 0 {
        return Err(libc::EINVAL);
    }
    let _ = call_rootd_supervisor(
        COMMERCIAL_MAX_ROOTD_OP_POST_INIT_LEASE_RECLAIM,
        service_id,
        pid,
    )?;
    Ok(())
}

fn call_rootd_supervisor(
    op: u16,
    arg0: u64,
    arg1: u64,
) -> Result<CommercialMaxProtocolResponse, i32> {
    let endpoint = lookup_service_endpoint(IPC_SERVICE_ROOTD)?;
    let mut request = CommercialMaxProtocolRequest::default();
    request.header.version = COMMERCIAL_MAX_PROTOCOL_ABI_VERSION;
    request.header.protocol = COMMERCIAL_MAX_PROTOCOL_ROOTD_SUPERVISOR;
    request.header.op = op;
    request.header.subject_pid = u64::from(std::process::id());
    request.header.subject_tid = current_tid();
    request.arg0 = arg0;
    request.arg1 = arg1;
    let mut response = CommercialMaxProtocolResponse::default();
    let call = unsafe {
        libc::syscall(
            SYS_RUSTOS_IPC_CALL as libc::c_long,
            endpoint,
            (&request as *const CommercialMaxProtocolRequest) as u64,
            size_of::<CommercialMaxProtocolRequest>() as u64,
            (&mut response as *mut CommercialMaxProtocolResponse) as u64,
            size_of::<CommercialMaxProtocolResponse>() as u64,
        ) as i64
    };
    if call < 0 {
        return Err((-call) as i32);
    }
    if call as usize != size_of::<CommercialMaxProtocolResponse>()
        || !response.is_valid_envelope_for(&request)
        || response.descriptor_count != 0
        || response.payload_len != 0
    {
        return Err(libc::EINVAL);
    }
    if response.status != 0 {
        return Err(response.status);
    }
    Ok(response)
}

fn rootd_service_id_for_exec(exec_path: &str) -> Option<u64> {
    match exec_path {
        NETD_EXEC_PATH => Some(IPC_SERVICE_NETD),
        DEVMGRD_EXEC_PATH => Some(IPC_SERVICE_DEVMGRD),
        INPUTD_EXEC_PATH => Some(IPC_SERVICE_INPUTD),
        STORAGED_EXEC_PATH => Some(IPC_SERVICE_STORAGED),
        RUNTIMED_EXEC_PATH => Some(IPC_SERVICE_SESSIOND),
        _ => None,
    }
}

fn report_rootd_service_lease_inner(service_id: u64, exec_path: &str, pid: u64) -> Result<(), i32> {
    let endpoint = lookup_service_endpoint(IPC_SERVICE_ROOTD)?;
    let mut request = CommercialMaxProtocolRequest::default();
    request.header.version = COMMERCIAL_MAX_PROTOCOL_ABI_VERSION;
    request.header.protocol = COMMERCIAL_MAX_PROTOCOL_ROOTD_SUPERVISOR;
    request.header.op = COMMERCIAL_MAX_ROOTD_OP_READINESS_SIGNAL;
    request.header.subject_pid = u64::from(std::process::id());
    request.header.subject_tid = current_tid();
    request.arg0 = service_id;
    request.arg1 = pid;
    let path = exec_path.as_bytes();
    if path.len() > request.path.len() || path.contains(&0) {
        return Err(libc::EINVAL);
    }
    request.path_len = path.len() as u32;
    request.path[..path.len()].copy_from_slice(path);
    let mut response = CommercialMaxProtocolResponse::default();
    let call = unsafe {
        libc::syscall(
            SYS_RUSTOS_IPC_CALL as libc::c_long,
            endpoint,
            (&request as *const CommercialMaxProtocolRequest) as u64,
            size_of::<CommercialMaxProtocolRequest>() as u64,
            (&mut response as *mut CommercialMaxProtocolResponse) as u64,
            size_of::<CommercialMaxProtocolResponse>() as u64,
        ) as i64
    };
    if call < 0 {
        return Err((-call) as i32);
    }
    if call as usize != size_of::<CommercialMaxProtocolResponse>()
        || !response.is_valid_envelope_for(&request)
        || response.descriptor_count != 0
        || response.payload_len != 0
        || response.value0 != service_id
        || response.value1 != pid
    {
        return Err(libc::EINVAL);
    }
    if response.status != 0 {
        return Err(response.status);
    }
    Ok(())
}

fn lookup_service_endpoint(service_id: u64) -> Result<u64, i32> {
    let endpoint = unsafe {
        libc::syscall(
            SYS_RUSTOS_IPC_LOOKUP_SERVICE_ENDPOINT as libc::c_long,
            service_id,
        ) as i64
    };
    if endpoint < 0 {
        Err((-endpoint) as i32)
    } else if endpoint == 0 {
        Err(libc::ENOENT)
    } else {
        Ok(endpoint as u64)
    }
}

fn current_tid() -> u64 {
    unsafe { libc::syscall(libc::SYS_gettid as libc::c_long) as u64 }
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
    retry_state: &mut BTreeMap<String, RetryState>,
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
                    record_launch_failure(retry_state, service.exec.as_str(), "exit", status);
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

fn record_launch_failure(
    retry_state: &mut BTreeMap<String, RetryState>,
    exec_path: &str,
    reason: &str,
    status: i32,
) {
    let state = retry_state
        .entry(exec_path.to_string())
        .or_insert_with(|| RetryState {
            next_attempt: Instant::now(),
            failures: 0,
        });
    state.failures = state.failures.saturating_add(1);
    if state.failures >= INITD_LAUNCH_MAX_FAILURES {
        fail_closed(&format!(
            "initd: fatal launch failure exec={exec_path} reason={reason} status={status} failures={}",
            state.failures
        ));
    }
    state.next_attempt = Instant::now() + RETRY_BACKOFF;
    observability_client::warn!(
        "initd",
        service,
        "launch retry scheduled exec={} reason={} status={} failures={} max={}",
        exec_path,
        reason,
        status,
        state.failures,
        INITD_LAUNCH_MAX_FAILURES
    );
}

fn fail_closed(message: &str) -> ! {
    boot_line(message);
    observability_client::error!("initd", service, "{message}");
    std::process::exit(111);
}

fn cleanup_spawned_service(
    pid: i32,
    terminate: impl FnOnce(i32) -> Result<(), i32>,
) -> Result<(), i32> {
    if pid <= 0 {
        return Err(libc::EINVAL);
    }
    match terminate(pid) {
        Ok(()) | Err(libc::ESRCH) => Ok(()),
        Err(errno) => Err(errno),
    }
}

fn terminate_spawned_service(pid: i32) -> Result<(), i32> {
    let status =
        unsafe { libc::syscall(libc::SYS_tgkill as libc::c_long, pid, pid, libc::SIGKILL) as i32 };
    if status < 0 {
        return Err(last_errno());
    }
    Ok(())
}

fn fail_closed_after_child_cleanup(pid: i32, message: &str) -> ! {
    if let Err(errno) = cleanup_spawned_service(pid, terminate_spawned_service) {
        fail_closed(&format!(
            "{message}; exact child cleanup rejected cleanup_errno={errno}"
        ));
    }
    fail_closed(message)
}

fn spawn_exec(exec_path: &str, env: &[CString]) -> Result<i32, i32> {
    boot_line(&format!("initd: spawn begin exec={exec_path}"));
    if service_ready(IPC_SERVICE_LOADERD) {
        return spawn_exec_via_loaderd(exec_path, env);
    }

    Err(libc::EAGAIN)
}

fn spawn_exec_via_loaderd(exec_path: &str, env: &[CString]) -> Result<i32, i32> {
    let argv = [CString::new(exec_path).map_err(|_| libc::EINVAL)?];
    let request = build_loader_spawn_request(exec_path, &argv, env)?;
    let endpoint = lookup_loader_endpoint()?;
    let mut response = LoaderSpawnResponse::default();
    boot_line(&format!(
        "initd: loader call begin exec={exec_path} endpoint={endpoint}"
    ));
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

fn activate_spawned_service(pid: i32) -> Result<(), i32> {
    let request = LoaderSpawnRequest {
        version: LOADER_REQUEST_ABI_VERSION,
        op: LOADER_OP_ACTIVATE,
        target_pid: u64::try_from(pid).map_err(|_| libc::EINVAL)?,
        requester_pid: u64::from(std::process::id()),
        ..LoaderSpawnRequest::default()
    };
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
        || response.op != LOADER_OP_ACTIVATE
        || response.pid != i64::from(pid)
    {
        return Err(libc::EINVAL);
    }
    if response.status != 0 {
        return Err(response.status);
    }
    boot_line(&format!("initd: service activated pid={pid}"));
    Ok(())
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
        flags: 1 | LOADER_SPAWN_FLAG_DEFER_START,
        console_session: 0,
        weight_micros: exec_weight_micros(exec_path),
        exec_path_len: exec_bytes.len() as u32,
        argv_count: u16::try_from(argv.len()).map_err(|_| libc::E2BIG)?,
        env_count: u16::try_from(env.len()).map_err(|_| libc::E2BIG)?,
        requester_pid: u64::from(std::process::id()),
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
    match exec_path {
        NETD_EXEC_PATH | DEVMGRD_EXEC_PATH => EARLY_POLICY_TASK_WEIGHT_MICROS,
        INPUTD_EXEC_PATH => TASK_WEIGHT_INTERACTIVE_FLAG | DISPLAY_CRITICAL_TASK_WEIGHT_MICROS,
        RUNTIMED_EXEC_PATH => DISPLAY_CRITICAL_TASK_WEIGHT_MICROS,
        _ => DEFAULT_INIT_TASK_WEIGHT_MICROS,
    }
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
        let mut line = message.as_bytes().to_vec();
        line.push(b'\n');
        let _ = libc::syscall(0x5255_0001 as libc::c_long, line.as_ptr(), line.len());
    }
}

#[cfg(test)]
mod tests {
    use super::{classify_service_ready_status, cleanup_spawned_service};

    #[test]
    fn failed_service_cleanup_accepts_only_exact_retirement_or_esrch() {
        let mut retired = 0;
        assert_eq!(
            cleanup_spawned_service(81, |pid| {
                retired = pid;
                Ok(())
            }),
            Ok(())
        );
        assert_eq!(retired, 81);
        assert_eq!(cleanup_spawned_service(82, |_| Err(libc::ESRCH)), Ok(()));
        assert_eq!(
            cleanup_spawned_service(83, |_| Err(libc::EPERM)),
            Err(libc::EPERM)
        );
        assert_eq!(cleanup_spawned_service(0, |_| Ok(())), Err(libc::EINVAL));
    }

    #[test]
    fn service_readiness_retries_only_an_unpublished_endpoint() {
        assert_eq!(classify_service_ready_status(41), Ok(true));
        assert_eq!(
            classify_service_ready_status(-(libc::ENOSYS as i64)),
            Ok(false)
        );
        assert_eq!(
            classify_service_ready_status(-(libc::EPERM as i64)),
            Err(libc::EPERM)
        );
        assert_eq!(classify_service_ready_status(0), Err(libc::EPROTO));
    }

    #[test]
    fn init_identity_is_published_before_any_loader_request_and_is_marked_requestless() {
        let source = include_str!("main.rs");
        let publish = source
            .find("publish_init_identity()")
            .expect("initd identity publication");
        let load = source
            .find("load_init_entries()")
            .expect("first startup load step");
        assert!(publish < load);
        assert!(source.contains("ZERO_TRUST_IDENTITY_ONLY_ENDPOINT"));
        assert!(source.contains("SYS_RUSTOS_IPC_REGISTER_SERVICE_ENDPOINT"));
    }
}
