//! Bounds validation shared by runqueue operations.

use nucleus_core::util::lockdep::MAX_TRACKED_CPUS;

use super::MAX_TASK;

#[inline]
pub(super) fn bitmap_location(slot: usize) -> (usize, u64) {
    assert!(slot < MAX_TASK, "scheduler rq slot exceeds capacity");
    (slot / 64, 1_u64 << (slot % 64))
}

#[inline]
pub(super) fn validate_cpu(cpu: usize) {
    assert!(cpu < MAX_TRACKED_CPUS, "scheduler rq CPU exceeds capacity");
}
