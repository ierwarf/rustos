//! Runtime launch and strict scheduling-class admission.
//!
//! - **Owner:** `runtimed` owns app/session launch policy; loaderd/procd own
//!   executable and process policy and `kernel-ps` owns scheduling mechanism.
//! - **Boundary:** Launch manifests, executable names, caller credentials,
//!   environment, and requested scheduling class are untrusted.
//! - **Lifecycle:** Resolve a signed declaration, prepare/launch exact child,
//!   admit optional class once, publish session ownership, or retire failure.
//! - **Concurrency:** Launch state is bounded and no mutable registry lock is
//!   held across loader calls.
//! - **Failure:** Unknown program, foreign caller, capacity, timeout, loader
//!   restart, and activation failure clean up the exact suspended child.
//! - **Forbidden:** No caller-selected strict priority, pathname bypass,
//!   implicit fallback program, or scheduler policy in ring0.
//! - **Evidence:** `scheduler-dispatch`, `deferred-process-activation`, and
//!   `runtime-control-ingress`.
use std::ffi::CString;
use std::mem::size_of;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::sync::atomic::Ordering;
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use runtime_control::read_bounded_config_snapshot;
use rustos_user_abi::performance::IPC_BOOT_CONTROL_HARD_LIMIT_MS;
use rustos_user_abi::syscall::{
    smp_qualification_bind_shape_valid, CommercialMaxProtocolRequest,
    CommercialMaxProtocolResponse, LoaderSpawnRequest, LoaderSpawnResponse,
    RustosIpcWaitServiceEndpointArgs, RustosSmpQualificationBindArgs,
    COMMERCIAL_MAX_PROTOCOL_ABI_VERSION, COMMERCIAL_MAX_PROTOCOL_ROOTD_SUPERVISOR,
    COMMERCIAL_MAX_ROOTD_OP_READINESS_SIGNAL, IPC_SERVICE_LOADERD, IPC_SERVICE_ROOTD,
    IPC_SERVICE_UISERVER, IPC_WAIT_SERVICE_ENDPOINT_ABI_VERSION, LOADER_OP_ACTIVATE,
    LOADER_OP_SPAWN_EXEC, LOADER_REQUEST_ABI_VERSION, LOADER_SPAWN_ARG_BYTES,
    LOADER_SPAWN_ENV_BYTES, LOADER_SPAWN_EXEC_PATH_CAPACITY, LOADER_SPAWN_FLAG_DEFER_START,
    SMP_QUALIFICATION_BIND_ABI_VERSION, SYS_RUSTOS_IPC_CALL_BOUNDED,
    SYS_RUSTOS_IPC_LOOKUP_SERVICE_ENDPOINT, SYS_RUSTOS_IPC_WAIT_SERVICE_ENDPOINT,
    SYS_RUSTOS_SMP_QUALIFICATION_BIND,
};

use super::kvm_smp_qualification::{
    qualification_contract_for_launch, KvmSmpQualificationContract,
};
use super::{
    boot_line, AT_FDCWD, CONSOLE_PATH, CONSOLE_SESSION_STATE_LOADING_IMAGE,
    CONSOLE_SESSION_STATE_RUNNING, CONSOLE_SESSION_STATE_SPAWNING, DEFAULT_USER_TASK_WEIGHT_MICROS,
    IDLE_POLL_INTERVAL, LOADER_ENDPOINT_CACHE, MAX_UNTRUSTED_TASK_WEIGHT_MICROS,
    MIN_EFFECTIVE_TASK_WEIGHT_MICROS, O_RDWR, SYS_OPENAT, UI_SERVER_EXEC_PATH,
    UI_SERVER_TASK_WEIGHT_MICROS,
};
use super::{BrokerState, LaunchEntry, RunningProcess};

const KVM_ACCEPTANCE_CONTRACT_PATH: &str = "/system/registry/system/kvm-acceptance-v1.env";
const KVM_ACCEPTANCE_CONTRACT_MAX_BYTES: usize = 256;

#[derive(Clone, Copy)]
struct KvmAcceptanceContract {
    ui_profile: bool,
    network_exercise: bool,
}

pub(super) fn spawn_tracked_process(
    state: &mut BrokerState,
    mut entry: LaunchEntry,
) -> Result<(), i32> {
    apply_kvm_acceptance_contract(&mut entry);
    let qualification_contract =
        qualification_contract_for_launch(&entry).map_err(|()| libc::EINVAL)?;
    boot_line(
        format!(
            "runtimed: spawn begin desktop_id={} exec={} console_hosted={} logical_admin={}",
            entry.desktop_file_id, entry.exec, entry.console_hosted, entry.logical_admin
        )
        .as_str(),
    );
    let session_handle = if entry.console_hosted {
        let _ = ensure_console_fd(state)?;
        let session = allocate_console_session(state);
        state.session_runtime.create_session(session);
        let _ = CONSOLE_SESSION_STATE_LOADING_IMAGE;
        Some(session)
    } else {
        None
    };
    let is_ui_server = entry.exec == UI_SERVER_EXEC_PATH;
    // Every catalog launch is a supervisor transaction: create the task in a
    // start-suspended state, record its ownership below, then activate it.
    // Letting ordinary apps become runnable before runtimed records the PID
    // leaves an unsupervised window and gives the scheduler no safe post-reply
    // handoff point under a busy UI/input IPC workload.
    let deferred_start = true;
    let pid = match spawn_exec(
        entry.exec.as_str(),
        entry.args.as_slice(),
        entry.env.as_slice(),
        entry.logical_admin,
        entry.weight_micros,
        session_handle.unwrap_or(0),
        deferred_start,
    ) {
        Ok(pid) => pid,
        Err(err) => {
            if let Some(session) = session_handle {
                state.session_runtime.remove_session(session);
                let _ = close_console_session(ensure_console_fd(state)?, session);
            }
            if is_permanent_launch_failure(err) {
                observability_client::warn!(
                    "runtimed",
                    service,
                    "spawn exec permanent failure desktop_id={} exec={} errno={err}",
                    entry.desktop_file_id,
                    entry.exec
                );
            } else {
                observability_client::error!(
                    "runtimed",
                    service,
                    "spawn exec failed desktop_id={} exec={} errno={err}",
                    entry.desktop_file_id,
                    entry.exec
                );
            }
            return Err(err);
        }
    };
    boot_line(
        format!(
            "runtimed: spawned desktop_id={} exec={} pid={}",
            entry.desktop_file_id, entry.exec, pid
        )
        .as_str(),
    );
    if is_ui_server {
        if let Err(err) = report_rootd_service_lease(IPC_SERVICE_UISERVER, entry.exec.as_str(), pid)
        {
            retire_failed_spawn_or_abort(pid, "rootd-lease-report");
            release_failed_session(state, session_handle);
            return Err(err);
        }
    }
    let desktop_file_id = entry.desktop_file_id.clone();
    let inserted_session_handle = session_handle.unwrap_or(0);
    if state.running.contains_key(&pid) {
        retire_failed_spawn_or_abort(pid, "duplicate-pid");
        release_failed_session(state, session_handle);
        return Err(libc::EEXIST);
    }
    state.running.insert(
        pid,
        RunningProcess {
            pid,
            package_id: entry.package_id,
            desktop_file_id: entry.desktop_file_id,
            display_name: entry.display_name,
            exec: entry.exec,
            session_handle: inserted_session_handle,
            restart: entry.restart,
            logical_admin: entry.logical_admin,
        },
    );

    // The private workload becomes runnable only after ring0 has bound this
    // exact suspended child to runtimed's live SESSIOND generation and the
    // closed qualification contract. Bind failure is terminal for the child;
    // falling through to loader activation would create unauthenticated KVM
    // evidence under the same executable path.
    if let Err((stage, err)) = bind_then_activate_spawned_process(
        pid,
        qualification_contract,
        bind_smp_qualification,
        activate_spawned_process,
    ) {
        state.running.remove(&pid);
        retire_failed_spawn_or_abort(pid, stage.cleanup_stage());
        release_failed_session(state, session_handle);
        return Err(err);
    }
    if is_ui_server {
        if let Err(err) = wait_for_service_endpoint(IPC_SERVICE_UISERVER, pid) {
            state.running.remove(&pid);
            retire_failed_spawn_or_abort(pid, "endpoint-wait");
            release_failed_session(state, session_handle);
            return Err(err);
        }
        boot_line(format!("runtimed: uiserver endpoint ready pid={pid}").as_str());
    }
    state.retry_after.remove(desktop_file_id.as_str());
    state.launch_failure_counts.remove(desktop_file_id.as_str());
    if inserted_session_handle != 0 {
        let _ = ensure_console_fd(state)?;
        let _ = CONSOLE_SESSION_STATE_SPAWNING;
        let _ = CONSOLE_SESSION_STATE_RUNNING;
        super::session::focus_session_after_spawn(state, inserted_session_handle);
    }
    Ok(())
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum PreActivationStage {
    SmpQualificationBind,
    LoaderActivate,
}

impl PreActivationStage {
    const fn cleanup_stage(self) -> &'static str {
        match self {
            Self::SmpQualificationBind => "smp-qualification-bind",
            Self::LoaderActivate => "loader-activate",
        }
    }
}

fn bind_then_activate_spawned_process<Bind, Activate>(
    pid: i32,
    qualification_contract: Option<KvmSmpQualificationContract>,
    mut bind: Bind,
    mut activate: Activate,
) -> Result<(), (PreActivationStage, i32)>
where
    Bind: FnMut(i32, KvmSmpQualificationContract) -> Result<(), i32>,
    Activate: FnMut(i32) -> Result<(), i32>,
{
    if let Some(contract) = qualification_contract {
        bind(pid, contract).map_err(|errno| (PreActivationStage::SmpQualificationBind, errno))?;
    }
    activate(pid).map_err(|errno| (PreActivationStage::LoaderActivate, errno))
}

fn smp_qualification_bind_args(
    pid: i32,
    contract: KvmSmpQualificationContract,
) -> Result<RustosSmpQualificationBindArgs, i32> {
    if pid <= 0 {
        return Err(libc::EINVAL);
    }
    let args = RustosSmpQualificationBindArgs {
        abi_version: SMP_QUALIFICATION_BIND_ABI_VERSION,
        target_pid: u64::try_from(pid).map_err(|_| libc::EINVAL)?,
        workers: u32::from(contract.workers),
        work_units: u64::from(contract.work_units),
        deadline_ms: contract.deadline_ms,
        ..RustosSmpQualificationBindArgs::default()
    };
    smp_qualification_bind_shape_valid(&args)
        .then_some(args)
        .ok_or(libc::EINVAL)
}

fn bind_smp_qualification(pid: i32, contract: KvmSmpQualificationContract) -> Result<(), i32> {
    let args = smp_qualification_bind_args(pid, contract)?;
    // SAFETY: `args` is a fully initialized, locally shape-validated fixed ABI
    // record and remains live for the duration of the synchronous syscall.
    let result = unsafe {
        libc::syscall(
            SYS_RUSTOS_SMP_QUALIFICATION_BIND as libc::c_long,
            (&args as *const RustosSmpQualificationBindArgs) as u64,
        ) as i64
    };
    if result < 0 {
        Err((-result) as i32)
    } else if result != 0 {
        Err(libc::EINVAL)
    } else {
        Ok(())
    }
}

fn apply_kvm_acceptance_contract(entry: &mut LaunchEntry) {
    // The private acceptance file is deliberately outside the signed
    // early-system extent set. uiserver is launched before storaged and may
    // therefore observe it as unavailable, while later application launches
    // can read it through the admitted DVM volume. Cache only a successfully
    // parsed contract: negative caching would permanently discard WayClick
    // and netprobe proof authority based on one expected bootstrap miss.
    static CONTRACT: OnceLock<KvmAcceptanceContract> = OnceLock::new();
    let contract = if let Some(contract) = CONTRACT.get().copied() {
        contract
    } else {
        let Some(contract) = load_kvm_acceptance_contract() else {
            return;
        };
        let _ = CONTRACT.set(contract);
        CONTRACT.get().copied().unwrap_or(contract)
    };
    if !contract.ui_profile && !contract.network_exercise {
        return;
    }
    if contract.ui_profile && entry.desktop_file_id == "uiserver.desktop" {
        upsert_env(&mut entry.env, "RUSTOS_UI_PROFILE=1");
        upsert_env(&mut entry.env, "RUSTOS_UI_BOOT_TRACE=1");
    }
    if contract.ui_profile && entry.desktop_file_id == "wayclick.desktop" {
        upsert_env(&mut entry.env, "RUSTOS_WAYCLICK_PROFILE=1");
    }
    // WayClick's continuous frame loop and its FPS profile lines are both
    // gated on that variable, so whether this branch was taken decides whether
    // the 55 FPS gate can be measured at all. The uiserver branch demonstrably
    // fires; this records the identity actually seen here so a mismatch names
    // itself instead of being inferred from an absent profile line.
    boot_line(
        format!(
            "runtimed: acceptance contract applied desktop_id={} ui_profile={} network_exercise={}",
            entry.desktop_file_id, contract.ui_profile, contract.network_exercise
        )
        .as_str(),
    );
    if contract.network_exercise && entry.desktop_file_id == "netprobe.desktop" {
        upsert_env(&mut entry.env, "RUSTOS_NETPROBE_QEMU=1");
    }
}

fn load_kvm_acceptance_contract() -> Option<KvmAcceptanceContract> {
    // This is an immutable policy snapshot. A sequential read would publish a
    // VFS cursor checkpoint and can lose the small KVM contract behind an
    // unrelated checkpoint burst; positioned bounded I/O is the same path
    // used by the admitted runtime registries.
    let contents = read_bounded_config_snapshot(
        KVM_ACCEPTANCE_CONTRACT_PATH,
        KVM_ACCEPTANCE_CONTRACT_MAX_BYTES,
    )
    .ok()?;
    parse_kvm_acceptance_contract(contents.as_str())
}

fn parse_kvm_acceptance_contract(contents: &str) -> Option<KvmAcceptanceContract> {
    let mut contract = false;
    let mut ui_profile = None;
    let mut network_exercise = None;
    for line in contents.lines() {
        match line {
            "contract=rustos-kvm-acceptance-v1" if !contract => contract = true,
            "ui_profile=0" if ui_profile.is_none() => ui_profile = Some(false),
            "ui_profile=1" if ui_profile.is_none() => ui_profile = Some(true),
            "network_exercise=0" if network_exercise.is_none() => network_exercise = Some(false),
            "network_exercise=1" if network_exercise.is_none() => network_exercise = Some(true),
            _ => return None,
        }
    }
    Some(KvmAcceptanceContract {
        ui_profile: ui_profile.filter(|_| contract)?,
        network_exercise: network_exercise?,
    })
}

fn upsert_env(env: &mut Vec<String>, value: &str) {
    let key = value.split_once('=').map(|(key, _)| key).unwrap_or(value);
    env.retain(|existing| {
        existing
            .split_once('=')
            .map(|(candidate, _)| candidate != key)
            .unwrap_or(true)
    });
    env.push(value.to_string());
}

fn release_failed_session(state: &mut BrokerState, session_handle: Option<u64>) {
    let Some(session) = session_handle else {
        return;
    };
    state.session_runtime.remove_session(session);
    super::session::clear_focused_session_if(state, session);
    if let Ok(console_fd) = ensure_console_fd(state) {
        let _ = close_console_session(console_fd, session);
    }
}

fn cleanup_failed_spawn(
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

fn retire_failed_spawn_or_abort(pid: i32, stage: &str) {
    if let Err(errno) = cleanup_failed_spawn(pid, terminate_pid) {
        panic!(
            "runtimed: fatal failed-spawn cleanup rejected stage={stage} pid={pid} errno={errno}"
        );
    }
}

fn report_rootd_service_lease(service_id: u64, exec_path: &str, pid: i32) -> Result<(), i32> {
    let endpoint = lookup_rootd_endpoint()?;
    let mut request = CommercialMaxProtocolRequest::default();
    request.header.version = COMMERCIAL_MAX_PROTOCOL_ABI_VERSION;
    request.header.protocol = COMMERCIAL_MAX_PROTOCOL_ROOTD_SUPERVISOR;
    request.header.op = COMMERCIAL_MAX_ROOTD_OP_READINESS_SIGNAL;
    request.header.subject_pid = u64::from(std::process::id());
    request.header.subject_tid = current_tid();
    request.arg0 = service_id;
    request.arg1 = u64::try_from(pid).map_err(|_| libc::EINVAL)?;
    let path = exec_path.as_bytes();
    if path.is_empty() || path.len() > request.path.len() || path.contains(&0) {
        return Err(libc::EINVAL);
    }
    request.path_len = path.len() as u32;
    request.path[..path.len()].copy_from_slice(path);

    let mut response = CommercialMaxProtocolResponse::default();
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
        || response.value1 != u64::try_from(pid).map_err(|_| libc::EINVAL)?
    {
        return Err(libc::EINVAL);
    }
    if response.status != 0 {
        return Err(response.status);
    }
    Ok(())
}

fn lookup_rootd_endpoint() -> Result<u64, i32> {
    let endpoint = lookup_service_endpoint(IPC_SERVICE_ROOTD);
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

pub(super) fn reap_children(state: &mut BrokerState) -> bool {
    let mut reaped_any = false;
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
            reaped_any = true;
            if let Some(process) = state.running.remove(&pid) {
                if process.session_handle != 0 {
                    state.session_runtime.remove_session(process.session_handle);
                    super::session::clear_focused_session_if(state, process.session_handle);
                    if let Ok(console_fd) = ensure_console_fd(state) {
                        let _ = close_console_session(console_fd, process.session_handle);
                    }
                }
                if process.restart {
                    super::socket::schedule_launch_retry(
                        state,
                        process.desktop_file_id.as_str(),
                        libc::ECHILD,
                    );
                }
            }
            continue;
        }
        if pid == 0 || (pid == -1 && last_errno() == libc::ECHILD) {
            break;
        }
        break;
    }
    reaped_any
}

pub(super) fn next_idle_delay(state: &BrokerState) -> Duration {
    let now = Instant::now();
    let retry_delay = state
        .retry_after
        .values()
        .map(|deadline| deadline.saturating_duration_since(now))
        .min()
        .unwrap_or(IDLE_POLL_INTERVAL);
    retry_delay.min(IDLE_POLL_INTERVAL)
}

pub(super) fn spawn_exec(
    exec_path: &str,
    argv: &[String],
    env: &[String],
    logical_admin: bool,
    weight_micros: u64,
    session_handle: u64,
    defer_start: bool,
) -> Result<i32, i32> {
    boot_line(format!("runtimed: loader request begin exec={}", exec_path).as_str());
    let argv_storage = build_exec_argv(exec_path, argv)?;
    let env_storage = build_exec_env(env)?;
    let request = build_loader_spawn_request(
        exec_path,
        &argv_storage,
        &env_storage,
        logical_admin,
        weight_micros,
        session_handle,
        defer_start,
    )?;
    let endpoint = lookup_loader_endpoint()?;
    let mut response = LoaderSpawnResponse::default();
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
    let Ok(pid) = i32::try_from(response.pid) else {
        return Err(libc::EOVERFLOW);
    };
    boot_line(
        format!(
            "runtimed: loader request returned exec={} pid={}",
            exec_path, pid
        )
        .as_str(),
    );
    Ok(pid)
}

fn build_loader_spawn_request(
    exec_path: &str,
    argv: &[CString],
    env: &[CString],
    logical_admin: bool,
    weight_micros: u64,
    session_handle: u64,
    defer_start: bool,
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
        flags: u32::from(logical_admin)
            | if defer_start {
                LOADER_SPAWN_FLAG_DEFER_START
            } else {
                0
            },
        console_session: session_handle,
        weight_micros: admitted_task_weight_micros(exec_path, weight_micros),
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

fn activate_spawned_process(pid: i32) -> Result<(), i32> {
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
        || response.op != LOADER_OP_ACTIVATE
        || response.pid != i64::from(pid)
    {
        return Err(libc::EINVAL);
    }
    if response.status != 0 {
        return Err(response.status);
    }
    boot_line(format!("runtimed: activated process pid={pid}").as_str());
    Ok(())
}

fn wait_for_service_endpoint(service_id: u64, pid: i32) -> Result<u64, i32> {
    let args = RustosIpcWaitServiceEndpointArgs {
        abi_version: IPC_WAIT_SERVICE_ENDPOINT_ABI_VERSION,
        service_id,
        expected_pid: u64::try_from(pid).map_err(|_| libc::EINVAL)?,
        timeout_ms: IPC_BOOT_CONTROL_HARD_LIMIT_MS,
        ..RustosIpcWaitServiceEndpointArgs::default()
    };
    let result = unsafe {
        libc::syscall(
            SYS_RUSTOS_IPC_WAIT_SERVICE_ENDPOINT as libc::c_long,
            (&args as *const RustosIpcWaitServiceEndpointArgs) as u64,
        ) as i64
    };
    if result < 0 {
        Err((-result) as i32)
    } else if result == 0 {
        Err(libc::ENOENT)
    } else {
        Ok(result as u64)
    }
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

/// Runtime catalog metadata can request ordinary fair-share weight, but it
/// cannot grant strict System scheduling. The sole exception is the fixed UI
/// server executable, whose launch is created by the trusted session path and
/// whose exact value is pinned here. This prevents a writable/compromised
/// desktop entry from starving input, bootstrap, or recovery by requesting a
/// large `X-RustOS-WeightMicros` value.
fn admitted_task_weight_micros(exec_path: &str, weight_micros: u64) -> u64 {
    if exec_path == UI_SERVER_EXEC_PATH {
        return UI_SERVER_TASK_WEIGHT_MICROS;
    }
    let requested = if weight_micros == 0 {
        DEFAULT_USER_TASK_WEIGHT_MICROS
    } else {
        weight_micros
    };
    requested.clamp(
        MIN_EFFECTIVE_TASK_WEIGHT_MICROS,
        MAX_UNTRUSTED_TASK_WEIGHT_MICROS,
    )
}

pub(super) fn lookup_service_endpoint(service_id: u64) -> i64 {
    unsafe {
        libc::syscall(
            SYS_RUSTOS_IPC_LOOKUP_SERVICE_ENDPOINT as libc::c_long,
            service_id,
        ) as i64
    }
}

fn lookup_loader_endpoint() -> Result<u64, i32> {
    let cached = LOADER_ENDPOINT_CACHE.load(Ordering::Relaxed);
    if cached != 0 {
        return Ok(cached);
    }
    let endpoint = lookup_service_endpoint(IPC_SERVICE_LOADERD);
    if endpoint < 0 {
        return Err((-endpoint) as i32);
    }
    let endpoint = endpoint as u64;
    if endpoint != 0 {
        LOADER_ENDPOINT_CACHE.store(endpoint, Ordering::Relaxed);
    }
    Ok(endpoint)
}

pub(super) fn loader_endpoint_ready() -> bool {
    lookup_loader_endpoint().is_ok()
}

pub(super) fn debug_line(message: &str) {
    super::debug_line(message);
}

pub(super) fn terminate_pid(pid: i32) -> Result<(), i32> {
    let rc =
        unsafe { libc::syscall(libc::SYS_tgkill as libc::c_long, pid, pid, libc::SIGKILL) as i32 };
    if rc < 0 {
        return Err(last_errno());
    }
    Ok(())
}

// --- console lifecycle (owned by runtimed's session policy endpoint) ---

fn allocate_console_session(state: &mut BrokerState) -> u64 {
    let session = state.next_session_handle.max(1);
    state.next_session_handle = session.wrapping_add(1).max(1);
    session
}

pub(super) fn close_console_session(_console_fd: RawFd, session_handle: u64) -> Result<bool, i32> {
    Ok(session_handle != 0)
}

fn open_device(path: &str, flags: usize) -> Result<OwnedFd, i32> {
    let path = CString::new(path).map_err(|_| libc::EINVAL)?;
    let raw_fd = unsafe {
        libc::syscall(
            SYS_OPENAT as libc::c_long,
            AT_FDCWD,
            path.as_ptr(),
            flags,
            0usize,
        ) as i32
    };
    if raw_fd < 0 {
        return Err(last_errno());
    }
    Ok(unsafe { OwnedFd::from_raw_fd(raw_fd) })
}

pub(super) fn ensure_console_fd(state: &mut BrokerState) -> Result<RawFd, i32> {
    if state.console_fd.is_none() {
        debug_line("runtimed: console open begin");
        boot_line("runtimed: console open begin");
        let fd = open_device(CONSOLE_PATH, O_RDWR)?;
        debug_line("runtimed: console open done");
        boot_line("runtimed: console ready");
        state.console_fd = Some(fd);
    }
    Ok(state
        .console_fd
        .as_ref()
        .map(|fd| fd.as_raw_fd())
        .unwrap_or(-1))
}

fn build_exec_argv(exec_path: &str, argv: &[String]) -> Result<Vec<CString>, i32> {
    use super::MAX_EXEC_ARG_COUNT;
    if !valid_exec_text(exec_path, false) {
        return Err(libc::EINVAL);
    }
    if argv.len() > MAX_EXEC_ARG_COUNT {
        return Err(libc::E2BIG);
    }
    if argv.is_empty() {
        return CString::new(exec_path)
            .map(|value| vec![value])
            .map_err(|_| libc::EINVAL);
    }
    argv.iter()
        .map(|arg| {
            if !valid_exec_text(arg.as_str(), false) {
                return Err(libc::EINVAL);
            }
            CString::new(arg.as_str()).map_err(|_| libc::EINVAL)
        })
        .collect()
}

fn build_exec_env(extra_env: &[String]) -> Result<Vec<CString>, i32> {
    use runtime_control::{
        load_runtime_default_env, RuntimeEnvScope, DEFAULT_RUNTIME_ENV_REGISTRY_PATH,
    };
    let default_env =
        load_runtime_default_env(DEFAULT_RUNTIME_ENV_REGISTRY_PATH, RuntimeEnvScope::Runtime)
            .map_err(runtime_registry_errno)?;
    build_exec_env_with_defaults(extra_env, &default_env)
}

fn build_exec_env_with_defaults(
    extra_env: &[String],
    default_env: &[String],
) -> Result<Vec<CString>, i32> {
    use super::MAX_EXEC_ENV_COUNT;
    if extra_env.len() > MAX_EXEC_ENV_COUNT {
        return Err(libc::E2BIG);
    }
    let mut env = Vec::with_capacity(extra_env.len().saturating_add(default_env.len()));
    for item in extra_env {
        if !valid_exec_text(item.as_str(), true) {
            return Err(libc::EINVAL);
        }
        env.push(item.clone());
    }
    for item in default_env {
        if !valid_exec_text(item, true) {
            return Err(libc::EINVAL);
        }
        push_env_if_missing(&mut env, item)?;
    }
    env.into_iter()
        .map(|item| CString::new(item).map_err(|_| libc::EINVAL))
        .collect()
}

fn push_env_if_missing(env: &mut Vec<String>, item: &str) -> Result<(), i32> {
    use super::MAX_EXEC_ENV_COUNT;
    let key = env_key(item);
    if env.iter().any(|candidate| env_key(candidate) == key) {
        return Ok(());
    }
    if env.len() >= MAX_EXEC_ENV_COUNT {
        return Err(libc::E2BIG);
    }
    env.push(item.to_string());
    Ok(())
}

fn env_key(value: &str) -> &str {
    value.split_once('=').map(|(key, _)| key).unwrap_or(value)
}

fn runtime_registry_errno(error: std::io::Error) -> i32 {
    match error.raw_os_error() {
        Some(errno) if errno > 0 => errno,
        _ => libc::EIO,
    }
}

fn valid_exec_text(value: &str, require_env_assignment: bool) -> bool {
    use super::MAX_EXEC_TEXT_BYTES;
    if value.is_empty() || value.len() > MAX_EXEC_TEXT_BYTES {
        return false;
    }
    if !value
        .bytes()
        .all(|byte| matches!(byte, b' '..=b'~') && byte != b'\\')
    {
        return false;
    }
    !require_env_assignment || valid_env_assignment(value)
}

fn valid_env_assignment(value: &str) -> bool {
    let Some((key, _)) = value.split_once('=') else {
        return false;
    };
    !key.is_empty()
        && key
            .bytes()
            .all(|byte| byte == b'_' || byte.is_ascii_alphanumeric())
}

pub(super) fn is_permanent_launch_failure(errno: i32) -> bool {
    matches!(
        errno,
        libc::EOPNOTSUPP | libc::ENOEXEC | libc::EINVAL | libc::ENOENT | libc::EACCES
    )
}

fn last_errno() -> i32 {
    std::io::Error::last_os_error()
        .raw_os_error()
        .unwrap_or(libc::EIO)
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;

    use super::{
        admitted_task_weight_micros, bind_then_activate_spawned_process, build_exec_argv,
        build_exec_env_with_defaults, cleanup_failed_spawn, parse_kvm_acceptance_contract,
        smp_qualification_bind_args, upsert_env,
    };
    use crate::{
        kvm_smp_qualification::KvmSmpQualificationContract, MAX_UNTRUSTED_TASK_WEIGHT_MICROS,
        UI_SERVER_EXEC_PATH, UI_SERVER_TASK_WEIGHT_MICROS,
    };
    use rustos_user_abi::syscall::SMP_QUALIFICATION_BIND_ABI_VERSION;

    const QUALIFICATION: KvmSmpQualificationContract = KvmSmpQualificationContract {
        workers: 4,
        work_units: 1_000_000,
        deadline_ms: 5_000,
    };

    #[test]
    fn qualification_bind_is_exact_and_precedes_activation() {
        let args = smp_qualification_bind_args(41, QUALIFICATION).expect("valid bind wire");
        assert_eq!(args.abi_version, SMP_QUALIFICATION_BIND_ABI_VERSION);
        assert_eq!(args.target_pid, 41);
        assert_eq!(args.workers, 4);
        assert_eq!(args.work_units, 1_000_000);
        assert_eq!(args.deadline_ms, 5_000);
        assert_eq!(args.flags, 0);
        assert_eq!(args.reserved0, 0);
        assert_eq!(args.reserved1, 0);
        assert_eq!(args.reserved2, 0);
        assert!(smp_qualification_bind_args(0, QUALIFICATION).is_err());
        assert!(smp_qualification_bind_args(-1, QUALIFICATION).is_err());
        assert!(smp_qualification_bind_args(
            41,
            KvmSmpQualificationContract {
                deadline_ms: 5_001,
                ..QUALIFICATION
            }
        )
        .is_err());

        let calls = RefCell::new(Vec::new());
        bind_then_activate_spawned_process(
            41,
            Some(QUALIFICATION),
            |pid, contract| {
                assert_eq!((pid, contract), (41, QUALIFICATION));
                calls.borrow_mut().push("bind");
                Ok(())
            },
            |pid| {
                assert_eq!(pid, 41);
                calls.borrow_mut().push("activate");
                Ok(())
            },
        )
        .expect("bind then activate");
        assert_eq!(&*calls.borrow(), &["bind", "activate"]);
    }

    #[test]
    fn qualification_bind_failure_never_activates_the_child() {
        let activation_count = RefCell::new(0_u32);
        let failure = bind_then_activate_spawned_process(
            42,
            Some(QUALIFICATION),
            |_, _| Err(libc::EPERM),
            |_| {
                *activation_count.borrow_mut() += 1;
                Ok(())
            },
        )
        .expect_err("bind failure must be terminal");
        assert_eq!(failure.1, libc::EPERM);
        assert_eq!(*activation_count.borrow(), 0);
    }

    #[test]
    fn ordinary_launch_skips_private_bind_and_activates_once() {
        let bind_count = RefCell::new(0_u32);
        let activation_count = RefCell::new(0_u32);
        bind_then_activate_spawned_process(
            43,
            None,
            |_, _| {
                *bind_count.borrow_mut() += 1;
                Ok(())
            },
            |pid| {
                assert_eq!(pid, 43);
                *activation_count.borrow_mut() += 1;
                Ok(())
            },
        )
        .expect("ordinary activation");
        assert_eq!(*bind_count.borrow(), 0);
        assert_eq!(*activation_count.borrow(), 1);
    }

    #[test]
    fn catalog_weight_cannot_promote_an_untrusted_program() {
        assert_eq!(
            admitted_task_weight_micros("apps/shell/shell.elf", u64::MAX),
            MAX_UNTRUSTED_TASK_WEIGHT_MICROS
        );
    }

    #[test]
    fn private_acceptance_contract_is_exact_and_boolean() {
        let contract = parse_kvm_acceptance_contract(
            "contract=rustos-kvm-acceptance-v1\nui_profile=1\nnetwork_exercise=0\n",
        )
        .expect("valid private acceptance contract");
        assert!(contract.ui_profile);
        assert!(!contract.network_exercise);
        assert!(parse_kvm_acceptance_contract("ui_profile=1\nnetwork_exercise=0\n").is_none());
        assert!(parse_kvm_acceptance_contract(
            "contract=rustos-kvm-acceptance-v1\nui_profile=yes\nnetwork_exercise=0\n"
        )
        .is_none());
    }

    #[test]
    fn acceptance_environment_replaces_manifest_default_once() {
        let mut env = vec![String::from("RUSTOS_UI_PROFILE=0"), String::from("A=1")];
        upsert_env(&mut env, "RUSTOS_UI_PROFILE=1");
        assert_eq!(
            env.iter()
                .filter(|value| value.starts_with("RUSTOS_UI_PROFILE="))
                .count(),
            1
        );
        assert!(env.iter().any(|value| value == "RUSTOS_UI_PROFILE=1"));
    }

    #[test]
    fn only_the_exact_ui_server_path_receives_system_weight() {
        assert_eq!(
            admitted_task_weight_micros(UI_SERVER_EXEC_PATH, 1),
            UI_SERVER_TASK_WEIGHT_MICROS
        );
        assert_eq!(
            admitted_task_weight_micros("services/uiserver/uiserver.elf.bak", 2_000),
            MAX_UNTRUSTED_TASK_WEIGHT_MICROS
        );
    }

    #[test]
    fn build_exec_argv_defaults_to_exec_path() {
        let argv = build_exec_argv("apps/demo/demo.elf", &[]).expect("valid default argv");
        assert_eq!(argv.len(), 1);
        assert_eq!(argv[0].to_str().unwrap(), "apps/demo/demo.elf");
    }

    #[test]
    fn build_exec_env_preserves_explicit_values_and_adds_defaults() {
        let defaults = [
            String::from("PATH=/bin:/usr/bin:/usr/local/bin"),
            String::from("HOME=/home/user"),
            String::from("XDG_RUNTIME_DIR=/run/user/1000"),
            String::from("WAYLAND_DISPLAY=wayland-0"),
            String::from("XDG_SESSION_TYPE=wayland"),
            String::from("XDG_CURRENT_DESKTOP=RustOS"),
        ];
        let env = build_exec_env_with_defaults(
            &[
                String::from("PATH=/custom/bin"),
                String::from("XDG_RUNTIME_DIR=/run/custom"),
            ],
            &defaults,
        )
        .expect("valid merged environment");
        let values = env
            .iter()
            .map(|item| item.to_str().unwrap().to_string())
            .collect::<Vec<_>>();
        assert!(values.iter().any(|item| item == "PATH=/custom/bin"));
        assert!(values
            .iter()
            .any(|item| item == "XDG_RUNTIME_DIR=/run/custom"));
        assert!(values.iter().any(|item| item == "HOME=/home/user"));
        assert!(values
            .iter()
            .any(|item| item == "WAYLAND_DISPLAY=wayland-0"));
        assert!(values.iter().any(|item| item == "XDG_SESSION_TYPE=wayland"));
        assert!(values
            .iter()
            .any(|item| item == "XDG_CURRENT_DESKTOP=RustOS"));
        assert!(!values
            .iter()
            .any(|item| item == "PATH=/bin:/usr/bin:/usr/local/bin"));
        assert!(!values
            .iter()
            .any(|item| item == "XDG_RUNTIME_DIR=/run/user/1000"));
    }

    #[test]
    fn build_exec_argv_rejects_invalid_input_instead_of_substituting_a_path() {
        assert_eq!(
            build_exec_argv("apps/demo\0demo.elf", &[]),
            Err(libc::EINVAL)
        );
    }

    #[test]
    fn failed_spawn_cleanup_accepts_only_exact_retirement_or_esrch() {
        let mut retired = 0;
        assert_eq!(
            cleanup_failed_spawn(77, |pid| {
                retired = pid;
                Ok(())
            }),
            Ok(())
        );
        assert_eq!(retired, 77);
        assert_eq!(cleanup_failed_spawn(78, |_| Err(libc::ESRCH)), Ok(()));
        assert_eq!(
            cleanup_failed_spawn(79, |_| Err(libc::EPERM)),
            Err(libc::EPERM)
        );
        assert_eq!(cleanup_failed_spawn(0, |_| Ok(())), Err(libc::EINVAL));
    }
}
