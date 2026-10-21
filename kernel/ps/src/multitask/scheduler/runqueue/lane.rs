//! Owner-word access for the IPC lane, split from `runqueue.rs` for size.
//!
//! - **Owner:** the lane settles the resident cell of the generation it leaves.
//! - **Evidence:** `irq_entry_tests/lane.rs`.
use super::*;

/// Owner CAS for the IPC lane, without the residency eviction hook. The caller
/// settles the resident cell of `expected.generation` itself (it takes it).
pub(in crate::multitask::scheduler) fn lane_cas(
    slot: usize, expected: RunOwnerSnapshot, next: RunOwnerSnapshot,
) -> Result<(), RunOwnerSnapshot> {
    OWNER_WORDS[slot].compare_exchange_raw(expected, next)
}

impl RunOwnerSnapshot {
    /// The Running claim of this owner on `cpu`: next generation, runnable,
    /// wait cleared. Same value `claim_dispatch_inner` publishes.
    pub(in crate::multitask::scheduler) fn lane_running(self, cpu: usize) -> Self {
        self.next(RunOwnerState::Running, Some(cpu))
    }
}
