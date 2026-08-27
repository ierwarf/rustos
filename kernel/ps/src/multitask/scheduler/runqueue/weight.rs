//! Per-slot fair-share weights outside the lifecycle catalog.
//!
//! Admission writes the immutable-for-one-generation weight before publishing
//! run ownership.  A self-demotion may replace it while the execution owner is
//! current; terminal release clears it before slot reuse.  Queue operations
//! therefore do not need `Scheduler::contexts` merely to preserve CFS share or
//! the trusted System-class admission bit.

use core::sync::atomic::{AtomicU32, Ordering};

use super::{MAX_TASK, RunOwnerState, owner};

static WEIGHTS: [AtomicU32; MAX_TASK] = [const { AtomicU32::new(0) }; MAX_TASK];

pub(in crate::multitask) fn reset_before_publication() {
    for weight in &WEIGHTS {
        weight.store(0, Ordering::Release);
    }
}

pub(in crate::multitask) fn initialize(slot: usize, value: u32) {
    assert_eq!(
        owner(slot).state,
        RunOwnerState::Dormant,
        "scheduler weight initialized after owner publication"
    );
    assert_ne!(value, 0, "scheduler weight is zero");
    WEIGHTS[slot].store(value, Ordering::Release);
}

#[inline]
pub(in crate::multitask) fn value(slot: usize) -> u32 {
    let value = WEIGHTS
        .get(slot)
        .expect("scheduler weight slot exceeds capacity")
        .load(Ordering::Acquire);
    assert_ne!(value, 0, "scheduler live slot has no fair-share weight");
    value
}

#[inline]
pub(in crate::multitask) fn replace(slot: usize, value: u32) {
    assert_ne!(value, 0, "scheduler weight is zero");
    WEIGHTS
        .get(slot)
        .expect("scheduler weight slot exceeds capacity")
        .store(value, Ordering::Release);
}

pub(in crate::multitask) fn clear(slot: usize) {
    WEIGHTS[slot].store(0, Ordering::Release);
}
