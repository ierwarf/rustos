use std::collections::{BTreeMap, VecDeque};
use std::mem::size_of;
use std::sync::atomic::Ordering;
use std::time::Instant;

use keyboard_core::KeyCode;
use runtime_control::{
    load_runtime_default_env, RuntimeEnvScope, DEFAULT_RUNTIME_ENV_REGISTRY_PATH,
};
use rustos_user_abi::console::{
    self as console_abi, ConsoleSendInputEventRequest, ConsoleSessionInfo, ConsoleSetFocusRequest,
    ConsoleSnapshotSessionOutputRequest, ConsoleSnapshotSessionsRequest, ConsoleStateInfo,
};
use rustos_user_abi::device::{InputEvent, INPUT_ACTION_RELEASED, INPUT_KIND_KEYBOARD};
use rustos_user_abi::linux::LinuxTermios;
use rustos_user_abi::syscall::{
    CommercialMaxCapabilityLeaseWire, CommercialMaxProtocolDescriptorWire,
    CommercialMaxProtocolRequest, CommercialMaxProtocolResponse, WaitSetSignalBrokerArgs,
    COMMERCIAL_MAX_PROTOCOL_ABI_VERSION, COMMERCIAL_MAX_PROTOCOL_SESSIOND,
    COMMERCIAL_MAX_SESSIOND_CONSOLE_ROUTE_READ, COMMERCIAL_MAX_SESSIOND_CONSOLE_ROUTE_READINESS,
    COMMERCIAL_MAX_SESSIOND_CONSOLE_ROUTE_WRITE, COMMERCIAL_MAX_SESSIOND_OP_CONSOLE_ROUTE,
    COMMERCIAL_MAX_SESSIOND_OP_FOREGROUND_FOCUS, COMMERCIAL_MAX_SESSIOND_OP_SESSION_GRAPH,
    COMMERCIAL_MAX_SESSIOND_OP_TTY_LINE_DISCIPLINE, COMMERCIAL_MAX_SESSIOND_OP_UI_BOOTSTRAP,
    IPC_SERVICE_DEVMGRD, IPC_SERVICE_SESSIOND, SESSIOND_CONSOLE_READINESS_LIVE,
    SESSIOND_CONSOLE_READINESS_READY, SYS_RUSTOS_IPC_ENDPOINT_CREATE, SYS_RUSTOS_IPC_REPLY,
    SYS_RUSTOS_IPC_TRY_RECV_WITH_SENDER, SYS_RUSTOS_WAITSET_SIGNAL_BROKER, WAITSET_ABI_VERSION,
    WAITSET_PROVIDER_SESSIOND,
};

use super::{
    boot_line, CONSOLE_SESSION_STATE_RUNNING, LINUX_FIONREAD, LINUX_TCGETS, LINUX_TCSETS,
    LINUX_TCSETSF, LINUX_TCSETSW, SESSION_GRAPH_GENERATION, UI_SERVER_BOOTSTRAP_ENV,
    UI_SERVER_DESKTOP_FILE_ID, UI_SERVER_DISPLAY_NAME, UI_SERVER_EXEC_PATH,
    UI_SERVER_TASK_WEIGHT_MICROS,
};
use super::{BrokerState, LaunchEntry};

const INPUT_BUFFER_CAPACITY: usize = 1024;
const EDIT_BUFFER_CAPACITY: usize = 256;
const OUTPUT_BUFFER_CAPACITY: usize = 4096;

pub(crate) struct SessionRuntime {
    sessions: BTreeMap<u64, TtySessionState>,
    output_generation: u64,
    input_readiness_generation: u64,
}

impl Default for SessionRuntime {
    fn default() -> Self {
        Self {
            sessions: BTreeMap::new(),
            output_generation: 0,
            input_readiness_generation: 1,
        }
    }
}

impl SessionRuntime {
    pub(crate) fn create_session(&mut self, session: u64) {
        self.sessions.entry(session).or_default();
    }

    pub(crate) fn remove_session(&mut self, session: u64) {
        if self.sessions.remove(&session).is_some() {
            self.input_readiness_generation = self
                .input_readiness_generation
                .checked_add(1)
                .expect("sessiond input readiness generation exhausted");
            #[cfg(not(test))]
            publish_input_readiness(session, self.input_readiness_generation);
        }
    }

    fn write_to_session(&mut self, session: u64, bytes: &[u8]) -> Option<usize> {
        let state = self.sessions.get_mut(&session)?;
        let written = state.write(bytes);
        if written != 0 {
            self.output_generation = self.output_generation.wrapping_add(1).max(1);
        }
        Some(written)
    }

    fn read_from_session(&mut self, session: u64, dest: &mut [u8]) -> Option<usize> {
        Some(self.sessions.get_mut(&session)?.read_input(dest))
    }

    fn snapshot_output(&self, session: u64, dest: &mut [u8]) -> Option<usize> {
        let state = self.sessions.get(&session)?;
        Some(state.snapshot_output(dest))
    }

    fn handle_input_event(&mut self, session: u64, event: InputEvent) -> Result<bool, i32> {
        if event.kind != INPUT_KIND_KEYBOARD {
            return Ok(false);
        }
        if event.action == INPUT_ACTION_RELEASED {
            return Ok(false);
        }
        let (changed, became_ready) = {
            let state = self.sessions.get_mut(&session).ok_or(libc::ENODEV)?;
            let was_ready = !state.input.is_empty();
            let changed = state.on_key_event(event)?;
            (changed, !was_ready && !state.input.is_empty())
        };
        if changed {
            self.output_generation = self.output_generation.wrapping_add(1).max(1);
        }
        if became_ready {
            self.input_readiness_generation = self
                .input_readiness_generation
                .checked_add(1)
                .expect("sessiond input readiness generation exhausted");
        }
        Ok(became_ready)
    }

    fn termios(&self, session: u64) -> Option<LinuxTermios> {
        self.sessions.get(&session).map(|state| state.termios)
    }

    fn set_termios(&mut self, session: u64, termios: LinuxTermios, flush_input: bool) -> bool {
        let Some(state) = self.sessions.get_mut(&session) else {
            return false;
        };
        state.set_termios(termios, flush_input);
        true
    }

    fn pending_input_len(&self, session: u64) -> Option<usize> {
        self.sessions.get(&session).map(|state| state.input.len())
    }

    fn output_generation(&self) -> u64 {
        self.output_generation
    }

    fn input_readiness_generation(&self) -> u64 {
        self.input_readiness_generation
    }

    fn input_readiness_snapshot(&self, session: u64) -> (bool, bool, u64) {
        (
            self.sessions
                .get(&session)
                .is_some_and(|state| !state.input.is_empty()),
            self.sessions.contains_key(&session),
            self.input_readiness_generation,
        )
    }
}

struct TtySessionState {
    input: VecDeque<u8>,
    edit: Vec<u8>,
    edit_cursor: usize,
    output: VecDeque<u8>,
    termios: LinuxTermios,
}

impl Default for TtySessionState {
    fn default() -> Self {
        Self {
            input: VecDeque::new(),
            edit: Vec::new(),
            edit_cursor: 0,
            output: VecDeque::new(),
            termios: LinuxTermios::default_console(),
        }
    }
}

impl TtySessionState {
    fn read_input(&mut self, dest: &mut [u8]) -> usize {
        let mut read = 0usize;
        while read < dest.len() {
            let Some(byte) = self.input.pop_front() else {
                break;
            };
            dest[read] = byte;
            read += 1;
        }
        read
    }

    fn write(&mut self, bytes: &[u8]) -> usize {
        if self.termios.maps_output_newline_to_crlf() {
            let mut previous = None;
            for &byte in bytes {
                if byte == b'\n' && previous != Some(b'\r') {
                    self.push_output(b'\r');
                }
                self.push_output(byte);
                previous = Some(byte);
            }
        } else {
            self.push_output_bytes(bytes);
        }
        bytes.len()
    }

    fn snapshot_output(&self, dest: &mut [u8]) -> usize {
        let count = dest.len().min(self.output.len());
        let skip = self.output.len().saturating_sub(count);
        for (idx, byte) in self.output.iter().skip(skip).take(count).enumerate() {
            dest[idx] = *byte;
        }
        count
    }

    fn on_key_event(&mut self, event: InputEvent) -> Result<bool, i32> {
        let before = self.output.len();
        if self.termios.is_canonical() {
            self.on_canonical_key_event(event)?;
        } else {
            self.on_noncanonical_key_event(event)?;
        }
        Ok(self.output.len() != before)
    }

    fn on_canonical_key_event(&mut self, event: InputEvent) -> Result<(), i32> {
        match KeyCode::from_u32(event.code) {
            Some(KeyCode::ArrowLeft) => self.move_cursor_left(),
            Some(KeyCode::ArrowRight) => self.move_cursor_right(),
            Some(KeyCode::Backspace) => self.handle_backspace(),
            Some(KeyCode::Enter | KeyCode::NumpadEnter) => self.commit_line(),
            _ => {
                let byte = input_event_text_byte(event)?;
                self.insert_edit_byte(byte);
            }
        }
        Ok(())
    }

    fn on_noncanonical_key_event(&mut self, event: InputEvent) -> Result<(), i32> {
        let bytes = noncanonical_input_bytes(self.termios, event)?;
        self.push_input_bytes(&bytes);
        if self.termios.echo_enabled() {
            self.echo_noncanonical_input(&bytes);
        }
        Ok(())
    }

    fn insert_edit_byte(&mut self, byte: u8) {
        if self.edit.len() == EDIT_BUFFER_CAPACITY {
            return;
        }
        let cursor = self.edit_cursor.min(self.edit.len());
        self.edit.insert(cursor, byte);
        self.edit_cursor = cursor + 1;
        if self.should_echo_canonical_input() {
            if cursor + 1 == self.edit.len() {
                self.push_output(byte);
            } else {
                let tail: Vec<u8> = self.edit[cursor..].to_vec();
                self.push_output_bytes(&tail);
                self.move_visual_cursor_left(self.edit.len() - self.edit_cursor);
            }
        }
    }

    fn handle_backspace(&mut self) {
        if self.edit_cursor == 0 || self.edit.is_empty() {
            return;
        }
        let delete_at = self.edit_cursor - 1;
        self.edit.remove(delete_at);
        self.edit_cursor -= 1;
        if self.should_echo_canonical_input() {
            if delete_at == self.edit.len() {
                self.push_output_bytes(b"\x08 \x08");
            } else {
                self.move_visual_cursor_left(1);
                let tail: Vec<u8> = self.edit[delete_at..].to_vec();
                self.push_output_bytes(&tail);
                self.push_output(b' ');
                self.move_visual_cursor_left(self.edit.len() - delete_at + 1);
            }
        }
    }

    fn move_cursor_left(&mut self) {
        if self.edit_cursor == 0 {
            return;
        }
        self.edit_cursor -= 1;
        if self.should_echo_canonical_input() {
            self.move_visual_cursor_left(1);
        }
    }

    fn move_cursor_right(&mut self) {
        if self.edit_cursor >= self.edit.len() {
            return;
        }
        self.edit_cursor += 1;
        if self.should_echo_canonical_input() {
            self.move_visual_cursor_right(1);
        }
    }

    fn commit_line(&mut self) {
        let required = self.edit.len().saturating_add(1);
        if self.input.len().saturating_add(required) > INPUT_BUFFER_CAPACITY {
            return;
        }
        if self.should_echo_canonical_input() {
            self.push_output_bytes(b"\r\n");
        }
        let edit = core::mem::take(&mut self.edit);
        self.push_input_bytes(&edit);
        self.push_input_bytes(b"\n");
        self.edit_cursor = 0;
    }

    fn set_termios(&mut self, termios: LinuxTermios, flush_input: bool) {
        if flush_input {
            self.input.clear();
            self.edit.clear();
            self.edit_cursor = 0;
        } else if self.termios.is_canonical() && !termios.is_canonical() {
            let edit = core::mem::take(&mut self.edit);
            self.push_input_bytes(&edit);
            self.edit_cursor = 0;
        }
        self.termios = termios;
    }

    fn should_echo_canonical_input(&self) -> bool {
        self.termios.is_canonical() && self.termios.echo_enabled()
    }

    fn echo_noncanonical_input(&mut self, bytes: &[u8]) {
        if bytes.len() != 1 {
            return;
        }
        match bytes[0] {
            b'\n' => self.push_output_bytes(b"\r\n"),
            0x08 | 0x7f => self.push_output_bytes(b"\x08 \x08"),
            b'\t' | 0x20..=0x7e => self.push_output(bytes[0]),
            byte if self.termios.echoes_control_chars() => {
                self.push_output(b'^');
                self.push_output(if byte == 0x7f {
                    b'?'
                } else {
                    byte.saturating_add(64)
                });
            }
            _ => {}
        }
    }

    fn move_visual_cursor_left(&mut self, count: usize) {
        self.write_cursor_move_sequence(count, b'D');
    }

    fn move_visual_cursor_right(&mut self, count: usize) {
        self.write_cursor_move_sequence(count, b'C');
    }

    fn write_cursor_move_sequence(&mut self, count: usize, direction: u8) {
        if count == 0 {
            return;
        }
        self.push_output_bytes(b"\x1b[");
        if count != 1 {
            for byte in count.to_string().bytes() {
                self.push_output(byte);
            }
        }
        self.push_output(direction);
    }

    fn push_input_bytes(&mut self, bytes: &[u8]) {
        let room = INPUT_BUFFER_CAPACITY.saturating_sub(self.input.len());
        for &byte in bytes.iter().take(room) {
            self.input.push_back(byte);
        }
    }

    fn push_output_bytes(&mut self, bytes: &[u8]) {
        for &byte in bytes {
            self.push_output(byte);
        }
    }

    fn push_output(&mut self, byte: u8) {
        if self.output.len() == OUTPUT_BUFFER_CAPACITY {
            let _ = self.output.pop_front();
        }
        self.output.push_back(byte);
    }
}

fn input_event_text_byte(event: InputEvent) -> Result<u8, i32> {
    if event.text == 0 || event.text > u8::MAX as u32 {
        return Err(libc::EINVAL);
    }
    Ok(event.text as u8)
}

fn noncanonical_input_bytes(termios: LinuxTermios, event: InputEvent) -> Result<Vec<u8>, i32> {
    let bytes: &[u8] = match KeyCode::from_u32(event.code) {
        Some(KeyCode::Backspace) => return Ok(vec![termios.erase_byte()]),
        Some(KeyCode::Enter | KeyCode::NumpadEnter) => b"\n",
        Some(KeyCode::ArrowUp) => b"\x1b[A",
        Some(KeyCode::ArrowDown) => b"\x1b[B",
        Some(KeyCode::ArrowRight) => b"\x1b[C",
        Some(KeyCode::ArrowLeft) => b"\x1b[D",
        Some(KeyCode::Home) => b"\x1b[H",
        Some(KeyCode::End) => b"\x1b[F",
        Some(KeyCode::Insert) => b"\x1b[2~",
        Some(KeyCode::Delete) => b"\x1b[3~",
        Some(KeyCode::PageUp) => b"\x1b[5~",
        Some(KeyCode::PageDown) => b"\x1b[6~",
        Some(KeyCode::Escape) => b"\x1b",
        _ => return Ok(vec![input_event_text_byte(event)?]),
    };
    Ok(bytes.to_vec())
}

pub(super) fn bootstrap_ui_server(state: &mut BrokerState) -> Result<(), i32> {
    boot_line("runtimed: waiting for devmgrd before ui bootstrap");
    wait_for_service_endpoint(IPC_SERVICE_DEVMGRD)?;
    match spawn_ui_server_once(state) {
        Err(errno) if ui_bootstrap_may_retry_immediately(errno) => {
            // Every bounded IPC layer revokes its reply capability before it
            // reports ETIMEDOUT, so the exact retry cannot overlap the first
            // environment/VFS/loader transaction. A second failure returns
            // to the ordinary bounded retry owner.
            boot_line("runtimed: ui bootstrap timed out; one immediate retry");
            spawn_ui_server_once(state)
        }
        result => result,
    }
}

fn spawn_ui_server_once(state: &mut BrokerState) -> Result<(), i32> {
    let (args, env) = ui_server_bootstrap_args_env()?;
    let entry = LaunchEntry {
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
    };
    super::spawn::spawn_tracked_process(state, entry)
}

fn ui_bootstrap_may_retry_immediately(errno: i32) -> bool {
    errno == libc::ETIMEDOUT
}

fn wait_for_service_endpoint(service_id: u64) -> Result<(), i32> {
    // Initd's deferred-start contract publishes devmgrd before it activates
    // runtimed. Repeating policy lookup here can only amplify one unavailable
    // dependency into thousands of rootd IPC turns. A transient violation is
    // retried by the existing 500 ms UI-bootstrap owner.
    let endpoint = super::spawn::lookup_service_endpoint(service_id);
    if endpoint > 0 {
        Ok(())
    } else if endpoint < 0 {
        Err((-endpoint) as i32)
    } else {
        Err(libc::ENOENT)
    }
}

pub(super) fn create_session_endpoint() -> Option<u64> {
    let started_at = Instant::now();
    super::spawn::debug_line("runtimed: session endpoint create begin");
    let endpoint = unsafe { libc::syscall(SYS_RUSTOS_IPC_ENDPOINT_CREATE as libc::c_long) as i64 };
    if endpoint < 0 {
        super::spawn::debug_line("runtimed: session endpoint create failed");
        return None;
    }
    super::spawn::debug_line(
        format!(
            "runtimed: session endpoint create done elapsed_ms={}",
            started_at.elapsed().as_millis()
        )
        .as_str(),
    );
    super::spawn::debug_line("runtimed: session endpoint register begin");
    let register =
        rustos_svc_runtime::ipc::register_service_endpoint(IPC_SERVICE_SESSIOND, endpoint as u64);
    if register < 0 {
        super::spawn::debug_line(
            format!(
                "runtimed: session endpoint register failed errno={}",
                -register
            )
            .as_str(),
        );
        return None;
    }
    super::spawn::debug_line(
        format!(
            "runtimed: session policy endpoint registered endpoint={endpoint} elapsed_ms={}",
            started_at.elapsed().as_millis()
        )
        .as_str(),
    );
    Some(endpoint as u64)
}

pub(super) fn service_session_endpoint(endpoint: Option<u64>, state: &mut BrokerState) -> bool {
    let Some(endpoint) = endpoint else {
        return false;
    };
    let mut request = CommercialMaxProtocolRequest::default();
    let mut reply_cap = 0_u64;
    let mut sender_pid = 0_u64;
    let mut sender_tid = 0_u64;
    let received = unsafe {
        libc::syscall(
            SYS_RUSTOS_IPC_TRY_RECV_WITH_SENDER as libc::c_long,
            endpoint,
            (&mut request as *mut CommercialMaxProtocolRequest) as u64,
            size_of::<CommercialMaxProtocolRequest>() as u64,
            (&mut reply_cap as *mut u64) as u64,
            (&mut sender_pid as *mut u64) as u64,
            (&mut sender_tid as *mut u64) as u64,
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
    let direct_sender = request.subject_is_exact_sender(sender_pid, sender_tid);
    let delegated_by_devmgrd = !direct_sender
        && rustos_svc_runtime::ipc::validate_service_owner(IPC_SERVICE_DEVMGRD, sender_pid) >= 0;
    response.status = if received as usize != size_of::<CommercialMaxProtocolRequest>() {
        libc::EINVAL
    } else if !session_ingress_identity_authorized(
        &request,
        sender_pid,
        sender_tid,
        delegated_by_devmgrd,
    ) {
        libc::EACCES
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
        super::spawn::debug_line("runtimed: session reply failed");
    }
    true
}

fn session_ingress_identity_authorized(
    request: &CommercialMaxProtocolRequest,
    sender_pid: u64,
    sender_tid: u64,
    delegated_by_devmgrd: bool,
) -> bool {
    request.subject_is_exact_sender(sender_pid, sender_tid)
        || delegated_by_devmgrd
            && request.header.subject_pid != 0
            && request.header.subject_tid != 0
            && request.header.op != COMMERCIAL_MAX_SESSIOND_OP_UI_BOOTSTRAP
}

fn handle_session_request(
    request: &CommercialMaxProtocolRequest,
    state: &mut BrokerState,
    response: &mut CommercialMaxProtocolResponse,
) -> i32 {
    if !request.has_valid_envelope() || request.header.protocol != COMMERCIAL_MAX_PROTOCOL_SESSIOND
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
            handle_tty_line_request(request, state, response)
        }
        COMMERCIAL_MAX_SESSIOND_OP_CONSOLE_ROUTE => {
            handle_console_route_request(request, state, response)
        }
        COMMERCIAL_MAX_SESSIOND_OP_FOREGROUND_FOCUS => {
            let status = handle_foreground_focus_request(request, state, response);
            if status != 0 {
                return status;
            }
            response.descriptor_count = 1;
            response.descriptors[0] =
                session_descriptor("foreground-focus", request.header.op, response.value0, 0);
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

fn handle_foreground_focus_request(
    request: &CommercialMaxProtocolRequest,
    state: &mut BrokerState,
    response: &mut CommercialMaxProtocolResponse,
) -> i32 {
    match request.arg0 {
        0 => {
            response.value0 = focused_session_handle(state);
            0
        }
        console_abi::CONSOLE_IOCTL_SET_FOCUS => {
            if request.payload_len as usize != size_of::<ConsoleSetFocusRequest>() {
                return libc::EINVAL;
            }
            let focus = super::util::read_unaligned::<ConsoleSetFocusRequest>(&request.payload);
            set_focused_session_handle(state, focus.session_handle)
        }
        _ => libc::ENOTTY,
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
                | COMMERCIAL_MAX_SESSIOND_CONSOLE_ROUTE_READ
                | COMMERCIAL_MAX_SESSIOND_CONSOLE_ROUTE_READINESS
                | COMMERCIAL_MAX_SESSIOND_CONSOLE_ROUTE_WRITE
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

fn handle_tty_line_request(
    request: &CommercialMaxProtocolRequest,
    state: &mut BrokerState,
    response: &mut CommercialMaxProtocolResponse,
) -> i32 {
    let session = request.arg2;
    if session == 0 {
        return libc::EINVAL;
    }
    response.descriptor_count = 1;
    response.descriptors[0] = session_descriptor("tty-line", request.header.op, session, 0);
    response.capability = session_capability("tty-line", request.header.op);
    match request.arg0 {
        LINUX_TCGETS => {
            let Some(termios) = state.session_runtime.termios(session) else {
                return libc::ENODEV;
            };
            copy_payload(response, super::util::as_bytes(&termios))
        }
        LINUX_TCSETS | LINUX_TCSETSW | LINUX_TCSETSF => {
            if request.payload_len as usize != size_of::<LinuxTermios>() {
                return libc::EINVAL;
            }
            let termios = super::util::read_unaligned::<LinuxTermios>(&request.payload);
            if !state
                .session_runtime
                .set_termios(session, termios, request.arg0 == LINUX_TCSETSF)
            {
                return libc::ENODEV;
            }
            0
        }
        LINUX_FIONREAD => {
            let Some(pending) = state.session_runtime.pending_input_len(session) else {
                return libc::ENODEV;
            };
            response.value0 = pending as u64;
            0
        }
        _ => libc::ENOTTY,
    }
}

fn handle_console_route_request(
    request: &CommercialMaxProtocolRequest,
    state: &mut BrokerState,
    response: &mut CommercialMaxProtocolResponse,
) -> i32 {
    response.descriptor_count = 1;
    response.descriptors[0] = session_descriptor(
        "console-route",
        request.header.op,
        request.arg2,
        request.arg0,
    );
    response.capability = session_capability("console-route", request.header.op);
    match request.arg0 {
        COMMERCIAL_MAX_SESSIOND_CONSOLE_ROUTE_WRITE => {
            let session = request.arg2;
            if session == 0 || request.payload_len as usize > request.payload.len() {
                return libc::EINVAL;
            }
            let payload_len = request.payload_len as usize;
            let Some(written) = state
                .session_runtime
                .write_to_session(session, &request.payload[..payload_len])
            else {
                return libc::ENODEV;
            };
            response.value0 = written as u64;
            0
        }
        COMMERCIAL_MAX_SESSIOND_CONSOLE_ROUTE_READ => {
            let session = request.arg2;
            let capacity = request.arg3.min(response.payload.len() as u64) as usize;
            if session == 0 || capacity == 0 {
                return libc::EINVAL;
            }
            let Some(read) = state
                .session_runtime
                .read_from_session(session, &mut response.payload[..capacity])
            else {
                return libc::ENODEV;
            };
            response.payload_len = read as u32;
            response.value0 = read as u64;
            0
        }
        COMMERCIAL_MAX_SESSIOND_CONSOLE_ROUTE_READINESS => {
            let session = request.arg2;
            if session == 0 || request.payload_len != 0 || request.arg3 != 0 {
                return libc::EINVAL;
            }
            let (ready, live, generation) = state.session_runtime.input_readiness_snapshot(session);
            response.value0 = if ready {
                SESSIOND_CONSOLE_READINESS_READY
            } else {
                0
            } | if live {
                SESSIOND_CONSOLE_READINESS_LIVE
            } else {
                0
            };
            response.value1 = generation;
            0
        }
        console_abi::CONSOLE_IOCTL_SNAPSHOT_SESSION_OUTPUT => {
            if request.payload_len as usize != size_of::<ConsoleSnapshotSessionOutputRequest>() {
                return libc::EINVAL;
            }
            let mut snapshot = super::util::read_unaligned::<ConsoleSnapshotSessionOutputRequest>(
                &request.payload,
            );
            let capacity = snapshot.capacity.min(response.payload.len() as u64) as usize;
            let mut bytes = vec![0_u8; capacity];
            let Some(count) = state
                .session_runtime
                .snapshot_output(snapshot.session_handle, &mut bytes)
            else {
                return libc::EINVAL;
            };
            snapshot.count = count as u64;
            let header_len = size_of::<ConsoleSnapshotSessionOutputRequest>();
            let payload_len = header_len.saturating_add(count);
            if payload_len > response.payload.len() {
                return libc::EINVAL;
            }
            response.payload[..header_len].copy_from_slice(super::util::as_bytes(&snapshot));
            response.payload[header_len..payload_len].copy_from_slice(&bytes[..count]);
            response.payload_len = payload_len as u32;
            response.value0 = count as u64;
            0
        }
        console_abi::CONSOLE_IOCTL_SEND_INPUT_EVENT => {
            if request.payload_len as usize != size_of::<ConsoleSendInputEventRequest>() {
                return libc::EINVAL;
            }
            let input =
                super::util::read_unaligned::<ConsoleSendInputEventRequest>(&request.payload);
            match state
                .session_runtime
                .handle_input_event(input.session_handle, input.event)
            {
                Ok(became_ready) => {
                    if became_ready {
                        publish_input_readiness(
                            input.session_handle,
                            state.session_runtime.input_readiness_generation(),
                        );
                    }
                    0
                }
                Err(errno) => errno,
            }
        }
        _ => {
            response.value0 = state
                .running
                .values()
                .filter(|program| program.session_handle != 0)
                .count() as u64;
            0
        }
    }
}

fn publish_input_readiness(session: u64, generation: u64) {
    let args = WaitSetSignalBrokerArgs {
        abi_version: WAITSET_ABI_VERSION,
        provider: WAITSET_PROVIDER_SESSIOND,
        flags: 0,
        object_id: session,
        generation,
        reserved0: 0,
    };
    let status = unsafe {
        libc::syscall(
            SYS_RUSTOS_WAITSET_SIGNAL_BROKER as libc::c_long,
            (&args as *const WaitSetSignalBrokerArgs) as u64,
        ) as i64
    };
    if status < 0 {
        boot_line("sessiond: input readiness publication failed");
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
            let generation = state
                .session_runtime
                .output_generation()
                .max(SESSION_GRAPH_GENERATION.fetch_add(1, Ordering::Relaxed));
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
    if state.focused_session_handle != 0 && session_exists(state, state.focused_session_handle) {
        return state.focused_session_handle;
    }
    fallback_focused_session_handle_excluding(state, 0)
}

fn set_focused_session_handle(state: &mut BrokerState, session_handle: u64) -> i32 {
    if session_handle == 0 || !session_exists(state, session_handle) {
        return libc::EINVAL;
    }
    state.focused_session_handle = session_handle;
    0
}

pub(super) fn focus_session_after_spawn(state: &mut BrokerState, session_handle: u64) {
    if session_handle != 0 {
        state.focused_session_handle = session_handle;
    }
}

pub(super) fn clear_focused_session_if(state: &mut BrokerState, session_handle: u64) {
    if session_handle != 0 && state.focused_session_handle == session_handle {
        state.focused_session_handle =
            fallback_focused_session_handle_excluding(state, session_handle);
    }
}

fn session_exists(state: &BrokerState, session_handle: u64) -> bool {
    state
        .running
        .values()
        .any(|program| program.session_handle == session_handle)
}

fn fallback_focused_session_handle_excluding(state: &BrokerState, excluded_session: u64) -> u64 {
    state
        .running
        .values()
        .filter(|program| program.session_handle != 0 && program.session_handle != excluded_session)
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

fn ui_server_bootstrap_args_env() -> Result<(Vec<String>, Vec<String>), i32> {
    // Bootstrap happens before the full launch catalog is needed. Keep its
    // two sealed service-local defaults in the binary and validate them
    // against the generated catalog when that catalog is admitted. Scanning
    // every desktop entry here serializes UI startup behind DVM cold reads.
    boot_line("runtimed: ui bootstrap env load begin");
    let mut env =
        load_runtime_default_env(DEFAULT_RUNTIME_ENV_REGISTRY_PATH, RuntimeEnvScope::Init)
            .map_err(runtime_registry_errno)?;
    boot_line("runtimed: ui bootstrap env load done");
    let manifest_env = UI_SERVER_BOOTSTRAP_ENV
        .iter()
        .map(|value| String::from(*value))
        .collect::<Vec<_>>();
    merge_manifest_env_into(&mut env, manifest_env.as_slice());
    Ok((Vec::new(), env))
}

fn runtime_registry_errno(error: std::io::Error) -> i32 {
    match error.raw_os_error() {
        Some(errno) if errno > 0 => errno,
        _ => libc::EIO,
    }
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

#[cfg(test)]
mod tests {
    use super::{
        session_ingress_identity_authorized, ui_bootstrap_may_retry_immediately, SessionRuntime,
    };
    use keyboard_core::KeyCode;
    use rustos_user_abi::device::{InputEvent, INPUT_ACTION_PRESSED, INPUT_KIND_KEYBOARD};
    use rustos_user_abi::linux::LinuxTermios;
    use rustos_user_abi::syscall::{
        CommercialMaxProtocolRequest, COMMERCIAL_MAX_SESSIOND_OP_UI_BOOTSTRAP,
    };

    #[test]
    fn session_ingress_requires_exact_sender_or_narrow_devmgrd_delegation() {
        let mut request = CommercialMaxProtocolRequest::default();
        request.header.subject_pid = 11;
        request.header.subject_tid = 13;
        assert!(session_ingress_identity_authorized(&request, 11, 13, false));
        assert!(!session_ingress_identity_authorized(
            &request, 17, 19, false
        ));
        assert!(session_ingress_identity_authorized(&request, 17, 19, true));
        request.header.op = COMMERCIAL_MAX_SESSIOND_OP_UI_BOOTSTRAP;
        assert!(!session_ingress_identity_authorized(&request, 17, 19, true));
    }

    #[test]
    fn ui_bootstrap_retries_only_a_revoked_timeout_transaction_immediately() {
        assert!(ui_bootstrap_may_retry_immediately(libc::ETIMEDOUT));
        assert!(!ui_bootstrap_may_retry_immediately(libc::EAGAIN));
        assert!(!ui_bootstrap_may_retry_immediately(libc::EINVAL));
    }

    fn key_event(code: u32, text: u8) -> InputEvent {
        InputEvent {
            kind: INPUT_KIND_KEYBOARD,
            action: INPUT_ACTION_PRESSED,
            code,
            value0: 0,
            value1: 0,
            modifiers: 0,
            text: text as u32,
        }
    }

    #[test]
    fn canonical_console_input_commits_text_on_enter() {
        let mut runtime = SessionRuntime::default();
        let session = 7;
        runtime.create_session(session);

        for byte in b"pwd" {
            runtime
                .handle_input_event(session, key_event(30, *byte))
                .expect("text key should be accepted");
        }

        let mut before_enter = [0_u8; 8];
        assert_eq!(
            runtime.read_from_session(session, &mut before_enter),
            Some(0)
        );

        runtime
            .handle_input_event(session, key_event(KeyCode::Enter as u32, b'\n'))
            .expect("enter should commit the edited line");

        let mut line = [0_u8; 8];
        let read = runtime
            .read_from_session(session, &mut line)
            .expect("live session read");
        assert_eq!(&line[..read], b"pwd\n");
    }

    #[test]
    fn console_readiness_generation_advances_only_when_input_becomes_ready() {
        let mut runtime = SessionRuntime::default();
        let session = 9;
        runtime.create_session(session);
        let initial = runtime.input_readiness_generation();

        assert!(!runtime
            .handle_input_event(session, key_event(30, b'x'))
            .expect("canonical edit is accepted but not readable"));
        assert_eq!(runtime.input_readiness_generation(), initial);
        assert!(runtime
            .handle_input_event(session, key_event(KeyCode::Enter as u32, b'\n'))
            .expect("enter makes the canonical line readable"));
        assert_eq!(runtime.input_readiness_generation(), initial + 1);

        assert!(!runtime
            .handle_input_event(session, key_event(KeyCode::Enter as u32, b'\n'))
            .expect("already-ready input does not republish"));
        assert_eq!(runtime.input_readiness_generation(), initial + 1);

        let mut drained = [0_u8; 8];
        assert_ne!(runtime.read_from_session(session, &mut drained), Some(0));
        assert!(runtime
            .handle_input_event(session, key_event(KeyCode::Enter as u32, b'\n'))
            .expect("a new empty-to-ready transition republishes"));
        assert_eq!(runtime.input_readiness_generation(), initial + 2);
    }

    #[test]
    fn console_close_revokes_readiness_without_resurrecting_the_session() {
        let mut runtime = SessionRuntime::default();
        let session = 10;
        runtime.create_session(session);
        let generation = runtime.input_readiness_generation();
        assert_eq!(
            runtime.input_readiness_snapshot(session),
            (false, true, generation)
        );

        runtime.remove_session(session);
        assert_eq!(
            runtime.input_readiness_snapshot(session),
            (false, false, generation + 1)
        );
        assert!(!runtime.sessions.contains_key(&session));

        let mut input = [0_u8; 1];
        assert_eq!(runtime.read_from_session(session, &mut input), None);
        assert_eq!(runtime.write_to_session(session, b"stale"), None);
        assert!(runtime.termios(session).is_none());
        assert!(!runtime.set_termios(session, LinuxTermios::default_console(), false));
        assert_eq!(runtime.pending_input_len(session), None);
        assert_eq!(
            runtime.handle_input_event(session, key_event(30, b'x')),
            Err(libc::ENODEV)
        );
        assert_eq!(
            runtime.input_readiness_snapshot(session),
            (false, false, generation + 1)
        );
        assert!(!runtime.sessions.contains_key(&session));
    }
}
