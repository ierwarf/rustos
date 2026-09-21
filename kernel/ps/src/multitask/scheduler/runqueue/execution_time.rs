//! CPU-local execution-time baseline publication.

use core::sync::atomic::{AtomicU64, Ordering};

use super::MAX_TASK;

static EXEC_START_LOCAL_NS: [AtomicU64; MAX_TASK] = [const { AtomicU64::new(0) }; MAX_TASK];

pub(super) fn clear(slot: usize) {
    set(slot, 0);
}

pub(in crate::multitask) fn value(slot: usize) -> u64 {
    EXEC_START_LOCAL_NS
        .get(slot)
        .expect("scheduler local execution baseline slot exceeds capacity")
        .load(Ordering::Acquire)
}

pub(in crate::multitask) fn set(slot: usize, value: u64) {
    EXEC_START_LOCAL_NS
        .get(slot)
        .expect("scheduler local execution baseline slot exceeds capacity")
        .store(value, Ordering::Release);
}

pub(super) fn reset_before_publication() {
    for baseline in &EXEC_START_LOCAL_NS {
        baseline.store(0, Ordering::Release);
    }
}
