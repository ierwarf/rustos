//! Cycle attribution for the exact-generation user-memory copy path.
//!
//! - **Owner:** this module owns the phase list only; accumulation, discard,
//!   and rendering belong to [`nucleus_core::debug::phase_profile`].
//! - **Boundary:** counters are diagnostics. Nothing reads them to admit or
//!   reject a copy, and no phase boundary changes the copy's own ordering.
//! - **Lifecycle:** charge per phase, then drain once per second from
//!   housekeeping.
//! - **Failure:** an unpaired or wrapped sample is discarded by the shared
//!   profile rather than accumulated.
//! - **Evidence:** `docs/benchmarks/README.md`.
//!
//! The IPC call profile prices a 16-byte user copy at ~12,000 cycles, which is
//! far more than the bytes cost. A copy does three separable things — bind the
//! caller's address space, admit the page span, then move the bytes — and the
//! call profile charges all three to one phase. These counters split them, so
//! the answer comes from measurement rather than from reading the call graph.

use nucleus_core::debug::LogCategory;
use nucleus_core::debug::phase_profile::{PhaseProfile, phase_now};

pub(crate) const USER_COPY_PHASE_COUNT: usize = 10;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum UserCopyPhase {
    /// Retaining the current process binding and entering its visible state,
    /// before any page of the buffer is looked at.
    ReadBind = 0,
    /// The standalone read admission of the page span.
    ReadValidate = 1,
    /// `copy_from_user`, which re-admits the span and then moves the bytes.
    ReadCopy = 2,
    /// The write direction of `ReadBind`.
    WriteBind = 3,
    /// The standalone write admission of the page span.
    WriteValidate = 4,
    /// `copy_into_user`, which re-admits the span and then moves the bytes.
    WriteCopy = 5,
    /// Reading the current task's user binding out of the scheduler.
    BindIdentity = 6,
    /// Retaining the process, which acquires the global process table.
    BindRetain = 7,
    /// Entering the retained process's visible state, which acquires the
    /// per-process state lock and then re-acquires the global process table to
    /// test visibility.
    BindVisible = 8,
    /// Dropping the retained process, which acquires the global process table
    /// a third time to decrement the reference count.
    BindRelease = 9,
}

static PROFILE: PhaseProfile<USER_COPY_PHASE_COUNT> = PhaseProfile::new(
    LogCategory::Process,
    [
        "usermem-phase-read-bind",
        "usermem-phase-read-validate",
        "usermem-phase-read-copy",
        "usermem-phase-write-bind",
        "usermem-phase-write-validate",
        "usermem-phase-write-copy",
        "usermem-phase-bind-identity",
        "usermem-phase-bind-retain",
        "usermem-phase-bind-visible",
        "usermem-phase-bind-release",
    ],
    "usermem-phase-discarded",
);

/// Reads the cycle counter for a phase boundary.
#[inline]
pub(crate) fn now() -> u64 {
    phase_now()
}

/// Charges `phase` with the interval since `since` and returns the boundary
/// timestamp, so consecutive phases chain without a second read.
#[inline]
pub(crate) fn charge(phase: UserCopyPhase, since: u64) -> u64 {
    PROFILE.charge(phase as usize, since)
}

/// Emits one fixed record per phase at most once per second and clears the
/// window. Returns the number of records emitted so housekeeping can count it
/// as work.
pub fn drain_user_copy_profile() -> usize {
    PROFILE.drain(
        crate::arch::rtc::ticks(),
        crate::arch::rtc::ticks_per_second(),
    )
}

/// Emits and clears the current window immediately, bypassing the one-second
/// gate. See `force_drain_ipc_call_profile` for why an isolated probe needs
/// this instead of waiting for the ordinary housekeeping cadence.
pub fn force_drain_user_copy_profile() -> usize {
    PROFILE.drain(crate::arch::rtc::ticks(), 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_phase_charges_its_own_slot() {
        // The discriminants index the name array, so a reordered enum that
        // silently mislabels every measurement must fail here.
        let phases = [
            (UserCopyPhase::ReadBind, 0),
            (UserCopyPhase::ReadValidate, 1),
            (UserCopyPhase::ReadCopy, 2),
            (UserCopyPhase::WriteBind, 3),
            (UserCopyPhase::WriteValidate, 4),
            (UserCopyPhase::WriteCopy, 5),
            (UserCopyPhase::BindIdentity, 6),
            (UserCopyPhase::BindRetain, 7),
            (UserCopyPhase::BindVisible, 8),
            (UserCopyPhase::BindRelease, 9),
        ];
        for (phase, index) in phases {
            assert_eq!(phase as usize, index, "phase {phase:?} moved slot");
        }
        assert_eq!(phases.len(), USER_COPY_PHASE_COUNT);
    }

    #[test]
    fn charging_returns_a_boundary_that_can_chain() {
        let start = now();
        let boundary = charge(UserCopyPhase::ReadCopy, start);
        assert!(boundary >= start);
    }
}
