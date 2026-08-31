//! Cycle attribution for the syscall entry and exit path.
//!
//! - **Owner:** this module owns the phase list only; accumulation, discard,
//!   and rendering belong to [`nucleus_core::debug::phase_profile`].
//! - **Boundary:** counters are diagnostics. No admission, termination, or
//!   SYSRET decision reads them.
//! - **Lifecycle:** charge per phase, then drain once per second from
//!   housekeeping.
//! - **Failure:** an unpaired or wrapped sample is discarded by the shared
//!   profile rather than accumulated.
//! - **Evidence:** `docs/benchmarks/README.md`.
//!
//! `null_syscall_getpid` costs ~3,400 cycles for a call that reads a per-CPU
//! published identity and takes no lock at all. Every syscall pays that, the
//! IPC ones included, so it is worth knowing which step of the common path
//! spends it rather than inferring from the call graph.

use nucleus_core::debug::LogCategory;
use nucleus_core::debug::phase_profile::PhaseProfile;

pub(super) const SYSCALL_PHASE_COUNT: usize = 4;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum SyscallPhase {
    /// `validate_syscall_entry_or_terminate`. Charged on entry and again
    /// before SYSRET, so the sample count is twice the syscall count.
    Validate = 0,
    /// Entry and exit tracing, likewise charged twice.
    Trace = 1,
    /// The ABI dispatch and the syscall body itself.
    Dispatch = 2,
    /// The deferred-reschedule software interrupt that keeps a hot syscall
    /// from starving the scheduler.
    RescheduleDeferred = 3,
}

// `Syscall` is compiled out by `config/rustos.toml` (`syscall = "off"`), which
// silently drops every record in that category. These live under `Compat`,
// which is where the code is and which the build leaves on.
static PROFILE: PhaseProfile<SYSCALL_PHASE_COUNT> = PhaseProfile::new(
    LogCategory::Compat,
    [
        "syscall-phase-validate",
        "syscall-phase-trace",
        "syscall-phase-dispatch",
        "syscall-phase-reschedule-deferred",
    ],
    "syscall-phase-discarded",
);

/// Reads the cycle counter for a phase boundary.
///
/// Compiled out unless `[syscall_telemetry] phase_profile` is on. The call
/// sites stay unconditional -- only the clock read and the accumulator go --
/// exactly as the lock and scheduler switches do it.
#[inline]
pub(super) fn now() -> u64 {
    #[cfg(rustos_syscall_phase_profile)]
    {
        phase_now()
    }
    #[cfg(not(rustos_syscall_phase_profile))]
    {
        0
    }
}

/// Charges `phase` with the interval since `since` and returns the boundary
/// timestamp, so consecutive phases chain without a second read.
#[inline]
pub(super) fn charge(phase: SyscallPhase, since: u64) -> u64 {
    #[cfg(rustos_syscall_phase_profile)]
    {
        PROFILE.charge(phase as usize, since)
    }
    #[cfg(not(rustos_syscall_phase_profile))]
    {
        let _ = (phase, since);
        0
    }
}

/// Emits one fixed record per phase at most once per second and clears the
/// window. Returns the number of records emitted so housekeeping can count it
/// as work.
pub fn drain_syscall_profile() -> usize {
    #[cfg(not(rustos_syscall_phase_profile))]
    {
        return 0;
    }
    #[cfg(rustos_syscall_phase_profile)]
    PROFILE.drain(
        crate::arch::rtc::ticks(),
        crate::arch::rtc::ticks_per_second(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_phase_charges_its_own_slot() {
        // The discriminants index the name array, so a reordered enum that
        // silently mislabels every measurement must fail here.
        let phases = [
            (SyscallPhase::Validate, 0),
            (SyscallPhase::Trace, 1),
            (SyscallPhase::Dispatch, 2),
            (SyscallPhase::RescheduleDeferred, 3),
        ];
        for (phase, index) in phases {
            assert_eq!(phase as usize, index, "phase {phase:?} moved slot");
        }
        assert_eq!(phases.len(), SYSCALL_PHASE_COUNT);
    }
}
