//! Cycle attribution for the tracked spin lock acquire and release path.
//!
//! - **Owner:** this module owns the phase list only; accumulation, discard,
//!   and rendering belong to [`crate::debug::phase_profile`].
//! - **Boundary:** counters are diagnostics. No lockdep decision, ordering
//!   check, or panic condition reads them.
//! - **Lifecycle:** charge per phase, then drain once per second from
//!   housekeeping.
//! - **Concurrency:** Relaxed atomic adds only, so a charge cannot deadlock
//!   against the lock it is measuring.
//! - **Forbidden:** no lock, no allocation, and no debugcon write on the charge
//!   path — any of the three would recurse through the path being measured.
//! - **Evidence:** `docs/benchmarks/README.md`.
//!
//! Every measured operation in this kernel costs ten to twenty thousand cycles
//! where a comparable microkernel spends hundreds, and each one is built from
//! tracked lock acquisitions. Attributing a lock acquisition to its parts
//! separates a cost that is inherent to the protected work from a cost that
//! belongs to the debug instrumentation wrapped around it.

use crate::debug::LogCategory;
use crate::debug::phase_profile::{PhaseProfile, phase_now};

pub(super) const LOCK_PHASE_COUNT: usize = 17;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum LockPhase {
    /// The lockdep graph work before the lock word is touched: IRQ usage
    /// recording, task-owner dependency edges, and the held-class scan.
    BeforeAcquire = 0,
    /// The `try_lock` loop itself. This is the only phase that measures
    /// contention rather than bookkeeping.
    Spin = 1,
    /// The held-stack publication after the lock word is taken.
    AfterAcquire = 2,
    /// Release ownership validation and the lock word handoff.
    Release = 3,
    /// The `CPUID` topology derivation, which is an unconditional VM exit.
    /// A nonzero sample count here means the dense identity map is not
    /// answering and every acquisition is paying an exit.
    HardwareApicId = 4,
    /// IRQ-context classification, the first step of `before_acquire`.
    BeforeIrqUsage = 5,
    /// The task-owned held-stack resolution and its dependency edges. This is
    /// the step that scans `TASK_HELD_STACKS`.
    BeforeTaskEdges = 6,
    /// The CPU-local held-stack recursion check and raw dependency edges.
    BeforeRawEdges = 7,
    /// Release ownership derivation: logical index, architectural identity,
    /// preemption depth, and the admissibility comparison.
    ReleaseIdentity = 8,
    /// Handing the lock word back.
    ReleaseUnlock = 9,
    /// Popping the CPU-local held-class stack.
    ReleaseStack = 10,
    /// The preemption depth decrement and its ownership correspondence check.
    ReleaseEnable = 11,
    /// One `current_cpu_index` derivation, counted but not timed.
    ///
    /// Timing it was tried and rejected: the derivation is cheap and called
    /// often enough that two counter reads per call pushed guest boot past the
    /// display provider's 2500 ms deadline. The sample count still gives the
    /// multiplier the timed phases are paying.
    CpuIndex = 12,
    /// `current_cpu_index` fell back to `CPUID` because this CPU's TSC_AUX
    /// token was unwritten or the token was never admitted.
    ApicFallbackToken = 13,
    /// An APIC-identity query ran before topology publication.
    ApicFallbackUnpublished = 14,
    /// An APIC-identity query named a logical index outside the admitted
    /// topology.
    ApicFallbackRange = 15,
    /// An APIC-identity query found the never-published sentinel in the dense
    /// map.
    ApicFallbackSentinel = 16,
}

static PROFILE: PhaseProfile<LOCK_PHASE_COUNT> = PhaseProfile::new(
    LogCategory::Debug,
    [
        "lock-phase-before-acquire",
        "lock-phase-spin",
        "lock-phase-after-acquire",
        "lock-phase-release",
        "lock-phase-hardware-apic-id",
        "lock-phase-before-irq-usage",
        "lock-phase-before-task-edges",
        "lock-phase-before-raw-edges",
        "lock-phase-release-identity",
        "lock-phase-release-unlock",
        "lock-phase-release-stack",
        "lock-phase-release-enable",
        "lock-phase-cpu-index",
        "lock-phase-apic-fallback-token",
        "lock-phase-apic-fallback-unpublished",
        "lock-phase-apic-fallback-range",
        "lock-phase-apic-fallback-sentinel",
    ],
    "lock-phase-discarded",
);

/// Reads the cycle counter for a phase boundary.
#[inline]
pub(super) fn now() -> u64 {
    phase_now()
}

/// Charges `phase` with the interval since `since` and returns the boundary
/// timestamp, so consecutive phases chain without a second read.
#[inline]
pub(super) fn charge(phase: LockPhase, since: u64) -> u64 {
    PROFILE.charge(phase as usize, since)
}

/// Emits one fixed record per phase at most once per second and clears the
/// window. Returns the number of records emitted so housekeeping can count it
/// as work.
///
/// The caller supplies the tick window because this crate owns no clock.
pub fn drain_lock_profile(now_tick: u64, window_ticks: u64) -> usize {
    PROFILE.drain(now_tick, window_ticks)
}
