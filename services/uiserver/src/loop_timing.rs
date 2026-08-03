//! Bounded attribution for the latency-sensitive UI owner turn.
//!
//! Wall time can include scheduler preemption as well as work performed by the
//! named phase.  Keep every synchronous boundary explicit and report the
//! residual instead of assigning an unexplained stall to rendering or IPC.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use crate::app::AppState;
use crate::sys::diag_line;

const MAX_SLOW_LOOP_LOGS: usize = 16;
static SLOW_LOOP_LOG_COUNT: AtomicUsize = AtomicUsize::new(0);

#[derive(Default)]
pub(crate) struct LoopPhaseTimings {
    pub(crate) gpu_completion: Duration,
    pub(crate) input: Duration,
    pub(crate) wayland: Duration,
    pub(crate) runtime: Duration,
    pub(crate) console: Duration,
    pub(crate) cursor: Duration,
    pub(crate) main_present: Duration,
    pub(crate) sleep: Duration,
}

impl LoopPhaseTimings {
    pub(crate) fn total_excluding_sleep(&self) -> Duration {
        self.gpu_completion
            + self.input
            + self.wayland
            + self.runtime
            + self.console
            + self.cursor
            + self.main_present
    }
}

pub(crate) fn log_slow_loop_iteration(
    state: &AppState,
    iteration_elapsed: Duration,
    timings: &LoopPhaseTimings,
    backlog_remaining: bool,
) {
    let index = SLOW_LOOP_LOG_COUNT.fetch_add(1, Ordering::Relaxed);
    if index >= MAX_SLOW_LOOP_LOGS {
        return;
    }
    let attributed = timings.total_excluding_sleep() + timings.sleep;
    let unattributed = iteration_elapsed.saturating_sub(attributed);
    diag_line(format!(
        "uiserver: slow loop iter_ms={} gpu_completion_ms={} input_ms={} wayland_ms={} runtime_ms={} console_ms={} cursor_ms={} present_ms={} sleep_ms={} unattributed_ms={} backlog={} console_windows={} wayland_windows={}",
        iteration_elapsed.as_millis(),
        timings.gpu_completion.as_millis(),
        timings.input.as_millis(),
        timings.wayland.as_millis(),
        timings.runtime.as_millis(),
        timings.console.as_millis(),
        timings.cursor.as_millis(),
        timings.main_present.as_millis(),
        timings.sleep.as_millis(),
        unattributed.as_millis(),
        backlog_remaining,
        state.console_windows.len(),
        state.wayland_windows.len(),
    ));
}

#[cfg(test)]
mod tests {
    use super::LoopPhaseTimings;
    use std::time::Duration;

    #[test]
    fn active_total_includes_gpu_completion_but_never_sleep() {
        let timings = LoopPhaseTimings {
            gpu_completion: Duration::from_millis(3),
            input: Duration::from_millis(5),
            sleep: Duration::from_millis(7),
            ..LoopPhaseTimings::default()
        };
        assert_eq!(timings.total_excluding_sleep(), Duration::from_millis(8));
    }
}
