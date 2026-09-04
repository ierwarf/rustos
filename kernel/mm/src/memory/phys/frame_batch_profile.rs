//! Bounded, diagnostic-only accounting for batched frame operations.
//!
//! This module is deliberately separate from allocator admission: its atomics
//! cannot allocate, take the allocator lock, or affect a physical-frame result.

use core::sync::atomic::{AtomicU64, Ordering};

/// Count-only workload evidence for the batched allocator. A drain publishes
/// `(frames, lock acquisitions)` for each class.
// ORDERING: Profile counts are diagnostic-only and intentionally approximate;
// the AcqRel drain claim below only elects one emitter for each time window.
struct FrameBatchProfile {
    alloc_frames: AtomicU64,
    alloc_batches: AtomicU64,
    alloc_short: AtomicU64,
    free_frames: AtomicU64,
    free_batches: AtomicU64,
    free_failures: AtomicU64,
    rollback_frames: AtomicU64,
    rollback_batches: AtomicU64,
    rollback_failures: AtomicU64,
    last_drain_tick: AtomicU64,
}

impl FrameBatchProfile {
    const fn new() -> Self {
        Self {
            alloc_frames: AtomicU64::new(0),
            alloc_batches: AtomicU64::new(0),
            alloc_short: AtomicU64::new(0),
            free_frames: AtomicU64::new(0),
            free_batches: AtomicU64::new(0),
            free_failures: AtomicU64::new(0),
            rollback_frames: AtomicU64::new(0),
            rollback_batches: AtomicU64::new(0),
            rollback_failures: AtomicU64::new(0),
            last_drain_tick: AtomicU64::new(0),
        }
    }

    fn record_allocation(&self, requested: usize, filled: usize) {
        self.alloc_frames
            .fetch_add(filled as u64, Ordering::Relaxed);
        self.alloc_batches.fetch_add(1, Ordering::Relaxed);
        if filled < requested {
            self.alloc_short.fetch_add(1, Ordering::Relaxed);
        }
    }

    fn record_free(&self, frames: usize, failed: usize, rollback: bool) {
        let freed = frames.saturating_sub(failed) as u64;
        if rollback {
            self.rollback_frames.fetch_add(freed, Ordering::Relaxed);
            self.rollback_batches.fetch_add(1, Ordering::Relaxed);
            self.rollback_failures
                .fetch_add(failed as u64, Ordering::Relaxed);
        } else {
            self.free_frames.fetch_add(freed, Ordering::Relaxed);
            self.free_batches.fetch_add(1, Ordering::Relaxed);
            self.free_failures
                .fetch_add(failed as u64, Ordering::Relaxed);
        }
    }
}

static FRAME_BATCH_PROFILE: FrameBatchProfile = FrameBatchProfile::new();

pub(super) fn record_allocation(requested: usize, filled: usize) {
    FRAME_BATCH_PROFILE.record_allocation(requested, filled);
}

pub(super) fn record_free(frames: usize, failed: usize, rollback: bool) {
    FRAME_BATCH_PROFILE.record_free(frames, failed, rollback);
}

fn emit_total(name: &'static str, frames: &AtomicU64, batches: &AtomicU64) -> usize {
    let frames = frames.swap(0, Ordering::Relaxed);
    let batches = batches.swap(0, Ordering::Relaxed);
    if frames == 0 && batches == 0 {
        return 0;
    }
    crate::debug::record_milestone(crate::debug::LogCategory::Memory, name, frames, batches);
    1
}

fn emit_scalar(name: &'static str, value: &AtomicU64) -> usize {
    let value = value.swap(0, Ordering::Relaxed);
    if value == 0 {
        return 0;
    }
    crate::debug::record_milestone(crate::debug::LogCategory::Memory, name, value, 0);
    1
}

/// Emits and clears one bounded count window. `window_ticks == 0` forces a
/// drain for an isolated benchmark boundary.
pub(super) fn drain(now_tick: u64, window_ticks: u64) -> usize {
    let last = FRAME_BATCH_PROFILE.last_drain_tick.load(Ordering::Relaxed);
    if window_ticks != 0 && now_tick.saturating_sub(last) < window_ticks {
        return 0;
    }
    // ORDERING: Winning this AcqRel claim elects one drain owner for the
    // window; the count swaps remain diagnostic-only relaxed operations.
    if FRAME_BATCH_PROFILE
        .last_drain_tick
        .compare_exchange(last, now_tick, Ordering::AcqRel, Ordering::Acquire)
        .is_err()
    {
        return 0;
    }

    let mut emitted = 0;
    emitted += emit_total(
        "frame-batch-alloc",
        &FRAME_BATCH_PROFILE.alloc_frames,
        &FRAME_BATCH_PROFILE.alloc_batches,
    );
    emitted += emit_total(
        "frame-batch-free",
        &FRAME_BATCH_PROFILE.free_frames,
        &FRAME_BATCH_PROFILE.free_batches,
    );
    emitted += emit_total(
        "frame-batch-rollback",
        &FRAME_BATCH_PROFILE.rollback_frames,
        &FRAME_BATCH_PROFILE.rollback_batches,
    );
    emitted += emit_scalar("frame-batch-short", &FRAME_BATCH_PROFILE.alloc_short);
    emitted += emit_scalar(
        "frame-batch-free-failure",
        &FRAME_BATCH_PROFILE.free_failures,
    );
    emitted += emit_scalar(
        "frame-batch-rollback-failure",
        &FRAME_BATCH_PROFILE.rollback_failures,
    );
    emitted
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rollback_workload_is_counted_separately_from_ordinary_free() {
        let profile = FrameBatchProfile::new();
        profile.record_allocation(8, 3);
        profile.record_free(3, 0, true);
        profile.record_free(4, 1, false);

        assert_eq!(profile.alloc_frames.load(Ordering::Relaxed), 3);
        assert_eq!(profile.alloc_short.load(Ordering::Relaxed), 1);
        assert_eq!(profile.rollback_frames.load(Ordering::Relaxed), 3);
        assert_eq!(profile.rollback_batches.load(Ordering::Relaxed), 1);
        assert_eq!(profile.rollback_failures.load(Ordering::Relaxed), 0);
        assert_eq!(profile.free_frames.load(Ordering::Relaxed), 3);
        assert_eq!(profile.free_batches.load(Ordering::Relaxed), 1);
        assert_eq!(profile.free_failures.load(Ordering::Relaxed), 1);
    }
}
