//! Post-root bootstrap, service dependency, and exact child-lease supervisor.
//!
//! - **Owner:** `initd` owns the signed startup graph and post-init children;
//!   rootd remains the authority for exact service leases.
//! - **Boundary:** Registry entries, loader replies, rootd lease state, child
//!   exits, and service endpoint publications are untrusted inputs.
//! - **Lifecycle:** Publish init identity, reconcile leases, create children
//!   suspended, activate exact PIDs, admit endpoint ownership, and supervise
//!   restart or terminal cleanup.
//! - **Concurrency:** The supervisor loop serializes graph mutation; only
//!   independent child initialization overlaps before an explicit barrier.
//! - **Failure:** Malformed state, foreign ownership, timeout, or uncertain
//!   cleanup terminates initd after bounded exact-child cleanup.
//! - **Forbidden:** No activation-as-readiness, fabricated dependency,
//!   unbounded wait, foreign PID adoption, or policy fallback in ring0.
//! - **Evidence:** `service-bootstrap-lifecycle`,
//!   `post-init-bootstrap-barrier`, `atomic-process-activation-batch`, and
//!   `post-init-supervisor-recovery`.

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
use rustos_user_abi::performance::IPC_BOOT_CONTROL_HARD_LIMIT_MS;
use rustos_user_abi::syscall::{
    CommercialMaxProtocolRequest, CommercialMaxProtocolResponse, LoaderSpawnRequest,
    LoaderSpawnResponse, RustosIpcWaitServiceEndpointArgs, RustosSchedulingContextAuthority,
    COMMERCIAL_MAX_PROTOCOL_ABI_VERSION, COMMERCIAL_MAX_PROTOCOL_ROOTD_SUPERVISOR,
    COMMERCIAL_MAX_ROOTD_OP_POST_INIT_LEASE_QUERY, COMMERCIAL_MAX_ROOTD_OP_POST_INIT_LEASE_RECLAIM,
    COMMERCIAL_MAX_ROOTD_OP_READINESS_SIGNAL, COMMERCIAL_MAX_ROOTD_OP_SCHEDULING_CONTEXT_GRANT,
    IPC_SERVICE_DEVMGRD, IPC_SERVICE_INITD, IPC_SERVICE_INPUTD, IPC_SERVICE_LINUX_SYSCALLD,
    IPC_SERVICE_LOADERD, IPC_SERVICE_NETD, IPC_SERVICE_ROOTD, IPC_SERVICE_SESSIOND,
    IPC_SERVICE_STORAGED, IPC_SERVICE_VFSD, IPC_WAIT_SERVICE_ENDPOINT_ABI_VERSION,
    IPC_WAIT_SERVICE_ENDPOINT_MAX_TIMEOUT_MS, LOADER_ACTIVATE_BATCH_MAX_TARGETS,
    LOADER_OP_SPAWN_EXEC, LOADER_REQUEST_ABI_VERSION, LOADER_SPAWN_ARG_BYTES,
    LOADER_SPAWN_ENV_BYTES, LOADER_SPAWN_EXEC_PATH_CAPACITY, LOADER_SPAWN_FLAG_DEFER_START,
    PRODUCT_MILESTONE_INIT_IDENTITY_READY, ROOTD_LEASE_STATE_EMPTY, ROOTD_LEASE_STATE_EXITED,
    ROOTD_LEASE_STATE_RUNNING, SYS_RUSTOS_IPC_CALL_BOUNDED, SYS_RUSTOS_IPC_ENDPOINT_CREATE,
    SYS_RUSTOS_IPC_LOOKUP_SERVICE_ENDPOINT, SYS_RUSTOS_IPC_REGISTER_SERVICE_ENDPOINT,
    SYS_RUSTOS_IPC_WAIT_SERVICE_ENDPOINT, SYS_RUSTOS_PRODUCT_MILESTONE,
    TASK_WEIGHT_INTERACTIVE_FLAG,
};

mod activation;
mod scheduling_context;

use scheduling_context::request_scheduling_context_authority;
mod boot_order;
mod bootstrap_barrier;

use activation::activate_pending_services;
use boot_order::{init_exec_priority, requires_immediate_activation_after_spawn};
use bootstrap_barrier::{
    bootstrap_endpoint_admissions_complete, consumer_requires_bootstrap_barrier,
    endpoint_admission_may_overlap, RUNTIMED_BOOTSTRAP_SERVICES,
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
// Until lifecycle and loader control share one event wait object, keep the
// supervisor's nonblocking sources on one bounded timer. A 2 ms cadence left
// an otherwise idle User-class task runnable often enough to crowd the UI.
const POLL_INTERVAL: Duration = Duration::from_millis(10);
const RETRY_BACKOFF: Duration = Duration::from_millis(50);
const ROOTD_LEASE_RECONCILIATION_INTERVAL: Duration = Duration::from_millis(100);
const ROOTD_LEASE_RECOVERY_TIMEOUT: Duration =
    Duration::from_millis(IPC_WAIT_SERVICE_ENDPOINT_MAX_TIMEOUT_MS);
const INITD_LAUNCH_MAX_FAILURES: u32 = 32;
const ROOTD_LEASE_REPORT_MAX_ATTEMPTS: u32 = 16;
const DEFAULT_INIT_TASK_WEIGHT_MICROS: u64 = 1_000;
const EARLY_POLICY_TASK_WEIGHT_MICROS: u64 = 4_000;
const DISPLAY_CRITICAL_TASK_WEIGHT_MICROS: u64 = 2_000;
// The signed early-system image contains runtimed, uiserver, their immutable
// registries, and the exact dynamic-loader closure.  Storage-DVM publication
// proves storage-policy liveness, not a prerequisite for this UI core.  Keep
// mutable applications and DVM-volume assets behind storaged readiness, but
// do not make the first visible desktop wait for an unrelated block FLUSH.
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
    /// Exact `(service_id, pid)` endpoint ownership has been observed after
    /// activation. A running child is not dependency authority until true.
    endpoint_ready: bool,
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
    // Debugcon bytes from multiple CPUs may interleave. The kernel-stamped
    // milestone is the durable acceptance witness for this authority boundary.
    // SAFETY: the milestone ABI accepts only three scalar values and returns
    // no borrowed memory or authority; the fixed identity is already live.
    let _ = unsafe {
        libc::syscall(
            SYS_RUSTOS_PRODUCT_MILESTONE as libc::c_long,
            PRODUCT_MILESTONE_INIT_IDENTITY_READY,
            0_u64,
            0_u64,
        )
    };
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

        let mut spawned_packages = running
            .values()
            .map(|service| service.package_id.clone())
            .collect::<BTreeSet<_>>();
        let mut ready_packages = running
            .values()
            .filter(|service| service.endpoint_ready)
            .map(|service| service.package_id.clone())
            .collect::<BTreeSet<_>>();
        let now = Instant::now();
        let mut launched_this_round = false;
        let mut pending_activations = Vec::<i32>::with_capacity(LOADER_ACTIVATE_BATCH_MAX_TARGETS);
        for entry in startup_entries.iter() {
            if !runtime_deps_satisfied(
                &entry.runtime_deps,
                &ready_packages,
                &launched_once_packages,
            ) {
                continue;
            }
            if consumer_requires_bootstrap_barrier(entry.exec.as_str()) {
                if !bootstrap_endpoint_admissions_complete(&running)
                    && !pending_activations.is_empty()
                {
                    activate_pending_services(
                        &mut pending_activations,
                        &mut running,
                        &mut ready_packages,
                        &mut launched_once_packages,
                        &mut defer_secondary_services_until,
                    );
                }
                settle_bootstrap_endpoint_barrier(
                    &mut running,
                    &mut ready_packages,
                    &mut launched_once_packages,
                );
            }
            if !launch_gate_satisfied(entry.exec.as_str(), &running) {
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
                if spawned_packages.contains(&entry.package_id) {
                    continue;
                }
            } else if spawned_packages.contains(&entry.package_id)
                || launched_once_packages.contains(entry.package_id.as_str())
            {
                continue;
            }

            if pending_activations.len() == LOADER_ACTIVATE_BATCH_MAX_TARGETS {
                activate_pending_services(
                    &mut pending_activations,
                    &mut running,
                    &mut ready_packages,
                    &mut launched_once_packages,
                    &mut defer_secondary_services_until,
                );
            }
            match spawn_exec(entry.exec.as_str(), &init_env) {
                Ok(pid) => {
                    running.insert(
                        pid,
                        RunningService {
                            package_id: entry.package_id.clone(),
                            exec: entry.exec.clone(),
                            restart: entry.restart,
                            endpoint_ready: false,
                        },
                    );
                    spawned_packages.insert(entry.package_id.clone());
                    retry_state.remove(entry.exec.as_str());
                    pending_activations.push(pid);
                    if let Err(err) = report_rootd_service_lease(entry.exec.as_str(), pid) {
                        fail_closed_after_children_cleanup(
                            &pending_activations,
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
                    if requires_immediate_activation_after_spawn(entry.exec.as_str()) {
                        // Runtimed owns the immutable early-image UI path and
                        // is independent of storaged. Publish it immediately
                        // so uiserver preparation overlaps the following
                        // DVM-storage child preparation instead of waiting in
                        // the same atomic activation cohort. Dependency-bound
                        // cohorts still use the normal batch below.
                        activate_pending_services(
                            &mut pending_activations,
                            &mut running,
                            &mut ready_packages,
                            &mut launched_once_packages,
                            &mut defer_secondary_services_until,
                        );
                    }
                    launched_this_round = true;
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

        activate_pending_services(
            &mut pending_activations,
            &mut running,
            &mut ready_packages,
            &mut launched_once_packages,
            &mut defer_secondary_services_until,
        );
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
    // SAFETY: The endpoint-create syscall takes no user pointers; its signed
    // return is validated before it becomes publication authority.
    let endpoint = unsafe { libc::syscall(SYS_RUSTOS_IPC_ENDPOINT_CREATE as libc::c_long) as i64 };
    if endpoint <= 0 {
        return Err(if endpoint < 0 {
            (-endpoint) as i32
        } else {
            libc::EIO
        });
    }
    // SAFETY: Both arguments are validated scalar ABI values and `endpoint`
    // names the live object returned above.
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

fn settle_bootstrap_endpoint_barrier(
    running: &mut BTreeMap<i32, RunningService>,
    ready_packages: &mut BTreeSet<String>,
    launched_once_packages: &mut BTreeSet<String>,
) {
    let pending = running
        .iter()
        .filter(|(_, service)| {
            endpoint_admission_may_overlap(service.exec.as_str()) && !service.endpoint_ready
        })
        .map(|(pid, _)| *pid)
        .collect::<Vec<_>>();
    for pid in pending {
        admit_running_service_endpoint(running, ready_packages, launched_once_packages, pid);
    }
}

fn admit_running_service_endpoint(
    running: &mut BTreeMap<i32, RunningService>,
    ready_packages: &mut BTreeSet<String>,
    launched_once_packages: &mut BTreeSet<String>,
    pid: i32,
) {
    let (exec, package_id, restart) = running
        .get(&pid)
        .map(|service| {
            (
                service.exec.clone(),
                service.package_id.clone(),
                service.restart,
            )
        })
        .unwrap_or_else(|| fail_closed(&format!("initd: endpoint barrier lost pid={pid}")));
    if let Err(err) = wait_reported_service_endpoint(exec.as_str(), pid) {
        fail_closed_after_child_cleanup(
            pid,
            &format!("initd: fatal service endpoint not ready exec={exec} pid={pid} errno={err}"),
        );
    }
    let service = running
        .get_mut(&pid)
        .unwrap_or_else(|| fail_closed(&format!("initd: endpoint admission lost pid={pid}")));
    service.endpoint_ready = true;
    ready_packages.insert(package_id.clone());
    if !restart {
        launched_once_packages.insert(package_id);
    }
    boot_line(&format!(
        "initd: service endpoint ready exec={exec} pid={pid}"
    ));
}

fn launch_gate_satisfied(exec: &str, running: &BTreeMap<i32, RunningService>) -> bool {
    match exec {
        SYSCALLD_EXEC_PATH | VFSD_EXEC_PATH => true,
        LOADERD_EXEC_PATH => {
            service_ready(IPC_SERVICE_LINUX_SYSCALLD) && service_ready(IPC_SERVICE_VFSD)
        }
        RUNTIMED_EXEC_PATH => {
            foundation_policy_services_ready() && runtimed_bootstrap_services_ready(running)
        }
        STORAGED_EXEC_PATH => {
            foundation_policy_services_ready() && runtimed_bootstrap_services_ready(running)
        }
        _ => foundation_policy_services_ready(),
    }
}

fn runtimed_bootstrap_services_ready(running: &BTreeMap<i32, RunningService>) -> bool {
    bootstrap_endpoint_admissions_complete(running)
        && RUNTIMED_BOOTSTRAP_SERVICES.into_iter().all(service_ready)
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
    // SAFETY: The lookup ABI accepts one scalar service identity and returns
    // a signed endpoint/status value without dereferencing user memory.
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
                                endpoint_ready: true,
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
    wait_reported_service_endpoint_with_timeout(exec_path, pid, IPC_BOOT_CONTROL_HARD_LIMIT_MS)
}

fn wait_reported_service_endpoint_with_timeout(
    exec_path: &str,
    pid: i32,
    timeout_ms: u64,
) -> Result<(), i32> {
    let Some(args) = endpoint_wait_args(exec_path, pid, timeout_ms)? else {
        return Ok(());
    };
    // SAFETY: `args` is a fully initialized ABI record that remains live for
    // the bounded syscall, which only reads this exact-sized record.
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
        "initd: endpoint ready event exec={exec_path} service_id={} pid={pid}",
        args.service_id
    ));
    Ok(())
}

fn endpoint_wait_args(
    exec_path: &str,
    pid: i32,
    timeout_ms: u64,
) -> Result<Option<RustosIpcWaitServiceEndpointArgs>, i32> {
    let Some(service_id) = rootd_service_id_for_exec(exec_path) else {
        return Ok(None);
    };
    if timeout_ms > IPC_WAIT_SERVICE_ENDPOINT_MAX_TIMEOUT_MS {
        return Err(libc::EINVAL);
    }
    Ok(Some(RustosIpcWaitServiceEndpointArgs {
        abi_version: IPC_WAIT_SERVICE_ENDPOINT_ABI_VERSION,
        service_id,
        expected_pid: u64::try_from(pid).map_err(|_| libc::EINVAL)?,
        timeout_ms,
        ..RustosIpcWaitServiceEndpointArgs::default()
    }))
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
    // SAFETY: Request and response are initialized, exact-sized ABI records
    // and remain exclusively live for the duration of the bounded call.
    let call = unsafe {
        libc::syscall(
            SYS_RUSTOS_IPC_CALL_BOUNDED as libc::c_long,
            endpoint,
            (&request as *const CommercialMaxProtocolRequest) as u64,
            size_of::<CommercialMaxProtocolRequest>() as u64,
            (&mut response as *mut CommercialMaxProtocolResponse) as u64,
            size_of::<CommercialMaxProtocolResponse>() as u64,
            IPC_BOOT_CONTROL_HARD_LIMIT_MS,
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

fn rootd_exec_for_service_id(service_id: u64) -> Option<&'static str> {
    match service_id {
        IPC_SERVICE_NETD => Some(NETD_EXEC_PATH),
        IPC_SERVICE_DEVMGRD => Some(DEVMGRD_EXEC_PATH),
        IPC_SERVICE_INPUTD => Some(INPUTD_EXEC_PATH),
        IPC_SERVICE_STORAGED => Some(STORAGED_EXEC_PATH),
        IPC_SERVICE_SESSIOND => Some(RUNTIMED_EXEC_PATH),
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
    // SAFETY: Request and response are initialized, exact-sized ABI records
    // and remain exclusively live for the duration of the bounded call.
    let call = unsafe {
        libc::syscall(
            SYS_RUSTOS_IPC_CALL_BOUNDED as libc::c_long,
            endpoint,
            (&request as *const CommercialMaxProtocolRequest) as u64,
            size_of::<CommercialMaxProtocolRequest>() as u64,
            (&mut response as *mut CommercialMaxProtocolResponse) as u64,
            size_of::<CommercialMaxProtocolResponse>() as u64,
            IPC_BOOT_CONTROL_HARD_LIMIT_MS,
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
    // SAFETY: The lookup ABI takes one scalar service identity and returns a
    // signed value that is checked before conversion to an endpoint handle.
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
    // SAFETY: gettid takes no pointers and returns the calling thread identity.
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
        // SAFETY: `status` is writable for the syscall duration, WNOHANG makes
        // the operation bounded, and the optional rusage pointer is null.
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
    // SAFETY: tgkill receives only the exact positive process/thread identity
    // already checked by `cleanup_spawned_service` and a fixed signal.
    let status =
        unsafe { libc::syscall(libc::SYS_tgkill as libc::c_long, pid, pid, libc::SIGKILL) as i32 };
    if status < 0 {
        return Err(last_errno());
    }
    Ok(())
}

fn fail_closed_after_child_cleanup(pid: i32, message: &str) -> ! {
    fail_closed_after_children_cleanup(&[pid], message)
}

fn fail_closed_after_children_cleanup(pids: &[i32], message: &str) -> ! {
    let mut first_cleanup_error = None;
    for pid in pids.iter().copied() {
        if let Err(errno) = cleanup_spawned_service(pid, terminate_spawned_service) {
            if first_cleanup_error.is_none() {
                first_cleanup_error = Some((pid, errno));
            }
        }
    }
    if let Some((pid, errno)) = first_cleanup_error {
        fail_closed(&format!(
            "{message}; exact child cleanup rejected pid={pid} cleanup_errno={errno}"
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
    // SAFETY: Request and response are initialized, exact-sized ABI records
    // and remain exclusively live for the duration of the bounded call.
    let call = unsafe {
        libc::syscall(
            SYS_RUSTOS_IPC_CALL_BOUNDED as libc::c_long,
            endpoint,
            (&request as *const LoaderSpawnRequest) as u64,
            size_of::<LoaderSpawnRequest>() as u64,
            (&mut response as *mut LoaderSpawnResponse) as u64,
            size_of::<LoaderSpawnResponse>() as u64,
            IPC_BOOT_CONTROL_HARD_LIMIT_MS,
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
        flags: 1 | LOADER_SPAWN_FLAG_DEFER_START,
        console_session: 0,
        weight_micros: exec_weight_micros(exec_path),
        exec_path_len: exec_bytes.len() as u32,
        argv_count: u16::try_from(argv.len()).map_err(|_| libc::E2BIG)?,
        env_count: u16::try_from(env.len()).map_err(|_| libc::E2BIG)?,
        requester_pid: u64::from(std::process::id()),
        scheduling_context: request_scheduling_context_authority(exec_path)?,
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
    // SAFETY: The lookup ABI takes one fixed scalar service identity; the
    // signed result is validated before entering the process-local cache.
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
        RUNTIMED_EXEC_PATH => TASK_WEIGHT_INTERACTIVE_FLAG | DISPLAY_CRITICAL_TASK_WEIGHT_MICROS,
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
    // SAFETY: The diagnostic syscall copies the byte slice synchronously;
    // `line` remains live and immutable for the syscall duration.
    unsafe {
        let mut line = message.as_bytes().to_vec();
        line.push(b'\n');
        let _ = libc::syscall(0x5255_0001 as libc::c_long, line.as_ptr(), line.len());
    }
}

#[cfg(test)]
mod tests {
    use super::{
        classify_service_ready_status, cleanup_spawned_service, endpoint_wait_args,
        exec_weight_micros, POLL_INTERVAL, RUNTIMED_EXEC_PATH, TASK_WEIGHT_INTERACTIVE_FLAG,
    };

    #[test]
    fn steady_supervisor_poll_is_bounded_without_two_millisecond_churn() {
        assert_eq!(POLL_INTERVAL, std::time::Duration::from_millis(10));
    }

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
    fn runtimed_bootstrap_donates_interactive_priority_to_the_loader_chain() {
        assert_ne!(
            exec_weight_micros(RUNTIMED_EXEC_PATH) & TASK_WEIGHT_INTERACTIVE_FLAG,
            0
        );
    }

    #[test]
    fn endpoint_barrier_wait_is_exact_pid_bound_and_bounded() {
        let args = endpoint_wait_args(super::NETD_EXEC_PATH, 73, 17)
            .expect("valid wait")
            .expect("service wait");
        assert_eq!(args.expected_pid, 73);
        assert_eq!(args.timeout_ms, 17);
        assert_eq!(args.service_id, rustos_user_abi::syscall::IPC_SERVICE_NETD);
        assert!(endpoint_wait_args(super::NETD_EXEC_PATH, -1, 17).is_err());
        assert!(endpoint_wait_args(
            super::NETD_EXEC_PATH,
            73,
            rustos_user_abi::syscall::IPC_WAIT_SERVICE_ENDPOINT_MAX_TIMEOUT_MS + 1,
        )
        .is_err());
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
