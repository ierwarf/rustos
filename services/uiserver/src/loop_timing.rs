//! Bounded attribution for the latency-sensitive UI owner turn.
//!
//! Wall time can include scheduler preemption as well as work performed by the
//! named phase.  Keep every synchronous boundary explicit and report the
//! residual instead of assigning an unexplained stall to rendering or IPC.

use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::time::Duration;

use crate::app::AppState;
use crate::sys::diag_line;

const MAX_SLOW_LOOP_LOGS: usize = 16;
static SLOW_LOOP_LOG_COUNT: AtomicUsize = AtomicUsize::new(0);

/// Monotonic identity for one UI owner turn.
///
/// Every phase record carries it so a frame can be followed from input,
/// through Wayland dispatch, render, and present, instead of leaving each
/// segment as an unrelated duration. Without one identity a stall can be seen
/// but not attributed to the frame it belongs to, which is the gap that made
/// the compositor pipeline unmeasurable end to end.
static NEXT_FRAME_SEQ: AtomicU64 = AtomicU64::new(1);

static CURRENT_FRAME_SEQ: AtomicU64 = AtomicU64::new(0);

/// Claims the next frame identity and publishes it as the current turn.
///
/// The UI owner is a single thread, so publishing the identity lets any record
/// emitted inside the turn name the frame it belongs to without threading a
/// parameter through every present and Wayland helper.
pub(crate) fn next_frame_seq() -> u64 {
    let seq = NEXT_FRAME_SEQ.fetch_add(1, Ordering::Relaxed);
    CURRENT_FRAME_SEQ.store(seq, Ordering::Relaxed);
    seq
}

/// Returns the identity of the owner turn currently in progress.
pub(crate) fn current_frame_seq() -> u64 {
    CURRENT_FRAME_SEQ.load(Ordering::Relaxed)
}

#[derive(Default)]
pub(crate) struct LoopPhaseTimings {
    /// Identity of the turn these durations belong to.
    pub(crate) frame_seq: u64,
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
        "uiserver: slow loop frame_seq={} iter_ms={} gpu_completion_ms={} input_ms={} wayland_ms={} runtime_ms={} console_ms={} cursor_ms={} present_ms={} sleep_ms={} unattributed_ms={} backlog={} console_windows={} wayland_windows={}",
        timings.frame_seq,
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

/// Frames between periodic attribution records.
///
/// A slow-frame log only proves a frame that already missed its budget. A
/// steady sample is what shows where a healthy frame spends its time, and it
/// is what the compositor pipeline needs to be judged against a frame-rate
/// contract rather than against the absence of stalls.
const FRAME_SAMPLE_INTERVAL: u64 = 240;

/// Whether this frame identity is one of the sampled ones.
///
/// Records that join a frame to something else share the sample cadence rather
/// than each choosing their own. Every debugcon byte exits to the host, and the
/// acceptance proof reads the same transport, so a per-frame record at 130 Hz
/// crowds out the per-second window records the proof counts.
pub(crate) fn frame_seq_is_sampled(frame_seq: u64) -> bool {
    frame_seq != 0 && frame_seq.is_multiple_of(FRAME_SAMPLE_INTERVAL)
}

/// Emits one attribution record per sample interval.
pub(crate) fn log_frame_sample(timings: &LoopPhaseTimings, iteration_elapsed: Duration) {
    if timings.frame_seq == 0 || !timings.frame_seq.is_multiple_of(FRAME_SAMPLE_INTERVAL) {
        return;
    }
    let attributed = timings.total_excluding_sleep() + timings.sleep;
    diag_line(format!(
        "uiserver: frame frame_seq={} iter_us={} gpu_completion_us={} input_us={} wayland_us={} runtime_us={} console_us={} cursor_us={} present_us={} sleep_us={} unattributed_us={}",
        timings.frame_seq,
        iteration_elapsed.as_micros(),
        timings.gpu_completion.as_micros(),
        timings.input.as_micros(),
        timings.wayland.as_micros(),
        timings.runtime.as_micros(),
        timings.console.as_micros(),
        timings.cursor.as_micros(),
        timings.main_present.as_micros(),
        timings.sleep.as_micros(),
        iteration_elapsed.saturating_sub(attributed).as_micros(),
    ));
}

#[cfg(test)]
mod tests {
    use super::LoopPhaseTimings;
    use std::time::Duration;

    /// Frame identity must be unique and monotonic, and the published current
    /// identity must match the one just claimed. Records emitted inside a turn
    /// read the published value, so a mismatch would attribute a phase to the
    /// wrong frame.
    #[test]
    fn frame_identity_is_monotonic_and_published_for_the_current_turn() {
        let first = super::next_frame_seq();
        assert_eq!(super::current_frame_seq(), first);
        let second = super::next_frame_seq();
        assert!(second > first, "frame identity must advance");
        assert_eq!(super::current_frame_seq(), second);
    }

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
