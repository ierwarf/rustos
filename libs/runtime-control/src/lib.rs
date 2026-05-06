use std::fs;
use std::io::{Read, Write};
use std::mem::size_of;
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::sync::OnceLock;

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
const DESKTOP_FILE_ID_CAPACITY: usize = 48;
const RUNNING_PROGRAM_NAME_CAPACITY: usize = 48;
const PROGRAM_PATH_CAPACITY: usize = 64;
const MAX_RUNTIME_PROGRAMS: usize = 64;
const DEFAULT_WEIGHT_MICROS: u64 = 50;

pub const DEFAULT_RUNTIME_SOCKET_PATH: &str = "/run/runtimed.sock";
pub const DEFAULT_APPLICATIONS_DIR: &str = "/usr/share/applications";
pub const DEFAULT_AUTOSTART_DIR: &str = "/etc/xdg/autostart";
pub const DEFAULT_STARTUP_REGISTRY_PATH: &str = "/system/registry/system/startup-programs.tsv";
pub const DEFAULT_DESKTOP_REGISTRY_PATH: &str = "/system/registry/system/desktop-programs.tsv";
pub const DEFAULT_RUNTIME_LAUNCH_REGISTRY_PATH: &str =
    "/system/registry/system/runtime-launch-programs.tsv";

static STARTUP_REGISTRY_CACHE: OnceLock<Option<Vec<StartupEntry>>> = OnceLock::new();
static DESKTOP_REGISTRY_CACHE: OnceLock<Option<Vec<DesktopProgramEntry>>> = OnceLock::new();

#[repr(C)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RuntimeRunningProgram {
    pub pid: u64,
    pub program_id: u32,
    reserved: u32,
    pub session_handle: u64,
    pub desktop_file_id: [u8; DESKTOP_FILE_ID_CAPACITY],
    pub display_name: [u8; RUNNING_PROGRAM_NAME_CAPACITY],
    pub exec_path: [u8; PROGRAM_PATH_CAPACITY],
}

impl Default for RuntimeRunningProgram {
    fn default() -> Self {
        Self {
            pid: 0,
            program_id: 0,
            reserved: 0,
            session_handle: 0,
            desktop_file_id: [0; DESKTOP_FILE_ID_CAPACITY],
            display_name: [0; RUNNING_PROGRAM_NAME_CAPACITY],
            exec_path: [0; PROGRAM_PATH_CAPACITY],
        }
    }
}

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
            version: PROTOCOL_VERSION,
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
        if response.count == 0 {
            return Ok(Vec::new());
        }

        let count = usize::try_from(response.count)
            .unwrap_or(0)
            .min(MAX_RUNTIME_PROGRAMS);
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
        let _ = self.exchange(&request)?;
        Ok(())
    }

    fn exchange(&self, request: &RuntimeRequest) -> Result<(RuntimeResponse, Vec<u8>), i32> {
        let mut stream = UnixStream::connect(&self.socket_path).map_err(io_errno)?;
        stream.write_all(as_bytes(request)).map_err(io_errno)?;

        let mut response = RuntimeResponse::default();
        stream
            .read_exact(as_bytes_mut(&mut response))
            .map_err(io_errno)?;
        if response.version != PROTOCOL_VERSION {
            return Err(libc::EPROTO);
        }
        if response.status != 0 {
            return Err(-response.status);
        }

        let payload_len = match response.op {
            OP_SNAPSHOT_RUNNING_PROGRAMS => {
                let count = usize::try_from(response.count).map_err(|_| libc::EOVERFLOW)?;
                if count > MAX_RUNTIME_PROGRAMS {
                    return Err(libc::EOVERFLOW);
                }
                count
                    .checked_mul(size_of::<RuntimeRunningProgram>())
                    .ok_or(libc::EOVERFLOW)?
            }
            _ if response.count == 0 => 0,
            _ => return Err(libc::EPROTO),
        };
        let mut payload = vec![0_u8; payload_len];
        if payload_len != 0 {
            stream.read_exact(&mut payload).map_err(io_errno)?;
        }
        Ok((response, payload))
    }
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
        if let Some(entries) = cached_startup_registry_entries() {
            return Ok(entries);
        }
        if path == DEFAULT_STARTUP_REGISTRY_PATH {
            return Err(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                "startup registry unavailable",
            ));
        }
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
        if let Some(entries) = cached_desktop_registry_entries() {
            return Ok(entries);
        }
    }
    load_desktop_entries(path, DesktopLoadMode::Applications)
}

pub fn load_autostart_program_entries(
    path: &str,
) -> Result<Vec<DesktopProgramEntry>, std::io::Error> {
    if path == DEFAULT_AUTOSTART_DIR {
        if let Some(entries) = cached_desktop_registry_entries() {
            return Ok(entries
                .into_iter()
                .filter(|entry| entry.autostart_enabled && !entry.hidden && !entry.no_display)
                .collect());
        }
    }
    load_desktop_entries(path, DesktopLoadMode::Autostart)
}

pub fn load_runtime_launch_program_entries(
    path: &str,
) -> Result<Vec<DesktopProgramEntry>, std::io::Error> {
    let mut entries = load_desktop_registry_entries(path)?;
    entries.retain(|entry| {
        entry.startup != StartupMode::None || (entry.autostart_enabled && !entry.hidden)
    });
    Ok(entries)
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
        let contents = fs::read_to_string(&path)?;
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
    let contents = fs::read_to_string(path)?;
    let mut entries = Vec::new();

    for line in contents.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Some(entry) = parse_desktop_registry_entry(line) {
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

fn load_startup_registry_entries(path: &str) -> Result<Vec<StartupEntry>, std::io::Error> {
    let contents = fs::read_to_string(path)?;
    let mut entries = Vec::new();

    for line in contents.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Some(entry) = parse_startup_registry_entry(line) {
            entries.push(entry);
        }
    }

    entries.sort_by(|lhs, rhs| {
        lhs.mode
            .cmp(&rhs.mode)
            .then_with(|| lhs.desktop_file_id.cmp(&rhs.desktop_file_id))
            .then_with(|| lhs.display_name.cmp(&rhs.display_name))
    });
    Ok(entries)
}

fn cached_startup_registry_entries() -> Option<Vec<StartupEntry>> {
    STARTUP_REGISTRY_CACHE
        .get_or_init(|| load_startup_registry_entries(DEFAULT_STARTUP_REGISTRY_PATH).ok())
        .clone()
}

fn cached_desktop_registry_entries() -> Option<Vec<DesktopProgramEntry>> {
    DESKTOP_REGISTRY_CACHE
        .get_or_init(|| load_desktop_registry_entries(DEFAULT_DESKTOP_REGISTRY_PATH).ok())
        .clone()
}

fn parse_startup_registry_entry(line: &str) -> Option<StartupEntry> {
    let exec = registry_field(line, "exec")?.to_string();
    let desktop_file_id = registry_field(line, "desktop_id")
        .map(str::to_string)
        .unwrap_or_else(|| fallback_startup_desktop_file_id(exec.as_str()));
    let package_id = registry_field(line, "package_id")
        .map(str::to_string)
        .unwrap_or_else(|| fallback_package_id(&desktop_file_id, exec.as_str()));
    let display_name = registry_field(line, "display_name")?.to_string();
    let mode = parse_startup_mode(registry_field(line, "mode")?)?;
    let runtime_deps = registry_field(line, "deps")
        .map(parse_registry_deps)
        .unwrap_or_default();

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
    let package_id = registry_field(line, "package_id")
        .map(str::to_string)
        .unwrap_or_else(|| {
            fallback_package_id(&desktop_file_id, registry_field(line, "exec").unwrap_or(""))
        });
    let display_name = registry_field(line, "display_name")?.to_string();
    let exec = registry_field(line, "exec")?.to_string();
    let startup = parse_startup_mode(registry_field(line, "startup")?)?;
    let weight_micros = registry_field(line, "weight")
        .and_then(|value| value.parse().ok())
        .unwrap_or(DEFAULT_WEIGHT_MICROS);
    let logical_admin = registry_field(line, "logical_admin")
        .map(parse_registry_bool)
        .unwrap_or(false);
    let console_hosted = registry_field(line, "console_hosted")
        .map(parse_registry_bool)
        .unwrap_or(false);
    let terminal = registry_field(line, "terminal")
        .map(parse_registry_bool)
        .unwrap_or(console_hosted);
    let args = registry_field(line, "args")
        .map(parse_registry_list)
        .unwrap_or_default();
    let env = registry_field(line, "env")
        .map(parse_registry_list)
        .unwrap_or_default();
    let runtime_deps = registry_field(line, "deps")
        .map(parse_registry_deps)
        .unwrap_or_default();
    let autostart_enabled = registry_field(line, "autostart_enabled")
        .map(parse_registry_bool)
        .unwrap_or(false);
    let hidden = registry_field(line, "hidden")
        .map(parse_registry_bool)
        .unwrap_or(false);
    let no_display = registry_field(line, "no_display")
        .map(parse_registry_bool)
        .unwrap_or(false);

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

fn parse_registry_bool(value: &str) -> bool {
    matches!(value, "1" | "true" | "True")
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

fn fallback_startup_desktop_file_id(exec: &str) -> String {
    Path::new(exec)
        .file_stem()
        .and_then(|value| value.to_str())
        .filter(|value| !value.is_empty())
        .map(|value| format!("{value}.desktop"))
        .unwrap_or_else(|| exec.to_string())
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

fn as_bytes<T>(value: &T) -> &[u8] {
    unsafe { std::slice::from_raw_parts((value as *const T).cast::<u8>(), size_of::<T>()) }
}

fn as_bytes_mut<T>(value: &mut T) -> &mut [u8] {
    unsafe { std::slice::from_raw_parts_mut((value as *mut T).cast::<u8>(), size_of::<T>()) }
}

#[cfg(test)]
mod tests {
    use super::{
        parse_desktop_program_entry, parse_exec_tokens, parse_startup_registry_entry,
        DesktopLoadMode, StartupMode,
    };
    use std::path::Path;

    #[test]
    fn parse_exec_tokens_handles_quotes_and_placeholders() {
        let tokens = parse_exec_tokens("\"apps/shell/shell.elf\" --login %f");
        assert_eq!(tokens, vec!["apps/shell/shell.elf", "--login"]);
    }

    #[test]
    fn parse_desktop_program_entry_reads_generated_metadata() {
        let entry = parse_desktop_program_entry(
            "[Desktop Entry]\nType=Application\nName=WayClick\nExec=apps/wayclick/wayclick.elf\nTerminal=false\nOnlyShowIn=RustOS;\nX-RustOS-PackageId=wayclick\nX-RustOS-Startup=none\nX-RustOS-Deps=runtimed,sessiond\nX-RustOS-WeightMicros=50\nX-RustOS-LogicalAdmin=false\nX-RustOS-ConsoleHosted=false\nX-RustOS-Argv=apps/wayclick/wayclick.elf|--test\nX-RustOS-Env=A=1|B=2\n",
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
    fn parse_registry_entries_default_to_empty_deps() {
        let entry = parse_startup_registry_entry(
            "desktop_id=runtimed.desktop\tmode=init\tdisplay_name=runtimed\texec=services/runtimed/runtimed.elf\tlaunch=none",
        )
        .expect("startup entry");
        assert_eq!(entry.package_id, "runtimed");
        assert!(entry.runtime_deps.is_empty());
    }
}
