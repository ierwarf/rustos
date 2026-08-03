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

use crate::sys::{diag_line, require_background_thread_class, running_on_rustos};

static ENABLED: AtomicBool = AtomicBool::new(false);
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

pub(crate) fn announce_if_enabled() {
    if enabled()
        && ANNOUNCED
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
    {
        diag_line("uiserver: acceptance profile enabled");
    }
}

fn exact_contract_enables_profile(contents: &str) -> bool {
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
            _ => return false,
        }
    }
    contract && ui_profile == Some(true) && network_exercise.is_some()
}

/// Bootstrap can start uiserver before the private acceptance registry exists.
/// Watch only during that bounded publication window; render/profile hot paths
/// remain a single atomic load and never cache an early negative result.
pub(crate) fn start_late_watcher() {
    if !running_on_rustos() || enabled() {
        return;
    }
    let _ = thread::Builder::new()
        .name(String::from("ui-acceptance-watch"))
        .spawn(|| {
            require_background_thread_class();
            let deadline = Instant::now() + WATCH_LIMIT;
            while Instant::now() < deadline {
                let enabled = read_bounded_config_snapshot(CONTRACT_PATH, CONTRACT_MAX_BYTES)
                    .ok()
                    .is_some_and(|contents| exact_contract_enables_profile(&contents));
                if enabled {
                    ENABLED.store(true, Ordering::Release);
                    announce_if_enabled();
                    return;
                }
                thread::sleep(WATCH_INTERVAL);
            }
        });
}

#[cfg(test)]
mod tests {
    use super::exact_contract_enables_profile;

    #[test]
    fn late_acceptance_profile_requires_the_exact_complete_contract() {
        assert!(exact_contract_enables_profile(
            "contract=rustos-kvm-acceptance-v1\nui_profile=1\nnetwork_exercise=0\n"
        ));
        assert!(!exact_contract_enables_profile(
            "contract=rustos-kvm-acceptance-v1\nui_profile=0\nnetwork_exercise=0\n"
        ));
        assert!(!exact_contract_enables_profile(
            "contract=rustos-kvm-acceptance-v1\nui_profile=1\n"
        ));
        assert!(!exact_contract_enables_profile(
            "contract=rustos-kvm-acceptance-v1\nui_profile=1\nnetwork_exercise=0\nui_profile=1\n"
        ));
    }
}
