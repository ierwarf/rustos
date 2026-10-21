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
use core::sync::atomic::{AtomicU64, Ordering};

use super::super::MAX_TASK;
use super::{CleanupState, KernelState, SlotState};
use crate::multitask::irq_leaf::{Guard, IrqLeaf};

const WORDS: usize = MAX_TASK.div_ceil(64);
static BUSY: [AtomicU64; WORDS] = [const { AtomicU64::new(0) }; WORDS];

/// A row leaf whose release republishes the row's busy bit. The owner names
/// the lock class; this wrapper only adds the release-time publication.
pub(super) struct StateLeaf<const CLASS: u8> {
    leaf: IrqLeaf<SlotState, CLASS>,
}

impl<const CLASS: u8> StateLeaf<CLASS> {
    pub(super) const fn new() -> Self {
        Self { leaf: IrqLeaf::new(SlotState::new()) }
    }
    #[track_caller]
    pub(super) fn lock(&self) -> StateGuard<'_, CLASS> {
        StateGuard { slot: slot_of(self), guard: self.leaf.lock() }
    }
}

/// The row index of a leaf inside `CELLS`; `None` for a standalone native
/// fixture cell, which no scanner can reach.
fn slot_of<const CLASS: u8>(leaf: &StateLeaf<CLASS>) -> Option<usize> {
    let base = super::CELLS.as_ptr() as usize;
    let offset = (leaf as *const StateLeaf<CLASS> as usize).checked_sub(base)?;
    let index = offset / core::mem::size_of::<super::CleanupCell>();
    (index < MAX_TASK).then_some(index)
}

pub(super) struct StateGuard<'a, const CLASS: u8> {
    slot: Option<usize>,
    guard: Guard<'a, SlotState, CLASS>,
}

impl<const CLASS: u8> Deref for StateGuard<'_, CLASS> {
    type Target = SlotState;
    fn deref(&self) -> &SlotState {
        &self.guard
    }
}

impl<const CLASS: u8> DerefMut for StateGuard<'_, CLASS> {
    fn deref_mut(&mut self) -> &mut SlotState {
        &mut self.guard
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
        let word = &BUSY[slot / 64];
        let bit = 1_u64 << (slot % 64);
        // ORDERING: Relaxed read only to skip a no-op RMW; the RMWs are Release
        // so a scanner's Acquire snapshot orders before its own leaf takes.
        let set = word.load(Ordering::Relaxed) & bit != 0;
        if idle && set {
            word.fetch_and(!bit, Ordering::Release);
        } else if !idle && !set {
            word.fetch_or(bit, Ordering::Release);
        }
    }
}

/// Slots whose row was not idle at its last release, ascending.
pub(super) fn slots() -> impl Iterator<Item = usize> {
    (0..WORDS).flat_map(|index| {
        // ORDERING: Acquire pairs with the Release publication in `Drop`.
        let mut bits = BUSY[index].load(Ordering::Acquire);
        core::iter::from_fn(move || {
            if bits == 0 {
                return None;
            }
            let bit = bits.trailing_zeros() as usize;
            bits &= bits - 1;
            Some(index * 64 + bit)
        })
    })
}
