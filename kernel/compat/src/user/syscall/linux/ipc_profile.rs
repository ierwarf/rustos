//! Cycle attribution for the synchronous IPC call path.
//!
//! - **Owner:** this module owns the phase list only; accumulation, discard,
//!   and rendering belong to [`nucleus_core::debug::phase_profile`].
//! - **Boundary:** counters are diagnostics. Nothing reads them to make a
//!   scheduling, admission, or timeout decision.
//! - **Lifecycle:** charge per phase, then drain once per second from
//!   housekeeping.
//! - **Failure:** an unpaired or wrapped sample is discarded by the shared
//!   profile rather than accumulated.
//! - **Evidence:** `docs/benchmarks/README.md`.
//!
//! The call path already had these boundaries, but sampled them with
//! `rtc::ticks()` at 1024 Hz. That is ~1 ms of resolution against phases the
//! bench measures in microseconds, so it can only see a stall, never a cost.

use nucleus_core::debug::LogCategory;
use nucleus_core::debug::phase_profile::{PhaseProfile, phase_now};

pub(super) const IPC_CALL_PHASE_COUNT: usize = 12;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum IpcCallPhase {
    /// Copying the request out of user memory.
    CopyRequest = 0,
    /// Deadline derivation, endpoint enqueue, receiver wake, donation bind,
    /// and the synchronous pick hint.
    Enqueue = 1,
    /// Every `take_endpoint_response_for_wait` in the reply wait. The count is
    /// the interesting half: it says how many takes one round trip performs.
    WaitTake = 2,
    /// Arming the block and the reply deadline waiter.
    WaitArm = 3,
    /// Disarming the reply deadline waiter.
    WaitDisarm = 4,
    /// The blocking transition itself: commit, yield, and resume.
    WaitBlocked = 5,
    /// Deadline expiry sampling around the takes.
    WaitDeadlineSample = 6,
    /// Copying the response back into user memory.
    WriteResponse = 7,
    /// Deriving the reply deadline, which probes the service table.
    EnqueueDeadline = 8,
    /// The request buffer allocation alone, split out of `CopyRequest`.
    CopyAlloc = 9,
    /// The IPC runtime's own endpoint enqueue, split out of `Enqueue`.
    EnqueueRuntime = 10,
    /// Receiver wake, donation bind, and the synchronous pick hint, split out
    /// of `Enqueue`.
    EnqueueWake = 11,
}

static PROFILE: PhaseProfile<IPC_CALL_PHASE_COUNT> = PhaseProfile::new(
    LogCategory::Compat,
    [
        "ipc-call-phase-copy-request",
        "ipc-call-phase-enqueue",
        "ipc-call-phase-wait-take",
        "ipc-call-phase-wait-arm",
        "ipc-call-phase-wait-disarm",
        "ipc-call-phase-wait-blocked",
        "ipc-call-phase-wait-deadline-sample",
        "ipc-call-phase-write-response",
        "ipc-call-phase-enqueue-deadline",
        "ipc-call-phase-copy-alloc",
        "ipc-call-phase-enqueue-runtime",
        "ipc-call-phase-enqueue-wake",
    ],
    "ipc-call-phase-discarded",
);

/// Reads the cycle counter for a phase boundary.
#[inline]
pub(super) fn now() -> u64 {
    phase_now()
}

/// Charges `phase` with the interval since `since` and returns the boundary
/// timestamp, so consecutive phases chain without a second read.
#[inline]
pub(super) fn charge(phase: IpcCallPhase, since: u64) -> u64 {
    PROFILE.charge(phase as usize, since)
}

/// Emits one fixed record per phase at most once per second and clears the
/// window. Returns the number of records emitted so housekeeping can count it
/// as work.
pub fn drain_ipc_call_profile() -> usize {
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
            (IpcCallPhase::CopyRequest, 0),
            (IpcCallPhase::Enqueue, 1),
            (IpcCallPhase::WaitTake, 2),
            (IpcCallPhase::WaitArm, 3),
            (IpcCallPhase::WaitDisarm, 4),
            (IpcCallPhase::WaitBlocked, 5),
            (IpcCallPhase::WaitDeadlineSample, 6),
            (IpcCallPhase::WriteResponse, 7),
            (IpcCallPhase::EnqueueDeadline, 8),
            (IpcCallPhase::CopyAlloc, 9),
            (IpcCallPhase::EnqueueRuntime, 10),
            (IpcCallPhase::EnqueueWake, 11),
        ];
        for (phase, index) in phases {
            assert_eq!(phase as usize, index, "phase {phase:?} moved slot");
        }
        assert_eq!(phases.len(), IPC_CALL_PHASE_COUNT);
    }

    #[test]
    fn charging_returns_a_boundary_that_can_chain() {
        let start = now();
        let boundary = charge(IpcCallPhase::WaitTake, start);
        assert!(boundary >= start);
    }
}
