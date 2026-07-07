use std::collections::{BTreeMap, BTreeSet};
use std::io::Write;
use std::os::fd::OwnedFd;
use std::sync::atomic::AtomicU64;
use std::thread;
use std::time::{Duration, Instant};

use runtime_control::{StartupMode, DEFAULT_RUNTIME_SOCKET_PATH};
use rustos_user_abi::console::{self as console_abi};

mod catalog;
mod session;
mod socket;
mod spawn;
mod util;

pub(crate) const DEFAULT_USER_TASK_WEIGHT_MICROS: u64 = 100;
pub(crate) const MIN_EFFECTIVE_TASK_WEIGHT_MICROS: u64 = 1_000;
pub(crate) const UI_SERVER_TASK_WEIGHT_MICROS: u64 = 2_000;
pub(crate) const IDLE_POLL_INTERVAL: Duration = Duration::from_millis(2);
pub(crate) const RETRY_BACKOFF: Duration = Duration::from_millis(100);
pub(crate) const SERVICE_REQUEST_TIMEOUT: Duration = Duration::from_secs(5);
pub(crate) const MAX_RUNTIME_CLIENTS_PER_TICK: usize = 8;
pub(crate) const MAX_POLICY_LAUNCH_ATTEMPTS_PER_TICK: usize = 1;
pub(crate) const PROTOCOL_VERSION: u16 = 1;
pub(crate) const OP_SNAPSHOT_RUNNING_PROGRAMS: u16 = 1;
pub(crate) const OP_REQUEST_LAUNCH_PATH: u16 = 2;
pub(crate) const OP_REQUEST_TERMINATE: u16 = 3;
pub(crate) const OP_NOTIFY_READY: u16 = 4;
pub(crate) const LAUNCH_TARGET_NEW_SESSION: u16 = 2;
pub(crate) const TERMINATE_TARGET_SESSION: u16 = 1;
pub(crate) const TERMINATE_TARGET_PID: u16 = 2;
pub(crate) const READY_COMPONENT_UI_SERVER: u16 = 1;
pub(crate) const MAX_REQUEST_PATH_BYTES: usize = 128;
pub(crate) const MAX_RUNTIME_PROGRAMS: usize = 64;
pub(crate) const MAX_EXEC_ARG_COUNT: usize = 32;
pub(crate) const MAX_EXEC_ENV_COUNT: usize = 64;
pub(crate) const MAX_EXEC_TEXT_BYTES: usize = 256;
pub(crate) const SYS_IOCTL: usize = 16;
pub(crate) const SYS_OPENAT: usize = 257;
pub(crate) const AT_FDCWD: isize = -100;
pub(crate) const O_RDWR: usize = 2;
pub(crate) const LINUX_TCGETS: u64 = libc::TCGETS as u64;
pub(crate) const LINUX_TCSETS: u64 = libc::TCSETS as u64;
pub(crate) const LINUX_TCSETSW: u64 = libc::TCSETSW as u64;
pub(crate) const LINUX_TCSETSF: u64 = libc::TCSETSF as u64;
pub(crate) const LINUX_FIONREAD: u64 = libc::FIONREAD as u64;
pub(crate) const CONSOLE_SESSION_STATE_LOADING_IMAGE: u16 =
    console_abi::CONSOLE_SESSION_STATE_LOADING_IMAGE;
pub(crate) const CONSOLE_SESSION_STATE_SPAWNING: u16 = console_abi::CONSOLE_SESSION_STATE_SPAWNING;
pub(crate) const CONSOLE_SESSION_STATE_RUNNING: u16 = console_abi::CONSOLE_SESSION_STATE_RUNNING;
pub(crate) const CONSOLE_PATH: &str = console_abi::CONSOLE_PATH;
pub(crate) const UI_SERVER_DESKTOP_FILE_ID: &str = "uiserver.desktop";
pub(crate) const UI_SERVER_DISPLAY_NAME: &str = "UI Server";
pub(crate) const UI_SERVER_EXEC_PATH: &str = "services/uiserver/uiserver.elf";
pub(crate) static LOADER_ENDPOINT_CACHE: AtomicU64 = AtomicU64::new(0);
pub(crate) static SESSION_GRAPH_GENERATION: AtomicU64 = AtomicU64::new(1);

#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub(crate) struct RuntimeRequest {
    pub(crate) version: u16,
    pub(crate) op: u16,
    pub(crate) target_kind: u16,
    pub(crate) reserved0: u16,
    pub(crate) text_len: u32,
    pub(crate) target_value: u64,
    pub(crate) text: [u8; MAX_REQUEST_PATH_BYTES],
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
pub(crate) struct RuntimeResponse {
    pub(crate) version: u16,
    pub(crate) op: u16,
    pub(crate) status: i32,
    pub(crate) count: u32,
}

#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd)]
pub(crate) struct LaunchEntry {
    pub(crate) package_id: String,
    pub(crate) desktop_file_id: String,
    pub(crate) display_name: String,
    pub(crate) exec: String,
    pub(crate) runtime_deps: Vec<String>,
    pub(crate) restart: bool,
    pub(crate) weight_micros: u64,
    pub(crate) logical_admin: bool,
    pub(crate) console_hosted: bool,
    pub(crate) args: Vec<String>,
    pub(crate) env: Vec<String>,
}

#[derive(Clone, Debug)]
pub(crate) struct RunningProcess {
    pub(crate) pid: i32,
    pub(crate) package_id: String,
    pub(crate) desktop_file_id: String,
    pub(crate) display_name: String,
    pub(crate) exec: String,
    pub(crate) session_handle: u64,
    pub(crate) restart: bool,
}

#[derive(Clone, Debug)]
pub(crate) struct ProgramMetadata {
    pub(crate) package_id: String,
    pub(crate) desktop_file_id: String,
    pub(crate) display_name: String,
    pub(crate) exec: String,
    pub(crate) runtime_deps: Vec<String>,
    pub(crate) startup: StartupMode,
    pub(crate) weight_micros: u64,
    pub(crate) logical_admin: bool,
    pub(crate) console_hosted: bool,
    pub(crate) args: Vec<String>,
    pub(crate) env: Vec<String>,
}

pub(crate) struct BrokerState {
    pub(crate) console_fd: Option<OwnedFd>,
    pub(crate) next_session_handle: u64,
    pub(crate) focused_session_handle: u64,
    pub(crate) session_runtime: session::SessionRuntime,
    pub(crate) running: BTreeMap<i32, RunningProcess>,
    pub(crate) launched_once: BTreeSet<String>,
    pub(crate) retry_after: BTreeMap<String, Instant>,
    pub(crate) permanent_launch_failures: BTreeMap<String, i32>,
    pub(crate) launch_entries: Vec<LaunchEntry>,
    pub(crate) programs: BTreeMap<String, ProgramMetadata>,
    pub(crate) ui_ready: bool,
    pub(crate) launch_catalog_loaded: bool,
}

pub(crate) fn boot_line(message: &str) {
    if option_env!("RUSTOS_LOGGING_BOOT_TRACE_ENABLED") != Some("true") {
        return;
    }
    let _ = std::io::stderr().write_all(message.as_bytes());
    let _ = std::io::stderr().write_all(b"\n");
}

fn main() {
    spawn::stderr_line("runtimed: service start");
    boot_line("runtimed: service start");
    let listener = match socket::bind_listener(DEFAULT_RUNTIME_SOCKET_PATH) {
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
    spawn::stderr_line("runtimed: runtime socket ready");
    boot_line("runtimed: runtime socket ready");
    let session_endpoint = session::create_session_endpoint();

    let mut state = BrokerState {
        console_fd: None,
        next_session_handle: 1,
        focused_session_handle: 0,
        session_runtime: session::SessionRuntime::default(),
        running: BTreeMap::new(),
        launched_once: BTreeSet::new(),
        retry_after: BTreeMap::new(),
        permanent_launch_failures: BTreeMap::new(),
        launch_entries: Vec::new(),
        programs: BTreeMap::new(),
        ui_ready: false,
        launch_catalog_loaded: false,
    };
    spawn::stderr_line("runtimed: bootstrap ui begin");
    boot_line("runtimed: bootstrap ui begin");
    if let Err(err) = session::bootstrap_ui_server(&mut state) {
        observability_client::error!(
            "runtimed",
            service,
            "bootstrap {} failed: errno={err}",
            UI_SERVER_EXEC_PATH
        );
    } else {
        spawn::stderr_line("runtimed: bootstrap ui done");
        boot_line("runtimed: bootstrap ui done");
    }
    loop {
        let mut did_work = false;
        did_work |= session::service_session_endpoint(session_endpoint, &mut state);
        did_work |= spawn::reap_children(&mut state);
        did_work |= socket::service_listener(&listener, &mut state);
        if state.ui_ready && !state.launch_catalog_loaded {
            did_work |= catalog::load_launch_catalog_into_state(&mut state);
        }
        did_work |= socket::ensure_policy_launches(&mut state);
        if did_work {
            continue;
        }
        thread::sleep(spawn::next_idle_delay(&state));
    }
}
