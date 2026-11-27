//! Which retirement rows are not idle, maintained at every row-leaf release.
//!
//! - **Owner:** each row's own leaf: the bit is recomputed from the row's
//!   state while that leaf is still held, so it can never lag a release.
//! - **Boundary:** an observation for scanners, never a claim. Every claimant
//!   still takes the row leaf and re-checks the exact state it needs.
//! - **Invariant:** a row that is claimable by any scanner is not idle, so its
//!   bit is set from the release that made it claimable until the release that
//!   returned it to idle. A row that turns busy after a scanner's snapshot is
//!   seen by the next scan, exactly as with the full walk this replaces.
//! - **Evidence:** the eight-vCPU lock-site census: the four retirement
//!   scanners (cleanup, kernel effects, terminal drain, plus its resource
//!   probe) walked all 127 rows on every futex call and housekeeping turn,
//!   ~0.5M leaf acquisitions a second with nothing to retire.

use core::ops::{Deref, DerefMut};

use super::{CleanupState, KernelState, SlotState};
use crate::multitask::irq_leaf::{Guard, IrqLeaf};
use crate::multitask::task_table;

pub(super) struct StateGuard<'a, const CLASS: u8> {
    slot: Option<usize>,
    guard: Guard<'a, SlotState, CLASS>,
}

impl<const CLASS: u8> Deref for StateGuard<'_, CLASS> {
    type Target = SlotState;
    fn deref(&self) -> &Self::Target {
        &self.guard
    }
}

impl<const CLASS: u8> DerefMut for StateGuard<'_, CLASS> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.guard
    }
}

/// A row leaf whose release republishes the row's busy bit. The owner names
/// the lock class; this wrapper only adds the release-time publication.
pub(in crate::multitask) struct StateLeaf<const CLASS: u8> {
    slot: Option<u32>,
    leaf: IrqLeaf<SlotState, CLASS>,
}

impl<const CLASS: u8> StateLeaf<CLASS> {
    #[cfg(test)]
    pub(super) const fn new() -> Self {
        Self { slot: None, leaf: IrqLeaf::new(SlotState::new()) }
    }
    pub(in crate::multitask) const fn new_for_slot(slot: u32) -> Self {
        Self { slot: Some(slot), leaf: IrqLeaf::new(SlotState::new()) }
    }
    #[track_caller]
    pub(super) fn lock(&self) -> StateGuard<'_, CLASS> {
        StateGuard { slot: self.slot.map(|s| s as usize), guard: self.leaf.lock() }
    }
}

impl<const CLASS: u8> Drop for StateGuard<'_, CLASS> {
    fn drop(&mut self) {
        // Runs before `guard` is released: the bit and the state it summarizes
        // change under the same leaf.
        let Some(slot) = self.slot else { return; };
        let state: &SlotState = &self.guard;
        let idle = state.reset_seal.is_none()
            && state.producer.is_none()
            && !state.producing
            && state.deferred.is_none()
            && state.reason.is_none()
            && state.cleanup == CleanupState::Vacant
            && state.kernel == KernelState::Vacant;
        task_table::set_cleanup_busy(slot, !idle);
        let reap_candidate = slot >= super::FIRST_DYNAMIC_TASK_SLOT
            && state.reset_seal.is_none()
            && state.task_id.is_some()
            && state.producer.is_none()
            && !state.producing
            && state.deferred.is_none()
            && matches!(state.cleanup, CleanupState::Vacant | CleanupState::Complete)
            && matches!(state.kernel, KernelState::Vacant | KernelState::Complete);
        super::super::reaper::set_retired_candidate(slot, reap_candidate);
    }
}

/// Slots whose row was not idle at its last release, ascending.
pub(super) fn slots() -> impl Iterator<Item = usize> {
    task_table::cleanup_busy_slots()
}
