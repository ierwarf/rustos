use std::collections::{BTreeMap, BTreeSet};
use std::ffi::CString;
use std::io::{ErrorKind, Read, Write};
use std::mem::size_of;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver};
use std::thread;
use std::time::{Duration, Instant};

use runtime_control::{
    DEFAULT_APPLICATIONS_DIR, DEFAULT_RUNTIME_ENV_REGISTRY_PATH,
    DEFAULT_RUNTIME_LAUNCH_REGISTRY_PATH, DEFAULT_RUNTIME_SOCKET_PATH, DesktopProgramEntry,
    RuntimeEnvScope, RuntimeRunningProgram, StartupMode, load_desktop_program_entries,
    load_runtime_default_env, load_runtime_launch_program_entries,
};
use rustos_user_abi::console::{
    self as console_abi, ConsoleCloseSessionRequest, ConsoleCreateSessionRequest,
    ConsoleSessionInfo, ConsoleSetFocusRequest, ConsoleSetSessionStateRequest,
    ConsoleSnapshotSessionsRequest, ConsoleStateInfo,
};
use rustos_user_abi::syscall::{
    COMMERCIAL_MAX_PROTOCOL_ABI_VERSION, COMMERCIAL_MAX_PROTOCOL_SESSIOND,
    COMMERCIAL_MAX_SESSIOND_OP_CONSOLE_ROUTE, COMMERCIAL_MAX_SESSIOND_OP_FOREGROUND_FOCUS,
    COMMERCIAL_MAX_SESSIOND_OP_SESSION_GRAPH, COMMERCIAL_MAX_SESSIOND_OP_TTY_LINE_DISCIPLINE,
    COMMERCIAL_MAX_SESSIOND_OP_UI_BOOTSTRAP, CommercialMaxCapabilityLeaseWire,
    CommercialMaxProtocolDescriptorWire, CommercialMaxProtocolRequest,
    CommercialMaxProtocolResponse, IPC_SERVICE_DEVMGRD, IPC_SERVICE_LOADERD, IPC_SERVICE_SESSIOND,
    LOADER_OP_SPAWN_EXEC, LOADER_REQUEST_ABI_VERSION, LOADER_SPAWN_ARG_BYTES,
    LOADER_SPAWN_ENV_BYTES, LOADER_SPAWN_EXEC_PATH_CAPACITY, LoaderSpawnRequest,
    LoaderSpawnResponse, RustosDeviceIoctlBrokerArgs, SYS_RUSTOS_DEVICE_IOCTL_BROKER,
    SYS_RUSTOS_IPC_CALL, SYS_RUSTOS_IPC_ENDPOINT_CREATE, SYS_RUSTOS_IPC_LOOKUP_SERVICE_ENDPOINT,
    SYS_RUSTOS_IPC_REGISTER_SERVICE_ENDPOINT, SYS_RUSTOS_IPC_REPLY, SYS_RUSTOS_IPC_TRY_RECV,
};

const DEFAULT_USER_TASK_WEIGHT_MICROS: u64 = 100;
const MIN_EFFECTIVE_TASK_WEIGHT_MICROS: u64 = 1_000;
const UI_SERVER_TASK_WEIGHT_MICROS: u64 = 2_000;
const IDLE_POLL_INTERVAL: Duration = Duration::from_millis(2);
const RETRY_BACKOFF: Duration = Duration::from_millis(100);
const SERVICE_REQUEST_TIMEOUT: Duration = Duration::from_secs(5);
const MAX_RUNTIME_CLIENTS_PER_TICK: usize = 8;
const MAX_POLICY_LAUNCH_ATTEMPTS_PER_TICK: usize = 1;
const PROTOCOL_VERSION: u16 = 1;
const OP_SNAPSHOT_RUNNING_PROGRAMS: u16 = 1;
const OP_REQUEST_LAUNCH_PATH: u16 = 2;
const OP_REQUEST_TERMINATE: u16 = 3;
const OP_NOTIFY_READY: u16 = 4;
const LAUNCH_TARGET_NEW_SESSION: u16 = 2;
const TERMINATE_TARGET_SESSION: u16 = 1;
const TERMINATE_TARGET_PID: u16 = 2;
const READY_COMPONENT_UI_SERVER: u16 = 1;
const MAX_REQUEST_PATH_BYTES: usize = 128;
const MAX_RUNTIME_PROGRAMS: usize = 64;
const MAX_EXEC_ARG_COUNT: usize = 32;
const MAX_EXEC_ENV_COUNT: usize = 64;
const MAX_EXEC_TEXT_BYTES: usize = 256;
const SYS_IOCTL: usize = 16;
const SYS_OPENAT: usize = 257;
const AT_FDCWD: isize = -100;
const O_RDWR: usize = 2;
const DEVMGRD_BOOTSTRAP_WAIT_TIMEOUT: Duration = Duration::from_secs(5);
const SERVICE_ENDPOINT_POLL_INTERVAL: Duration = Duration::from_millis(10);
const CONSOLE_SESSION_STATE_LOADING_IMAGE: u16 = console_abi::CONSOLE_SESSION_STATE_LOADING_IMAGE;
const CONSOLE_SESSION_STATE_SPAWNING: u16 = console_abi::CONSOLE_SESSION_STATE_SPAWNING;
const CONSOLE_SESSION_STATE_RUNNING: u16 = console_abi::CONSOLE_SESSION_STATE_RUNNING;
const CONSOLE_PATH: &str = console_abi::CONSOLE_PATH;
const UI_SERVER_DESKTOP_FILE_ID: &str = "uiserver.desktop";
const UI_SERVER_DISPLAY_NAME: &str = "UI Server";
const UI_SERVER_EXEC_PATH: &str = "services/uiserver/uiserver.elf";
static LOADER_ENDPOINT_CACHE: AtomicU64 = AtomicU64::new(0);
static SESSION_GRAPH_GENERATION: AtomicU64 = AtomicU64::new(1);

#[repr(C)]
#[derive(Clone, Copy, Debug)]
struct RuntimeRequest {
    version: u16,
    op: u16,
    target_kind: u16,
    reserved0: u16,
    text_len: u32,
    target_value: u64,
    text: [u8; MAX_REQUEST_PATH_BYTES],
}

impl Default for RuntimeRequest {
    fn default() -> Self {
        Self {
            version: 0,
            op: 0,
            target_kind: 0,
            reserved0: 0,
            text_len: 0,
            target_value: 0,
            text: [0; MAX_REQUEST_PATH_BYTES],
        }
    }
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
struct RuntimeResponse {
    version: u16,
    op: u16,
    status: i32,
    count: u32,
}

#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd)]
struct LaunchEntry {
    package_id: String,
    desktop_file_id: String,
    display_name: String,
    exec: String,
    runtime_deps: Vec<String>,
    restart: bool,
    weight_micros: u64,
    logical_admin: bool,
    console_hosted: bool,
    args: Vec<String>,
    env: Vec<String>,
}

#[derive(Clone, Debug)]
struct RunningProcess {
    pid: i32,
    package_id: String,
    desktop_file_id: String,
    display_name: String,
    exec: String,
    session_handle: u64,
    restart: bool,
}

#[derive(Clone, Debug)]
struct ProgramMetadata {
    package_id: String,
    desktop_file_id: String,
    display_name: String,
    exec: String,
    runtime_deps: Vec<String>,
    startup: StartupMode,
    weight_micros: u64,
    logical_admin: bool,
    console_hosted: bool,
    args: Vec<String>,
    env: Vec<String>,
}

struct BrokerState {
    console_fd: Option<OwnedFd>,
    running: BTreeMap<i32, RunningProcess>,
    launched_once: BTreeSet<String>,
    retry_after: BTreeMap<String, Instant>,
    permanent_launch_failures: BTreeMap<String, i32>,
    launch_entries: Vec<LaunchEntry>,
    programs: BTreeMap<String, ProgramMetadata>,
    ui_ready: bool,
    launch_catalog_loaded: bool,
}

struct LaunchCatalog {
    programs: BTreeMap<String, ProgramMetadata>,
    launch_entries: Vec<LaunchEntry>,
    elapsed_ms: u128,
}

fn boot_line(message: &str) {
    if option_env!("RUSTOS_LOGGING_BOOT_TRACE_ENABLED") != Some("true") {
        return;
    }
    let _ = std::io::stderr().write_all(message.as_bytes());
    let _ = std::io::stderr().write_all(b"\n");
}

fn main() {
    stderr_line("runtimed: service start");
    boot_line("runtimed: service start");
    let listener = match bind_listener(DEFAULT_RUNTIME_SOCKET_PATH) {
        Ok(listener) => listener,
        Err(err) => {
            observability_client::error!(
                "runtimed",
                service,
                "bind {} failed: errno={err}",
                DEFAULT_RUNTIME_SOCKET_PATH
            );
            return;
        }
    };
    stderr_line("runtimed: runtime socket ready");
    boot_line("runtimed: runtime socket ready");
    let session_endpoint = create_session_endpoint();

    let mut state = BrokerState {
        console_fd: None,
        running: BTreeMap::new(),
        launched_once: BTreeSet::new(),
        retry_after: BTreeMap::new(),
        permanent_launch_failures: BTreeMap::new(),
        launch_entries: Vec::new(),
        programs: BTreeMap::new(),
        ui_ready: false,
        launch_catalog_loaded: false,
    };
    let mut launch_catalog = Some(start_launch_catalog_loader());
    stderr_line("runtimed: bootstrap ui begin");
    boot_line("runtimed: bootstrap ui begin");
    if let Err(err) = bootstrap_ui_server(&mut state) {
        observability_client::error!(
            "runtimed",
            service,
            "bootstrap {} failed: errno={err}",
            UI_SERVER_EXEC_PATH
        );
    } else {
        stderr_line("runtimed: bootstrap ui done");
        boot_line("runtimed: bootstrap ui done");
    }
    loop {
        let mut did_work = false;
        did_work |= service_session_endpoint(session_endpoint, &mut state);
        did_work |= reap_children(&mut state);
        did_work |= service_listener(&listener, &mut state);
        if state.ui_ready && !state.launch_catalog_loaded && launch_catalog.is_none() {
            launch_catalog = Some(start_launch_catalog_loader());
            did_work = true;
        }
        if let Some(receiver) = launch_catalog.as_ref() {
            did_work |= receive_launch_catalog(&mut state, receiver);
        }
        did_work |= ensure_policy_launches(&mut state);
        if did_work {
            continue;
        }
        thread::sleep(next_idle_delay(&state));
    }
}

fn start_launch_catalog_loader() -> Receiver<LaunchCatalog> {
    let (sender, receiver) = mpsc::sync_channel(1);
    thread::spawn(move || {
        boot_line("runtimed: launch catalog load begin");
        let started_at = Instant::now();
        let (programs, launch_entries) = load_launch_catalog();
        let elapsed_ms = started_at.elapsed().as_millis();
        let _ = sender.send(LaunchCatalog {
            programs,
            launch_entries,
            elapsed_ms,
        });
        boot_line("runtimed: launch catalog load done");
    });
    receiver
}

fn receive_launch_catalog(state: &mut BrokerState, receiver: &Receiver<LaunchCatalog>) -> bool {
    if state.launch_catalog_loaded {
        return false;
    }
    let Ok(catalog) = receiver.try_recv() else {
        return false;
    };
    observability_client::info!(
        "runtimed",
        service,
        "launch catalog summary programs={} policies={} elapsed_ms={}",
        catalog.programs.len(),
        catalog.launch_entries.len(),
        catalog.elapsed_ms
    );
    boot_line(
        format!(
            "runtimed: launch catalog summary programs={} policies={} elapsed_ms={}",
            catalog.programs.len(),
            catalog.launch_entries.len(),
            catalog.elapsed_ms
        )
        .as_str(),
    );
    state.programs = catalog.programs;
    state.launch_entries = catalog.launch_entries;
    state.launch_catalog_loaded = true;
    true
}

fn bootstrap_ui_server(state: &mut BrokerState) -> Result<(), i32> {
    boot_line("runtimed: waiting for devmgrd before ui bootstrap");
    wait_for_service_endpoint(IPC_SERVICE_DEVMGRD, DEVMGRD_BOOTSTRAP_WAIT_TIMEOUT)?;
    let (args, env) = ui_server_bootstrap_args_env();
    spawn_tracked_process(
        state,
        LaunchEntry {
            desktop_file_id: String::from(UI_SERVER_DESKTOP_FILE_ID),
            package_id: String::from("uiserver"),
            display_name: String::from(UI_SERVER_DISPLAY_NAME),
            exec: String::from(UI_SERVER_EXEC_PATH),
            runtime_deps: Vec::new(),
            restart: true,
            weight_micros: UI_SERVER_TASK_WEIGHT_MICROS,
            logical_admin: false,
            console_hosted: false,
            args,
            env,
        },
    )
}

fn wait_for_service_endpoint(service_id: u64, timeout: Duration) -> Result<(), i32> {
    let deadline = Instant::now() + timeout;
    loop {
        if lookup_service_endpoint(service_id) > 0 {
            return Ok(());
        }
        if Instant::now() >= deadline {
            return Err(libc::ETIMEDOUT);
        }
        thread::sleep(SERVICE_ENDPOINT_POLL_INTERVAL);
    }
}

fn create_session_endpoint() -> Option<u64> {
    let endpoint = unsafe { libc::syscall(SYS_RUSTOS_IPC_ENDPOINT_CREATE as libc::c_long) as i64 };
    if endpoint < 0 {
        stderr_line("runtimed: session endpoint create failed");
        return None;
    }
    let register = unsafe {
        libc::syscall(
            SYS_RUSTOS_IPC_REGISTER_SERVICE_ENDPOINT as libc::c_long,
            IPC_SERVICE_SESSIOND,
            endpoint as u64,
        ) as i64
    };
    if register < 0 {
        stderr_line("runtimed: session endpoint register failed");
        return None;
    }
    stderr_line("runtimed: session policy endpoint registered");
    Some(endpoint as u64)
}

fn service_session_endpoint(endpoint: Option<u64>, state: &mut BrokerState) -> bool {
    let Some(endpoint) = endpoint else {
        return false;
    };
    let mut request = CommercialMaxProtocolRequest::default();
    let mut reply_cap = 0_u64;
    let received = unsafe {
        libc::syscall(
            SYS_RUSTOS_IPC_TRY_RECV as libc::c_long,
            endpoint,
            (&mut request as *mut CommercialMaxProtocolRequest) as u64,
            size_of::<CommercialMaxProtocolRequest>() as u64,
            (&mut reply_cap as *mut u64) as u64,
        ) as i64
    };
    if received < 0 {
        return false;
    }
    let mut response = CommercialMaxProtocolResponse {
        header: request.header,
        ..CommercialMaxProtocolResponse::default()
    };
    response.header.version = COMMERCIAL_MAX_PROTOCOL_ABI_VERSION;
    response.status = if received as usize != size_of::<CommercialMaxProtocolRequest>() {
        libc::EINVAL
    } else {
        handle_session_request(&request, state, &mut response)
    };
    let reply = unsafe {
        libc::syscall(
            SYS_RUSTOS_IPC_REPLY as libc::c_long,
            reply_cap,
            (&response as *const CommercialMaxProtocolResponse) as u64,
            size_of::<CommercialMaxProtocolResponse>() as u64,
        ) as i64
    };
    if reply < 0 {
        stderr_line("runtimed: session reply failed");
    }
    true
}

fn handle_session_request(
    request: &CommercialMaxProtocolRequest,
    state: &BrokerState,
    response: &mut CommercialMaxProtocolResponse,
) -> i32 {
    if request.header.version != COMMERCIAL_MAX_PROTOCOL_ABI_VERSION
        || request.header.protocol != COMMERCIAL_MAX_PROTOCOL_SESSIOND
        || request.header.flags != 0
        || request.path_len as usize > request.path.len()
        || request.payload_len as usize > request.payload.len()
    {
        return libc::EINVAL;
    }
    if !session_op_accepts_ioctl(request.header.op, request.arg0) {
        return libc::ENOTTY;
    }
    match request.header.op {
        COMMERCIAL_MAX_SESSIOND_OP_SESSION_GRAPH => {
            handle_session_graph_request(request, state, response)
        }
        COMMERCIAL_MAX_SESSIOND_OP_TTY_LINE_DISCIPLINE => {
            response.descriptor_count = 1;
            response.value0 = u64::from(state.console_fd.is_some());
            response.descriptors[0] = session_descriptor(
                "tty-line",
                request.header.op,
                state
                    .console_fd
                    .as_ref()
                    .map_or(0, |fd| fd.as_raw_fd() as u64),
                0,
            );
            response.capability = session_capability("tty-line", request.header.op);
            0
        }
        COMMERCIAL_MAX_SESSIOND_OP_CONSOLE_ROUTE => {
            response.value0 = state
                .running
                .values()
                .filter(|program| program.session_handle != 0)
                .count() as u64;
            response.descriptor_count = 1;
            response.descriptors[0] =
                session_descriptor("console-route", request.header.op, response.value0, 0);
            response.capability = session_capability("console-route", request.header.op);
            0
        }
        COMMERCIAL_MAX_SESSIOND_OP_FOREGROUND_FOCUS => {
            let focused_session = state
                .running
                .values()
                .filter(|program| program.session_handle != 0)
                .map(|program| program.session_handle)
                .max()
                .unwrap_or(0);
            response.value0 = focused_session;
            response.descriptor_count = 1;
            response.descriptors[0] =
                session_descriptor("foreground-focus", request.header.op, focused_session, 0);
            response.capability = session_capability("foreground-focus", request.header.op);
            0
        }
        COMMERCIAL_MAX_SESSIOND_OP_UI_BOOTSTRAP => {
            response.value0 = u64::from(state.ui_ready);
            response.value1 = u64::from(state.launch_catalog_loaded);
            response.descriptor_count = 1;
            response.descriptors[0] = session_descriptor(
                "ui-bootstrap",
                request.header.op,
                response.value0,
                response.value1,
            );
            0
        }
        _ => libc::EINVAL,
    }
}

fn session_op_accepts_ioctl(op: u16, request_number: u64) -> bool {
    if request_number == 0 {
        return true;
    }
    match op {
        COMMERCIAL_MAX_SESSIOND_OP_SESSION_GRAPH => matches!(
            request_number,
            console_abi::CONSOLE_IOCTL_GET_STATE | console_abi::CONSOLE_IOCTL_SNAPSHOT_SESSIONS
        ),
        COMMERCIAL_MAX_SESSIOND_OP_CONSOLE_ROUTE => matches!(
            request_number,
            console_abi::CONSOLE_IOCTL_SNAPSHOT_SESSION_OUTPUT
                | console_abi::CONSOLE_IOCTL_SEND_INPUT_EVENT
                | console_abi::CONSOLE_IOCTL_CREATE_SESSION
                | console_abi::CONSOLE_IOCTL_CLOSE_SESSION
                | console_abi::CONSOLE_IOCTL_BIND_CURRENT_SESSION
                | console_abi::CONSOLE_IOCTL_SET_SESSION_STATE
        ),
        COMMERCIAL_MAX_SESSIOND_OP_FOREGROUND_FOCUS => {
            request_number == console_abi::CONSOLE_IOCTL_SET_FOCUS
        }
        COMMERCIAL_MAX_SESSIOND_OP_TTY_LINE_DISCIPLINE
        | COMMERCIAL_MAX_SESSIOND_OP_UI_BOOTSTRAP => false,
        _ => false,
    }
}

fn handle_session_graph_request(
    request: &CommercialMaxProtocolRequest,
    state: &BrokerState,
    response: &mut CommercialMaxProtocolResponse,
) -> i32 {
    let session_count = state
        .running
        .values()
        .filter(|program| program.session_handle != 0)
        .count();
    response.value0 = focused_session_handle(state);
    response.value1 = session_count as u64;
    fill_session_program_descriptors(state, response);

    match request.arg0 {
        0 => 0,
        console_abi::CONSOLE_IOCTL_GET_STATE => {
            let info = ConsoleStateInfo {
                focused_session_handle: response.value0,
                session_count: session_count as u32,
                reserved: 0,
            };
            copy_payload(response, as_bytes(&info))
        }
        console_abi::CONSOLE_IOCTL_SNAPSHOT_SESSIONS => {
            if request.payload_len as usize != size_of::<ConsoleSnapshotSessionsRequest>() {
                return libc::EINVAL;
            }
            let mut snapshot = read_unaligned::<ConsoleSnapshotSessionsRequest>(&request.payload);
            let capacity = snapshot
                .capacity
                .min(console_abi::MAX_CONSOLE_SESSIONS as u64) as usize;
            let mut payload_len = size_of::<ConsoleSnapshotSessionsRequest>();
            let max_payload_len = payload_len
                .saturating_add(capacity.saturating_mul(size_of::<ConsoleSessionInfo>()));
            if max_payload_len > response.payload.len() {
                return libc::EINVAL;
            }

            let focused = focused_session_handle(state);
            let generation = SESSION_GRAPH_GENERATION.fetch_add(1, Ordering::Relaxed);
            let mut written = 0usize;
            for program in state.running.values() {
                if program.session_handle == 0 || written >= capacity {
                    continue;
                }
                let mut info = ConsoleSessionInfo {
                    session_handle: program.session_handle,
                    state: CONSOLE_SESSION_STATE_RUNNING,
                    focused: u16::from(program.session_handle == focused),
                    reserved: 0,
                    output_generation: generation,
                    ..ConsoleSessionInfo::default()
                };
                copy_ascii_into(&mut info.title, &program.display_name);
                let bytes = as_bytes(&info);
                response.payload[payload_len..payload_len + bytes.len()].copy_from_slice(bytes);
                payload_len += bytes.len();
                written += 1;
            }
            snapshot.count = written as u64;
            response.payload[..size_of::<ConsoleSnapshotSessionsRequest>()]
                .copy_from_slice(as_bytes(&snapshot));
            response.payload_len = payload_len as u32;
            0
        }
        _ => 0,
    }
}

fn focused_session_handle(state: &BrokerState) -> u64 {
    state
        .running
        .values()
        .filter(|program| program.session_handle != 0)
        .map(|program| program.session_handle)
        .max()
        .unwrap_or(0)
}

fn copy_payload(response: &mut CommercialMaxProtocolResponse, bytes: &[u8]) -> i32 {
    if bytes.len() > response.payload.len() {
        return libc::EINVAL;
    }
    response.payload[..bytes.len()].copy_from_slice(bytes);
    response.payload_len = bytes.len() as u32;
    0
}

fn fill_session_program_descriptors(
    state: &BrokerState,
    response: &mut CommercialMaxProtocolResponse,
) {
    let mut count = 0usize;
    for program in state.running.values() {
        if count >= response.descriptors.len() {
            break;
        }
        response.descriptors[count] = session_descriptor(
            program.desktop_file_id.as_str(),
            COMMERCIAL_MAX_SESSIOND_OP_SESSION_GRAPH,
            program.pid as u64,
            program.session_handle,
        );
        count += 1;
    }
    response.descriptor_count = count as u16;
}

fn session_descriptor(
    label: &str,
    op: u16,
    value0: u64,
    value1: u64,
) -> CommercialMaxProtocolDescriptorWire {
    let mut descriptor = CommercialMaxProtocolDescriptorWire {
        protocol: COMMERCIAL_MAX_PROTOCOL_SESSIOND,
        op,
        flags: 0,
        service_id: IPC_SERVICE_SESSIOND,
        capability_mask: session_capability_mask(op),
        value0,
        value1,
        ..CommercialMaxProtocolDescriptorWire::default()
    };
    copy_label(label, &mut descriptor.name, &mut descriptor.name_len);
    descriptor
}

fn session_capability(label: &str, op: u16) -> CommercialMaxCapabilityLeaseWire {
    let mut capability = CommercialMaxCapabilityLeaseWire {
        lease_id: ((COMMERCIAL_MAX_PROTOCOL_SESSIOND as u64) << 32) | u64::from(op),
        service_id: IPC_SERVICE_SESSIOND,
        capability_mask: session_capability_mask(op),
        rights_mask: session_capability_mask(op),
        ..CommercialMaxCapabilityLeaseWire::default()
    };
    copy_label(label, &mut capability.label, &mut capability.label_len);
    capability
}

fn session_capability_mask(op: u16) -> u64 {
    match op {
        COMMERCIAL_MAX_SESSIOND_OP_SESSION_GRAPH => 1 << 0,
        COMMERCIAL_MAX_SESSIOND_OP_TTY_LINE_DISCIPLINE => 1 << 1,
        COMMERCIAL_MAX_SESSIOND_OP_CONSOLE_ROUTE => 1 << 2,
        COMMERCIAL_MAX_SESSIOND_OP_FOREGROUND_FOCUS => 1 << 3,
        COMMERCIAL_MAX_SESSIOND_OP_UI_BOOTSTRAP => 1 << 4,
        _ => 0,
    }
}

fn ui_server_bootstrap_args_env() -> (Vec<String>, Vec<String>) {
    // Bootstrap happens before the launch catalog loader has finished, so we
    // pull the uiserver desktop entry (and the Init-scope env defaults) up
    // front. Reading from the registry warms the OnceLock cache; the catalog
    // loader thread reuses it without a second disk read.
    let mut env = Vec::new();
    if let Ok(values) =
        load_runtime_default_env(DEFAULT_RUNTIME_ENV_REGISTRY_PATH, RuntimeEnvScope::Init)
    {
        env.extend(values);
    }

    let mut args = Vec::new();
    if let Ok(entries) = load_desktop_program_entries(DEFAULT_APPLICATIONS_DIR) {
        if let Some(entry) = entries
            .into_iter()
            .find(|entry| entry.desktop_file_id == UI_SERVER_DESKTOP_FILE_ID)
        {
            args = entry.args;
            merge_manifest_env_into(&mut env, &entry.env);
        }
    }
    (args, env)
}

fn merge_manifest_env_into(env: &mut Vec<String>, manifest_env: &[String]) {
    for value in manifest_env {
        let Some(eq) = value.find('=') else {
            continue;
        };
        let key_prefix = &value[..=eq];
        env.retain(|existing| !existing.starts_with(key_prefix));
        env.push(value.clone());
    }
}

fn bind_listener(path: &str) -> Result<UnixListener, i32> {
    let started_at = Instant::now();
    boot_line("runtimed: bind listener begin");

    let socket_fd = unsafe {
        libc::socket(
            libc::AF_UNIX,
            libc::SOCK_STREAM | libc::SOCK_NONBLOCK | libc::SOCK_CLOEXEC,
            0,
        )
    };
    if socket_fd < 0 {
        return Err(last_errno());
    }
    boot_line(
        format!(
            "runtimed: bind listener socket elapsed_ms={}",
            started_at.elapsed().as_millis()
        )
        .as_str(),
    );

    let path = CString::new(path).map_err(|_| libc::EINVAL)?;
    let unlink_rc = unsafe { libc::unlink(path.as_ptr()) };
    if unlink_rc < 0 {
        let err = last_errno();
        if err != libc::ENOENT {
            let _ = unsafe { libc::close(socket_fd) };
            return Err(err);
        }
    }
    boot_line(
        format!(
            "runtimed: bind listener unlink elapsed_ms={}",
            started_at.elapsed().as_millis()
        )
        .as_str(),
    );

    let path_bytes = path.as_bytes_with_nul();
    let mut addr = unsafe { std::mem::zeroed::<libc::sockaddr_un>() };
    addr.sun_family = libc::AF_UNIX as libc::sa_family_t;
    if path_bytes.len() > addr.sun_path.len() {
        let _ = unsafe { libc::close(socket_fd) };
        return Err(libc::ENAMETOOLONG);
    }
    for (index, byte) in path_bytes.iter().enumerate() {
        addr.sun_path[index] = *byte as libc::c_char;
    }

    let bind_rc = unsafe {
        libc::bind(
            socket_fd,
            (&addr as *const libc::sockaddr_un).cast::<libc::sockaddr>(),
            size_of::<libc::sockaddr_un>() as libc::socklen_t,
        )
    };
    if bind_rc < 0 {
        let err = last_errno();
        let _ = unsafe { libc::close(socket_fd) };
        return Err(err);
    }
    boot_line(
        format!(
            "runtimed: bind listener bind elapsed_ms={}",
            started_at.elapsed().as_millis()
        )
        .as_str(),
    );

    if unsafe { libc::listen(socket_fd, 16) } < 0 {
        let err = last_errno();
        let _ = unsafe { libc::close(socket_fd) };
        return Err(err);
    }
    boot_line(
        format!(
            "runtimed: bind listener listen elapsed_ms={}",
            started_at.elapsed().as_millis()
        )
        .as_str(),
    );

    Ok(unsafe { UnixListener::from_raw_fd(socket_fd) })
}

fn service_listener(listener: &UnixListener, state: &mut BrokerState) -> bool {
    let mut did_work = false;
    for _ in 0..MAX_RUNTIME_CLIENTS_PER_TICK {
        match accept_runtime_client(listener) {
            Ok((mut stream, _)) => {
                did_work = true;
                if let Err(err) = service_stream(&mut stream, state) {
                    let _ = write_response(
                        &mut stream,
                        RuntimeResponse {
                            version: PROTOCOL_VERSION,
                            op: 0,
                            status: -err,
                            count: 0,
                        },
                    );
                }
            }
            Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => break,
            Err(err) => {
                observability_client::error!(
                    "runtimed",
                    service,
                    "accept failed: errno={}",
                    io_errno(err)
                );
                break;
            }
        }
    }
    did_work
}

fn accept_runtime_client(listener: &UnixListener) -> std::io::Result<(UnixStream, ())> {
    let fd = unsafe {
        libc::accept4(
            listener.as_raw_fd(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            libc::SOCK_NONBLOCK | libc::SOCK_CLOEXEC,
        )
    };
    if fd < 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok((unsafe { UnixStream::from_raw_fd(fd) }, ()))
}

fn service_stream(stream: &mut UnixStream, state: &mut BrokerState) -> Result<(), i32> {
    let mut request = RuntimeRequest::default();
    read_exact_retry(stream, as_bytes_mut(&mut request))?;
    if request.version != PROTOCOL_VERSION {
        return Err(libc::EPROTO);
    }
    validate_runtime_request(&request)?;

    match request.op {
        OP_SNAPSHOT_RUNNING_PROGRAMS => handle_snapshot(stream, state),
        OP_REQUEST_LAUNCH_PATH => handle_launch(stream, state, request),
        OP_REQUEST_TERMINATE => handle_terminate(stream, state, request),
        OP_NOTIFY_READY => handle_ready(stream, state, request),
        _ => Err(libc::EINVAL),
    }
}

fn read_exact_retry(stream: &mut UnixStream, mut bytes: &mut [u8]) -> Result<(), i32> {
    let deadline = Instant::now() + SERVICE_REQUEST_TIMEOUT;
    while !bytes.is_empty() {
        match stream.read(bytes) {
            Ok(0) => return Err(libc::EPIPE),
            Ok(read) => {
                let remaining = bytes;
                bytes = &mut remaining[read..];
            }
            Err(err) if err.kind() == ErrorKind::WouldBlock => {
                if Instant::now() >= deadline {
                    return Err(libc::ETIMEDOUT);
                }
                thread::yield_now();
            }
            Err(err) => return Err(io_errno(err)),
        }
    }
    Ok(())
}

fn handle_snapshot(stream: &mut UnixStream, state: &BrokerState) -> Result<(), i32> {
    let mut programs = state
        .running
        .values()
        .take(MAX_RUNTIME_PROGRAMS)
        .map(|program| {
            let mut snapshot = RuntimeRunningProgram::default();
            snapshot.pid = program.pid as u64;
            snapshot.program_id = 0;
            snapshot.session_handle = program.session_handle;
            copy_ascii_into(&mut snapshot.desktop_file_id, &program.desktop_file_id);
            copy_ascii_into(&mut snapshot.display_name, &program.display_name);
            copy_ascii_into(&mut snapshot.exec_path, &program.exec);
            snapshot
        })
        .collect::<Vec<_>>();
    programs.sort_by_key(|program| program.pid);

    write_response(
        stream,
        RuntimeResponse {
            version: PROTOCOL_VERSION,
            op: OP_SNAPSHOT_RUNNING_PROGRAMS,
            status: 0,
            count: u32::try_from(programs.len()).unwrap_or(u32::MAX),
        },
    )?;
    if !programs.is_empty() {
        stream
            .write_all(unsafe {
                std::slice::from_raw_parts(
                    programs.as_ptr().cast::<u8>(),
                    std::mem::size_of_val(programs.as_slice()),
                )
            })
            .map_err(io_errno)?;
    }
    Ok(())
}

fn handle_launch(
    stream: &mut UnixStream,
    state: &mut BrokerState,
    request: RuntimeRequest,
) -> Result<(), i32> {
    if request.target_kind != LAUNCH_TARGET_NEW_SESSION {
        return Err(libc::EOPNOTSUPP);
    }
    let target = request_path(&request)?;
    let metadata = resolve_program_request(state, target.as_str());
    if !runtime_deps_satisfied(
        &metadata.runtime_deps,
        &running_packages(state),
        &state.launched_once,
    ) {
        return Err(libc::EAGAIN);
    }
    spawn_tracked_process(
        state,
        LaunchEntry {
            package_id: metadata.package_id,
            desktop_file_id: metadata.desktop_file_id,
            display_name: metadata.display_name,
            exec: metadata.exec,
            runtime_deps: metadata.runtime_deps,
            restart: false,
            weight_micros: metadata.weight_micros,
            logical_admin: metadata.logical_admin,
            console_hosted: metadata.console_hosted,
            args: metadata.args,
            env: metadata.env,
        },
    )?;
    write_response(
        stream,
        RuntimeResponse {
            version: PROTOCOL_VERSION,
            op: OP_REQUEST_LAUNCH_PATH,
            status: 0,
            count: 0,
        },
    )
}

fn handle_terminate(
    stream: &mut UnixStream,
    state: &mut BrokerState,
    request: RuntimeRequest,
) -> Result<(), i32> {
    let mut terminated = false;
    match request.target_kind {
        TERMINATE_TARGET_PID => {
            let pid = i32::try_from(request.target_value).map_err(|_| libc::EINVAL)?;
            terminate_pid(pid)?;
            terminated = true;
        }
        TERMINATE_TARGET_SESSION => {
            if request.target_value == 0 {
                return Err(libc::EINVAL);
            }
            let pids = state
                .running
                .values()
                .filter(|program| program.session_handle == request.target_value)
                .map(|program| program.pid)
                .collect::<Vec<_>>();
            for pid in pids {
                match terminate_pid(pid) {
                    Ok(()) => terminated = true,
                    Err(err) if err == libc::ESRCH => {
                        state.running.remove(&pid);
                    }
                    Err(err) => return Err(err),
                }
            }
            if close_console_session(ensure_console_fd(state)?, request.target_value)? {
                terminated = true;
            }
        }
        _ => return Err(libc::EOPNOTSUPP),
    }

    if !terminated {
        return Err(libc::ESRCH);
    }

    write_response(
        stream,
        RuntimeResponse {
            version: PROTOCOL_VERSION,
            op: OP_REQUEST_TERMINATE,
            status: 0,
            count: 0,
        },
    )
}

fn handle_ready(
    stream: &mut UnixStream,
    state: &mut BrokerState,
    request: RuntimeRequest,
) -> Result<(), i32> {
    if request.target_kind != READY_COMPONENT_UI_SERVER {
        return Err(libc::EOPNOTSUPP);
    }
    state.ui_ready = true;
    observability_client::info!("runtimed", service, "ui ready received");
    boot_line("runtimed: ui ready received");
    write_response(
        stream,
        RuntimeResponse {
            version: PROTOCOL_VERSION,
            op: OP_NOTIFY_READY,
            status: 0,
            count: 0,
        },
    )
}

fn ensure_policy_launches(state: &mut BrokerState) -> bool {
    let now = Instant::now();
    let running_programs = state
        .running
        .values()
        .map(|program| program.desktop_file_id.clone())
        .collect::<BTreeSet<_>>();
    let running_packages = running_packages(state);
    let mut pending_service_launch = false;
    let mut pending_desktop_launch = false;
    let mut launched_any = false;
    for entry in &state.launch_entries {
        if state
            .permanent_launch_failures
            .contains_key(entry.desktop_file_id.as_str())
        {
            continue;
        }
        if state
            .retry_after
            .get(entry.desktop_file_id.as_str())
            .is_some_and(|deadline| now < *deadline)
        {
            continue;
        }
        let already_satisfied = if entry.restart {
            running_packages.contains(&entry.package_id)
        } else {
            running_programs.contains(&entry.desktop_file_id)
                || state.launched_once.contains(entry.package_id.as_str())
        };
        if already_satisfied {
            continue;
        }
        if !runtime_deps_satisfied(&entry.runtime_deps, &running_packages, &state.launched_once) {
            continue;
        }
        if entry.exec.starts_with("services/") {
            pending_service_launch = true;
        } else {
            pending_desktop_launch = true;
        }
    }

    let mut attempts = 0usize;
    for entry in state.launch_entries.clone() {
        if attempts >= MAX_POLICY_LAUNCH_ATTEMPTS_PER_TICK {
            break;
        }
        if state
            .permanent_launch_failures
            .contains_key(entry.desktop_file_id.as_str())
        {
            continue;
        }
        if state
            .retry_after
            .get(entry.desktop_file_id.as_str())
            .is_some_and(|deadline| now < *deadline)
        {
            continue;
        }
        if pending_service_launch
            && !entry.exec.starts_with("services/")
            && (!state.ui_ready || !pending_desktop_launch)
        {
            continue;
        }
        if !state.ui_ready && entry.exec != UI_SERVER_EXEC_PATH {
            continue;
        }
        if state.ui_ready
            && pending_desktop_launch
            && entry.exec.starts_with("services/")
            && entry.exec != UI_SERVER_EXEC_PATH
        {
            continue;
        }
        if !entry.exec.starts_with("services/") && !loader_endpoint_ready() {
            state.retry_after.insert(
                entry.desktop_file_id.clone(),
                Instant::now() + RETRY_BACKOFF,
            );
            continue;
        }

        if entry.restart {
            if running_packages.contains(&entry.package_id) {
                continue;
            }
        } else if running_programs.contains(&entry.desktop_file_id)
            || state.launched_once.contains(entry.package_id.as_str())
        {
            continue;
        }
        if !runtime_deps_satisfied(&entry.runtime_deps, &running_packages, &state.launched_once) {
            continue;
        }

        attempts += 1;
        match spawn_tracked_process(state, entry.clone()) {
            Ok(()) => {
                observability_client::info!(
                    "runtimed",
                    service,
                    "launched {} ({})",
                    entry.desktop_file_id,
                    entry.exec
                );
                launched_any = true;
                if !entry.restart {
                    state.launched_once.insert(entry.package_id);
                }
            }
            Err(err) => {
                if is_permanent_launch_failure(err) {
                    observability_client::warn!(
                        "runtimed",
                        service,
                        "launch {} ({}) disabled after permanent failure: errno={err}",
                        entry.desktop_file_id,
                        entry.exec
                    );
                    state
                        .permanent_launch_failures
                        .insert(entry.desktop_file_id, err);
                } else {
                    observability_client::error!(
                        "runtimed",
                        service,
                        "launch {} ({}) failed: errno={err}",
                        entry.desktop_file_id,
                        entry.exec
                    );
                    state
                        .retry_after
                        .insert(entry.desktop_file_id, Instant::now() + RETRY_BACKOFF);
                }
            }
        }
    }
    launched_any
}

fn is_permanent_launch_failure(errno: i32) -> bool {
    matches!(
        errno,
        libc::EOPNOTSUPP | libc::ENOEXEC | libc::EINVAL | libc::ENOENT | libc::EACCES
    )
}

fn spawn_tracked_process(state: &mut BrokerState, entry: LaunchEntry) -> Result<(), i32> {
    boot_line(
        format!(
            "runtimed: spawn begin desktop_id={} exec={} console_hosted={} logical_admin={}",
            entry.desktop_file_id, entry.exec, entry.console_hosted, entry.logical_admin
        )
        .as_str(),
    );
    let session_handle = if entry.console_hosted {
        let console_fd = ensure_console_fd(state)?;
        let session = create_console_session(
            console_fd,
            0,
            entry.display_name.as_str(),
            entry.exec.as_str(),
        )?;
        set_console_session_state(console_fd, session, CONSOLE_SESSION_STATE_LOADING_IMAGE)?;
        Some(session)
    } else {
        None
    };
    let pid = match spawn_exec(
        entry.exec.as_str(),
        entry.args.as_slice(),
        entry.env.as_slice(),
        entry.logical_admin,
        entry.weight_micros,
        session_handle.unwrap_or(0),
    ) {
        Ok(pid) => pid,
        Err(err) => {
            if let Some(session) = session_handle {
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
    state.retry_after.remove(entry.desktop_file_id.as_str());
    if let Some(session) = session_handle {
        let console_fd = ensure_console_fd(state)?;
        set_console_session_state(console_fd, session, CONSOLE_SESSION_STATE_SPAWNING)?;
        set_console_session_state(console_fd, session, CONSOLE_SESSION_STATE_RUNNING)?;
        let _ = console_set_focus(console_fd, session);
    }
    state.running.insert(
        pid,
        RunningProcess {
            pid,
            package_id: entry.package_id,
            desktop_file_id: entry.desktop_file_id,
            display_name: entry.display_name,
            exec: entry.exec,
            session_handle: session_handle.unwrap_or(0),
            restart: entry.restart,
        },
    );
    Ok(())
}

fn reap_children(state: &mut BrokerState) -> bool {
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
                    if let Ok(console_fd) = ensure_console_fd(state) {
                        let _ = close_console_session(console_fd, process.session_handle);
                    }
                }
                if process.restart {
                    state
                        .retry_after
                        .insert(process.desktop_file_id, Instant::now() + RETRY_BACKOFF);
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

fn next_idle_delay(state: &BrokerState) -> Duration {
    let now = Instant::now();
    let retry_delay = state
        .retry_after
        .values()
        .map(|deadline| deadline.saturating_duration_since(now))
        .min()
        .unwrap_or(IDLE_POLL_INTERVAL);
    retry_delay.min(IDLE_POLL_INTERVAL)
}

fn spawn_exec(
    exec_path: &str,
    argv: &[String],
    env: &[String],
    logical_admin: bool,
    weight_micros: u64,
    session_handle: u64,
) -> Result<i32, i32> {
    boot_line(format!("runtimed: loader request begin exec={}", exec_path).as_str());
    let argv_storage = build_exec_argv(exec_path, argv);
    let env_storage = build_exec_env(env);
    let request = build_loader_spawn_request(
        exec_path,
        &argv_storage,
        &env_storage,
        logical_admin,
        weight_micros,
        session_handle,
    )?;
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
        flags: u32::from(logical_admin),
        console_session: session_handle,
        weight_micros: effective_task_weight_micros(weight_micros),
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

fn effective_task_weight_micros(weight_micros: u64) -> u64 {
    let requested = if weight_micros == 0 {
        DEFAULT_USER_TASK_WEIGHT_MICROS
    } else {
        weight_micros
    };
    requested.max(MIN_EFFECTIVE_TASK_WEIGHT_MICROS)
}

fn lookup_service_endpoint(service_id: u64) -> i64 {
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

fn loader_endpoint_ready() -> bool {
    lookup_loader_endpoint().is_ok()
}

fn stderr_line(message: &str) {
    let mut line = message.as_bytes().to_vec();
    line.push(b'\n');
    unsafe {
        libc::write(
            libc::STDERR_FILENO,
            line.as_ptr().cast::<libc::c_void>(),
            line.len(),
        );
    }
}

fn terminate_pid(pid: i32) -> Result<(), i32> {
    let rc =
        unsafe { libc::syscall(libc::SYS_tgkill as libc::c_long, pid, pid, libc::SIGKILL) as i32 };
    if rc < 0 {
        return Err(last_errno());
    }
    Ok(())
}

fn load_launch_catalog() -> (BTreeMap<String, ProgramMetadata>, Vec<LaunchEntry>) {
    let load_started = Instant::now();
    let registry_entries =
        load_runtime_launch_program_entries(DEFAULT_RUNTIME_LAUNCH_REGISTRY_PATH)
            .unwrap_or_default();
    let registry_elapsed = load_started.elapsed().as_millis();
    observability_client::info!(
        "runtimed",
        service,
        "launch registry entries={} elapsed_ms={}",
        registry_entries.len(),
        registry_elapsed
    );
    boot_line(
        format!(
            "runtimed: launch registry entries={} elapsed_ms={}",
            registry_entries.len(),
            registry_elapsed
        )
        .as_str(),
    );

    let mut programs = BTreeMap::new();
    for entry in registry_entries.iter().cloned() {
        insert_program_metadata(&mut programs, entry);
    }
    let autostart_entries = registry_entries
        .iter()
        .filter(|entry| entry.autostart_enabled && !entry.hidden && !entry.no_display)
        .cloned()
        .collect::<Vec<_>>();

    let launch_started = Instant::now();
    let launch_entries = load_launch_entries(&programs, autostart_entries);
    let launch_elapsed = launch_started.elapsed().as_millis();
    observability_client::info!(
        "runtimed",
        service,
        "launch policies={} elapsed_ms={}",
        launch_entries.len(),
        launch_elapsed
    );
    boot_line(
        format!(
            "runtimed: launch policies={} elapsed_ms={}",
            launch_entries.len(),
            launch_elapsed
        )
        .as_str(),
    );

    (programs, launch_entries)
}

fn load_launch_entries(
    programs: &BTreeMap<String, ProgramMetadata>,
    autostart_entries: Vec<DesktopProgramEntry>,
) -> Vec<LaunchEntry> {
    let mut seen = BTreeSet::<String>::new();
    let mut entries = Vec::<LaunchEntry>::new();

    for metadata in programs.values() {
        if !matches!(
            metadata.startup,
            StartupMode::Session | StartupMode::Desktop
        ) {
            continue;
        }
        if !seen.insert(metadata.desktop_file_id.clone()) {
            continue;
        }
        entries.push(LaunchEntry {
            package_id: metadata.package_id.clone(),
            desktop_file_id: metadata.desktop_file_id.clone(),
            display_name: metadata.display_name.clone(),
            exec: metadata.exec.clone(),
            runtime_deps: metadata.runtime_deps.clone(),
            restart: metadata.exec.starts_with("services/"),
            weight_micros: metadata.weight_micros,
            logical_admin: metadata.logical_admin,
            console_hosted: metadata.console_hosted,
            args: metadata.args.clone(),
            env: metadata.env.clone(),
        });
    }

    for entry in autostart_entries {
        if !seen.insert(entry.desktop_file_id.clone()) {
            continue;
        }
        let metadata = programs
            .get(entry.desktop_file_id.as_str())
            .cloned()
            .unwrap_or_else(|| program_metadata_from_desktop_entry(entry.clone()));
        entries.push(LaunchEntry {
            package_id: metadata.package_id,
            desktop_file_id: metadata.desktop_file_id,
            display_name: metadata.display_name,
            exec: metadata.exec.clone(),
            runtime_deps: metadata.runtime_deps,
            restart: metadata.exec.starts_with("services/"),
            weight_micros: metadata.weight_micros,
            logical_admin: metadata.logical_admin,
            console_hosted: metadata.console_hosted,
            args: metadata.args,
            env: metadata.env,
        });
    }

    entries.sort_by(|lhs, rhs| {
        launch_entry_priority(lhs)
            .cmp(&launch_entry_priority(rhs))
            .then_with(|| lhs.desktop_file_id.cmp(&rhs.desktop_file_id))
            .then_with(|| lhs.display_name.cmp(&rhs.display_name))
            .then_with(|| lhs.exec.cmp(&rhs.exec))
    });

    entries
}

fn launch_entry_priority(entry: &LaunchEntry) -> (u8, u8, &str) {
    let service_rank = if entry.exec == UI_SERVER_EXEC_PATH {
        0
    } else if entry.exec.starts_with("services/") {
        3
    } else if entry.console_hosted {
        2
    } else {
        1
    };
    let restart_rank = u8::from(entry.restart);
    (service_rank, restart_rank, entry.desktop_file_id.as_str())
}

fn load_program_metadata() -> BTreeMap<String, ProgramMetadata> {
    let mut map = BTreeMap::new();
    if let Ok(entries) = load_desktop_program_entries(DEFAULT_APPLICATIONS_DIR) {
        for entry in entries {
            insert_program_metadata(&mut map, entry);
        }
    }
    map
}

fn load_program_metadata_for_target(target: &str) -> Option<ProgramMetadata> {
    let mut programs = load_program_metadata();
    programs.remove(target).or_else(|| {
        programs
            .into_values()
            .find(|program| program.exec == target)
    })
}

fn insert_program_metadata(
    map: &mut BTreeMap<String, ProgramMetadata>,
    entry: DesktopProgramEntry,
) {
    let key = entry.desktop_file_id.clone();
    map.entry(key)
        .or_insert_with(|| program_metadata_from_desktop_entry(entry));
}

fn program_metadata_from_desktop_entry(entry: DesktopProgramEntry) -> ProgramMetadata {
    ProgramMetadata {
        package_id: entry.package_id,
        desktop_file_id: entry.desktop_file_id,
        display_name: if entry.display_name.is_empty() {
            fallback_display_name(entry.exec.as_str())
        } else {
            entry.display_name
        },
        exec: entry.exec,
        runtime_deps: entry.runtime_deps,
        startup: entry.startup,
        weight_micros: entry.weight_micros,
        logical_admin: entry.logical_admin,
        console_hosted: entry.console_hosted,
        args: entry.args,
        env: entry.env,
    }
}

fn resolve_program_request(state: &BrokerState, target: &str) -> ProgramMetadata {
    state
        .programs
        .get(target)
        .cloned()
        .or_else(|| {
            state
                .programs
                .values()
                .find(|program| program.exec == target)
                .cloned()
        })
        .or_else(|| load_program_metadata_for_target(target))
        .unwrap_or_else(|| ProgramMetadata {
            package_id: package_id_from_target(target),
            desktop_file_id: target.to_string(),
            display_name: fallback_display_name(target),
            exec: target.to_string(),
            runtime_deps: Vec::new(),
            startup: StartupMode::None,
            weight_micros: DEFAULT_USER_TASK_WEIGHT_MICROS,
            logical_admin: false,
            console_hosted: false,
            args: Vec::new(),
            env: Vec::new(),
        })
}

fn fallback_display_name(exec: &str) -> String {
    Path::new(exec)
        .file_stem()
        .and_then(|name| name.to_str())
        .unwrap_or(exec)
        .to_string()
}

fn package_id_from_target(target: &str) -> String {
    Path::new(target)
        .file_stem()
        .and_then(|name| name.to_str())
        .unwrap_or(target)
        .strip_suffix(".desktop")
        .unwrap_or_else(|| {
            Path::new(target)
                .file_stem()
                .and_then(|name| name.to_str())
                .unwrap_or(target)
        })
        .to_string()
}

fn running_packages(state: &BrokerState) -> BTreeSet<String> {
    state
        .running
        .values()
        .map(|program| program.package_id.clone())
        .collect()
}

fn runtime_deps_satisfied(
    deps: &[String],
    running_packages: &BTreeSet<String>,
    launched_once_packages: &BTreeSet<String>,
) -> bool {
    deps.iter()
        .all(|dep| running_packages.contains(dep) || launched_once_packages.contains(dep))
}

fn request_path(request: &RuntimeRequest) -> Result<String, i32> {
    let len = usize::try_from(request.text_len).map_err(|_| libc::EINVAL)?;
    if len > request.text.len() {
        return Err(libc::EINVAL);
    }
    let path = String::from_utf8(request.text[..len].to_vec()).map_err(|_| libc::EINVAL)?;
    if !valid_request_text(path.as_str()) {
        return Err(libc::EINVAL);
    }
    Ok(path)
}

fn validate_runtime_request(request: &RuntimeRequest) -> Result<(), i32> {
    if request.reserved0 != 0 {
        return Err(libc::EINVAL);
    }
    let text_len = usize::try_from(request.text_len).map_err(|_| libc::EINVAL)?;
    if text_len > request.text.len() {
        return Err(libc::EINVAL);
    }
    match request.op {
        OP_SNAPSHOT_RUNNING_PROGRAMS => {
            if request.target_kind != 0 || request.target_value != 0 || text_len != 0 {
                return Err(libc::EINVAL);
            }
        }
        OP_NOTIFY_READY => {
            if request.target_kind != READY_COMPONENT_UI_SERVER
                || request.target_value != 0
                || text_len != 0
            {
                return Err(libc::EINVAL);
            }
        }
        OP_REQUEST_TERMINATE => {
            if !matches!(
                request.target_kind,
                TERMINATE_TARGET_SESSION | TERMINATE_TARGET_PID
            ) || request.target_value == 0
                || text_len != 0
            {
                return Err(libc::EINVAL);
            }
        }
        OP_REQUEST_LAUNCH_PATH => {
            if request.target_kind != LAUNCH_TARGET_NEW_SESSION
                || request.target_value != 0
                || text_len == 0
            {
                return Err(libc::EINVAL);
            }
        }
        _ => return Err(libc::EINVAL),
    }
    Ok(())
}

fn valid_request_text(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= MAX_REQUEST_PATH_BYTES
        && value
            .bytes()
            .all(|byte| matches!(byte, b' '..=b'~') && byte != b'\\')
}

fn write_response(stream: &mut UnixStream, response: RuntimeResponse) -> Result<(), i32> {
    stream.write_all(as_bytes(&response)).map_err(io_errno)
}

fn copy_ascii_into(dest: &mut [u8], value: &str) {
    dest.fill(0);
    for (index, byte) in value.bytes().enumerate() {
        if index >= dest.len() {
            break;
        }
        dest[index] = match byte {
            b' '..=b'~' => byte,
            _ => b'?',
        };
    }
}

fn copy_label(label: &str, target: &mut [u8], len: &mut u16) {
    let bytes = label.as_bytes();
    let count = bytes.len().min(target.len());
    target[..count].copy_from_slice(&bytes[..count]);
    *len = count as u16;
}

fn io_errno(err: std::io::Error) -> i32 {
    err.raw_os_error().unwrap_or(libc::EIO)
}

fn last_errno() -> i32 {
    std::io::Error::last_os_error()
        .raw_os_error()
        .unwrap_or(libc::EIO)
}

fn build_exec_argv(exec_path: &str, argv: &[String]) -> Vec<CString> {
    if argv.is_empty() {
        return vec![c_string_or_fallback(exec_path, "/")];
    }

    let mut storage = argv
        .iter()
        .take(MAX_EXEC_ARG_COUNT)
        .filter(|arg| valid_exec_text(arg.as_str(), false))
        .filter_map(|arg| CString::new(arg.as_str()).ok())
        .collect::<Vec<_>>();
    if storage.is_empty() {
        storage.push(c_string_or_fallback(exec_path, "/"));
    }
    storage
}

fn build_exec_env(extra_env: &[String]) -> Vec<CString> {
    let default_env =
        load_runtime_default_env(DEFAULT_RUNTIME_ENV_REGISTRY_PATH, RuntimeEnvScope::Runtime)
            .unwrap_or_default();
    build_exec_env_with_defaults(extra_env, &default_env)
}

fn build_exec_env_with_defaults(extra_env: &[String], default_env: &[String]) -> Vec<CString> {
    let mut env = extra_env
        .iter()
        .filter(|item| valid_exec_text(item.as_str(), true))
        .take(MAX_EXEC_ENV_COUNT)
        .cloned()
        .collect::<Vec<_>>();
    for item in default_env {
        push_env_if_missing(&mut env, item);
    }
    env.into_iter()
        .filter_map(|item| CString::new(item).ok())
        .collect()
}

fn push_env_if_missing(env: &mut Vec<String>, item: &str) {
    if env.len() >= MAX_EXEC_ENV_COUNT {
        return;
    }
    let key = env_key(item);
    if env.iter().any(|candidate| env_key(candidate) == key) {
        return;
    }
    env.push(item.to_string());
}

fn env_key(value: &str) -> &str {
    value.split_once('=').map(|(key, _)| key).unwrap_or(value)
}

fn c_string_or_fallback(value: &str, fallback: &str) -> CString {
    CString::new(value).unwrap_or_else(|_| CString::new(fallback).unwrap())
}

fn valid_exec_text(value: &str, require_env_assignment: bool) -> bool {
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

#[cfg(test)]
mod tests {
    use super::{build_exec_argv, build_exec_env_with_defaults};

    #[test]
    fn build_exec_argv_defaults_to_exec_path() {
        let argv = build_exec_argv("apps/demo/demo.elf", &[]);
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
        );
        let values = env
            .iter()
            .map(|item| item.to_str().unwrap().to_string())
            .collect::<Vec<_>>();
        assert!(values.iter().any(|item| item == "PATH=/custom/bin"));
        assert!(
            values
                .iter()
                .any(|item| item == "XDG_RUNTIME_DIR=/run/custom")
        );
        assert!(values.iter().any(|item| item == "HOME=/home/user"));
        assert!(
            values
                .iter()
                .any(|item| item == "WAYLAND_DISPLAY=wayland-0")
        );
        assert!(values.iter().any(|item| item == "XDG_SESSION_TYPE=wayland"));
        assert!(
            values
                .iter()
                .any(|item| item == "XDG_CURRENT_DESKTOP=RustOS")
        );
        assert!(
            !values
                .iter()
                .any(|item| item == "PATH=/bin:/usr/bin:/usr/local/bin")
        );
        assert!(
            !values
                .iter()
                .any(|item| item == "XDG_RUNTIME_DIR=/run/user/1000")
        );
    }
}

const CONSOLE_IOCTL_SET_FOCUS: usize = console_abi::CONSOLE_IOCTL_SET_FOCUS as usize;
const CONSOLE_IOCTL_CREATE_SESSION: usize = console_abi::CONSOLE_IOCTL_CREATE_SESSION as usize;
const CONSOLE_IOCTL_CLOSE_SESSION: usize = console_abi::CONSOLE_IOCTL_CLOSE_SESSION as usize;
const CONSOLE_IOCTL_SET_SESSION_STATE: usize =
    console_abi::CONSOLE_IOCTL_SET_SESSION_STATE as usize;

fn create_console_session(
    console_fd: RawFd,
    program_id: u32,
    title: &str,
    exec_path: &str,
) -> Result<u64, i32> {
    let mut request = ConsoleCreateSessionRequest::new(
        program_id,
        title.as_ptr() as u64,
        title.len() as u64,
        exec_path.as_ptr() as u64,
        exec_path.len() as u64,
    );
    sessiond_console_ioctl(console_fd, CONSOLE_IOCTL_CREATE_SESSION, &mut request)?;
    Ok(request.session_handle)
}

fn close_console_session(console_fd: RawFd, session_handle: u64) -> Result<bool, i32> {
    let mut request = ConsoleCloseSessionRequest::new(session_handle);
    match sessiond_console_ioctl(console_fd, CONSOLE_IOCTL_CLOSE_SESSION, &mut request) {
        Ok(()) => Ok(true),
        Err(err) if err == libc::ENOENT || err == libc::EINVAL => Ok(false),
        Err(err) => Err(err),
    }
}

fn set_console_session_state(
    console_fd: RawFd,
    session_handle: u64,
    state: u16,
) -> Result<(), i32> {
    let mut request = ConsoleSetSessionStateRequest::new(session_handle, state);
    sessiond_console_ioctl(console_fd, CONSOLE_IOCTL_SET_SESSION_STATE, &mut request)
}

fn console_set_focus(console_fd: RawFd, session_handle: u64) -> Result<(), i32> {
    let mut request = ConsoleSetFocusRequest::new(session_handle);
    sessiond_console_ioctl(console_fd, CONSOLE_IOCTL_SET_FOCUS, &mut request)
}

fn sessiond_console_ioctl<T>(console_fd: RawFd, request: usize, arg: &mut T) -> Result<(), i32> {
    let args = RustosDeviceIoctlBrokerArgs {
        process_id: 0,
        fd: console_fd as u64,
        request: request as u64,
        arg: arg as *mut T as u64,
        reserved0: 0,
    };
    let rc = unsafe {
        libc::syscall(
            SYS_RUSTOS_DEVICE_IOCTL_BROKER as libc::c_long,
            (&args as *const RustosDeviceIoctlBrokerArgs) as u64,
        ) as i64
    };
    if rc < 0 {
        let errno = (-rc) as i32;
        if errno == libc::EPERM {
            return ioctl_with_mut(console_fd, request, arg);
        }
        return Err(errno);
    }
    Ok(())
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

fn ensure_console_fd(state: &mut BrokerState) -> Result<RawFd, i32> {
    if state.console_fd.is_none() {
        stderr_line("runtimed: console open begin");
        boot_line("runtimed: console open begin");
        let fd = open_device(CONSOLE_PATH, O_RDWR)?;
        stderr_line("runtimed: console open done");
        boot_line("runtimed: console ready");
        state.console_fd = Some(fd);
    }
    Ok(state
        .console_fd
        .as_ref()
        .map(|fd| fd.as_raw_fd())
        .unwrap_or(-1))
}

fn ioctl_with_mut<T>(fd: RawFd, request: usize, arg: &mut T) -> Result<(), i32> {
    let rc = unsafe { libc::syscall(SYS_IOCTL as libc::c_long, fd, request, arg as *mut T) as i32 };
    if rc < 0 {
        return Err(last_errno());
    }
    Ok(())
}

fn as_bytes<T>(value: &T) -> &[u8] {
    unsafe { std::slice::from_raw_parts((value as *const T).cast::<u8>(), size_of::<T>()) }
}

fn as_bytes_mut<T>(value: &mut T) -> &mut [u8] {
    unsafe { std::slice::from_raw_parts_mut((value as *mut T).cast::<u8>(), size_of::<T>()) }
}

fn read_unaligned<T: Copy>(bytes: &[u8]) -> T {
    assert!(bytes.len() >= size_of::<T>());
    unsafe { bytes.as_ptr().cast::<T>().read_unaligned() }
}
