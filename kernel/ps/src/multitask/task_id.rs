//! Monotonic scheduler task identity allocation.
//!
//! Task ids never wrap or recycle. Slot generations protect storage reuse;
//! this counter protects every cross-subsystem reference to a task lifetime.

use core::sync::atomic::{AtomicU64, Ordering};

pub(super) static NEXT_TASK_ID: AtomicU64 = AtomicU64::new(0);

pub(super) fn allocate_task_id_from(counter: &AtomicU64) -> Option<u64> {
    counter
        .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |current| {
            current.checked_add(1)
        })
        .ok()
}

pub(super) fn allocate_task_id() -> Option<u64> {
    allocate_task_id_from(&NEXT_TASK_ID)
}
