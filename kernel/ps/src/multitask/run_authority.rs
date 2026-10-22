//! Scheduler run-authority divergence telemetry.
//!
//! - **Owner:** nothing. This module holds no ownership state and grants no
//!   authority; it only counts disagreements that the dispatch diagnostic
//!   observes between `runqueue.rs`'s `RunOwnerWord` and the original physical
//!   Current bank.
//! - **Boundary:** `scheduler/runqueue.rs` owns the authoritative owner word
//!   per slot; `scheduler/runtime_profile.rs::observe_original_current` is the
//!   only producer.
//! - **Lifecycle:** an observational profile sweep, never a dispatch grant or
//!   an atomic cross-owner lifetime snapshot. The window is taken by the
//!   profile drain.
//! - **Concurrency:** relaxed counters.
//! - **Failure:** a disagreement is reported, never fatal.
//! - **Forbidden:** no third owner word. A shadow copy of `RunOwnerWord` is
//!   exactly the "shadow ready state" `runqueue.rs` forbids from authorizing
//!   dispatch.
//!
//! The comparison against the retired legacy ready/blocked tables has been
//! removed together with those tables; only the two disagreements that the
//! original-Current observer can still detect remain.

use core::sync::atomic::{AtomicU64, Ordering};

/// How the owner word and the original physical Current disagree about a slot.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub(super) enum Mismatch {
    /// Both sources say the slot is executing, on different CPUs. This is
    /// literally a task running on two CPUs.
    RunningOnDifferentCpus = 1,
    /// The published pair says a CPU is executing the slot and the runqueue
    /// does not own it as running. The owner word would be free to hand the
    /// same slot to another CPU.
    LegacyRunningQueueNot = 6,
    /// Independent physical Current lacks its original UID execution permit.
    /// This observer owns only telemetry, never execution admission.
    CurrentPermitMissing = 8,
}

/// First mismatch of the window, packed, or 0 for none.
static FIRST_MISMATCH: AtomicU64 = AtomicU64::new(0);
static MISMATCH_COUNT: AtomicU64 = AtomicU64::new(0);

const MISMATCH_PRESENT: u64 = 1 << 63;

/// Records one disagreement.
///
/// First rather than last: a later mismatch is usually the wreckage of the
/// first, and the first is the one whose cause is still nearby.
pub(super) fn record(slot: usize, kind: Mismatch) {
    MISMATCH_COUNT.fetch_add(1, Ordering::Relaxed);
    let packed = MISMATCH_PRESENT | ((slot as u64 & 0xFFFF) << 32) | (kind as u64);
    // ORDERING: Relaxed diagnostic scalar only; neither field grants authority.
    // The count and first code may straddle drains on another CPU.
    let _ = FIRST_MISMATCH.compare_exchange(0, packed, Ordering::Relaxed, Ordering::Relaxed);
}

/// Takes the window's record, clearing it for the next window.
pub(super) fn take_window() -> Option<(u64, u64)> {
    let first = FIRST_MISMATCH.swap(0, Ordering::Relaxed);
    let count = MISMATCH_COUNT.swap(0, Ordering::Relaxed);
    (first != 0).then_some((first, count))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_window_reports_the_first_mismatch_with_a_total() {
        assert_eq!(take_window(), None);
        record(9, Mismatch::LegacyRunningQueueNot);
        record(11, Mismatch::RunningOnDifferentCpus);
        let (first, count) = take_window().expect("window recorded");
        assert_eq!((first >> 32) & 0xFFFF, 9);
        assert_eq!(first & 0xFF, Mismatch::LegacyRunningQueueNot as u64);
        assert_eq!(count, 2);
        assert_eq!(take_window(), None);
    }
}
