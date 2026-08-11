use std::collections::{BTreeMap, BTreeSet};
use std::os::fd::OwnedFd;
use std::sync::atomic::AtomicU64;
use std::thread;
use std::time::{Duration, Instant};

use runtime_control::{StartupMode, DEFAULT_RUNTIME_SOCKET_PATH};
// The wire protocol has exactly one definition, and this is the server end of
// it. These used to be a private second copy of every constant and both frame
// structs, which nothing checked against the client's copy.
pub(crate) use runtime_control::protocol::{
    op_carries_program_payload, running_programs_digest, RuntimeRequest, RuntimeResponse,
    LAUNCH_TARGET_NEW_SESSION, MAX_REQUEST_PATH_BYTES, MAX_RUNTIME_PROGRAMS, MAX_RUNTIME_WATCHERS,
    OP_NOTIFY_READY, OP_REQUEST_LAUNCH_PATH, OP_REQUEST_TERMINATE, OP_SNAPSHOT_RUNNING_PROGRAMS,
    OP_WATCH_RUNNING_PROGRAMS, PROTOCOL_VERSION, READY_COMPONENT_UI_SERVER,
    RUNTIME_WATCH_MAX_WAIT_MS, TERMINATE_TARGET_PID, TERMINATE_TARGET_SESSION,
};
use rustos_user_abi::console as console_abi;
use rustos_user_abi::performance::IPC_CONTROL_DRAIN_BUDGET;
use rustos_user_abi::syscall::{
    SYS_RUSTOS_DEBUG_PRINT, SYS_RUSTOS_SCHED_DEMOTE_SELF, TASK_WEIGHT_INTERACTIVE_FLAG,
};

mod catalog;
mod kvm_smp_qualification;
mod session;
mod socket;
mod spawn;
mod util;

pub(crate) const DEFAULT_USER_TASK_WEIGHT_MICROS: u64 = 100;
pub(crate) const MIN_EFFECTIVE_TASK_WEIGHT_MICROS: u64 = 1_000;
/// A runtime launch-catalog entry is data, not a realtime capability. Keep all
/// non-UI launches below the kernel's strict System-class admission point even
/// if a compromised or malformed registry asks for an arbitrarily high weight.
pub(crate) const MAX_UNTRUSTED_TASK_WEIGHT_MICROS: u64 = 1_000;
pub(crate) const UI_SERVER_CATALOG_WEIGHT_MICROS: u64 = 2_000;
pub(crate) const UI_SERVER_TASK_WEIGHT_MICROS: u64 =
    TASK_WEIGHT_INTERACTIVE_FLAG | UI_SERVER_CATALOG_WEIGHT_MICROS;
// Session IPC, child lifecycle, and the AF_UNIX listener do not yet share one
// wait object. Bound their idle observation latency without waking this
// User-class supervisor five hundred times per second at steady state.
pub(crate) const IDLE_POLL_INTERVAL: Duration = Duration::from_millis(10);
pub(crate) const RETRY_BACKOFF: Duration = Duration::from_millis(100);
pub(crate) const MAX_LAUNCH_RETRY_BACKOFF: Duration = Duration::from_secs(5);
// Runtimed also owns the sessiond endpoint. A single session request must not
// be separated from the next already-queued request by catalog, launch, reap,
// and AF_UNIX control work. The bound preserves those owners' progress while
// draining the synchronous session dependency burst that feeds the UI loop.
const SESSION_REQUEST_DRAIN_BUDGET: usize = IPC_CONTROL_DRAIN_BUDGET;
// A DVM-volume `EAGAIN` is an explicit readiness transition, not a failed
// launch. Keep retry traffic below the UI-core and storage-ready paths.
pub(crate) const STORAGE_NOT_READY_RETRY_BACKOFF: Duration = Duration::from_millis(250);
const UI_BOOTSTRAP_RETRY_BACKOFF: Duration = Duration::from_millis(500);
pub(crate) const SERVICE_REQUEST_TIMEOUT: Duration = Duration::from_secs(5);
pub(crate) const MAX_RUNTIME_CLIENTS_PER_TICK: usize = 8;
pub(crate) const MAX_PENDING_RUNTIME_CLIENTS: usize = 16;
pub(crate) const MAX_POLICY_LAUNCH_ATTEMPTS_PER_TICK: usize = 1;
pub(crate) const MAX_EXEC_ARG_COUNT: usize = 32;
pub(crate) const MAX_EXEC_ENV_COUNT: usize = 64;
pub(crate) const MAX_EXEC_TEXT_BYTES: usize = 256;
pub(crate) const SYS_OPENAT: usize = 257;
pub(crate) const AT_FDCWD: isize = -100;
pub(crate) const O_RDWR: usize = 2;
pub(crate) const LINUX_TCGETS: u64 = libc::TCGETS;
pub(crate) const LINUX_TCSETS: u64 = libc::TCSETS;
pub(crate) const LINUX_TCSETSW: u64 = libc::TCSETSW;
pub(crate) const LINUX_TCSETSF: u64 = libc::TCSETSF;
pub(crate) const LINUX_FIONREAD: u64 = libc::FIONREAD;
pub(crate) const CONSOLE_SESSION_STATE_LOADING_IMAGE: u16 =
    console_abi::CONSOLE_SESSION_STATE_LOADING_IMAGE;
pub(crate) const CONSOLE_SESSION_STATE_SPAWNING: u16 = console_abi::CONSOLE_SESSION_STATE_SPAWNING;
pub(crate) const CONSOLE_SESSION_STATE_RUNNING: u16 = console_abi::CONSOLE_SESSION_STATE_RUNNING;
pub(crate) const CONSOLE_PATH: &str = console_abi::CONSOLE_PATH;
pub(crate) const UI_SERVER_DESKTOP_FILE_ID: &str = "uiserver.desktop";
pub(crate) const UI_SERVER_DISPLAY_NAME: &str = "UI Server";
pub(crate) const UI_SERVER_EXEC_PATH: &str = "services/uiserver/uiserver.elf";
pub(crate) const UI_SERVER_BOOTSTRAP_ENV: [&str; 2] =
    ["RUSTOS_UI_PROFILE=0", "RUSTOS_UI_BOOT_TRACE=0"];
pub(crate) static LOADER_ENDPOINT_CACHE: AtomicU64 = AtomicU64::new(0);

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
    /// Set only by the private KVM contract injector. Signed catalog metadata
    /// cannot manufacture qualification authority by copying reserved names.
    pub(crate) private_smp_qualification:
        Option<kvm_smp_qualification::KvmSmpQualificationContract>,
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
    pub(crate) logical_admin: bool,
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
    pub(crate) launch_failure_counts: BTreeMap<String, u32>,
    pub(crate) permanent_launch_failures: BTreeMap<String, i32>,
    pub(crate) launch_entries: Vec<LaunchEntry>,
    pub(crate) programs: BTreeMap<String, ProgramMetadata>,
    pub(crate) ui_ready: bool,
    pub(crate) launch_catalog_loaded: bool,
    pub(crate) launch_catalog_retry_after: Option<Instant>,
    pub(crate) launch_catalog_last_error: Option<i32>,
    pub(crate) qualification_catalog_resolved: bool,
    pub(crate) qualification_catalog_retry_after: Option<Instant>,
    pub(crate) qualification_catalog_last_error: Option<i32>,
    pub(crate) qualification_catalog_failures: u32,
}

impl Default for BrokerState {
    fn default() -> Self {
        Self {
            console_fd: None,
            // Zero is the "no session" handle every caller compares against,
            // so a real session may never be issued it.
            next_session_handle: 1,
            focused_session_handle: 0,
            session_runtime: session::SessionRuntime::default(),
            running: BTreeMap::new(),
            launched_once: BTreeSet::new(),
            retry_after: BTreeMap::new(),
            launch_failure_counts: BTreeMap::new(),
            permanent_launch_failures: BTreeMap::new(),
            launch_entries: Vec::new(),
            programs: BTreeMap::new(),
            ui_ready: false,
            launch_catalog_loaded: false,
            launch_catalog_retry_after: None,
            launch_catalog_last_error: None,
            qualification_catalog_resolved: false,
            qualification_catalog_retry_after: None,
            qualification_catalog_last_error: None,
            qualification_catalog_failures: 0,
        }
    }
}

pub(crate) fn boot_line(message: &str) {
    if option_env!("RUSTOS_LOGGING_BOOT_TRACE_ENABLED") != Some("true") {
        return;
    }
    debug_line(message);
}

// Debugcon bypasses runtimed's own session/console IPC path during bootstrap.
pub(crate) fn debug_line(message: &str) {
    let bytes = message.as_bytes();
    let len = bytes.len().min(1023);
    let mut line = [0_u8; 1024];
    line[..len].copy_from_slice(&bytes[..len]);
    line[len] = b'\n';
    unsafe {
        let _ = libc::syscall(
            SYS_RUSTOS_DEBUG_PRINT as libc::c_long,
            line.as_ptr(),
            len + 1,
        );
    }
}

fn main() {
    spawn::debug_line("runtimed: service start");
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
    spawn::debug_line("runtimed: runtime socket ready");
    boot_line("runtimed: runtime socket ready");
    let session_endpoint = match session::create_session_endpoint() {
        Some(endpoint) => Some(endpoint),
        None => {
            spawn::debug_line("runtimed: fatal session identity publication failed");
            boot_line("runtimed: fatal session identity publication failed");
            return;
        }
    };

    let mut state = BrokerState::default();
    let mut runtime_connections = socket::RuntimeConnections::default();
    let _ = ensure_ui_bootstrap(&mut state);
    loop {
        let mut did_work = false;
        did_work |= drain_session_request_burst(session_endpoint, &mut state) != 0;
        // Immediately after the drain, so a keystroke delivered by that same
        // burst releases the reader parked on it within this pass rather than
        // after another idle interval.
        did_work |= session::service_console_read_waiters(&mut state);
        did_work |= spawn::reap_children(&mut state);
        // Publish the running set to anyone parked on a change before the pass
        // does anything else with it. A launch later in this pass sets
        // `did_work`, which sends the loop straight back here, so a watcher
        // never waits out an idle interval for an edge this pass produced.
        did_work |= socket::service_running_program_watchers(&mut runtime_connections, &state);
        did_work |= ensure_ui_bootstrap(&mut state);
        // The private SMP qualification workload validates scheduler and IPC
        // progress, not display readiness.  Load the signed catalog after the
        // synchronous UI bootstrap attempt even when the display provider
        // later fails; `ensure_policy_launches` still keeps every ordinary
        // service and desktop entry behind the UI-ready policy boundary.
        if policy_catalog_load_due(state.launch_catalog_loaded) {
            did_work |= catalog::load_launch_catalog_into_state(&mut state);
        }
        // The signed ordinary launch catalog is an early-system dependency;
        // the private qualification contract is deliberately DVM-volume
        // state. Reconcile the latter independently so an unavailable storage
        // topology cannot hold the ordinary UI/application catalog hostage,
        // while a requested qualification remains pending until an exact
        // snapshot is visible.
        did_work |= catalog::reconcile_kvm_smp_qualification_into_state(&mut state);
        // Converge signed launch policy before servicing another control
        // client. In particular, a background snapshot client must not delay
        // the first desktop launch after the catalog-ready transition.
        did_work |= socket::ensure_policy_launches(&mut state);
        did_work |= socket::service_listener(&listener, &mut runtime_connections, &mut state);
        if did_work {
            continue;
        }
        // Idle. Wait on the session endpoint rather than on a timer.
        //
        // This loop used to sleep a flat interval here, which meant an arriving
        // request waited out the remainder of that sleep before the broker
        // looked at it. Measured at 8 vCPU it put about 10.9 ms in front of
        // every console round trip, and the shell issues one of those per
        // keystroke.
        //
        // The endpoint is the only source with a wake object, so it is the one
        // worth blocking on; the budget preserves the old visiting interval for
        // child exits, the control socket, and retry deadlines, none of which
        // can wake anything. Without an endpoint there is nothing to block on,
        // so that case keeps the timer.
        let idle_delay =
            spawn::next_idle_delay(&state, runtime_connections.earliest_watch_deadline());
        if session_endpoint.is_some() {
            session::service_session_endpoint(session_endpoint, &mut state, Some(idle_delay));
        } else {
            thread::sleep(idle_delay);
        }
    }
}

fn policy_catalog_load_due(launch_catalog_loaded: bool) -> bool {
    !launch_catalog_loaded
}

fn drain_session_request_burst(endpoint: Option<u64>, state: &mut BrokerState) -> usize {
    drain_bounded_requests(SESSION_REQUEST_DRAIN_BUDGET, || {
        session::service_session_endpoint(endpoint, state, None)
    })
}

fn drain_bounded_requests(mut remaining: usize, mut serve_one: impl FnMut() -> bool) -> usize {
    let mut served = 0;
    while remaining != 0 && serve_one() {
        served += 1;
        remaining -= 1;
    }
    served
}

fn ensure_ui_bootstrap(state: &mut BrokerState) -> bool {
    if state.ui_ready
        || state
            .running
            .values()
            .any(|process| process.exec == UI_SERVER_EXEC_PATH)
        || state
            .permanent_launch_failures
            .contains_key(UI_SERVER_DESKTOP_FILE_ID)
        || state
            .retry_after
            .get(UI_SERVER_DESKTOP_FILE_ID)
            .is_some_and(|deadline| Instant::now() < *deadline)
    {
        return false;
    }
    spawn::debug_line("runtimed: bootstrap ui begin");
    boot_line("runtimed: bootstrap ui begin");
    match session::bootstrap_ui_server(state) {
        Ok(()) => {
            spawn::debug_line("runtimed: bootstrap ui done");
            boot_line("runtimed: bootstrap ui done");
            require_post_ui_user_class();
            true
        }
        Err(err) => {
            if spawn::is_permanent_launch_failure(err) {
                state
                    .permanent_launch_failures
                    .insert(String::from(UI_SERVER_DESKTOP_FILE_ID), err);
            } else {
                state.retry_after.insert(
                    String::from(UI_SERVER_DESKTOP_FILE_ID),
                    Instant::now() + UI_BOOTSTRAP_RETRY_BACKOFF,
                );
            }
            observability_client::error!(
                "runtimed",
                service,
                "bootstrap {} failed: errno={err}; retry={}",
                UI_SERVER_EXEC_PATH,
                !spawn::is_permanent_launch_failure(err)
            );
            true
        }
    }
}

fn require_post_ui_user_class() {
    let status = unsafe { libc::syscall(SYS_RUSTOS_SCHED_DEMOTE_SELF as libc::c_long) as i64 };
    if status == 0 {
        spawn::debug_line("runtimed: post-ui scheduling class=user");
        return;
    }
    spawn::debug_line("runtimed: fatal post-ui scheduling demotion failed");
    std::process::exit(134);
}

#[cfg(test)]
mod tests {
    use super::{
        drain_bounded_requests, policy_catalog_load_due, IDLE_POLL_INTERVAL,
        IPC_CONTROL_DRAIN_BUDGET, SESSION_REQUEST_DRAIN_BUDGET,
    };

    #[test]
    fn policy_catalog_load_is_not_gated_by_ui_readiness() {
        assert!(policy_catalog_load_due(false));
        assert!(!policy_catalog_load_due(true));

        let source = include_str!("main.rs");
        let production_gate = concat!(
            "if policy_catalog_load_due",
            "(state.launch_catalog_loaded) {"
        );
        let forbidden_ui_gate = concat!(
            "if state.ui_ready",
            " && policy_catalog_load_due",
            "(state.launch_catalog_loaded) {"
        );
        assert!(source.contains(production_gate));
        assert!(!source.contains(forbidden_ui_gate));
    }

    #[test]
    fn steady_supervisor_poll_is_bounded_without_two_millisecond_churn() {
        assert_eq!(IDLE_POLL_INTERVAL, std::time::Duration::from_millis(10));
    }

    #[test]
    fn idle_supervisor_waits_on_session_messages_with_a_deadline() {
        let source = include_str!("main.rs");
        assert!(source.contains(
            "session::service_session_endpoint(session_endpoint, &mut state, Some(idle_delay))"
        ));
        assert_eq!(
            source
                .lines()
                .filter(|line| line.trim() == "thread::sleep(idle_delay);")
                .count(),
            1
        );
    }

    #[test]
    fn session_control_drain_services_a_bounded_ready_burst() {
        assert_eq!(SESSION_REQUEST_DRAIN_BUDGET, IPC_CONTROL_DRAIN_BUDGET);
        assert_eq!(SESSION_REQUEST_DRAIN_BUDGET, 32);
        let mut ready = SESSION_REQUEST_DRAIN_BUDGET + 8;
        let served = drain_bounded_requests(SESSION_REQUEST_DRAIN_BUDGET, || {
            if ready == 0 {
                return false;
            }
            ready -= 1;
            true
        });
        assert_eq!(served, SESSION_REQUEST_DRAIN_BUDGET);
        assert_eq!(ready, 8);

        let served = drain_bounded_requests(SESSION_REQUEST_DRAIN_BUDGET, || {
            if ready == 0 {
                return false;
            }
            ready -= 1;
            true
        });
        assert_eq!(served, 8);
        assert_eq!(ready, 0);
    }
}
