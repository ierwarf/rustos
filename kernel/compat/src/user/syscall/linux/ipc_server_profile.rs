//! Cycle attribution for the synchronous IPC receive/reply path.
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
//! [`ipc_profile`](super::ipc_profile) prices the *caller* half of a
//! synchronous round trip. `ipc_reply_recv.rs`, `syscall_linux_rustos_ipc_recv`,
//! and `syscall_linux_rustos_ipc_reply` charged nothing, which is why the
//! reply-to-return half of a round trip was 94% dark. These four phases mirror
//! the caller's four cleanly-attributable ones (`copy-request`, `enqueue`,
//! `write-response`, `enqueue-deadline`): each is charged exactly once per
//! syscall invocation, never once per retry inside a blocking loop, so each
//! divides into one round trip the same way the caller's four already do.

use nucleus_core::debug::LogCategory;
use nucleus_core::debug::phase_profile::{PhaseProfile, phase_now};

pub(super) const IPC_SERVER_PHASE_COUNT: usize = 4;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum IpcServerPhase {
    /// From a receive syscall's first attempt to the request actually being
    /// taken off the endpoint, including any block/wake cycles in between.
    RecvTake = 0,
    /// Writing the request, reply capability, and sender identity into the
    /// receiver's user memory.
    RecvWrite = 1,
    /// Copying the response out of user memory and publishing it to the
    /// waiting caller's reply slot.
    ReplyPublish = 2,
    /// The donation bind and direct hand-back to the woken caller.
    ReplyWake = 3,
}

static PROFILE: PhaseProfile<IPC_SERVER_PHASE_COUNT> = PhaseProfile::new(
    LogCategory::Compat,
    [
        "ipc-server-phase-recv-take",
        "ipc-server-phase-recv-write",
        "ipc-server-phase-reply-publish",
        "ipc-server-phase-reply-wake",
    ],
    "ipc-server-phase-discarded",
);

/// Reads the cycle counter for a phase boundary.
#[inline]
pub(super) fn now() -> u64 {
    phase_now()
}

/// Charges `phase` with the interval since `since` and returns the boundary
/// timestamp, so consecutive phases chain without a second read.
#[inline]
pub(super) fn charge(phase: IpcServerPhase, since: u64) -> u64 {
    PROFILE.charge(phase as usize, since)
}

/// Emits one fixed record per phase at most once per second and clears the
/// window. Returns the number of records emitted so housekeeping can count it
/// as work.
pub fn drain_ipc_server_profile() -> usize {
    PROFILE.drain(
        crate::arch::rtc::ticks(),
        crate::arch::rtc::ticks_per_second(),
    )
}

/// See `ipc_profile::force_drain_ipc_call_profile`: the same forced,
/// window-bypassing drain, for the receiver's four phases.
pub fn force_drain_ipc_server_profile() -> usize {
    PROFILE.drain(crate::arch::rtc::ticks(), 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_phase_charges_its_own_slot() {
        let phases = [
            (IpcServerPhase::RecvTake, 0),
            (IpcServerPhase::RecvWrite, 1),
            (IpcServerPhase::ReplyPublish, 2),
            (IpcServerPhase::ReplyWake, 3),
        ];
        for (phase, index) in phases {
            assert_eq!(phase as usize, index, "phase {phase:?} moved slot");
        }
        assert_eq!(phases.len(), IPC_SERVER_PHASE_COUNT);
    }

    #[test]
    fn charging_returns_a_boundary_that_can_chain() {
        let start = now();
        let boundary = charge(IpcServerPhase::RecvTake, start);
        assert!(boundary >= start);
    }
}
