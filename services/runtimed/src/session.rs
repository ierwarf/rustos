use std::mem::size_of;
use std::os::fd::AsRawFd;
use std::sync::atomic::Ordering;
use std::thread;
use std::time::{Duration, Instant};

use rustos_user_abi::console::{
    self as console_abi, ConsoleSessionInfo, ConsoleSnapshotSessionsRequest, ConsoleStateInfo,
};
use rustos_user_abi::syscall::{
    COMMERCIAL_MAX_PROTOCOL_ABI_VERSION, COMMERCIAL_MAX_PROTOCOL_SESSIOND,
    COMMERCIAL_MAX_SESSIOND_OP_CONSOLE_ROUTE, COMMERCIAL_MAX_SESSIOND_OP_FOREGROUND_FOCUS,
    COMMERCIAL_MAX_SESSIOND_OP_SESSION_GRAPH, COMMERCIAL_MAX_SESSIOND_OP_TTY_LINE_DISCIPLINE,
    COMMERCIAL_MAX_SESSIOND_OP_UI_BOOTSTRAP, CommercialMaxCapabilityLeaseWire,
    CommercialMaxProtocolDescriptorWire, CommercialMaxProtocolRequest,
    CommercialMaxProtocolResponse, IPC_SERVICE_DEVMGRD, IPC_SERVICE_SESSIOND,
    SYS_RUSTOS_IPC_ENDPOINT_CREATE, SYS_RUSTOS_IPC_REGISTER_SERVICE_ENDPOINT,
    SYS_RUSTOS_IPC_REPLY, SYS_RUSTOS_IPC_TRY_RECV,
};
use runtime_control::{
    DEFAULT_APPLICATIONS_DIR, DEFAULT_RUNTIME_ENV_REGISTRY_PATH, RuntimeEnvScope,
    load_desktop_program_entries, load_runtime_default_env,
};

use super::{BrokerState, LaunchEntry};
use super::{
    CONSOLE_SESSION_STATE_RUNNING, DEVMGRD_BOOTSTRAP_WAIT_TIMEOUT, LINUX_FIONREAD, LINUX_TCGETS,
    LINUX_TCSETS, LINUX_TCSETSF, LINUX_TCSETSW, SERVICE_ENDPOINT_POLL_INTERVAL,
    SESSION_GRAPH_GENERATION, UI_SERVER_DESKTOP_FILE_ID, UI_SERVER_DISPLAY_NAME,
    UI_SERVER_EXEC_PATH, UI_SERVER_TASK_WEIGHT_MICROS, boot_line,
};

pub(super) fn bootstrap_ui_server(state: &mut BrokerState) -> Result<(), i32> {
    boot_line("runtimed: waiting for devmgrd before ui bootstrap");
    wait_for_service_endpoint(IPC_SERVICE_DEVMGRD, DEVMGRD_BOOTSTRAP_WAIT_TIMEOUT)?;
    let (args, env) = ui_server_bootstrap_args_env();
    super::spawn::spawn_tracked_process(
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
        if super::spawn::lookup_service_endpoint(service_id) > 0 {
            return Ok(());
        }
        if Instant::now() >= deadline {
            return Err(libc::ETIMEDOUT);
        }
        thread::sleep(SERVICE_ENDPOINT_POLL_INTERVAL);
    }
}

pub(super) fn create_session_endpoint() -> Option<u64> {
    let endpoint =
        unsafe { libc::syscall(SYS_RUSTOS_IPC_ENDPOINT_CREATE as libc::c_long) as i64 };
    if endpoint < 0 {
        super::spawn::stderr_line("runtimed: session endpoint create failed");
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
        super::spawn::stderr_line("runtimed: session endpoint register failed");
        return None;
    }
    super::spawn::stderr_line("runtimed: session policy endpoint registered");
    Some(endpoint as u64)
}

pub(super) fn service_session_endpoint(endpoint: Option<u64>, state: &mut BrokerState) -> bool {
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
        super::spawn::stderr_line("runtimed: session reply failed");
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
        COMMERCIAL_MAX_SESSIOND_OP_TTY_LINE_DISCIPLINE => matches!(
            request_number,
            LINUX_TCGETS | LINUX_TCSETS | LINUX_TCSETSW | LINUX_TCSETSF | LINUX_FIONREAD
        ),
        COMMERCIAL_MAX_SESSIOND_OP_UI_BOOTSTRAP => false,
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
            copy_payload(response, super::util::as_bytes(&info))
        }
        console_abi::CONSOLE_IOCTL_SNAPSHOT_SESSIONS => {
            if request.payload_len as usize != size_of::<ConsoleSnapshotSessionsRequest>() {
                return libc::EINVAL;
            }
            let mut snapshot =
                super::util::read_unaligned::<ConsoleSnapshotSessionsRequest>(&request.payload);
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
                super::util::copy_ascii_into(&mut info.title, &program.display_name);
                let bytes = super::util::as_bytes(&info);
                response.payload[payload_len..payload_len + bytes.len()].copy_from_slice(bytes);
                payload_len += bytes.len();
                written += 1;
            }
            snapshot.count = written as u64;
            response.payload[..size_of::<ConsoleSnapshotSessionsRequest>()]
                .copy_from_slice(super::util::as_bytes(&snapshot));
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
    super::util::copy_label(label, &mut descriptor.name, &mut descriptor.name_len);
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
    super::util::copy_label(label, &mut capability.label, &mut capability.label_len);
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
