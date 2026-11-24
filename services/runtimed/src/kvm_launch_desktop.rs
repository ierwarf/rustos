//! KVM-only one-shot desktop launch (`kvm-smoke --launch-desktop <id>`).
//!
//! Owner: runtimed. Boundary: one private, unsigned registry file on the per-run
//! KVM boot disk (`kvm-launch-desktop-v1.env`), written only by xtask for that
//! option and absent from every product image. Without the file this module
//! changes nothing: the one bounded read answers `NotFound` and the request is
//! settled as "none".
//!
//! Lifecycle: after the UI-ready edge and the signed catalog commit, read the
//! contract once (off the broker loop), resolve its id against the loaded
//! program catalog, and start that program through the same tracked-launch path
//! a catalog launch uses (console session, loader, rootd lease, reaper
//! custody). The outcome is one debugcon record that xtask requires or fails on.
//!
//! Failure: a malformed contract, an id that is not a launchable catalog
//! program, or a permanent launch error is a hard error (`... FAIL ...` record,
//! no retry). Storage not yet readable and transient launch errors retry on a
//! bounded backoff for a bounded number of attempts (see `MAX_LAUNCH_ATTEMPTS`).
//! Forbidden: launching anything but a manual, non-service catalog program;
//! acting on a contract more than once per boot.

use std::time::Instant;

use runtime_control::read_bounded_config_snapshot;

use super::{debug_line, offload::Offload, BrokerState, LaunchEntry};

pub(super) const CONTRACT_PATH: &str = "/system/registry/system/kvm-launch-desktop-v1.env";
/// Must equal `LAUNCH_DESKTOP_CONTRACT_HEADER` in `tools/xtask/src/kvm/launch_desktop.rs`.
const CONTRACT_HEADER: &str = "contract=rustos-kvm-launch-desktop-v1";
/// Must equal `LAUNCH_DESKTOP_ID_MAX_BYTES` in the xtask writer.
const MAX_DESKTOP_ID_BYTES: usize = 64;
const CONTRACT_MAX_BYTES: usize = 160;
/// Transient failures include `EAGAIN` from the application budget while a
/// session harness program (`ipcbench`, which holds the default 12/48 request
/// for its whole run) still runs; the window must outlast it. The backoff caps
/// at `MAX_LAUNCH_RETRY_BACKOFF`, so this bounds the retry to about 2.4 minutes
/// and the run's own timeout ends it sooner.
const MAX_LAUNCH_ATTEMPTS: u32 = 32;
/// Contract reads while the volume is not yet readable (unchanged bound).
const MAX_CONTRACT_READ_ATTEMPTS: u32 = 20;

type ContractLoad = Result<Option<String>, i32>;

#[derive(Debug, Eq, PartialEq)]
enum Phase {
    /// The contract has not been read yet.
    Unresolved,
    /// The contract named this id; the launch has not been started.
    Requested(String),
    /// The tracked launch of this id is in flight.
    Launching(String),
    /// Settled: no request, launched, or refused. Never revisited.
    Done,
}

pub(crate) struct LaunchDesktopRequest {
    phase: Phase,
    retry_after: Option<Instant>,
    failures: u32,
    last_error: Option<i32>,
    waiting_for_probes_logged: bool,
    load: Option<Offload<ContractLoad>>,
}

impl Default for LaunchDesktopRequest {
    fn default() -> Self {
        Self {
            phase: Phase::Unresolved,
            retry_after: None,
            failures: 0,
            last_error: None,
            waiting_for_probes_logged: false,
            load: Offload::start("runtimed-launch-desktop", load_contract),
        }
    }
}

/// One bounded positioned read. Only a genuinely absent file means "no
/// request"; every other failure is surfaced so a requested launch is never
/// silently dropped.
pub(crate) fn load_contract() -> ContractLoad {
    snapshot_to_request(read_bounded_config_snapshot(CONTRACT_PATH, CONTRACT_MAX_BYTES))
}

fn snapshot_to_request(snapshot: Result<String, std::io::Error>) -> ContractLoad {
    match snapshot {
        Ok(contents) => parse_contract(contents.as_str())
            .map(Some)
            .map_err(|()| libc::EINVAL),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(match error.raw_os_error() {
            Some(errno) if errno > 0 => errno,
            _ => libc::EIO,
        }),
    }
}

/// Exact layout: header line, `desktop_id=<id>` line, one final newline.
fn parse_contract(contents: &str) -> Result<String, ()> {
    let mut lines = contents.split_terminator('\n');
    if lines.next() != Some(CONTRACT_HEADER) {
        return Err(());
    }
    let id = lines
        .next()
        .and_then(|line| line.strip_prefix("desktop_id="))
        .ok_or(())?;
    if lines.next().is_some() || !contents.ends_with('\n') || !valid_desktop_id(id) {
        return Err(());
    }
    Ok(id.to_owned())
}

/// The registry file-name grammar: `[A-Za-z0-9_-]+` before `.desktop`, no
/// separators, bounded length.
fn valid_desktop_id(id: &str) -> bool {
    let Some(stem) = id.strip_suffix(".desktop") else {
        return false;
    };
    !stem.is_empty()
        && id.len() <= MAX_DESKTOP_ID_BYTES
        && stem
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
}

fn fail(id: &str, reason: &str, errno: i32) {
    debug_line(
        format!("runtimed: kvm launch-desktop FAIL desktop_id={id} reason={reason} errno={errno}")
            .as_str(),
    );
    observability_client::error!(
        "runtimed",
        service,
        "kvm launch-desktop refused desktop_id={id} reason={reason} errno={errno}"
    );
}

impl LaunchDesktopRequest {
    fn finish_failed(&mut self, id: &str, reason: &str, errno: i32) {
        fail(id, reason, errno);
        self.phase = Phase::Done;
    }

    fn defer(&mut self, errno: i32) {
        self.failures = self.failures.saturating_add(1);
        self.last_error = Some(errno);
        let exponent = self.failures.saturating_sub(1).min(6);
        self.retry_after = Some(
            Instant::now()
                + super::STORAGE_NOT_READY_RETRY_BACKOFF
                    .saturating_mul(1_u32 << exponent)
                    .min(super::MAX_LAUNCH_RETRY_BACKOFF),
        );
    }

    /// Apply one contract read. `true` when the state changed.
    fn resolve(&mut self, loaded: ContractLoad) -> bool {
        match loaded {
            Ok(None) => {
                self.phase = Phase::Done;
            }
            Ok(Some(id)) => {
                debug_line(format!("runtimed: kvm launch-desktop requested desktop_id={id}").as_str());
                self.failures = 0;
                self.retry_after = None;
                self.last_error = None;
                self.waiting_for_probes_logged = false;
                self.phase = Phase::Requested(id);
            }
            Err(libc::EINVAL) => self.finish_failed("-", "malformed-contract", libc::EINVAL),
            Err(errno) => {
                // The volume may not be mounted yet; bounded retry.
                self.defer(errno);
                if self.failures >= MAX_CONTRACT_READ_ATTEMPTS {
                    self.finish_failed("-", "contract-unreadable", errno);
                }
                return false;
            }
        }
        true
    }

    fn poll_contract(&mut self) -> Option<ContractLoad> {
        let Some(load) = self.load.as_ref() else {
            return Some(load_contract());
        };
        if let Some(loaded) = load.poll() {
            return Some(loaded);
        }
        if load.disconnected() {
            self.load = None;
            return Some(load_contract());
        }
        if !load.busy() && !load.request() && load.disconnected() {
            self.load = None;
            return Some(load_contract());
        }
        None
    }
}

/// Advance the request by at most one step. Called every broker pass; free
/// (one comparison) once the request is settled.
pub(super) fn drive(state: &mut BrokerState) -> bool {
    if state.kvm_launch_desktop.phase == Phase::Done {
        // Settled for the boot (every ordinary boot settles on the first read):
        // dropping the handle ends the otherwise idle reader thread, which
        // would hold a task slot for the life of the service.
        state.kvm_launch_desktop.load = None;
        return false;
    }
    if !state.ui_ready
        || !state.launch_catalog_loaded
        || state
            .kvm_launch_desktop
            .retry_after
            .is_some_and(|deadline| Instant::now() < deadline)
    {
        return false;
    }
    match &state.kvm_launch_desktop.phase {
        Phase::Done | Phase::Launching(_) => false,
        Phase::Unresolved => {
            let Some(loaded) = state.kvm_launch_desktop.poll_contract() else {
                return false;
            };
            state.kvm_launch_desktop.resolve(loaded)
        }
        Phase::Requested(id) => {
            let id = id.clone();
            start_launch(state, id)
        }
    }
}

fn is_finite_boot_probe(state: &BrokerState, entry: &LaunchEntry, target_id: &str) -> bool {
    if entry.desktop_file_id == target_id
        || entry.restart
        || entry.exec.starts_with("services/")
        || entry.desktop_file_id == "netprobe.desktop"
    {
        return false;
    }
    entry.private_smp_qualification.is_some()
        || state
            .programs
            .get(entry.desktop_file_id.as_str())
            .is_some_and(|meta| meta.startup == crate::StartupMode::Session)
}

fn runtime_deps_achievable(
    state: &BrokerState,
    deps: &[String],
    running_packages: &std::collections::BTreeSet<String>,
) -> bool {
    deps.iter().all(|dep| {
        running_packages.contains(dep.as_str())
            || state.launched_once.contains(dep.as_str())
            || state.launch_entries.iter().any(|candidate| {
                candidate.package_id == *dep
                    && !state
                        .permanent_launch_failures
                        .contains_key(candidate.desktop_file_id.as_str())
            })
    })
}

fn boot_probes_pending_or_running(state: &BrokerState, target_id: &str) -> bool {
    let running_packages = super::catalog::running_packages(state);
    state.launch_entries.iter().any(|entry| {
        if !is_finite_boot_probe(state, entry, target_id) {
            return false;
        }
        if state
            .running
            .values()
            .any(|process| process.desktop_file_id == entry.desktop_file_id)
        {
            return true;
        }
        !state.launched_once.contains(entry.package_id.as_str())
            && !state
                .permanent_launch_failures
                .contains_key(entry.desktop_file_id.as_str())
            && runtime_deps_achievable(state, &entry.runtime_deps, &running_packages)
    })
}

pub(super) fn on_process_reaped(state: &mut BrokerState) {
    if matches!(state.kvm_launch_desktop.phase, Phase::Requested(_))
        && state.kvm_launch_desktop.last_error == Some(libc::EAGAIN)
    {
        state.kvm_launch_desktop.retry_after = None;
    }
}

fn start_launch(state: &mut BrokerState, id: String) -> bool {
    if state.launch_in_flight.is_some() {
        return false;
    }
    if boot_probes_pending_or_running(state, &id) {
        if !state.kvm_launch_desktop.waiting_for_probes_logged {
            state.kvm_launch_desktop.waiting_for_probes_logged = true;
            debug_line(
                format!("runtimed: kvm launch-desktop waiting for boot probes desktop_id={id}")
                    .as_str(),
            );
        }
        return false;
    }
    // Strictly the catalog's own entry for this exact id; resolution by exec
    // path (which the RPC path also allows) is not accepted here.
    let Some(metadata) = state.programs.get(id.as_str()).cloned() else {
        state.kvm_launch_desktop.finish_failed(&id, "unknown-desktop-id", libc::ENOENT);
        return true;
    };
    if metadata.exec.starts_with("services/") {
        state.kvm_launch_desktop.finish_failed(&id, "not-a-desktop-program", libc::EPERM);
        return true;
    }
    if state
        .running
        .values()
        .any(|process| process.desktop_file_id == id)
    {
        debug_line(
            format!("runtimed: kvm launch-desktop launched desktop_id={id} already_running=1")
                .as_str(),
        );
        state.kvm_launch_desktop.phase = Phase::Done;
        return true;
    }
    if !super::catalog::runtime_deps_satisfied(
        &metadata.runtime_deps,
        &super::catalog::running_packages(state),
        &state.launched_once,
    ) {
        return false;
    }
    let entry = LaunchEntry {
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
        application_request: metadata.application_request,
        private_smp_qualification: None,
    };
    match super::spawn::begin_tracked_launch(state, entry) {
        Ok(()) => {
            state.kvm_launch_desktop.phase = Phase::Launching(id);
        }
        Err(errno) if super::spawn::is_permanent_launch_failure(errno) => {
            state.kvm_launch_desktop.finish_failed(&id, "permanent-launch-failure", errno);
        }
        Err(errno) => retry_or_fail(&mut state.kvm_launch_desktop, &id, errno),
    }
    true
}

fn retry_or_fail(request: &mut LaunchDesktopRequest, id: &str, errno: i32) {
    request.defer(errno);
    if request.failures >= MAX_LAUNCH_ATTEMPTS {
        request.finish_failed(id, "launch-failed", errno);
    } else {
        request.phase = Phase::Requested(id.to_owned());
    }
}

/// The tracked launch of `desktop_file_id` finished. Settles the request when
/// it is the one in flight; every other launch is none of this module's concern.
pub(super) fn report_completion(
    state: &mut BrokerState,
    desktop_file_id: &str,
    result: Result<(), i32>,
) {
    let request = &mut state.kvm_launch_desktop;
    let Phase::Launching(id) = &request.phase else {
        return;
    };
    if id != desktop_file_id {
        return;
    }
    let id = id.clone();
    match result {
        Ok(()) => {
            debug_line(format!("runtimed: kvm launch-desktop launched desktop_id={id}").as_str());
            request.phase = Phase::Done;
        }
        Err(errno) if super::spawn::is_permanent_launch_failure(errno) => {
            request.finish_failed(&id, "permanent-launch-failure", errno);
        }
        Err(errno) => retry_or_fail(request, &id, errno),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const GOOD: &str = "contract=rustos-kvm-launch-desktop-v1\ndesktop_id=chrome.desktop\n";

    fn request() -> LaunchDesktopRequest {
        LaunchDesktopRequest {
            phase: Phase::Unresolved,
            retry_after: None,
            failures: 0,
            last_error: None,
            waiting_for_probes_logged: false,
            load: None,
        }
    }

    #[test]
    fn the_exact_contract_names_one_id() {
        assert_eq!(parse_contract(GOOD).as_deref(), Ok("chrome.desktop"));
    }

    #[test]
    fn every_other_shape_is_refused() {
        for bad in [
            "",
            "contract=rustos-kvm-launch-desktop-v1\n",
            "contract=rustos-kvm-launch-desktop-v1\ndesktop_id=chrome.desktop",
            "contract=rustos-kvm-launch-desktop-v1\ndesktop_id=chrome.desktop\nextra=1\n",
            "contract=rustos-kvm-launch-desktop-v1\r\ndesktop_id=chrome.desktop\r\n",
            "contract=rustos-kvm-launch-desktop-v2\ndesktop_id=chrome.desktop\n",
            "desktop_id=chrome.desktop\ncontract=rustos-kvm-launch-desktop-v1\n",
            "contract=rustos-kvm-launch-desktop-v1\ndesktop_id=chrome\n",
            "contract=rustos-kvm-launch-desktop-v1\ndesktop_id=../chrome.desktop\n",
            "contract=rustos-kvm-launch-desktop-v1\ndesktop_id=a/b.desktop\n",
            "contract=rustos-kvm-launch-desktop-v1\ndesktop_id=.desktop\n",
            "contract=rustos-kvm-launch-desktop-v1\ndesktop_id=a.b.desktop\n",
            "contract=rustos-kvm-launch-desktop-v1\n\ndesktop_id=chrome.desktop\n",
        ] {
            assert!(parse_contract(bad).is_err(), "{bad:?}");
        }
        let long = format!(
            "contract=rustos-kvm-launch-desktop-v1\ndesktop_id={}.desktop\n",
            "a".repeat(MAX_DESKTOP_ID_BYTES)
        );
        assert!(parse_contract(&long).is_err());
    }

    #[test]
    fn an_absent_file_is_no_request_and_any_other_failure_is_not() {
        let absent = std::io::Error::from(std::io::ErrorKind::NotFound);
        assert_eq!(snapshot_to_request(Err(absent)), Ok(None));
        assert_eq!(snapshot_to_request(Err(std::io::Error::from_raw_os_error(libc::EAGAIN))), Err(libc::EAGAIN));
        assert_eq!(snapshot_to_request(Err(std::io::Error::from(std::io::ErrorKind::TimedOut))), Err(libc::EIO));
        assert_eq!(snapshot_to_request(Ok(String::from("junk\n"))), Err(libc::EINVAL));
        assert_eq!(snapshot_to_request(Ok(GOOD.to_owned())), Ok(Some(String::from("chrome.desktop"))));
    }

    #[test]
    fn no_contract_settles_without_any_launch() {
        let mut request = request();
        assert!(request.resolve(Ok(None)));
        assert_eq!(request.phase, Phase::Done);
    }

    #[test]
    fn a_settled_request_releases_its_reader_thread() {
        let mut state = BrokerState::default();
        assert!(state.kvm_launch_desktop.load.is_some());
        state.kvm_launch_desktop.phase = Phase::Done;
        assert!(!drive(&mut state));
        assert!(state.kvm_launch_desktop.load.is_none());
    }

    #[test]
    fn a_malformed_contract_is_a_hard_error_not_a_retry() {
        let mut request = request();
        assert!(request.resolve(Err(libc::EINVAL)));
        assert_eq!(request.phase, Phase::Done);
    }

    #[test]
    fn unreadable_storage_retries_then_gives_up() {
        let mut request = request();
        assert!(!request.resolve(Err(libc::EAGAIN)));
        assert_eq!(request.phase, Phase::Unresolved);
        assert!(request.retry_after.is_some());
        request.failures = MAX_CONTRACT_READ_ATTEMPTS;
        request.resolve(Err(libc::EAGAIN));
        assert_eq!(request.phase, Phase::Done);
    }

    #[test]
    fn a_transient_launch_failure_retries_a_bounded_number_of_times() {
        let mut request = request();
        for attempt in 1..MAX_LAUNCH_ATTEMPTS {
            retry_or_fail(&mut request, "chrome.desktop", libc::EAGAIN);
            assert_eq!(request.phase, Phase::Requested(String::from("chrome.desktop")), "{attempt}");
        }
        retry_or_fail(&mut request, "chrome.desktop", libc::EAGAIN);
        assert_eq!(request.phase, Phase::Done);
    }

    #[test]
    fn nothing_happens_before_the_ui_is_ready_or_for_other_launches() {
        let mut state = BrokerState::default();
        state.launch_catalog_loaded = true;
        state.kvm_launch_desktop = request();
        assert!(!drive(&mut state), "UI not ready");
        assert_eq!(state.kvm_launch_desktop.phase, Phase::Unresolved);

        state.kvm_launch_desktop.phase = Phase::Launching(String::from("chrome.desktop"));
        report_completion(&mut state, "shell.desktop", Ok(()));
        assert_eq!(state.kvm_launch_desktop.phase, Phase::Launching(String::from("chrome.desktop")));
        report_completion(&mut state, "chrome.desktop", Ok(()));
        assert_eq!(state.kvm_launch_desktop.phase, Phase::Done);
    }

    #[test]
    fn an_unknown_id_is_refused_against_the_loaded_catalog() {
        let mut state = BrokerState::default();
        state.ui_ready = true;
        state.launch_catalog_loaded = true;
        state.kvm_launch_desktop = request();
        state.kvm_launch_desktop.phase = Phase::Requested(String::from("nonesuch.desktop"));
        assert!(drive(&mut state));
        assert_eq!(state.kvm_launch_desktop.phase, Phase::Done);
        assert!(state.launch_in_flight.is_none());
    }

    #[test]
    fn the_loop_never_reads_the_contract_inline_when_a_worker_exists() {
        let source = include_str!("kvm_launch_desktop.rs");
        let drive_body = source
            .split_once("pub(super) fn drive(")
            .expect("drive exists")
            .1
            .split_once("fn start_launch(")
            .expect("drive ends")
            .0;
        assert!(!drive_body.contains("read_bounded_config_snapshot("));
    }

    #[test]
    fn waits_for_running_and_pending_session_probes_before_launching_desktop() {
        use std::time::Duration;

        let mut state = BrokerState::default();
        state.ui_ready = true;
        state.launch_catalog_loaded = true;
        state.kvm_launch_desktop = request();
        state.kvm_launch_desktop.phase = Phase::Requested(String::from("chrome.desktop"));

        let session_meta = |id: &str, deps: Vec<&str>| crate::ProgramMetadata {
            package_id: id.trim_end_matches(".desktop").to_owned(),
            desktop_file_id: id.to_owned(),
            display_name: id.to_owned(),
            exec: format!("apps/{id}/app.elf"),
            runtime_deps: deps.into_iter().map(str::to_owned).collect(),
            startup: crate::StartupMode::Session,
            weight_micros: 100,
            logical_admin: false,
            console_hosted: false,
            args: Vec::new(),
            env: Vec::new(),
            application_request: runtime_control::ApplicationRequest::NONE,
        };
        let session_entry = |id: &str, deps: Vec<&str>| LaunchEntry {
            package_id: id.trim_end_matches(".desktop").to_owned(),
            desktop_file_id: id.to_owned(),
            display_name: id.to_owned(),
            exec: format!("apps/{id}/app.elf"),
            runtime_deps: deps.into_iter().map(str::to_owned).collect(),
            restart: false,
            weight_micros: 100,
            logical_admin: false,
            console_hosted: false,
            args: Vec::new(),
            env: Vec::new(),
            application_request: runtime_control::ApplicationRequest::NONE,
            private_smp_qualification: None,
        };

        state.programs.insert(String::from("ipcbench.desktop"), session_meta("ipcbench.desktop", vec![]));
        state.programs.insert(String::from("netprobe.desktop"), session_meta("netprobe.desktop", vec![]));
        state.programs.insert(String::from("abifuzz.desktop"), session_meta("abifuzz.desktop", vec!["wayclick"]));
        state.launch_entries.push(session_entry("ipcbench.desktop", vec![]));
        state.launch_entries.push(session_entry("netprobe.desktop", vec![]));
        state.launch_entries.push(session_entry("abifuzz.desktop", vec!["wayclick"]));

        // Pending ipcbench must defer chrome.desktop cleanly without burning failures.
        assert!(!drive(&mut state));
        assert_eq!(state.kvm_launch_desktop.failures, 0);
        assert!(state.kvm_launch_desktop.retry_after.is_none());

        // Running ipcbench must also defer chrome.desktop cleanly.
        state.running.insert(
            99,
            crate::RunningProcess {
                pid: 99,
                package_id: String::from("ipcbench"),
                desktop_file_id: String::from("ipcbench.desktop"),
                display_name: String::from("ipcbench"),
                exec: String::from("apps/ipcbench/ipcbench.elf"),
                session_handle: 0,
                restart: false,
                logical_admin: false,
            },
        );
        assert!(!drive(&mut state));
        assert_eq!(state.kvm_launch_desktop.failures, 0);

        // Once ipcbench exits and is reaped, running netprobe and unsatisfiable
        // abifuzz must not block chrome.desktop; it proceeds to catalog lookup.
        state.running.remove(&99);
        state.launched_once.insert(String::from("ipcbench"));
        state.running.insert(
            100,
            crate::RunningProcess {
                pid: 100,
                package_id: String::from("netprobe"),
                desktop_file_id: String::from("netprobe.desktop"),
                display_name: String::from("netprobe"),
                exec: String::from("apps/netprobe/netprobe.elf"),
                session_handle: 0,
                restart: false,
                logical_admin: false,
            },
        );
        // chrome.desktop isn't in state.programs here, so proceeding reaches
        // unknown-desktop-id and settles to Done.
        assert!(drive(&mut state));
        assert_eq!(state.kvm_launch_desktop.phase, Phase::Done);

        // Reaping a process clears an EAGAIN retry_after backoff, but keeps non-EAGAIN.
        let mut state = BrokerState::default();
        state.kvm_launch_desktop = request();
        retry_or_fail(&mut state.kvm_launch_desktop, "chrome.desktop", libc::EAGAIN);
        assert!(state.kvm_launch_desktop.retry_after.is_some());
        on_process_reaped(&mut state);
        assert!(state.kvm_launch_desktop.retry_after.is_none());

        state.kvm_launch_desktop.retry_after = Some(Instant::now() + Duration::from_secs(5));
        state.kvm_launch_desktop.last_error = Some(libc::EIO);
        on_process_reaped(&mut state);
        assert!(state.kvm_launch_desktop.retry_after.is_some());
    }
}
