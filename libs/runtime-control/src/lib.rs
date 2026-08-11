use std::ffi::CString;
use std::fs;
use std::io::{ErrorKind, Read, Write};
use std::mem::size_of;
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::sync::OnceLock;
use std::time::{Duration, Instant};

const DEFAULT_WEIGHT_MICROS: u64 = 100;
pub(crate) const RPC_IO_TIMEOUT: Duration = Duration::from_secs(5);

mod config_snapshot;
pub mod protocol;

pub use config_snapshot::read_bounded_config_snapshot;
use config_snapshot::read_config_snapshot;
pub use protocol::RuntimeRunningProgram;
use protocol::{
    op_carries_program_payload, running_programs_digest, RuntimeRequest, RuntimeResponse,
    LAUNCH_TARGET_NEW_SESSION, MAX_REQUEST_PATH_BYTES, MAX_RUNTIME_PROGRAMS,
    NO_RUNNING_PROGRAMS_DIGEST, OP_NOTIFY_READY, OP_REQUEST_LAUNCH_PATH, OP_REQUEST_TERMINATE,
    OP_SNAPSHOT_RUNNING_PROGRAMS, OP_WATCH_RUNNING_PROGRAMS, PROTOCOL_VERSION,
    READY_COMPONENT_UI_SERVER, RUNTIME_WATCH_MAX_WAIT_MS, TERMINATE_TARGET_PID,
    TERMINATE_TARGET_SESSION,
};

pub const DEFAULT_RUNTIME_SOCKET_PATH: &str = "/run/runtimed.sock";
pub const DEFAULT_APPLICATIONS_DIR: &str = "/usr/share/applications";
pub const DEFAULT_AUTOSTART_DIR: &str = "/etc/xdg/autostart";
pub const DEFAULT_STARTUP_REGISTRY_PATH: &str = "/system/registry/system/startup-programs.tsv";
pub const DEFAULT_DESKTOP_REGISTRY_PATH: &str = "/system/registry/system/desktop-programs.tsv";
pub const DEFAULT_RUNTIME_LAUNCH_REGISTRY_PATH: &str =
    "/system/registry/system/runtime-launch-programs.tsv";
pub const DEFAULT_RUNTIME_ENV_REGISTRY_PATH: &str = "/system/registry/system/runtime-env.tsv";

static STARTUP_REGISTRY_CACHE: OnceLock<Vec<StartupEntry>> = OnceLock::new();
static DESKTOP_REGISTRY_CACHE: OnceLock<Vec<DesktopProgramEntry>> = OnceLock::new();
static RUNTIME_LAUNCH_REGISTRY_CACHE: OnceLock<Vec<DesktopProgramEntry>> = OnceLock::new();
static RUNTIME_ENV_REGISTRY_CACHE: OnceLock<Vec<RuntimeEnvEntry>> = OnceLock::new();

#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd)]
pub enum StartupMode {
    None,
    Init,
    Session,
    Desktop,
}

#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd)]
pub struct StartupEntry {
    pub package_id: String,
    pub mode: StartupMode,
    pub desktop_file_id: String,
    pub display_name: String,
    pub exec: String,
    pub runtime_deps: Vec<String>,
}

#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd)]
pub struct DesktopProgramEntry {
    pub package_id: String,
    pub desktop_file_id: String,
    pub display_name: String,
    pub exec: String,
    pub startup: StartupMode,
    pub terminal: bool,
    pub autostart_enabled: bool,
    pub weight_micros: u64,
    pub logical_admin: bool,
    pub console_hosted: bool,
    pub args: Vec<String>,
    pub env: Vec<String>,
    pub runtime_deps: Vec<String>,
    pub hidden: bool,
    pub no_display: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RuntimeEnvScope {
    Init,
    Runtime,
}

impl RuntimeEnvScope {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Init => "init",
            Self::Runtime => "runtime",
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd)]
struct RuntimeEnvEntry {
    scope: String,
    key: String,
    value: String,
}

pub struct RuntimeClient {
    socket_path: String,
}

impl RuntimeClient {
    pub fn open_default() -> Result<Self, i32> {
        Self::open(DEFAULT_RUNTIME_SOCKET_PATH)
    }

    pub fn open(path: &str) -> Result<Self, i32> {
        Ok(Self {
            socket_path: path.to_string(),
        })
    }

    pub fn snapshot_running_programs(&self) -> Result<Vec<RuntimeRunningProgram>, i32> {
        let request = RuntimeRequest {
            op: OP_SNAPSHOT_RUNNING_PROGRAMS,
            ..RuntimeRequest::default()
        };
        let (response, payload) = self.exchange(&request)?;
        decode_running_programs(&response, &payload)
    }

    /// Ask to be told when the running set stops matching `known_digest`, and
    /// wait up to `wait` for that to happen.
    ///
    /// This is the same reply as [`Self::snapshot_running_programs`] with the
    /// answer withheld while there is nothing new to say, so a caller that only
    /// reacts to changes stops paying a round trip per interval to be told
    /// "still the same". Pass [`RUNNING_PROGRAMS_DIGEST_UNKNOWN`] on the first
    /// call to be answered immediately.
    ///
    /// Returns the running set and the digest to hand back next time. An
    /// unchanged digest means the server re-armed rather than observed a
    /// change, which is a liveness signal, not an error.
    pub fn watch_running_programs(
        &self,
        known_digest: u64,
        wait: Duration,
    ) -> Result<(Vec<RuntimeRunningProgram>, u64), i32> {
        let wait_ms = u16::try_from(wait.as_millis()).unwrap_or(u16::MAX);
        let request = RuntimeRequest {
            op: OP_WATCH_RUNNING_PROGRAMS,
            target_value: known_digest,
            wait_ms: wait_ms.min(RUNTIME_WATCH_MAX_WAIT_MS),
            ..RuntimeRequest::default()
        };
        let (response, payload) = self.exchange_with_deadline(&request, watch_io_timeout(wait))?;
        let programs = decode_running_programs(&response, &payload)?;
        let digest = running_programs_digest(&programs);
        Ok((programs, digest))
    }

    pub fn request_launch_program_new_session(&self, desktop_file_id: &str) -> Result<(), i32> {
        let request = request_with_path(
            OP_REQUEST_LAUNCH_PATH,
            LAUNCH_TARGET_NEW_SESSION,
            0,
            desktop_file_id,
        )?;
        let _ = self.exchange(&request)?;
        Ok(())
    }

    pub fn request_launch_path_new_session(&self, exec_path: &str) -> Result<(), i32> {
        self.request_launch_program_new_session(exec_path)
    }

    pub fn request_terminate_session(&self, session_handle: u64) -> Result<(), i32> {
        let request = RuntimeRequest {
            op: OP_REQUEST_TERMINATE,
            target_kind: TERMINATE_TARGET_SESSION,
            target_value: session_handle,
            ..RuntimeRequest::default()
        };
        let _ = self.exchange(&request)?;
        Ok(())
    }

    pub fn request_terminate_pid(&self, pid: u64) -> Result<(), i32> {
        let request = RuntimeRequest {
            op: OP_REQUEST_TERMINATE,
            target_kind: TERMINATE_TARGET_PID,
            target_value: pid,
            ..RuntimeRequest::default()
        };
        let _ = self.exchange(&request)?;
        Ok(())
    }

    pub fn notify_ui_ready(&self) -> Result<(), i32> {
        let request = RuntimeRequest {
            op: OP_NOTIFY_READY,
            target_kind: READY_COMPONENT_UI_SERVER,
            ..RuntimeRequest::default()
        };
        self.send_oneway(&request)?;
        Ok(())
    }

    fn send_oneway(&self, request: &RuntimeRequest) -> Result<(), i32> {
        let mut stream = connect_nonblocking_unix(&self.socket_path)?;
        write_all_retry(&mut stream, as_bytes(request))
    }

    fn exchange(&self, request: &RuntimeRequest) -> Result<(RuntimeResponse, Vec<u8>), i32> {
        self.exchange_with_deadline(request, RPC_IO_TIMEOUT)
    }

    fn exchange_with_deadline(
        &self,
        request: &RuntimeRequest,
        io_timeout: Duration,
    ) -> Result<(RuntimeResponse, Vec<u8>), i32> {
        let mut stream = connect_nonblocking_unix(&self.socket_path)?;
        write_all_retry_until(&mut stream, as_bytes(request), io_timeout)?;

        let mut response = RuntimeResponse::default();
        read_exact_retry_until(&mut stream, as_bytes_mut(&mut response), io_timeout)?;
        let payload_len = response_payload_len(request, &response)?;
        let mut payload = vec![0_u8; payload_len];
        if payload_len != 0 {
            read_exact_retry_until(&mut stream, &mut payload, io_timeout)?;
        }
        Ok((response, payload))
    }
}

/// The digest to present before any reply has been seen.
pub const RUNNING_PROGRAMS_DIGEST_UNKNOWN: u64 = NO_RUNNING_PROGRAMS_DIGEST;

/// How long a watch may sit on the socket before the caller calls the server
/// dead. A parked reply is expected to take the full requested wait, so the
/// deadline has to leave room for it plus the round trip that carries it.
fn watch_io_timeout(wait: Duration) -> Duration {
    wait.saturating_add(RPC_IO_TIMEOUT)
}

fn decode_running_programs(
    response: &RuntimeResponse,
    payload: &[u8],
) -> Result<Vec<RuntimeRunningProgram>, i32> {
    if response.count == 0 {
        return Ok(Vec::new());
    }

    let count = usize::try_from(response.count).map_err(|_| libc::EOVERFLOW)?;
    if count > MAX_RUNTIME_PROGRAMS {
        return Err(libc::EOVERFLOW);
    }
    let expected = count
        .checked_mul(size_of::<RuntimeRunningProgram>())
        .ok_or(libc::EOVERFLOW)?;
    if payload.len() != expected {
        return Err(libc::EIO);
    }

    let mut programs = vec![RuntimeRunningProgram::default(); count];
    unsafe {
        std::ptr::copy_nonoverlapping(
            payload.as_ptr(),
            programs.as_mut_ptr().cast::<u8>(),
            expected,
        );
    }
    Ok(programs)
}

/// Validate the complete response identity before a caller can treat an RPC as
/// successful.  A peer may return an error response with opcode zero, but a
/// successful response must echo the exact request opcode and its payload
/// shape.  This prevents a stale or cross-request reply from becoming a false
/// launch/terminate success.
fn response_payload_len(
    request: &RuntimeRequest,
    response: &RuntimeResponse,
) -> Result<usize, i32> {
    if response.version != PROTOCOL_VERSION {
        return Err(libc::EPROTO);
    }
    if response.status < 0 {
        return Err(response.status.checked_neg().ok_or(libc::EPROTO)?);
    }
    if response.status > 0 || response.op != request.op {
        return Err(libc::EPROTO);
    }

    if op_carries_program_payload(request.op) {
        let count = usize::try_from(response.count).map_err(|_| libc::EOVERFLOW)?;
        if count > MAX_RUNTIME_PROGRAMS {
            return Err(libc::EOVERFLOW);
        }
        return count
            .checked_mul(size_of::<RuntimeRunningProgram>())
            .ok_or(libc::EOVERFLOW);
    }

    match request.op {
        OP_REQUEST_LAUNCH_PATH | OP_REQUEST_TERMINATE | OP_NOTIFY_READY if response.count == 0 => {
            Ok(0)
        }
        _ => Err(libc::EPROTO),
    }
}

fn connect_nonblocking_unix(path: &str) -> Result<UnixStream, i32> {
    let fd = unsafe {
        libc::socket(
            libc::AF_UNIX,
            libc::SOCK_STREAM | libc::SOCK_NONBLOCK | libc::SOCK_CLOEXEC,
            0,
        )
    };
    if fd < 0 {
        return Err(last_errno());
    }

    let path = CString::new(path).map_err(|_| libc::EINVAL)?;
    let mut addr = unsafe { std::mem::zeroed::<libc::sockaddr_un>() };
    addr.sun_family = libc::AF_UNIX as libc::sa_family_t;
    let path_bytes = path.as_bytes_with_nul();
    if path_bytes.len() > addr.sun_path.len() {
        let _ = unsafe { libc::close(fd) };
        return Err(libc::ENAMETOOLONG);
    }
    for (index, byte) in path_bytes.iter().enumerate() {
        addr.sun_path[index] = *byte as libc::c_char;
    }

    let rc = unsafe {
        libc::connect(
            fd,
            (&addr as *const libc::sockaddr_un).cast::<libc::sockaddr>(),
            size_of::<libc::sockaddr_un>() as libc::socklen_t,
        )
    };
    if rc < 0 {
        let err = last_errno();
        let _ = unsafe { libc::close(fd) };
        return Err(err);
    }

    Ok(unsafe { UnixStream::from_raw_fd(fd) })
}

/// Wait until the socket is ready in `events`, or the deadline passes.
///
/// # Why this is a poll and not a yield
/// The stream is deliberately non-blocking so a dead runtimed cannot hang the
/// caller at connect. That made every short read or write spin
/// `thread::yield_now()` until the peer caught up - up to `RPC_IO_TIMEOUT` of a
/// core, burned by uiserver and sessiond on *every* runtimed RPC, competing for
/// CPU with the very loop they are waiting for. The descriptor is pollable, so
/// waiting on it directly costs nothing while idle and wakes on the first byte.
///
/// The caller's deadline is passed in rather than recomputed here, so the total
/// bound stays `RPC_IO_TIMEOUT` for the whole transfer instead of resetting on
/// every partial chunk.
fn wait_for_socket_ready(stream: &UnixStream, deadline: Instant, events: i16) -> Result<(), i32> {
    let now = Instant::now();
    if now >= deadline {
        return Err(libc::ETIMEDOUT);
    }
    let timeout_ms = i32::try_from((deadline - now).as_millis())
        .unwrap_or(i32::MAX)
        .max(1);
    let mut poll_fd = libc::pollfd {
        fd: stream.as_raw_fd(),
        events,
        revents: 0,
    };
    let ready = unsafe { libc::poll(&mut poll_fd, 1, timeout_ms) };
    if ready < 0 {
        let err = last_errno();
        // A signal is not a transfer failure. Returning lets the caller retry
        // the operation; the shared deadline still bounds the loop.
        if err == libc::EINTR {
            return Ok(());
        }
        return Err(err);
    }
    if ready == 0 {
        return Err(libc::ETIMEDOUT);
    }
    Ok(())
}

fn write_all_retry_until(
    stream: &mut UnixStream,
    mut bytes: &[u8],
    io_timeout: Duration,
) -> Result<(), i32> {
    let deadline = Instant::now() + io_timeout;
    while !bytes.is_empty() {
        match stream.write(bytes) {
            Ok(0) => return Err(libc::EPIPE),
            Ok(written) => bytes = &bytes[written..],
            Err(err) if err.kind() == ErrorKind::WouldBlock => {
                wait_for_socket_ready(stream, deadline, libc::POLLOUT)?;
            }
            Err(err) => return Err(io_errno(err)),
        }
    }
    Ok(())
}

fn write_all_retry(stream: &mut UnixStream, bytes: &[u8]) -> Result<(), i32> {
    write_all_retry_until(stream, bytes, RPC_IO_TIMEOUT)
}

fn read_exact_retry_until(
    stream: &mut UnixStream,
    mut bytes: &mut [u8],
    io_timeout: Duration,
) -> Result<(), i32> {
    let deadline = Instant::now() + io_timeout;
    while !bytes.is_empty() {
        match stream.read(bytes) {
            Ok(0) => return Err(libc::EPIPE),
            Ok(read) => {
                let remaining = bytes;
                bytes = &mut remaining[read..];
            }
            Err(err) if err.kind() == ErrorKind::WouldBlock => {
                wait_for_socket_ready(stream, deadline, libc::POLLIN)?;
            }
            Err(err) => return Err(io_errno(err)),
        }
    }
    Ok(())
}

pub fn decode_c_string(bytes: &[u8]) -> String {
    let end = bytes
        .iter()
        .position(|byte| *byte == 0)
        .unwrap_or(bytes.len());
    String::from_utf8_lossy(&bytes[..end]).into_owned()
}

pub fn load_startup_entries(path: &str) -> Result<Vec<StartupEntry>, std::io::Error> {
    if matches!(
        path,
        DEFAULT_APPLICATIONS_DIR | DEFAULT_STARTUP_REGISTRY_PATH
    ) {
        return cached_startup_registry_entries();
    }

    let mut entries = load_desktop_program_entries(path)?
        .into_iter()
        .filter(|entry| entry.startup != StartupMode::None)
        .map(|entry| StartupEntry {
            package_id: entry.package_id,
            mode: entry.startup,
            desktop_file_id: entry.desktop_file_id,
            display_name: entry.display_name,
            exec: entry.exec,
            runtime_deps: entry.runtime_deps,
        })
        .collect::<Vec<_>>();
    entries.sort_by(|lhs, rhs| {
        lhs.mode
            .cmp(&rhs.mode)
            .then_with(|| lhs.desktop_file_id.cmp(&rhs.desktop_file_id))
            .then_with(|| lhs.display_name.cmp(&rhs.display_name))
    });
    Ok(entries)
}

pub fn load_desktop_program_entries(
    path: &str,
) -> Result<Vec<DesktopProgramEntry>, std::io::Error> {
    if path == DEFAULT_APPLICATIONS_DIR {
        return cached_desktop_registry_entries();
    }
    load_desktop_entries(path, DesktopLoadMode::Applications)
}

pub fn load_autostart_program_entries(
    path: &str,
) -> Result<Vec<DesktopProgramEntry>, std::io::Error> {
    if path == DEFAULT_AUTOSTART_DIR {
        return Ok(cached_desktop_registry_entries()?
            .into_iter()
            .filter(|entry| entry.autostart_enabled && !entry.hidden && !entry.no_display)
            .collect());
    }
    load_desktop_entries(path, DesktopLoadMode::Autostart)
}

pub fn load_runtime_launch_program_entries(
    path: &str,
) -> Result<Vec<DesktopProgramEntry>, std::io::Error> {
    let mut entries = if path == DEFAULT_RUNTIME_LAUNCH_REGISTRY_PATH {
        cached_runtime_launch_registry_entries()?
    } else {
        load_desktop_registry_entries(path)?
    };
    entries.retain(|entry| {
        entry.startup != StartupMode::None || (entry.autostart_enabled && !entry.hidden)
    });
    Ok(entries)
}

pub fn load_runtime_default_env(
    path: &str,
    scope: RuntimeEnvScope,
) -> Result<Vec<String>, std::io::Error> {
    let entries = if path == DEFAULT_RUNTIME_ENV_REGISTRY_PATH {
        cached_runtime_env_registry_entries()?
    } else {
        load_runtime_env_registry_entries(path)?
    };
    Ok(entries
        .into_iter()
        .filter(|entry| entry.scope == scope.as_str())
        .map(|entry| format!("{}={}", entry.key, entry.value))
        .collect())
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum DesktopLoadMode {
    Applications,
    Autostart,
}

fn load_desktop_entries(
    path: &str,
    mode: DesktopLoadMode,
) -> Result<Vec<DesktopProgramEntry>, std::io::Error> {
    let read_dir = fs::read_dir(path)?;
    let mut paths = read_dir
        .filter_map(|entry| entry.ok().map(|entry| entry.path()))
        .filter(|path| path.extension().and_then(|ext| ext.to_str()) == Some("desktop"))
        .collect::<Vec<_>>();
    paths.sort();

    let mut entries = Vec::new();
    for path in paths {
        let contents = read_config_snapshot(&path)?;
        if let Some(entry) = parse_desktop_program_entry(&contents, &path, mode) {
            entries.push(entry);
        }
    }

    entries.sort_by(|lhs, rhs| {
        lhs.desktop_file_id
            .cmp(&rhs.desktop_file_id)
            .then_with(|| lhs.display_name.cmp(&rhs.display_name))
    });
    Ok(entries)
}

fn load_desktop_registry_entries(path: &str) -> Result<Vec<DesktopProgramEntry>, std::io::Error> {
    let contents = read_config_snapshot(path)?;
    let mut entries = Vec::new();

    for (line_number, line) in contents.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let entry = parse_desktop_registry_entry(line).ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("invalid desktop registry entry at line {}", line_number + 1),
            )
        })?;
        entries.push(entry);
    }

    entries.sort_by(|lhs, rhs| {
        lhs.desktop_file_id
            .cmp(&rhs.desktop_file_id)
            .then_with(|| lhs.display_name.cmp(&rhs.display_name))
    });
    Ok(entries)
}

fn load_startup_registry_entries(path: &str) -> Result<Vec<StartupEntry>, std::io::Error> {
    let contents = read_config_snapshot(path)?;
    let mut entries = Vec::new();

    for (line_number, line) in contents.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let entry = parse_startup_registry_entry(line).ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("invalid startup registry entry at line {}", line_number + 1),
            )
        })?;
        entries.push(entry);
    }

    entries.sort_by(|lhs, rhs| {
        lhs.mode
            .cmp(&rhs.mode)
            .then_with(|| lhs.desktop_file_id.cmp(&rhs.desktop_file_id))
            .then_with(|| lhs.display_name.cmp(&rhs.display_name))
    });
    Ok(entries)
}

fn cached_startup_registry_entries() -> Result<Vec<StartupEntry>, std::io::Error> {
    if let Some(entries) = STARTUP_REGISTRY_CACHE.get() {
        return Ok(entries.clone());
    }
    let entries = load_startup_registry_entries(DEFAULT_STARTUP_REGISTRY_PATH)?;
    let _ = STARTUP_REGISTRY_CACHE.set(entries);
    STARTUP_REGISTRY_CACHE
        .get()
        .cloned()
        .ok_or_else(|| std::io::Error::other("startup registry cache initialization failed"))
}

fn cached_desktop_registry_entries() -> Result<Vec<DesktopProgramEntry>, std::io::Error> {
    if let Some(entries) = DESKTOP_REGISTRY_CACHE.get() {
        return Ok(entries.clone());
    }
    let entries = load_desktop_registry_entries(DEFAULT_DESKTOP_REGISTRY_PATH)?;
    let _ = DESKTOP_REGISTRY_CACHE.set(entries);
    DESKTOP_REGISTRY_CACHE
        .get()
        .cloned()
        .ok_or_else(|| std::io::Error::other("desktop registry cache initialization failed"))
}

fn cached_runtime_launch_registry_entries() -> Result<Vec<DesktopProgramEntry>, std::io::Error> {
    if let Some(entries) = RUNTIME_LAUNCH_REGISTRY_CACHE.get() {
        return Ok(entries.clone());
    }
    let entries = load_desktop_registry_entries(DEFAULT_RUNTIME_LAUNCH_REGISTRY_PATH)?;
    let _ = RUNTIME_LAUNCH_REGISTRY_CACHE.set(entries);
    RUNTIME_LAUNCH_REGISTRY_CACHE
        .get()
        .cloned()
        .ok_or_else(|| std::io::Error::other("runtime launch registry cache initialization failed"))
}

fn cached_runtime_env_registry_entries() -> Result<Vec<RuntimeEnvEntry>, std::io::Error> {
    if let Some(entries) = RUNTIME_ENV_REGISTRY_CACHE.get() {
        return Ok(entries.clone());
    }
    let entries = load_runtime_env_registry_entries(DEFAULT_RUNTIME_ENV_REGISTRY_PATH)?;
    let _ = RUNTIME_ENV_REGISTRY_CACHE.set(entries);
    RUNTIME_ENV_REGISTRY_CACHE.get().cloned().ok_or_else(|| {
        std::io::Error::other("runtime environment registry cache initialization failed")
    })
}

fn load_runtime_env_registry_entries(path: &str) -> Result<Vec<RuntimeEnvEntry>, std::io::Error> {
    let contents = read_config_snapshot(path)?;
    let mut entries = Vec::new();

    for (line_number, line) in contents.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let entry = parse_runtime_env_registry_entry(line).ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!(
                    "invalid runtime environment registry entry at line {}",
                    line_number + 1
                ),
            )
        })?;
        entries.push(entry);
    }

    entries.sort();
    Ok(entries)
}

fn parse_runtime_env_registry_entry(line: &str) -> Option<RuntimeEnvEntry> {
    let scope = registry_field(line, "scope")?;
    let key = registry_field(line, "key")?;
    let value = registry_field(line, "value")?;
    if !config_snapshot::valid_env_key(key) || value.as_bytes().contains(&0) {
        return None;
    }
    Some(RuntimeEnvEntry {
        scope: scope.to_string(),
        key: key.to_string(),
        value: value.to_string(),
    })
}

fn parse_startup_registry_entry(line: &str) -> Option<StartupEntry> {
    let exec = registry_field(line, "exec")?.to_string();
    let desktop_file_id = registry_field(line, "desktop_id")?.to_string();
    let package_id = registry_field(line, "package_id")?.to_string();
    let display_name = registry_field(line, "display_name")?.to_string();
    let mode = parse_startup_mode(registry_field(line, "mode")?)?;
    let runtime_deps = parse_registry_deps(registry_field(line, "deps")?);
    if desktop_file_id.is_empty()
        || package_id.is_empty()
        || display_name.is_empty()
        || exec.is_empty()
    {
        return None;
    }

    Some(StartupEntry {
        package_id,
        mode,
        desktop_file_id,
        display_name,
        exec,
        runtime_deps,
    })
}

fn parse_desktop_registry_entry(line: &str) -> Option<DesktopProgramEntry> {
    let desktop_file_id = registry_field(line, "desktop_id")?.to_string();
    let package_id = registry_field(line, "package_id")?.to_string();
    let display_name = registry_field(line, "display_name")?.to_string();
    let exec = registry_field(line, "exec")?.to_string();
    let startup = parse_startup_mode(registry_field(line, "startup")?)?;
    let weight_micros = registry_field(line, "weight")?.parse().ok()?;
    let logical_admin = parse_registry_bool(registry_field(line, "logical_admin")?)?;
    let console_hosted = parse_registry_bool(registry_field(line, "console_hosted")?)?;
    let terminal = parse_registry_bool(registry_field(line, "terminal")?)?;
    let args = parse_registry_list(registry_field(line, "args")?);
    let env = parse_registry_list(registry_field(line, "env")?);
    let runtime_deps = parse_registry_deps(registry_field(line, "deps")?);
    let autostart_enabled = parse_registry_bool(registry_field(line, "autostart_enabled")?)?;
    let hidden = parse_registry_bool(registry_field(line, "hidden")?)?;
    let no_display = parse_registry_bool(registry_field(line, "no_display")?)?;
    if desktop_file_id.is_empty()
        || package_id.is_empty()
        || display_name.is_empty()
        || exec.is_empty()
    {
        return None;
    }

    Some(DesktopProgramEntry {
        package_id,
        desktop_file_id,
        display_name,
        exec,
        startup,
        terminal,
        autostart_enabled,
        weight_micros,
        logical_admin,
        console_hosted,
        args,
        env,
        runtime_deps,
        hidden,
        no_display,
    })
}

fn parse_desktop_program_entry(
    contents: &str,
    path: &Path,
    mode: DesktopLoadMode,
) -> Option<DesktopProgramEntry> {
    let mut in_desktop_entry = false;
    let mut entry_type = None::<&str>;
    let mut hidden = false;
    let mut no_display = false;
    let mut enabled = true;
    let mut only_show_in = None::<&str>;
    let mut not_show_in = None::<&str>;
    let mut display_name = None::<String>;
    let mut exec_tokens = Vec::<String>::new();
    let mut terminal = false;
    let mut startup = StartupMode::None;
    let mut weight_micros = DEFAULT_WEIGHT_MICROS;
    let mut logical_admin = false;
    let mut console_hosted = None::<bool>;
    let mut args = None::<Vec<String>>;
    let mut env = Vec::<String>::new();
    let mut package_id = None::<String>;
    let mut runtime_deps = Vec::<String>::new();

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
            "Name" => display_name = Some(value.to_string()),
            "Exec" => exec_tokens = parse_exec_tokens(value),
            "Terminal" => terminal = parse_desktop_bool(value),
            "Hidden" => hidden = parse_desktop_bool(value),
            "NoDisplay" => no_display = parse_desktop_bool(value),
            "X-GNOME-Autostart-enabled" => enabled = parse_desktop_bool(value),
            "OnlyShowIn" => only_show_in = Some(value),
            "NotShowIn" => not_show_in = Some(value),
            "X-RustOS-Startup" => startup = parse_startup_mode(value)?,
            "X-RustOS-WeightMicros" => {
                weight_micros = value.parse().ok().unwrap_or(DEFAULT_WEIGHT_MICROS)
            }
            "X-RustOS-LogicalAdmin" => logical_admin = parse_desktop_bool(value),
            "X-RustOS-ConsoleHosted" => console_hosted = Some(parse_desktop_bool(value)),
            "X-RustOS-Argv" => args = Some(parse_desktop_list(value)),
            "X-RustOS-Env" => env = parse_desktop_list(value),
            "X-RustOS-PackageId" => package_id = Some(value.to_string()),
            "X-RustOS-Deps" => runtime_deps = parse_desktop_deps(value),
            _ => {}
        }
    }

    if !matches!(entry_type, None | Some("Application")) {
        return None;
    }
    if !desktop_is_visible_to_rustos(only_show_in, not_show_in) {
        return None;
    }
    if mode == DesktopLoadMode::Autostart && (!enabled || hidden || no_display) {
        return None;
    }

    let args = match args {
        Some(args) if !args.is_empty() => args,
        _ => exec_tokens.clone(),
    };
    let exec = args
        .first()
        .cloned()
        .or_else(|| exec_tokens.first().cloned())?;
    let desktop_file_id = path.file_name()?.to_str()?.to_string();
    let package_id = package_id.unwrap_or_else(|| fallback_package_id(&desktop_file_id, &exec));
    let display_name =
        display_name.unwrap_or_else(|| fallback_display_name(&desktop_file_id, &exec));

    Some(DesktopProgramEntry {
        package_id,
        desktop_file_id,
        display_name,
        exec,
        startup,
        terminal,
        autostart_enabled: enabled,
        weight_micros,
        logical_admin,
        console_hosted: console_hosted.unwrap_or(terminal),
        args,
        env,
        runtime_deps,
        hidden,
        no_display,
    })
}

fn desktop_is_visible_to_rustos(only_show_in: Option<&str>, not_show_in: Option<&str>) -> bool {
    if let Some(only_show_in) = only_show_in {
        if !desktop_list_contains(only_show_in, "RustOS") {
            return false;
        }
    }
    if let Some(not_show_in) = not_show_in {
        if desktop_list_contains(not_show_in, "RustOS") {
            return false;
        }
    }
    true
}

fn parse_exec_tokens(value: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut current = String::new();
    let mut quote = None::<char>;
    let mut escaped = false;

    for ch in value.trim().chars() {
        if escaped {
            current.push(ch);
            escaped = false;
            continue;
        }
        match ch {
            '\\' if quote.is_some() => escaped = true,
            '\'' | '"' if quote == Some(ch) => quote = None,
            '\'' | '"' if quote.is_none() => quote = Some(ch),
            ch if ch.is_whitespace() && quote.is_none() => {
                push_exec_token(&mut tokens, &mut current);
            }
            _ => current.push(ch),
        }
    }
    push_exec_token(&mut tokens, &mut current);

    tokens
        .into_iter()
        .filter(|token| !token.starts_with('%'))
        .collect()
}

fn push_exec_token(tokens: &mut Vec<String>, current: &mut String) {
    if current.is_empty() {
        return;
    }
    tokens.push(std::mem::take(current));
}

fn parse_startup_mode(value: &str) -> Option<StartupMode> {
    match value {
        "none" => Some(StartupMode::None),
        "init" => Some(StartupMode::Init),
        "session" => Some(StartupMode::Session),
        "desktop" => Some(StartupMode::Desktop),
        _ => None,
    }
}

fn parse_desktop_bool(value: &str) -> bool {
    matches!(value, "1" | "true" | "True" | "yes" | "Yes")
}

fn parse_desktop_list(value: &str) -> Vec<String> {
    if value.is_empty() {
        return Vec::new();
    }
    value
        .split('|')
        .filter(|item| !item.is_empty())
        .map(str::to_string)
        .collect()
}

fn parse_desktop_deps(value: &str) -> Vec<String> {
    if value.is_empty() {
        return Vec::new();
    }
    value
        .split(',')
        .filter(|item| !item.is_empty())
        .map(str::to_string)
        .collect()
}

fn parse_registry_bool(value: &str) -> Option<bool> {
    match value {
        "0" => Some(false),
        "1" => Some(true),
        _ => None,
    }
}

fn parse_registry_list(value: &str) -> Vec<String> {
    if value.is_empty() {
        return Vec::new();
    }
    value
        .split('|')
        .filter(|item| !item.is_empty())
        .map(str::to_string)
        .collect()
}

fn parse_registry_deps(value: &str) -> Vec<String> {
    parse_desktop_deps(value)
}

fn registry_field<'a>(line: &'a str, key: &str) -> Option<&'a str> {
    line.split('\t').find_map(|token| {
        let (candidate, value) = token.split_once('=')?;
        (candidate == key).then_some(value)
    })
}

fn fallback_package_id(desktop_file_id: &str, exec: &str) -> String {
    Path::new(desktop_file_id)
        .file_stem()
        .and_then(|value| value.to_str())
        .filter(|value| !value.is_empty())
        .or_else(|| {
            Path::new(exec)
                .file_stem()
                .and_then(|value| value.to_str())
                .filter(|value| !value.is_empty())
        })
        .unwrap_or(exec)
        .to_string()
}

fn desktop_list_contains(value: &str, entry: &str) -> bool {
    value
        .split(';')
        .map(str::trim)
        .any(|candidate| !candidate.is_empty() && candidate == entry)
}

fn fallback_display_name(desktop_file_id: &str, exec: &str) -> String {
    Path::new(desktop_file_id)
        .file_stem()
        .and_then(|value| value.to_str())
        .filter(|value| !value.is_empty())
        .unwrap_or(exec)
        .to_string()
}

fn request_with_path(
    op: u16,
    target_kind: u16,
    target_value: u64,
    path: &str,
) -> Result<RuntimeRequest, i32> {
    let bytes = path.as_bytes();
    if bytes.is_empty() {
        return Err(libc::EINVAL);
    }
    if bytes.len() > MAX_REQUEST_PATH_BYTES {
        return Err(libc::ENAMETOOLONG);
    }
    if !bytes
        .iter()
        .all(|byte| matches!(*byte, b' '..=b'~') && *byte != b'\\')
    {
        return Err(libc::EINVAL);
    }
    let mut request = RuntimeRequest {
        op,
        target_kind,
        target_value,
        text_len: bytes.len() as u32,
        ..RuntimeRequest::default()
    };
    request.text[..bytes.len()].copy_from_slice(bytes);
    Ok(request)
}

fn io_errno(err: std::io::Error) -> i32 {
    err.raw_os_error().unwrap_or(libc::EIO)
}

fn last_errno() -> i32 {
    std::io::Error::last_os_error()
        .raw_os_error()
        .unwrap_or(libc::EIO)
}

fn as_bytes<T>(value: &T) -> &[u8] {
    unsafe { std::slice::from_raw_parts((value as *const T).cast::<u8>(), size_of::<T>()) }
}

fn as_bytes_mut<T>(value: &mut T) -> &mut [u8] {
    unsafe { std::slice::from_raw_parts_mut((value as *mut T).cast::<u8>(), size_of::<T>()) }
}

#[cfg(test)]
mod tests {
    use super::{
        parse_desktop_program_entry, parse_desktop_registry_entry, parse_exec_tokens,
        parse_startup_registry_entry, response_payload_len, DesktopLoadMode, RuntimeRequest,
        RuntimeResponse, StartupMode, MAX_RUNTIME_PROGRAMS, OP_REQUEST_LAUNCH_PATH,
        OP_SNAPSHOT_RUNNING_PROGRAMS, PROTOCOL_VERSION,
    };
    use std::path::Path;

    fn runtime_rpc_outcome(result: Result<usize, i32>, request_op: u16) -> (&'static str, usize) {
        match result {
            Ok(bytes) => {
                let count = if request_op == OP_SNAPSHOT_RUNNING_PROGRAMS {
                    bytes / std::mem::size_of::<super::RuntimeRunningProgram>()
                } else {
                    0
                };
                ("success", count)
            }
            Err(errno) if errno == libc::EOVERFLOW => ("overflow", 0),
            Err(errno) if errno == libc::EPROTO => ("protocol", 0),
            Err(_) => ("server-error", 0),
        }
    }

    fn op_name(op: u16) -> &'static str {
        match op {
            super::OP_SNAPSHOT_RUNNING_PROGRAMS => "snapshot",
            super::OP_REQUEST_LAUNCH_PATH => "launch",
            super::OP_REQUEST_TERMINATE => "terminate",
            super::OP_NOTIFY_READY => "ready",
            _ => "unknown",
        }
    }

    #[test]
    fn parse_exec_tokens_handles_quotes_and_placeholders() {
        let tokens = parse_exec_tokens("\"apps/shell/shell.elf\" --login %f");
        assert_eq!(tokens, vec!["apps/shell/shell.elf", "--login"]);
    }

    #[test]
    fn parse_desktop_program_entry_reads_generated_metadata() {
        let entry = parse_desktop_program_entry(
            "[Desktop Entry]\nType=Application\nName=WayClick\nExec=apps/wayclick/wayclick.elf\nTerminal=false\nOnlyShowIn=RustOS;\nX-RustOS-PackageId=wayclick\nX-RustOS-Startup=none\nX-RustOS-Deps=runtimed,sessiond\nX-RustOS-WeightMicros=100\nX-RustOS-LogicalAdmin=false\nX-RustOS-ConsoleHosted=false\nX-RustOS-Argv=apps/wayclick/wayclick.elf|--test\nX-RustOS-Env=A=1|B=2\n",
            Path::new("/usr/share/applications/wayclick.desktop"),
            DesktopLoadMode::Applications,
        )
        .expect("desktop entry");
        assert_eq!(entry.package_id, "wayclick");
        assert_eq!(entry.desktop_file_id, "wayclick.desktop");
        assert_eq!(entry.display_name, "WayClick");
        assert_eq!(entry.exec, "apps/wayclick/wayclick.elf");
        assert_eq!(entry.startup, StartupMode::None);
        assert!(!entry.terminal);
        assert!(!entry.console_hosted);
        assert_eq!(entry.args, vec!["apps/wayclick/wayclick.elf", "--test"]);
        assert_eq!(entry.env, vec!["A=1", "B=2"]);
        assert_eq!(entry.runtime_deps, vec!["runtimed", "sessiond"]);
    }

    #[test]
    fn parse_desktop_program_entry_filters_disabled_autostart_entries() {
        let entry = parse_desktop_program_entry(
            "[Desktop Entry]\nType=Application\nName=UI Server\nExec=services/uiserver/uiserver.elf\nOnlyShowIn=RustOS;\nX-GNOME-Autostart-enabled=false\n",
            Path::new("/etc/xdg/autostart/uiserver.desktop"),
            DesktopLoadMode::Autostart,
        );
        assert!(entry.is_none());
    }

    #[test]
    fn parse_startup_registry_entry_reads_desktop_id() {
        let entry = parse_startup_registry_entry(
            "desktop_id=runtimed.desktop\tpackage_id=runtimed\tmode=init\tdisplay_name=runtimed\texec=services/runtimed/runtimed.elf\tlaunch=none\tdeps=bootd,storaged",
        )
        .expect("startup entry");
        assert_eq!(entry.package_id, "runtimed");
        assert_eq!(entry.desktop_file_id, "runtimed.desktop");
        assert_eq!(entry.mode, StartupMode::Init);
        assert_eq!(entry.exec, "services/runtimed/runtimed.elf");
        assert_eq!(entry.runtime_deps, vec!["bootd", "storaged"]);
    }

    #[test]
    fn parse_startup_registry_entry_rejects_missing_package_id() {
        let entry = parse_startup_registry_entry(
            "desktop_id=runtimed.desktop\tmode=init\tdisplay_name=runtimed\texec=services/runtimed/runtimed.elf\tlaunch=none",
        );
        assert!(entry.is_none());
    }

    #[test]
    fn parse_desktop_registry_entry_rejects_missing_policy_fields() {
        let entry = parse_desktop_registry_entry(
            "desktop_id=uiserver.desktop\tpackage_id=uiserver\tstartup=desktop\tdisplay_name=UI Server\texec=services/uiserver/uiserver.elf\tweight=100",
        );
        assert!(entry.is_none());
    }

    #[test]
    fn successful_response_must_echo_the_request_opcode() {
        let request = RuntimeRequest {
            op: OP_REQUEST_LAUNCH_PATH,
            ..RuntimeRequest::default()
        };
        let response = RuntimeResponse {
            version: PROTOCOL_VERSION,
            op: OP_SNAPSHOT_RUNNING_PROGRAMS,
            status: 0,
            count: 0,
        };
        assert_eq!(response_payload_len(&request, &response), Err(libc::EPROTO));
    }

    #[test]
    fn malformed_status_and_oversized_snapshot_fail_closed() {
        let launch = RuntimeRequest {
            op: OP_REQUEST_LAUNCH_PATH,
            ..RuntimeRequest::default()
        };
        let malformed_status = RuntimeResponse {
            version: PROTOCOL_VERSION,
            op: OP_REQUEST_LAUNCH_PATH,
            status: i32::MIN,
            count: 0,
        };
        assert_eq!(
            response_payload_len(&launch, &malformed_status),
            Err(libc::EPROTO)
        );

        let snapshot = RuntimeRequest {
            op: OP_SNAPSHOT_RUNNING_PROGRAMS,
            ..RuntimeRequest::default()
        };
        let oversized = RuntimeResponse {
            version: PROTOCOL_VERSION,
            op: OP_SNAPSHOT_RUNNING_PROGRAMS,
            status: 0,
            count: (MAX_RUNTIME_PROGRAMS + 1) as u32,
        };
        assert_eq!(
            response_payload_len(&snapshot, &oversized),
            Err(libc::EOVERFLOW)
        );

        let command_payload = RuntimeResponse {
            version: PROTOCOL_VERSION,
            op: OP_REQUEST_LAUNCH_PATH,
            status: 0,
            count: 1,
        };
        assert_eq!(
            response_payload_len(&launch, &command_payload),
            Err(libc::EPROTO)
        );
    }

    #[test]
    fn emit_runtime_control_rpc_formal_trace() {
        let Some(output) = std::env::var_os("RUSTOS_FORMAL_TRACE_OUT") else {
            return;
        };
        let cases = [
            (
                OP_SNAPSHOT_RUNNING_PROGRAMS,
                OP_SNAPSHOT_RUNNING_PROGRAMS,
                0,
                2,
            ),
            (
                OP_SNAPSHOT_RUNNING_PROGRAMS,
                OP_SNAPSHOT_RUNNING_PROGRAMS,
                0,
                65,
            ),
            (OP_REQUEST_LAUNCH_PATH, OP_REQUEST_LAUNCH_PATH, 0, 0),
            (OP_REQUEST_LAUNCH_PATH, OP_REQUEST_LAUNCH_PATH, 0, 1),
            (OP_REQUEST_LAUNCH_PATH, OP_SNAPSHOT_RUNNING_PROGRAMS, 0, 0),
            (OP_REQUEST_LAUNCH_PATH, 0, -libc::EIO, 0),
        ];
        let mut trace = std::fs::File::create(output).expect("create formal trace");
        use std::io::Write as _;
        for (sequence, (request_op, response_op, status, count)) in cases.into_iter().enumerate() {
            let request = RuntimeRequest {
                op: request_op,
                ..RuntimeRequest::default()
            };
            let response = RuntimeResponse {
                version: PROTOCOL_VERSION,
                op: response_op,
                status,
                count,
            };
            let (outcome, payload_count) =
                runtime_rpc_outcome(response_payload_len(&request, &response), request_op);
            let status_kind = if status == 0 { "ok" } else { "server-error" };
            writeln!(
                trace,
                "{{\"schema\":\"rustos-formal-trace-v1\",\"model\":\"runtime-control-rpc/RuntimeControlRpc\",\"sequence\":{},\"action\":\"ReceiveResponse\",\"request_op\":\"{}\",\"response_op\":\"{}\",\"status\":\"{}\",\"version\":\"current\",\"count\":{},\"outcome\":\"{}\",\"payload_count\":{}}}",
                sequence,
                op_name(request_op),
                op_name(response_op),
                status_kind,
                count,
                outcome,
                payload_count,
            )
            .expect("write formal trace");
        }
    }
}

#[cfg(kani)]
mod verification {
    use super::{
        response_payload_len, RuntimeRequest, RuntimeResponse, MAX_RUNTIME_PROGRAMS,
        OP_SNAPSHOT_RUNNING_PROGRAMS, OP_WATCH_RUNNING_PROGRAMS, PROTOCOL_VERSION,
    };

    #[kani::proof]
    fn successful_response_never_crosses_rpc_identity() {
        let request_op: u16 = kani::any();
        let response_op: u16 = kani::any();
        kani::assume(request_op != response_op);
        let request = RuntimeRequest {
            op: request_op,
            ..RuntimeRequest::default()
        };
        let response = RuntimeResponse {
            version: PROTOCOL_VERSION,
            op: response_op,
            status: 0,
            count: kani::any(),
        };
        kani::cover!(request_op != response_op);
        assert_eq!(response_payload_len(&request, &response), Err(libc::EPROTO));
    }

    #[kani::proof]
    fn successful_snapshot_never_accepts_an_unbounded_payload_count() {
        let count: u32 = kani::any();
        kani::assume(count > MAX_RUNTIME_PROGRAMS as u32);
        let request = RuntimeRequest {
            op: OP_SNAPSHOT_RUNNING_PROGRAMS,
            ..RuntimeRequest::default()
        };
        let response = RuntimeResponse {
            version: PROTOCOL_VERSION,
            op: OP_SNAPSHOT_RUNNING_PROGRAMS,
            status: 0,
            count,
        };
        kani::cover!(count > MAX_RUNTIME_PROGRAMS as u32);
        assert_eq!(
            response_payload_len(&request, &response),
            Err(libc::EOVERFLOW)
        );
    }

    /// A watch reply arrives late by design, and lateness must buy it nothing.
    /// It is admitted under the same bound as the snapshot it stands in for.
    #[kani::proof]
    fn a_parked_watch_reply_is_admitted_exactly_like_a_snapshot() {
        let count: u32 = kani::any();
        let watch = RuntimeRequest {
            op: OP_WATCH_RUNNING_PROGRAMS,
            ..RuntimeRequest::default()
        };
        let snapshot = RuntimeRequest {
            op: OP_SNAPSHOT_RUNNING_PROGRAMS,
            ..RuntimeRequest::default()
        };
        let reply = |op: u16| RuntimeResponse {
            version: PROTOCOL_VERSION,
            op,
            status: 0,
            count,
        };
        kani::cover!(count > MAX_RUNTIME_PROGRAMS as u32);
        assert_eq!(
            response_payload_len(&watch, &reply(OP_WATCH_RUNNING_PROGRAMS)),
            response_payload_len(&snapshot, &reply(OP_SNAPSHOT_RUNNING_PROGRAMS))
        );
    }

    #[kani::proof]
    fn malformed_minimum_status_never_overflows_into_success() {
        let request = RuntimeRequest::default();
        let response = RuntimeResponse {
            version: PROTOCOL_VERSION,
            op: request.op,
            status: i32::MIN,
            count: 0,
        };
        kani::cover!(response.status == i32::MIN);
        assert_eq!(response_payload_len(&request, &response), Err(libc::EPROTO));
    }

    #[kani::proof]
    fn successful_command_never_accepts_a_payload() {
        let count: u32 = kani::any();
        kani::assume(count > 0);
        let request = RuntimeRequest {
            op: super::OP_REQUEST_LAUNCH_PATH,
            ..RuntimeRequest::default()
        };
        let response = RuntimeResponse {
            version: PROTOCOL_VERSION,
            op: super::OP_REQUEST_LAUNCH_PATH,
            status: 0,
            count,
        };
        kani::cover!(count > 0);
        assert_eq!(response_payload_len(&request, &response), Err(libc::EPROTO));
    }

    #[kani::proof]
    fn well_formed_server_error_preserves_the_errno() {
        let errno: i32 = kani::any();
        kani::assume(errno > 0);
        let request = RuntimeRequest {
            op: super::OP_REQUEST_LAUNCH_PATH,
            ..RuntimeRequest::default()
        };
        let response = RuntimeResponse {
            version: PROTOCOL_VERSION,
            op: kani::any(),
            status: -errno,
            count: kani::any(),
        };
        kani::cover!(errno > 0);
        assert_eq!(response_payload_len(&request, &response), Err(errno));
    }
}
