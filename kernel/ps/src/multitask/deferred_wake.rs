//! CPU-local deferred scheduler wakes for raw-lock/IRQ contexts.
//!
//! An interrupt may arrive while the interrupted task owns any raw leaf lock.
//! Such an IRQ cannot acquire the global scheduler lock without creating an
//! inverse dependency or dispatching the interrupted raw owner. It records a
//! bounded, deduplicated wake intent here; the next safe scheduler admission
//! drains the exact CPU's queue.

use core::cell::UnsafeCell;

use x86_64::instructions::interrupts;

use super::MAX_SCHEDULER_TASKS;

const CPU_COUNT: usize = nucleus_core::util::lockdep::MAX_TRACKED_CPUS;

#[derive(Clone, Copy)]
pub(super) struct DeferredWakeBatch {
    tasks: [u64; MAX_SCHEDULER_TASKS],
    len: usize,
}

impl DeferredWakeBatch {
    const fn new() -> Self {
        Self {
            tasks: [0; MAX_SCHEDULER_TASKS],
            len: 0,
        }
    }

    pub(super) fn iter(&self) -> impl Iterator<Item = u64> + '_ {
        self.tasks[..self.len].iter().copied()
    }

    pub(super) fn len(&self) -> usize {
        self.len
    }
}

struct DeferredWakeQueues([UnsafeCell<DeferredWakeBatch>; CPU_COUNT]);

// SAFETY: only the indexed CPU accesses its queue, and every mutation excludes
// local interrupts. Running kernel frames never migrate while a raw lock is
// held, so another CPU cannot alias the same cell.
unsafe impl Sync for DeferredWakeQueues {}

static DEFERRED_WAKES: DeferredWakeQueues =
    DeferredWakeQueues([const { UnsafeCell::new(DeferredWakeBatch::new()) }; CPU_COUNT]);

pub(super) fn defer_current_cpu(task_id: u64) -> bool {
    assert_ne!(task_id, 0, "deferred scheduler wake requires task identity");
    interrupts::without_interrupts(|| {
        let cpu = nucleus_core::util::lockdep::current_cpu_index();
        // SAFETY: local interrupts are excluded for this CPU-owned queue.
        let queue = unsafe { &mut *DEFERRED_WAKES.0[cpu].get() };
        if queue.tasks[..queue.len].contains(&task_id) {
            return true;
        }
        assert!(
            queue.len < queue.tasks.len(),
            "deferred scheduler wake queue exhausted cpu={} task_id={}",
            cpu,
            task_id
        );
        queue.tasks[queue.len] = task_id;
        queue.len += 1;
        true
    })
}

pub(super) fn take_current_cpu(logical_index: usize) -> DeferredWakeBatch {
    assert!(
        logical_index < CPU_COUNT,
        "deferred scheduler wake drain CPU exceeds capacity"
    );
    interrupts::without_interrupts(|| {
        // SAFETY: the indexed CPU is the caller and local interrupts are
        // excluded across the complete take-and-clear transition.
        let queue = unsafe { &mut *DEFERRED_WAKES.0[logical_index].get() };
        core::mem::replace(queue, DeferredWakeBatch::new())
    })
}

#[cfg(test)]
mod tests {
    use super::DeferredWakeBatch;

    #[test]
    fn deferred_wake_batch_preserves_bounded_fifo_order() {
        let mut batch = DeferredWakeBatch::new();
        batch.tasks[0] = 11;
        batch.tasks[1] = 17;
        batch.len = 2;
        assert_eq!(batch.iter().collect::<std::vec::Vec<_>>(), [11, 17]);
    }
}
