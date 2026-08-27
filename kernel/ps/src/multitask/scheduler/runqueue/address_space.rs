//! Per-slot dispatch address-space roots outside the lifecycle catalog.
//!
//! Admission installs a nonzero root before run-owner publication.  Exec may
//! replace it only after its existing exact target-quiescence transaction, and
//! terminal release clears it before the slot can be reused.

use core::sync::atomic::{AtomicU64, Ordering};

use super::{MAX_TASK, RunOwnerState, owner};

static ROOTS: [AtomicU64; MAX_TASK] = [const { AtomicU64::new(0) }; MAX_TASK];

pub(in crate::multitask) fn reset_before_publication() {
    for root in &ROOTS {
        root.store(0, Ordering::Release);
    }
}

pub(in crate::multitask) fn initialize(slot: usize, value: u64) {
    assert_eq!(
        owner(slot).state,
        RunOwnerState::Dormant,
        "scheduler address-space root initialized after owner publication"
    );
    assert_ne!(value, 0, "scheduler address-space root is zero");
    ROOTS[slot].store(value, Ordering::Release);
}

#[inline]
pub(in crate::multitask) fn root(slot: usize) -> u64 {
    ROOTS
        .get(slot)
        .expect("scheduler address-space root slot exceeds capacity")
        .load(Ordering::Acquire)
}

#[inline]
pub(in crate::multitask) fn replace(slot: usize, value: u64) {
    assert_ne!(value, 0, "scheduler address-space root is zero");
    ROOTS
        .get(slot)
        .expect("scheduler address-space root slot exceeds capacity")
        .store(value, Ordering::Release);
}

pub(in crate::multitask) fn clear(slot: usize) {
    ROOTS[slot].store(0, Ordering::Release);
}
