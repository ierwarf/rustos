//! Per-slot dispatch address-space roots outside the lifecycle catalog.
//!
//! Admission installs a nonzero root before run-owner publication.  Exec may
//! replace it only after its existing exact target-quiescence transaction, and
//! terminal release clears it before the slot can be reused.

use core::sync::atomic::Ordering;

use super::{RunOwnerState, owner};
use crate::multitask::task_table;

pub(in crate::multitask) fn reset_before_publication() {
    for slot in task_table::allocated_slots() {
        task_table::get(slot)
            .address_space_root
            .store(0, Ordering::Release);
    }
}

pub(in crate::multitask) fn initialize(slot: usize, value: u64) {
    assert_eq!(
        owner(slot).state,
        RunOwnerState::Dormant,
        "scheduler address-space root initialized after owner publication"
    );
    assert_ne!(value, 0, "scheduler address-space root is zero");
    task_table::get(slot)
        .address_space_root
        .store(value, Ordering::Release);
}

#[inline]
pub(in crate::multitask) fn root(slot: usize) -> u64 {
    task_table::get(slot)
        .address_space_root
        .load(Ordering::Acquire)
}

#[inline]
pub(in crate::multitask) fn replace(slot: usize, value: u64) {
    super::super::outgoing_image::assert_mutable(slot);
    assert_ne!(value, 0, "scheduler address-space root is zero");
    task_table::get(slot)
        .address_space_root
        .store(value, Ordering::Release);
}

pub(in crate::multitask) fn clear(slot: usize) {
    super::super::outgoing_image::assert_mutable(slot);
    task_table::get(slot)
        .address_space_root
        .store(0, Ordering::Release);
}
