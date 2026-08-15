//! Bounded per-phase cycle attribution shared by kernel subsystems.
//!
//! - **Owner:** this module owns accumulation and rendering only; every phase
//!   boundary is chosen by the path that charges it.
//! - **Boundary:** counters are diagnostics. Nothing reads them to make a
//!   scheduling, admission, or timeout decision.
//! - **Lifecycle:** charge per phase into fixed atomics, then drain one
//!   fixed-size record set per tick window from housekeeping.
//! - **Concurrency:** Relaxed atomic adds only. A drain that races a charge
//!   loses at most that sample, which cannot corrupt a total.
//! - **Failure:** an unpaired or wrapped sample is discarded rather than
//!   accumulated, so a preempted phase cannot inflate a total.
//! - **Forbidden:** no allocation, no lock, and no debugcon write on the
//!   charge path; rendering happens only in the drain.
//! - **Evidence:** `docs/benchmarks/README.md`.
//!
//! Kernel paths that already had phase boundaries sampled them with the 1024 Hz
//! RTC tick. That is ~1 ms of resolution against phases measured in
//! microseconds, so it can only ever see a stall, never a cost. A profile here
//! sits on the same boundaries with a cycle sample instead.
//!
//! The tick window is supplied by the caller rather than read here, so this
//! module stays free of any clock source dependency and stays testable on the
//! host.

use core::sync::atomic::{AtomicU64, Ordering};

use super::{LogCategory, record_milestone};

/// One sample longer than this is treated as a preemption, not a cost, and is
/// discarded. Instrumented phases are microseconds; a hundred milliseconds of
/// elapsed cycles means the task lost the CPU inside the phase.
pub const MAX_PLAUSIBLE_PHASE_CYCLES: u64 = 400_000_000;

/// Reads the cycle counter for a phase boundary.
#[cfg(target_arch = "x86_64")]
#[inline]
pub fn phase_now() -> u64 {
    // SAFETY: RDTSC has no memory operand and no privilege requirement.
    unsafe { core::arch::x86_64::_rdtsc() }
}

#[cfg(not(target_arch = "x86_64"))]
#[inline]
pub fn phase_now() -> u64 {
    0
}

/// Fixed-width cycle and sample accumulator for one instrumented path.
pub struct PhaseProfile<const N: usize> {
    category: LogCategory,
    names: [&'static str; N],
    discarded_name: &'static str,
    cycles: [AtomicU64; N],
    counts: [AtomicU64; N],
    discarded: AtomicU64,
    last_drain_tick: AtomicU64,
}

impl<const N: usize> PhaseProfile<N> {
    /// Builds a profile whose milestone names are fixed at construction, so a
    /// charge site can never name a phase that the drain does not render.
    pub const fn new(
        category: LogCategory,
        names: [&'static str; N],
        discarded_name: &'static str,
    ) -> Self {
        Self {
            category,
            names,
            discarded_name,
            cycles: [const { AtomicU64::new(0) }; N],
            counts: [const { AtomicU64::new(0) }; N],
            discarded: AtomicU64::new(0),
            last_drain_tick: AtomicU64::new(0),
        }
    }

    /// Charges `phase` with the interval since `since` and returns the boundary
    /// timestamp, so consecutive phases chain without a second counter read.
    ///
    /// An out-of-range `phase` is dropped rather than panicking: this runs on
    /// the syscall path, where a diagnostic must never be able to fault.
    #[inline]
    pub fn charge(&self, phase: usize, since: u64) -> u64 {
        let boundary = phase_now();
        if phase >= N {
            return boundary;
        }
        let elapsed = boundary.wrapping_sub(since);
        if elapsed > MAX_PLAUSIBLE_PHASE_CYCLES {
            self.discarded.fetch_add(1, Ordering::Relaxed);
            return boundary;
        }
        // ORDERING: Relaxed is exact here. These counters order nothing and are
        // published only by the drain's own milestone sequence.
        self.cycles[phase].fetch_add(elapsed, Ordering::Relaxed);
        self.counts[phase].fetch_add(1, Ordering::Relaxed);
        boundary
    }

    /// Emits one fixed record per phase at most once per tick window and clears
    /// the window. Returns the number of records emitted so housekeeping can
    /// count it as work.
    ///
    /// The caller supplies the clock so this module depends on no timer.
    pub fn drain(&self, now_tick: u64, window_ticks: u64) -> usize {
        self.drain_into(now_tick, window_ticks, |category, name, cycles, count| {
            record_milestone(category, name, cycles, count)
        })
    }

    /// The drain with its sink supplied, so a test can observe exactly what a
    /// window emits without publishing into the global milestone ring.
    fn drain_into(
        &self,
        now_tick: u64,
        window_ticks: u64,
        mut emit: impl FnMut(LogCategory, &'static str, u64, u64),
    ) -> usize {
        let window_ticks = window_ticks.max(1);
        let last = self.last_drain_tick.load(Ordering::Relaxed);
        if now_tick.saturating_sub(last) < window_ticks {
            return 0;
        }
        if self
            .last_drain_tick
            .compare_exchange(last, now_tick, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return 0;
        }

        let mut emitted = 0;
        for index in 0..N {
            let cycles = self.cycles[index].swap(0, Ordering::Relaxed);
            let count = self.counts[index].swap(0, Ordering::Relaxed);
            if count == 0 {
                continue;
            }
            // arg0=cycles in the window, arg1=samples. The consumer divides; the
            // kernel must not, because an average loses the sample count that
            // says how many times one operation performed the phase.
            emit(self.category, self.names[index], cycles, count);
            emitted += 1;
        }
        let discarded = self.discarded.swap(0, Ordering::Relaxed);
        if discarded != 0 {
            emit(self.category, self.discarded_name, discarded, 0);
            emitted += 1;
        }
        emitted
    }

    /// Reads a phase total without clearing it. Test-only: the drain is the
    /// sole consumer in a running kernel.
    #[cfg(test)]
    fn totals(&self, phase: usize) -> (u64, u64) {
        (
            self.cycles[phase].load(Ordering::Relaxed),
            self.counts[phase].load(Ordering::Relaxed),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const NAMES: [&str; 3] = ["test-phase-a", "test-phase-b", "test-phase-c"];

    fn profile() -> PhaseProfile<3> {
        PhaseProfile::new(LogCategory::Debug, NAMES, "test-phase-discarded")
    }

    #[test]
    fn a_charge_accumulates_cycles_and_one_sample() {
        let profile = profile();
        let start = phase_now();
        let boundary = profile.charge(1, start);

        let (cycles, count) = profile.totals(1);
        assert_eq!(count, 1);
        assert!(boundary >= start);
        assert!(cycles <= MAX_PLAUSIBLE_PHASE_CYCLES);
    }

    #[test]
    fn an_implausibly_long_sample_is_discarded_instead_of_accumulated() {
        let profile = profile();
        // A `since` far in the past models a phase the task was preempted in.
        let stale = phase_now().wrapping_sub(MAX_PLAUSIBLE_PHASE_CYCLES * 2);
        let _ = profile.charge(0, stale);

        assert_eq!(profile.totals(0), (0, 0));
        assert_eq!(profile.discarded.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn an_out_of_range_phase_is_dropped_rather_than_panicking() {
        let profile = profile();
        let start = phase_now();
        let boundary = profile.charge(99, start);

        assert!(boundary >= start);
        assert_eq!(profile.discarded.load(Ordering::Relaxed), 0);
        for phase in 0..3 {
            assert_eq!(profile.totals(phase), (0, 0));
        }
    }

    #[test]
    fn a_drain_inside_the_window_neither_emits_nor_clears() {
        let profile = profile();
        let _ = profile.charge(2, phase_now());
        profile.last_drain_tick.store(1_000, Ordering::Relaxed);

        assert_eq!(profile.drain_into(1_500, 1_024, |_, _, _, _| {}), 0);
        assert_eq!(profile.totals(2).1, 1);
    }

    #[test]
    fn a_drain_past_the_window_clears_every_phase() {
        let profile = profile();
        let _ = profile.charge(0, phase_now());
        let _ = profile.charge(2, phase_now());
        profile.last_drain_tick.store(1_000, Ordering::Relaxed);

        // Two phases carry samples; the untouched one must not emit a record.
        let mut emitted = alloc::vec::Vec::new();
        assert_eq!(
            profile.drain_into(3_000, 1_024, |_, name, _, count| emitted
                .push((name, count))),
            2
        );
        assert_eq!(emitted, [("test-phase-a", 1), ("test-phase-c", 1)]);
        assert_eq!(profile.totals(0), (0, 0));
        assert_eq!(profile.totals(2), (0, 0));
    }

    #[test]
    fn a_zero_window_still_advances_rather_than_dividing_by_zero() {
        let profile = profile();
        let _ = profile.charge(1, phase_now());

        assert_eq!(profile.drain_into(7, 0, |_, _, _, _| {}), 1);
        assert_eq!(profile.last_drain_tick.load(Ordering::Relaxed), 7);
    }
}
