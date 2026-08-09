//! Test-only deferred target-reschedule observation and reset support.
//!
//! This module drives the production deferred-flush seam with deterministic
//! online/request state so the live scheduler-guard release path remains
//! directly observable without IRQ or APIC hardware.

use core::sync::atomic::{AtomicU64, Ordering};

static TEST_DEFERRED_TARGET_RESCHEDULE_FLUSH_EPOCH: AtomicU64 = AtomicU64::new(0);
static TEST_DEFERRED_TARGET_ONLINE: AtomicU64 = AtomicU64::new(0);
static TEST_DEFERRED_TARGET_REQUEST_PENDING: AtomicU64 = AtomicU64::new(0);
static TEST_DEFERRED_TARGET_SENT: AtomicU64 = AtomicU64::new(0);

pub(crate) fn flush_deferred_target_reschedules() {
    super::flush_deferred_target_reschedules_with(
        true,
        0,
        |target| {
            (TEST_DEFERRED_TARGET_ONLINE.load(Ordering::Acquire) & (1_u64 << target) != 0)
                .then_some(1)
        },
        |target| {
            (TEST_DEFERRED_TARGET_REQUEST_PENDING.load(Ordering::Acquire) & (1_u64 << target) != 0)
                .then_some(1)
        },
        |target, generation, sequence| {
            assert_eq!(generation, 1);
            assert_eq!(sequence, 1);
            TEST_DEFERRED_TARGET_SENT.fetch_or(1_u64 << target, Ordering::AcqRel);
            TEST_DEFERRED_TARGET_RESCHEDULE_FLUSH_EPOCH.fetch_add(1, Ordering::AcqRel);
        },
    );
}

pub(crate) fn test_deferred_target_reschedule_flush_epoch() -> u64 {
    TEST_DEFERRED_TARGET_RESCHEDULE_FLUSH_EPOCH.load(Ordering::Acquire)
}

pub(crate) fn prepare_test_deferred_target_reschedule(target: usize) {
    assert!(target > 0 && target < nucleus_core::util::lockdep::MAX_TRACKED_CPUS);
    let bit = 1_u64 << target;
    super::TARGET_RESCHEDULE_IPI_PENDING.store(bit, Ordering::Release);
    TEST_DEFERRED_TARGET_ONLINE.store(bit, Ordering::Release);
    TEST_DEFERRED_TARGET_REQUEST_PENDING.store(bit, Ordering::Release);
}

pub(crate) fn test_deferred_target_reschedule_sent_mask() -> u64 {
    TEST_DEFERRED_TARGET_SENT.load(Ordering::Acquire)
}

pub(crate) fn test_deferred_target_reschedule_pending_mask() -> u64 {
    super::TARGET_RESCHEDULE_IPI_PENDING.load(Ordering::Acquire)
}

pub(crate) fn reset_test_deferred_target_reschedule_flush_epoch() {
    super::TARGET_RESCHEDULE_IPI_PENDING.store(0, Ordering::Release);
    TEST_DEFERRED_TARGET_ONLINE.store(0, Ordering::Release);
    TEST_DEFERRED_TARGET_REQUEST_PENDING.store(0, Ordering::Release);
    TEST_DEFERRED_TARGET_SENT.store(0, Ordering::Release);
    TEST_DEFERRED_TARGET_RESCHEDULE_FLUSH_EPOCH.store(0, Ordering::Release);
}
