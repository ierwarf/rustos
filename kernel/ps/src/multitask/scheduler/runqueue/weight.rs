//! Per-slot fair-share weights outside the lifecycle catalog.
//!
//! Admission writes the immutable-for-one-generation weight before publishing
//! run ownership.  A self-demotion may replace it while the execution owner is
//! current; terminal release clears it before slot reuse.  Queue operations
//! therefore do not need `Scheduler::contexts` merely to preserve CFS share or
//! the trusted System-class admission bit.

use core::sync::atomic::{AtomicU64, Ordering};

use super::{RunOwnerState, owner};
use crate::multitask::task_table;

/// Monotonic epoch bumped whenever any live task's base weight/class changes
/// (`replace` or live `clear`). Read-only during steady-state dispatch.
static BASE_WEIGHT_CLASS_EPOCH: AtomicU64 = AtomicU64::new(0);

#[inline]
pub(super) fn base_class_epoch() -> u64 {
    // ORDERING: Acquire pairs with AcqRel bumps in `replace`/`clear`.
    BASE_WEIGHT_CLASS_EPOCH.load(Ordering::Acquire)
}

pub(in crate::multitask) fn reset_before_publication() {
    for slot in task_table::allocated_slots() {
        task_table::get(slot).weight.store(0, Ordering::Release);
    }
    BASE_WEIGHT_CLASS_EPOCH.fetch_add(1, Ordering::AcqRel);
}

pub(in crate::multitask) fn initialize(slot: usize, value: u32) {
    assert_eq!(
        owner(slot).state,
        RunOwnerState::Dormant,
        "scheduler weight initialized after owner publication"
    );
    assert_ne!(value, 0, "scheduler weight is zero");
    task_table::get(slot).weight.store(value, Ordering::Release);
}

/// Policy observation only: terminal withdrawal may clear a sampled bitmap
/// member before its class scan. Absence is incomplete, never a User class or
/// a new lifetime/claim grant. Actual queue effects require original custody.
#[inline]
pub(in crate::multitask::scheduler) fn observe(slot: usize) -> Option<u32> {
    let value = task_table::try_get(slot)?.weight.load(Ordering::Acquire);
    (value != 0).then_some(value)
}

#[inline]
pub(in crate::multitask) fn value(slot: usize) -> u32 {
    let value = observe(slot).unwrap_or(0);
    assert_ne!(value, 0, "scheduler live slot has no fair-share weight");
    value
}

#[inline]
pub(in crate::multitask) fn replace(slot: usize, value: u32) {
    assert_ne!(value, 0, "scheduler weight is zero");
    super::super::donation_ledger::publish_base_class(|| {
        task_table::get(slot).weight.store(value, Ordering::Release);
        BASE_WEIGHT_CLASS_EPOCH.fetch_add(1, Ordering::AcqRel);
    });
    super::note_slot_resync_if_local(slot);
}

pub(in crate::multitask) fn clear(slot: usize) {
    task_table::get(slot).weight.store(0, Ordering::Release);
    let snap = owner(slot);
    if snap.state != RunOwnerState::Dormant && !snap.state.is_terminal() {
        BASE_WEIGHT_CLASS_EPOCH.fetch_add(1, Ordering::AcqRel);
        if let Some(cpu) = snap.cpu {
            super::note_overdue_resync_needed(cpu);
        }
    }
}
