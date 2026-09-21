//! Generation-reset idle classification for local fair selection.
//!
//! This tag is advisory only: the owner word remains dispatch authority. It is
//! cleared before a retired slot becomes reusable, so an idle slot cannot leak
//! its classification to the next task lifetime.

use core::sync::atomic::{AtomicBool, Ordering};

use super::MAX_TASK;

static IDLE_SLOT: [AtomicBool; MAX_TASK] = [const { AtomicBool::new(false) }; MAX_TASK];

/// Publishes immutable-for-one-slot-lifetime idle classification before a
/// local picker can observe the task.
pub(in crate::multitask) fn set(slot: usize, idle: bool) {
    IDLE_SLOT
        .get(slot)
        .expect("scheduler idle slot exceeds capacity")
        .store(idle, Ordering::Release);
}

pub(in crate::multitask) fn is_idle(slot: usize) -> bool {
    IDLE_SLOT
        .get(slot)
        .is_some_and(|idle| idle.load(Ordering::Acquire))
}

pub(super) fn clear(slot: usize) {
    IDLE_SLOT
        .get(slot)
        .expect("scheduler idle slot exceeds capacity")
        .store(false, Ordering::Release);
}

pub(super) fn reset_before_publication() {
    for idle in &IDLE_SLOT {
        idle.store(false, Ordering::Release);
    }
}
