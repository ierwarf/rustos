//! One keystroke's round trip, stamped at every hand-off it crosses.
//!
//! The `input-to-present` figure reports how long a keystroke waited, and the
//! per-phase breakdown beside it accounts for well under one percent of that
//! wait. That is not a contradiction: every phase the UI loop can time runs on
//! this side of an IPC boundary, so the whole of the missing time is spent in
//! hand-offs the loop never sees.
//!
//! This records the same keystroke at each of those hand-offs instead:
//!
//! ```text
//!   arm ----> submit ----> send ----> delivered ----> refresh ----> snapshot ----> present
//!       (a)          (b)        (c)            (d)          (e)             (f)
//! ```
//!
//! * `a` queueing onto the console command worker
//! * `b` that worker picking the command up
//! * `c` the `uiserver -> devmgrd -> runtimed` ioctl that carries the key
//! * `d` the broker reporting, through the parked graph wait, that the echo
//!   exists
//! * `e` the refresh worker getting scheduled and entering its next collect
//! * `f` the three snapshot ioctls that fetch it
//! * `g` the main loop waking and presenting
//!
//! Every stamp is taken in this process from one `Instant` epoch, so the
//! differences are transport and scheduling, never clock skew. Only the first
//! keystroke of a burst is traced, matching `input_awaiting_present`, so both
//! numbers on the reported line describe the same key.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

/// Stamps are microseconds since the epoch, biased by one so that a recorded
/// zero is impossible and `UNSET` stays unambiguous.
const UNSET: u64 = 0;

struct KeyTrace {
    epoch: Instant,
    armed: AtomicU64,
    submit: AtomicU64,
    send: AtomicU64,
    delivered: AtomicU64,
    refresh: AtomicU64,
    graph_wait: AtomicU64,
    graph: AtomicU64,
    state_start: AtomicU64,
    state_done: AtomicU64,
    sessions_start: AtomicU64,
    sessions_done: AtomicU64,
    output_start: AtomicU64,
    output_done: AtomicU64,
    snapshot: AtomicU64,
    /// How many times the refresh worker ran its collect while this keystroke
    /// was in flight, and how those rounds ended.
    ///
    /// The stamps above are one-shot by design - they name the first crossing
    /// of each hand-off - which means a hop that spans several rounds of the
    /// worker's loop reports as one long wait with no way to tell what happened
    /// inside it. That is not hypothetical: `snapshot_us` was observed at 803 ms
    /// with every ioctl inside it measured at about 1 ms, and the stamps alone
    /// could not say whether the worker was parked, retrying, or blocked. These
    /// counters answer that directly.
    collect_rounds: AtomicU64,
    empty_collects: AtomicU64,
    failed_collects: AtomicU64,
    last_collect_errno: AtomicU64,
    graph_stalls: AtomicU64,
    graph_wait_errors: AtomicU64,
    last_graph_wait_errno: AtomicU64,
    /// Wall time the refresh worker actually spent inside its two blocking
    /// calls while this keystroke was in flight.
    ///
    /// `refresh_us` measures delivery to the worker's next loop top, and it is
    /// the dominant term in every slow sample - but it cannot say which of
    /// three very different things the worker was doing across it: parked in a
    /// graph wait that was answered late, blocked in a snapshot ioctl, or
    /// simply not scheduled. These two accumulate the first two, so whatever
    /// `refresh_us` has left over is the third.
    parked_us: AtomicU64,
    collecting_us: AtomicU64,
}

impl KeyTrace {
    fn new() -> Self {
        Self {
            epoch: Instant::now(),
            armed: AtomicU64::new(UNSET),
            submit: AtomicU64::new(UNSET),
            send: AtomicU64::new(UNSET),
            delivered: AtomicU64::new(UNSET),
            refresh: AtomicU64::new(UNSET),
            graph_wait: AtomicU64::new(UNSET),
            graph: AtomicU64::new(UNSET),
            state_start: AtomicU64::new(UNSET),
            state_done: AtomicU64::new(UNSET),
            sessions_start: AtomicU64::new(UNSET),
            sessions_done: AtomicU64::new(UNSET),
            output_start: AtomicU64::new(UNSET),
            output_done: AtomicU64::new(UNSET),
            snapshot: AtomicU64::new(UNSET),
            collect_rounds: AtomicU64::new(0),
            empty_collects: AtomicU64::new(0),
            failed_collects: AtomicU64::new(0),
            last_collect_errno: AtomicU64::new(0),
            graph_stalls: AtomicU64::new(0),
            graph_wait_errors: AtomicU64::new(0),
            last_graph_wait_errno: AtomicU64::new(0),
            parked_us: AtomicU64::new(0),
            collecting_us: AtomicU64::new(0),
        }
    }

    /// Count an event, rather than stamping its first occurrence. Unlike
    /// [`KeyTrace::mark`] every crossing is recorded, because how many times a
    /// round repeated is the whole question these answer.
    fn count(&self, slot: &AtomicU64) {
        if self.armed.load(Ordering::Acquire) == UNSET {
            return;
        }
        slot.fetch_add(1, Ordering::Relaxed);
    }

    /// Accumulate a duration against a slot, for the spans where the question
    /// is how much time was spent rather than when it started.
    fn add(&self, slot: &AtomicU64, elapsed: Duration) {
        if self.armed.load(Ordering::Acquire) == UNSET {
            return;
        }
        let micros = elapsed.as_micros().min(u128::from(u64::MAX)) as u64;
        slot.fetch_add(micros, Ordering::Relaxed);
    }

    fn now(&self) -> u64 {
        (self
            .epoch
            .elapsed()
            .as_micros()
            .min(u128::from(u64::MAX - 1)) as u64)
            .saturating_add(1)
    }

    /// Record the first crossing of a hand-off, ignoring later ones. A burst of
    /// keys shares one trace, and the first key is the one being reported.
    fn mark(&self, slot: &AtomicU64) {
        if self.armed.load(Ordering::Acquire) == UNSET {
            return;
        }
        let _ = slot.compare_exchange(UNSET, self.now(), Ordering::AcqRel, Ordering::Acquire);
    }
}

fn trace() -> &'static KeyTrace {
    static TRACE: OnceLock<KeyTrace> = OnceLock::new();
    TRACE.get_or_init(KeyTrace::new)
}

/// Begin tracing a keystroke, unless one is already in flight.
///
/// The marks are cleared before the trace is armed, so a stamp left over from
/// the previous keystroke can never be read as part of this one.
pub(crate) fn arm() {
    let trace = trace();
    if trace.armed.load(Ordering::Acquire) != UNSET {
        return;
    }
    trace.submit.store(UNSET, Ordering::Release);
    trace.send.store(UNSET, Ordering::Release);
    trace.delivered.store(UNSET, Ordering::Release);
    trace.refresh.store(UNSET, Ordering::Release);
    trace.graph_wait.store(UNSET, Ordering::Release);
    trace.graph.store(UNSET, Ordering::Release);
    trace.state_start.store(UNSET, Ordering::Release);
    trace.state_done.store(UNSET, Ordering::Release);
    trace.sessions_start.store(UNSET, Ordering::Release);
    trace.sessions_done.store(UNSET, Ordering::Release);
    trace.output_start.store(UNSET, Ordering::Release);
    trace.output_done.store(UNSET, Ordering::Release);
    trace.snapshot.store(UNSET, Ordering::Release);
    trace.collect_rounds.store(0, Ordering::Release);
    trace.empty_collects.store(0, Ordering::Release);
    trace.failed_collects.store(0, Ordering::Release);
    trace.last_collect_errno.store(0, Ordering::Release);
    trace.graph_stalls.store(0, Ordering::Release);
    trace.graph_wait_errors.store(0, Ordering::Release);
    trace.last_graph_wait_errno.store(0, Ordering::Release);
    trace.parked_us.store(0, Ordering::Release);
    trace.collecting_us.store(0, Ordering::Release);
    trace.armed.store(trace.now(), Ordering::Release);
}

/// The refresh worker entered its collect.
pub(crate) fn count_collect_round() {
    let trace = trace();
    trace.count(&trace.collect_rounds);
}

/// A collect succeeded and found nothing to fetch.
pub(crate) fn count_empty_collect() {
    let trace = trace();
    trace.count(&trace.empty_collects);
}

/// A collect failed, in whole or for one session. The errno is kept because a
/// rail that refuses and one that is merely congested need different fixes.
pub(crate) fn count_failed_collect(errno: i32) {
    let trace = trace();
    trace.count(&trace.failed_collects);
    if trace.armed.load(Ordering::Acquire) != UNSET {
        trace
            .last_collect_errno
            .store(u64::from(errno.unsigned_abs()), Ordering::Relaxed);
    }
}

/// A parked graph wait came back with the generation the caller already held -
/// it ran out its budget instead of reporting an edge.
pub(crate) fn count_graph_stall() {
    let trace = trace();
    trace.count(&trace.graph_stalls);
}

/// Add the wall time of one completed graph wait, however it ended.
pub(crate) fn add_parked(elapsed: Duration) {
    let trace = trace();
    trace.add(&trace.parked_us, elapsed);
}

/// Add the wall time of one completed collect, however it ended.
pub(crate) fn add_collecting(elapsed: Duration) {
    let trace = trace();
    trace.add(&trace.collecting_us, elapsed);
}

/// A graph wait failed rather than returning a generation.
///
/// This was the one hand-off with no counter at all, and it is the one that
/// silently converts the readiness rail back into a poll: a retryable errno is
/// answered with a fixed sleep and nothing else, so a wait that fails every
/// round is indistinguishable in every other number here from a wait that is
/// parking normally. `graph_wait_us` reports `-` for both.
pub(crate) fn count_graph_wait_error(errno: i32) {
    let trace = trace();
    trace.count(&trace.graph_wait_errors);
    if trace.armed.load(Ordering::Acquire) != UNSET {
        trace
            .last_graph_wait_errno
            .store(u64::from(errno.unsigned_abs()), Ordering::Relaxed);
    }
}

pub(crate) fn mark_submit() {
    let trace = trace();
    trace.mark(&trace.submit);
}

pub(crate) fn mark_send() {
    let trace = trace();
    trace.mark(&trace.send);
}

pub(crate) fn mark_delivered() {
    let trace = trace();
    trace.mark(&trace.delivered);
}

pub(crate) fn mark_refresh() {
    let trace = trace();
    trace.mark(&trace.refresh);
}

pub(crate) fn mark_graph_wait() {
    let trace = trace();
    trace.mark(&trace.graph_wait);
}

pub(crate) fn mark_graph() {
    let trace = trace();
    trace.mark(&trace.graph);
}

pub(crate) fn mark_state_start() {
    let trace = trace();
    trace.mark(&trace.state_start);
}

pub(crate) fn mark_state_done() {
    let trace = trace();
    trace.mark(&trace.state_done);
}

pub(crate) fn mark_sessions_start() {
    let trace = trace();
    trace.mark(&trace.sessions_start);
}

pub(crate) fn mark_sessions_done() {
    let trace = trace();
    trace.mark(&trace.sessions_done);
}

pub(crate) fn mark_output_start() {
    let trace = trace();
    trace.mark(&trace.output_start);
}

pub(crate) fn mark_output_done() {
    let trace = trace();
    trace.mark(&trace.output_done);
}

pub(crate) fn mark_snapshot() {
    let trace = trace();
    trace.mark(&trace.snapshot);
}

/// Microseconds spent in each hand-off, in the order a keystroke crosses them.
///
/// A hop whose far stamp is missing reports `None` rather than folding into its
/// neighbour, so a hand-off that never happened cannot be read as one that was
/// fast.
pub(crate) struct KeyTraceReport {
    pub(crate) total_us: u64,
    pub(crate) queue_us: Option<u64>,
    pub(crate) pickup_us: Option<u64>,
    pub(crate) deliver_us: Option<u64>,
    pub(crate) refresh_us: Option<u64>,
    pub(crate) graph_wait_us: Option<u64>,
    pub(crate) echo_us: Option<u64>,
    pub(crate) state_us: Option<u64>,
    pub(crate) sessions_us: Option<u64>,
    pub(crate) output_us: Option<u64>,
    pub(crate) snapshot_us: Option<u64>,
    pub(crate) present_us: Option<u64>,
    pub(crate) collect_rounds: u64,
    pub(crate) empty_collects: u64,
    pub(crate) failed_collects: u64,
    pub(crate) last_collect_errno: u64,
    pub(crate) graph_stalls: u64,
    pub(crate) graph_wait_errors: u64,
    pub(crate) last_graph_wait_errno: u64,
    pub(crate) parked_us: u64,
    pub(crate) collecting_us: u64,
}

impl KeyTraceReport {
    fn field(name: &str, value: Option<u64>) -> String {
        match value {
            Some(value) => format!("{name}={value}"),
            None => format!("{name}=-"),
        }
    }
}

impl std::fmt::Display for KeyTraceReport {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "total_us={} {} {} {} {} {} {} {} {} {} {} {} \
             rounds={} empty={} failed={} errno={} stalls={} wait_errors={} wait_errno={} parked_us={} collecting_us={}",
            self.total_us,
            Self::field("queue_us", self.queue_us),
            Self::field("pickup_us", self.pickup_us),
            Self::field("deliver_us", self.deliver_us),
            Self::field("refresh_us", self.refresh_us),
            Self::field("graph_wait_us", self.graph_wait_us),
            Self::field("echo_us", self.echo_us),
            Self::field("state_us", self.state_us),
            Self::field("sessions_us", self.sessions_us),
            Self::field("output_us", self.output_us),
            Self::field("snapshot_us", self.snapshot_us),
            Self::field("present_us", self.present_us),
            self.collect_rounds,
            self.empty_collects,
            self.failed_collects,
            self.last_collect_errno,
            self.graph_stalls,
            self.graph_wait_errors,
            self.last_graph_wait_errno,
            self.parked_us,
            self.collecting_us,
        )
    }
}

fn hop(from: u64, to: u64) -> Option<u64> {
    if from == UNSET || to == UNSET {
        return None;
    }
    Some(to.saturating_sub(from))
}

/// Close the trace and report it once the echo has actually been fetched.
///
/// A present that carries no console refresh is not this keystroke's echo -
/// it may be a cursor blink or a Wayland frame - so reporting on the first
/// present would time an unrelated frame and call it the round trip. The trace
/// stays armed until the snapshot that holds the echo has been taken.
pub(crate) fn report_if_echoed() -> Option<KeyTraceReport> {
    if trace().snapshot.load(Ordering::Acquire) == UNSET {
        return None;
    }
    close()
}

/// Close a trace whose echo never arrived, so one lost keystroke cannot stop
/// every later one from being traced. The report is kept, with the hops it
/// never crossed reported as missing.
pub(crate) fn report_if_stale(max_age: Duration) -> Option<KeyTraceReport> {
    let trace = trace();
    let armed = trace.armed.load(Ordering::Acquire);
    if armed == UNSET {
        return None;
    }
    let age = trace.now().saturating_sub(armed);
    if u128::from(age) < max_age.as_micros() {
        return None;
    }
    close()
}

fn close() -> Option<KeyTraceReport> {
    let trace = trace();
    let armed = trace.armed.swap(UNSET, Ordering::AcqRel);
    if armed == UNSET {
        return None;
    }
    let presented = trace.now();
    let submit = trace.submit.load(Ordering::Acquire);
    let send = trace.send.load(Ordering::Acquire);
    let delivered = trace.delivered.load(Ordering::Acquire);
    let refresh = trace.refresh.load(Ordering::Acquire);
    let graph_wait = trace.graph_wait.load(Ordering::Acquire);
    let graph = trace.graph.load(Ordering::Acquire);
    let state_start = trace.state_start.load(Ordering::Acquire);
    let state_done = trace.state_done.load(Ordering::Acquire);
    let sessions_start = trace.sessions_start.load(Ordering::Acquire);
    let sessions_done = trace.sessions_done.load(Ordering::Acquire);
    let output_start = trace.output_start.load(Ordering::Acquire);
    let output_done = trace.output_done.load(Ordering::Acquire);
    let snapshot = trace.snapshot.load(Ordering::Acquire);
    Some(KeyTraceReport {
        total_us: presented.saturating_sub(armed),
        queue_us: hop(armed, submit),
        pickup_us: hop(submit, send),
        deliver_us: hop(send, delivered),
        refresh_us: hop(delivered, refresh),
        graph_wait_us: hop(graph_wait, graph),
        echo_us: hop(delivered, graph),
        state_us: hop(state_start, state_done),
        sessions_us: hop(sessions_start, sessions_done),
        output_us: hop(output_start, output_done),
        snapshot_us: hop(graph, snapshot),
        present_us: hop(snapshot, presented),
        collect_rounds: trace.collect_rounds.load(Ordering::Acquire),
        empty_collects: trace.empty_collects.load(Ordering::Acquire),
        failed_collects: trace.failed_collects.load(Ordering::Acquire),
        last_collect_errno: trace.last_collect_errno.load(Ordering::Acquire),
        graph_stalls: trace.graph_stalls.load(Ordering::Acquire),
        graph_wait_errors: trace.graph_wait_errors.load(Ordering::Acquire),
        last_graph_wait_errno: trace.last_graph_wait_errno.load(Ordering::Acquire),
        parked_us: trace.parked_us.load(Ordering::Acquire),
        collecting_us: trace.collecting_us.load(Ordering::Acquire),
    })
}

#[cfg(test)]
mod tests {
    use super::{
        arm, mark_delivered, mark_graph, mark_graph_wait, mark_refresh, mark_send, mark_snapshot,
        mark_submit, report_if_echoed, report_if_stale,
    };
    use std::time::Duration;

    /// The trace is a process-wide singleton, so every case that must not run
    /// concurrently with another shares this one test.
    #[test]
    fn a_trace_closes_on_the_echo_and_reports_only_the_hops_it_crossed() {
        assert!(
            report_if_echoed().is_none(),
            "an unarmed trace has no keystroke to report"
        );

        // Marks taken while nothing is armed belong to no keystroke.
        mark_graph();
        mark_snapshot();
        assert!(report_if_echoed().is_none());

        arm();
        mark_submit();
        mark_send();
        mark_delivered();
        mark_refresh();
        mark_graph_wait();
        mark_graph();
        mark_snapshot();
        let full = report_if_echoed().expect("a fetched echo closes the trace");
        for hop in [
            full.queue_us,
            full.pickup_us,
            full.deliver_us,
            full.refresh_us,
            full.graph_wait_us,
            full.echo_us,
            full.snapshot_us,
            full.present_us,
        ] {
            assert!(hop.is_some(), "every crossed hand-off is attributed");
        }

        // A present that carries no echo is some other frame, and timing it
        // would report an unrelated frame as this keystroke's round trip.
        arm();
        mark_submit();
        mark_send();
        mark_delivered();
        assert!(
            report_if_echoed().is_none(),
            "the trace stays armed until the echo is in hand"
        );

        // ... but it must not stay armed forever, and the hops it never
        // crossed are reported as missing rather than as zero.
        assert!(report_if_stale(Duration::from_secs(3_600)).is_none());
        let stale = report_if_stale(Duration::ZERO).expect("a stale trace still reports");
        assert!(stale.deliver_us.is_some());
        assert!(stale.echo_us.is_none());
        assert!(stale.snapshot_us.is_none());
        assert!(stale.present_us.is_none());
        assert!(report_if_stale(Duration::ZERO).is_none(), "closed once");
    }
}
