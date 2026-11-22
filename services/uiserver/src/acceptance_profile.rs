//! Owner: uiserver private acceptance-observability policy.
//! Boundary: exact xtask registry contract to a local atomic profiler flag.
//! Lifecycle: initialize from env, watch registry for at most 30 seconds, then stop.
//! Concurrency: one demoted watcher publishes with Release; hot paths use Acquire.
//! Failure: malformed, late, or unreadable contracts leave profiling disabled.
//! Forbidden: unbounded polling, partial token acceptance, duplicate announcements.
//! Evidence: focused parser test plus the AcceptanceProfilePublication model.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::OnceLock;
use std::thread;
use std::time::{Duration, Instant};

use runtime_control::read_bounded_config_snapshot;

use crate::sys::{diag_line, running_on_rustos, spawn_ui_thread, UiThreadRole};

static ENABLED: AtomicBool = AtomicBool::new(false);
static INPUT_COMPLETION: AtomicBool = AtomicBool::new(false);
static ANNOUNCED: AtomicBool = AtomicBool::new(false);
static INITIALIZED: OnceLock<()> = OnceLock::new();

const CONTRACT_PATH: &str = "/system/registry/system/kvm-acceptance-v1.env";
const CONTRACT_MAX_BYTES: usize = 256;
const WATCH_INTERVAL: Duration = Duration::from_millis(250);
const WATCH_LIMIT: Duration = Duration::from_secs(30);

pub(crate) fn enabled() -> bool {
    INITIALIZED.get_or_init(|| {
        let enabled = match std::env::var("RUSTOS_UI_PROFILE") {
            Ok(value) => matches!(value.as_str(), "1" | "true" | "TRUE" | "yes" | "YES"),
            Err(_) => false,
        };
        ENABLED.store(enabled, Ordering::Release);
    });
    ENABLED.load(Ordering::Acquire)
}

pub(crate) fn input_completion_enabled() -> bool {
    INPUT_COMPLETION.load(Ordering::Acquire)
}

fn exact_input_completion_contract(contents: &str) -> bool {
    contents == "contract=rustos-ipcbench-probe-v1\nprobe=core_gui\n"
}

fn publish_input_completion_selector() {
    let enabled = read_bounded_config_snapshot(
        "/system/registry/system/ipcbench-probe-v1.env",
        CONTRACT_MAX_BYTES,
    )
    .ok()
    .is_some_and(|contents| exact_input_completion_contract(&contents));
    INPUT_COMPLETION.store(enabled, Ordering::Release);
}

pub(crate) fn announce_if_enabled() {
    if enabled()
        && ANNOUNCED
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
    {
        diag_line("uiserver: acceptance profile enabled");
    }
}

/// What the private acceptance contract asks of uiserver.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct AcceptanceRequest {
    ui_profile: bool,
    ui_pixel_probe: bool,
}

/// Exact parse of `kvm-acceptance-v1.env`: the same key set `runtimed` accepts
/// (`ui_profile`, `network_exercise`, optional `google_ping` and
/// `ui_pixel_probe`), each at most once, nothing else.
fn parse_contract(contents: &str) -> Option<AcceptanceRequest> {
    let mut contract = false;
    let mut ui_profile = None;
    let mut network_exercise = None;
    let mut google_ping = None;
    let mut ui_pixel_probe = None;
    for line in contents.lines() {
        match line {
            "contract=rustos-kvm-acceptance-v1" if !contract => contract = true,
            "ui_profile=0" if ui_profile.is_none() => ui_profile = Some(false),
            "ui_profile=1" if ui_profile.is_none() => ui_profile = Some(true),
            "network_exercise=0" if network_exercise.is_none() => network_exercise = Some(false),
            "network_exercise=1" if network_exercise.is_none() => network_exercise = Some(true),
            "google_ping=0" | "google_ping=1" if google_ping.is_none() => google_ping = Some(()),
            "ui_pixel_probe=0" if ui_pixel_probe.is_none() => ui_pixel_probe = Some(false),
            "ui_pixel_probe=1" if ui_pixel_probe.is_none() => ui_pixel_probe = Some(true),
            _ => return None,
        }
    }
    if !contract || network_exercise.is_none() {
        return None;
    }
    Some(AcceptanceRequest {
        ui_profile: ui_profile?,
        ui_pixel_probe: ui_pixel_probe.unwrap_or(false),
    })
}

/// Bootstrap can start uiserver before the private acceptance registry exists.
/// Watch only during that bounded publication window; render/profile hot paths
/// remain a single atomic load and never cache an early negative result.
pub(crate) fn start_late_watcher() {
    if !running_on_rustos() {
        return;
    }
    if enabled() {
        // Runtimed may already have admitted the private profile contract and
        // supplied RUSTOS_UI_PROFILE=1 before this process starts.
        publish_input_completion_selector();
        return;
    }
    let _ = spawn_ui_thread(UiThreadRole::Background, "ui-acceptance-watch", || {
        let deadline = Instant::now() + WATCH_LIMIT;
        while Instant::now() < deadline {
            let request = read_bounded_config_snapshot(CONTRACT_PATH, CONTRACT_MAX_BYTES)
                .ok()
                .and_then(|contents| parse_contract(&contents));
            if let Some(request) = request.filter(|r| r.ui_profile || r.ui_pixel_probe) {
                if request.ui_pixel_probe {
                    crate::pixel_probe::enable();
                }
                if request.ui_profile {
                    publish_input_completion_selector();
                    ENABLED.store(true, Ordering::Release);
                    announce_if_enabled();
                }
                return;
            }
            thread::sleep(WATCH_INTERVAL);
        }
        diag_line("uiserver: acceptance contract watch ended without a request");
    });
}

#[cfg(test)]
mod tests {
    use super::{exact_input_completion_contract, parse_contract, AcceptanceRequest};

    #[test]
    fn input_completion_requires_the_exact_private_benchmark_selector() {
        assert!(exact_input_completion_contract(
            "contract=rustos-ipcbench-probe-v1\nprobe=core_gui\n"
        ));
        assert!(!exact_input_completion_contract(
            "contract=rustos-ipcbench-probe-v1\nprobe=core_gui\nextra=1\n"
        ));
        assert!(!exact_input_completion_contract(
            "contract=rustos-ipcbench-probe-v1\nprobe=core_state\n"
        ));
    }

    #[test]
    fn late_acceptance_contract_requires_the_exact_complete_key_set() {
        let profile = parse_contract(
            "contract=rustos-kvm-acceptance-v1\nui_profile=1\nnetwork_exercise=0\n",
        );
        assert_eq!(profile, Some(AcceptanceRequest { ui_profile: true, ui_pixel_probe: false }));
        // Every key the xtask writer emits, in its order.
        let full = parse_contract(
            "contract=rustos-kvm-acceptance-v1\nui_profile=0\nnetwork_exercise=0\ngoogle_ping=0\nui_pixel_probe=1\n",
        );
        assert_eq!(full, Some(AcceptanceRequest { ui_profile: false, ui_pixel_probe: true }));
        assert!(parse_contract("contract=rustos-kvm-acceptance-v1\nui_profile=1\n").is_none());
        assert!(parse_contract(
            "contract=rustos-kvm-acceptance-v1\nui_profile=1\nnetwork_exercise=0\nui_profile=1\n"
        )
        .is_none());
        assert!(parse_contract(
            "contract=rustos-kvm-acceptance-v1\nui_profile=1\nnetwork_exercise=0\nui_pixel_probe=yes\n"
        )
        .is_none());
        assert!(parse_contract(
            "contract=rustos-kvm-acceptance-v1\nui_profile=1\nnetwork_exercise=0\nextra=1\n"
        )
        .is_none());
    }
}
