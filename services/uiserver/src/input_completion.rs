//! Private, opt-in input-consumption to observed DVM-completion benchmark.
//!
//! Owner: the UI thread supplies cursor-moving input drains and exact accepted
//! GPU frames. Boundary: only an exact private KVM selector enables recording.
//! Completion requires both scene compiler receipts. This is not evdev-to-photon
//! or application key-echo latency.
//! Lifecycle: 16 completed warmup frames, then 128 input-bearing frames; freeze
//! before exporting any records. Backpressure coalesces drains without resetting
//! their oldest timestamp. No allocation, formatting or logging while sampling.
//! Concurrency: one UI owner, a private mutex, and no callbacks under that lock.
//! Failure: duplicate, stale, lost or cancelled receipts cannot produce success.
//! Forbidden: treating CPU ioctl return, submission, or a timeout as completion.

use std::sync::Mutex;
use std::time::Instant;

pub(crate) const SAMPLES: usize = 128;
const WARMUP: usize = 16;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct Burst {
    first: Instant,
    last: Instant,
    drains: u64,
    events: u64,
    x: u32,
    y: u32,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct Ticket {
    id: usize,
    warmup: bool,
    burst: Burst,
    work: SubmitWork,
    generation: u64,
    epoch: u64,
    surface: u32,
}

/// Clock boundaries of the accepted attempt after a slot was selected.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct SubmitWork {
    pub(crate) start: Instant,
    pub(crate) copy: Instant,
    pub(crate) ioctl: Instant,
    pub(crate) accepted: Instant,
    pub(crate) cpu_ns: Option<u64>,
    /// Dirty scene, rebuild, cursor, Wayland, console, dock and clock bits.
    pub(crate) kind: u8,
    pub(crate) upload_pixels: u64,
}

pub(crate) fn thread_cpu_ns() -> Option<u64> {
    if !crate::acceptance_profile::input_completion_enabled() {
        return None;
    }
    let mut ts = libc::timespec { tv_sec: 0, tv_nsec: 0 };
    // SAFETY: this read-only clock writes a live timespec on the stack.
    if unsafe { libc::clock_gettime(libc::CLOCK_THREAD_CPUTIME_ID, &mut ts) } != 0 {
        return None;
    }
    let ns = u64::try_from(ts.tv_nsec).ok()?;
    if ns >= 1_000_000_000 {
        return None;
    }
    u64::try_from(ts.tv_sec).ok()?.checked_mul(1_000_000_000)?.checked_add(ns)
}

#[derive(Clone, Copy)]
struct Completed {
    ticket: Ticket,
    completed: Instant,
}

struct Collector {
    origin: Option<Instant>,
    pending: Option<Burst>,
    warmup_issued: usize,
    warmup_completed: usize,
    issued: usize,
    completed: usize,
    failed: u64,
    exported: bool,
    entries: [Option<Completed>; SAMPLES],
}

impl Collector {
    const fn new() -> Self {
        Self {
            origin: None,
            pending: None,
            warmup_issued: 0,
            warmup_completed: 0,
            issued: 0,
            completed: 0,
            failed: 0,
            exported: false,
            entries: [None; SAMPLES],
        }
    }

    fn input(&mut self, now: Instant, events: u64, x: u32, y: u32) {
        if self.exported || self.failed != 0 || self.issued == SAMPLES || events == 0 {
            return;
        }
        self.origin.get_or_insert(now);
        if let Some(pending) = &mut self.pending {
            if now < pending.last {
                self.failed += 1;
                return;
            }
            pending.last = now;
            pending.drains += 1;
            pending.events += events;
            pending.x = x;
            pending.y = y;
        } else {
            self.pending = Some(Burst {
                first: now,
                last: now,
                drains: 1,
                events,
                x,
                y,
            });
        }
    }

    fn submit(
        &mut self,
        work: SubmitWork,
        generation: u64,
        epoch: u64,
        surface: u32,
    ) -> Option<Ticket> {
        if self.exported || self.failed != 0 || self.issued == SAMPLES {
            return None;
        }
        let burst = self.pending.take()?;
        if work.start < burst.last
            || work.copy < work.start
            || work.ioctl < work.copy
            || work.accepted < work.ioctl
            || work.cpu_ns.is_none()
            || generation == 0
            || epoch == 0
        {
            self.failed += 1;
            return None;
        }
        let warmup = self.warmup_completed < WARMUP;
        let id = if warmup {
            // Do not start measured frames while a warmup receipt is in flight.
            if self.warmup_issued == WARMUP {
                return None;
            }
            let id = self.warmup_issued;
            self.warmup_issued += 1;
            id
        } else {
            let id = self.issued;
            self.issued += 1;
            id
        };
        Some(Ticket {
            id,
            warmup,
            burst,
            work,
            generation,
            epoch,
            surface,
        })
    }

    fn complete(&mut self, ticket: Ticket, now: Instant) {
        if now < ticket.work.accepted {
            self.failed += 1;
            return;
        }
        if ticket.warmup {
            // GPU retirement is FIFO. A duplicate or reordered warmup is invalid.
            if ticket.id != self.warmup_completed || ticket.id >= self.warmup_issued {
                self.failed += 1;
                return;
            }
            self.warmup_completed += 1;
            return;
        }
        if ticket.id != self.completed
            || ticket.id >= self.issued
            || self.entries[ticket.id].is_some()
        {
            self.failed += 1;
            return;
        }
        self.entries[ticket.id] = Some(Completed {
            ticket,
            completed: now,
        });
        self.completed += 1;
    }

    fn provider_lost(&mut self) {
        // A recovery between warmup and measurement is still a cohort change,
        // even when no ticket is in flight. Already frozen batches are outside
        // that transition; an unused collector has no provider cohort yet.
        if self.origin.is_some() && self.completed < SAMPLES && !self.exported {
            self.failed += 1;
        }
    }

    fn ready(&self) -> bool {
        !self.exported && (self.failed != 0 || self.completed == SAMPLES)
    }
}

static COLLECTOR: Mutex<Collector> = Mutex::new(Collector::new());

pub(crate) fn input(now: Instant, events: u64, x: u32, y: u32) {
    if crate::acceptance_profile::input_completion_enabled() {
        COLLECTOR.lock().unwrap().input(now, events, x, y);
    }
}

pub(crate) fn submitted(
    work: SubmitWork,
    generation: u64,
    epoch: u64,
    surface: u32,
) -> Option<Ticket> {
    if !crate::acceptance_profile::input_completion_enabled() {
        return None;
    }
    COLLECTOR
        .lock()
        .unwrap()
        .submit(work, generation, epoch, surface)
}

pub(crate) fn completed(ticket: Option<Ticket>) {
    if let Some(ticket) = ticket {
        COLLECTOR.lock().unwrap().complete(ticket, Instant::now());
    }
}

pub(crate) fn provider_lost() {
    if crate::acceptance_profile::input_completion_enabled() {
        COLLECTOR.lock().unwrap().provider_lost();
    }
}

pub(crate) fn health(drops: u64, errors: u64) {
    if crate::acceptance_profile::input_completion_enabled() && (drops != 0 || errors != 0) {
        COLLECTOR.lock().unwrap().failed += 1;
    }
}

/// Called outside submit/retire timing. The immutable complete batch is copied
/// under the lock, then exported outside the measurement window. The host
/// rejects a lost/truncated debug record instead of accepting a partial batch.
pub(crate) fn maybe_emit() {
    if !crate::acceptance_profile::input_completion_enabled() {
        return;
    }
    let snapshot = {
        let mut collector = COLLECTOR.lock().unwrap();
        if !collector.ready() {
            return;
        }
        collector.exported = true;
        (
            collector.origin,
            collector.entries,
            collector.completed,
            collector.failed,
        )
    };
    let (origin, entries, completed, failed) = snapshot;
    if failed != 0 || completed != SAMPLES {
        crate::sys::debug_line(&format!(
            "uiserver: input-completion FAIL completed={completed} failed={failed}"
        ));
        return;
    }
    let origin = origin.expect("completed input frames have an origin");
    crate::sys::debug_line("uiserver: input-completion BEGIN v=2 samples=128 warmup=16 endpoint=consume-to-observed-dvm-completion");
    for entry in entries.into_iter().flatten() {
        let t = entry.ticket;
        let micros = |stamp: Instant| stamp.duration_since(origin).as_micros();
        crate::sys::debug_line(&format!(
            "uiserver: input-completion sample id={} first={} last={} submit={} done={} gen={} epoch={} surface={} drains={} events={} x={} y={} start={} copy={} ioctl={} cpu_ns={} k={} px={}",
            t.id, micros(t.burst.first), micros(t.burst.last), micros(t.work.accepted),
            micros(entry.completed), t.generation, t.epoch, t.surface,
            t.burst.drains, t.burst.events, t.burst.x, t.burst.y,
            micros(t.work.start), micros(t.work.copy), micros(t.work.ioctl),
            t.work.cpu_ns.expect("only valid CPU stamps are exported"), t.work.kind,
            t.work.upload_pixels,
        ));
    }
    crate::sys::debug_line("uiserver: input-completion END samples=128 failed=0");
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn advance(now: Instant, us: u64) -> Instant {
        now + Duration::from_micros(us)
    }

    fn stamps(now: Instant) -> SubmitWork {
        SubmitWork { start: now, copy: now, ioctl: now, accepted: now, cpu_ns: Some(0),
            kind: 0, upload_pixels: 0 }
    }

    fn warmed(now: Instant) -> Collector {
        let mut collector = Collector::new();
        for _ in 0..WARMUP {
            collector.input(now, 1, 2, 3);
            let ticket = collector.submit(stamps(now), 1, 1, 7).unwrap();
            collector.complete(ticket, now);
        }
        collector
    }

    #[test]
    fn backpressure_retains_oldest_input_and_latest_cursor() {
        let now = Instant::now();
        let mut collector = warmed(now);
        collector.input(now, 2, 10, 20);
        collector.input(advance(now, 10), 3, 30, 40);
        let ticket = collector.submit(stamps(advance(now, 20)), 2, 5, 9).unwrap();
        assert_eq!(ticket.burst.first, now);
        assert_eq!((ticket.burst.events, ticket.burst.drains), (5, 2));
        assert_eq!((ticket.burst.x, ticket.burst.y), (30, 40));
        collector.complete(ticket, advance(now, 50));
        assert_eq!(collector.completed, 1);
        assert!(!collector.ready());
    }

    #[test]
    fn submission_is_not_completion_and_initial_synchronous_completion_is_counted() {
        let now = Instant::now();
        let mut collector = Collector::new();
        for _ in 0..WARMUP {
            collector.input(now, 1, 2, 3);
            let ticket = collector.submit(stamps(now), 1, 1, 7).unwrap();
            assert!(ticket.warmup);
        }
        collector.input(now, 1, 2, 3);
        assert!(collector.submit(stamps(now), 1, 1, 7).is_none());
        assert_eq!(collector.issued, 0);
        // An initial blocking GPU retirement may complete before present returns.
        let mut collector = warmed(now);
        for id in 0..SAMPLES {
            collector.input(now, 1, 2, 3);
            let ticket = collector.submit(stamps(now), 1, id as u64 + 1, 7).unwrap();
            assert_eq!(ticket.id, id);
            assert_eq!(collector.completed, id);
            collector.complete(ticket, now);
        }
        assert!(collector.ready());
        collector.input(now, 1, 2, 3);
        assert!(collector.submit(stamps(now), 1, 999, 7).is_none());
        assert_eq!(collector.completed, SAMPLES);
    }

    #[test]
    fn missing_and_cancelled_receipts_never_yield_a_successful_batch() {
        let now = Instant::now();
        let mut collector = warmed(now);
        let mut pending = None;
        for _ in 0..SAMPLES {
            collector.input(now, 1, 2, 3);
            let ticket = collector.submit(stamps(now), 1, 1, 7).unwrap();
            if ticket.id == SAMPLES - 1 {
                pending = Some(ticket);
            } else {
                collector.complete(ticket, now);
            }
        }
        assert_eq!(collector.issued, SAMPLES);
        assert_eq!(collector.completed, SAMPLES - 1);
        assert!(!collector.ready()); // Timeout/missing receipt cannot be success.
        assert!(pending.is_some());
        collector.provider_lost(); // Teardown cancels the entire started cohort.
        assert!(collector.ready());
        assert_eq!(collector.failed, 1);
    }

    #[test]
    fn idle_provider_reset_after_warmup_invalidates_the_entire_cohort() {
        let now = Instant::now();
        let mut collector = Collector::new();
        collector.provider_lost();
        assert_eq!(collector.failed, 0); // No measurement has started.
        let mut collector = warmed(now);
        assert!(collector.pending.is_none());
        assert_eq!(collector.warmup_completed, WARMUP);
        assert_eq!(collector.issued, 0);
        collector.provider_lost();
        collector.input(now, 1, 2, 3);
        assert!(collector.submit(stamps(now), 2, 1, 7).is_none());
        assert_eq!(collector.failed, 1);
        assert!(collector.ready()); // Emit FAIL, never 128 fresh-provider rows.
    }

    #[test]
    fn duplicate_and_reversed_receipts_fail_closed() {
        let now = Instant::now();
        let mut collector = warmed(now);
        collector.input(now, 1, 2, 3);
        let ticket = collector.submit(stamps(advance(now, 1)), 1, 1, 7).unwrap();
        collector.complete(ticket, now);
        assert_eq!(collector.failed, 1);
        let mut collector = warmed(now);
        collector.input(now, 1, 2, 3);
        let ticket = collector.submit(stamps(now), 1, 1, 7).unwrap();
        collector.complete(ticket, now);
        collector.complete(ticket, now);
        assert_eq!(collector.failed, 1);
        assert_eq!(collector.completed, 1);
    }
}
