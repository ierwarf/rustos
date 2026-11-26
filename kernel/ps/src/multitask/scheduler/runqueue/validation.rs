//! Bounds validation shared by runqueue operations.

use nucleus_core::util::lockdep::MAX_TRACKED_CPUS;


#[inline]
pub(super) fn validate_slot(slot: usize) {
    assert!(
        crate::multitask::task_table::try_get(slot).is_some(),
        "scheduler rq slot is not allocated in TaskTable"
    );
}

#[inline]
pub(super) fn validate_cpu(cpu: usize) {
    assert!(cpu < MAX_TRACKED_CPUS, "scheduler rq CPU exceeds capacity");
}
