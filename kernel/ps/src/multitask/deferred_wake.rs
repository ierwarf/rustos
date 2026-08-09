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

/// Appends one unique task identity to the exact CPU-owned batch.
///
/// Callers provide the local-IRQ exclusion that makes the mutable queue access
/// safe. Keeping the batch operation separate makes the static CPU-local
/// storage testable without executing CLI on a host process.
fn defer_into_queue(queue: &mut DeferredWakeBatch, cpu: usize, task_id: u64) -> bool {
    assert_ne!(task_id, 0, "deferred scheduler wake requires task identity");
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
}

/// Moves one CPU-owned batch out while replacing it with an empty queue.
fn take_queue(queue: &mut DeferredWakeBatch) -> DeferredWakeBatch {
    core::mem::replace(queue, DeferredWakeBatch::new())
}

pub(super) fn defer_current_cpu(task_id: u64) -> bool {
    interrupts::without_interrupts(|| {
        let cpu = nucleus_core::util::lockdep::current_cpu_index();
        // SAFETY: local interrupts are excluded for this CPU-owned queue.
        let queue = unsafe { &mut *DEFERRED_WAKES.0[cpu].get() };
        defer_into_queue(queue, cpu, task_id)
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
        take_queue(queue)
    })
}

#[cfg(test)]
static TEST_DEFERRED_WAKE_QUEUES_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Serializes host witnesses and restores every CPU-local queue on drop.
///
/// Tests invoke the exact production batch helpers on `DEFERRED_WAKES`, but
/// never enter `without_interrupts`, whose CLI instruction is invalid in a
/// user-mode test process.
#[cfg(test)]
struct TestDeferredWakeQueuesRestore {
    _serial: std::sync::MutexGuard<'static, ()>,
    saved: [DeferredWakeBatch; CPU_COUNT],
}

#[cfg(test)]
impl TestDeferredWakeQueuesRestore {
    fn new() -> Self {
        let serial = TEST_DEFERRED_WAKE_QUEUES_LOCK.lock().unwrap();
        let saved = core::array::from_fn(|cpu| {
            // SAFETY: the test-only serial guard excludes every fixture user,
            // and host test code never enters the production IRQ path.
            unsafe { *DEFERRED_WAKES.0[cpu].get() }
        });
        for cpu in 0..CPU_COUNT {
            // SAFETY: identical to the save above; the original value is
            // restored by this fixture's Drop implementation.
            unsafe { *DEFERRED_WAKES.0[cpu].get() = DeferredWakeBatch::new() };
        }
        Self {
            _serial: serial,
            saved,
        }
    }

    fn defer(&mut self, cpu: usize, task_id: u64) -> bool {
        assert!(cpu < CPU_COUNT, "test deferred wake CPU exceeds capacity");
        // SAFETY: this fixture holds the test serialization guard and owns the
        // saved/restored lifetime of every static CPU-local queue.
        let queue = unsafe { &mut *DEFERRED_WAKES.0[cpu].get() };
        defer_into_queue(queue, cpu, task_id)
    }

    fn take(&mut self, cpu: usize) -> DeferredWakeBatch {
        assert!(cpu < CPU_COUNT, "test deferred wake CPU exceeds capacity");
        // SAFETY: see `defer`; this calls the exact production drain helper.
        let queue = unsafe { &mut *DEFERRED_WAKES.0[cpu].get() };
        take_queue(queue)
    }

    fn batch(&self, cpu: usize) -> DeferredWakeBatch {
        assert!(cpu < CPU_COUNT, "test deferred wake CPU exceeds capacity");
        // SAFETY: the fixture has exclusive access and only returns a Copy
        // snapshot, so the queue cannot escape mutable ownership.
        unsafe { *DEFERRED_WAKES.0[cpu].get() }
    }
}

#[cfg(test)]
impl Drop for TestDeferredWakeQueuesRestore {
    fn drop(&mut self) {
        for cpu in 0..CPU_COUNT {
            // SAFETY: this fixture still owns the serial guard while restoring
            // the exact pre-test static queue image.
            unsafe { *DEFERRED_WAKES.0[cpu].get() = self.saved[cpu] };
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deferred_wake_batch_preserves_bounded_fifo_order() {
        let mut batch = DeferredWakeBatch::new();
        batch.tasks[0] = 11;
        batch.tasks[1] = 17;
        batch.len = 2;
        assert_eq!(batch.iter().collect::<std::vec::Vec<_>>(), [11, 17]);
    }

    #[test]
    fn deferred_wake_drain_clears_exact_cpu_queue() {
        let mut queues = TestDeferredWakeQueuesRestore::new();
        let target_cpu = 0;
        let other_cpu = 1;
        assert!(queues.defer(target_cpu, 0x301));
        assert!(queues.defer(target_cpu, 0x302));
        assert!(queues.defer(other_cpu, 0x401));

        let drained = queues.take(target_cpu);
        assert_eq!(drained.iter().collect::<std::vec::Vec<_>>(), [0x301, 0x302]);
        assert_eq!(drained.len(), 2);
        assert_eq!(queues.batch(target_cpu).len(), 0);
        assert_eq!(
            queues.batch(other_cpu).iter().collect::<std::vec::Vec<_>>(),
            [0x401],
            "draining one CPU must not consume another CPU's deferred wake"
        );
    }

    #[test]
    fn deferred_wake_deduplicates_one_cpu() {
        let mut queues = TestDeferredWakeQueuesRestore::new();
        assert!(queues.defer(0, 0x501));
        assert!(
            queues.defer(0, 0x501),
            "a duplicate enqueue is absorbed by its CPU-local queue"
        );
        assert!(
            queues.defer(1, 0x501),
            "a different CPU owns an independent deferred-wake authority"
        );
        assert_eq!(
            queues.batch(0).iter().collect::<std::vec::Vec<_>>(),
            [0x501]
        );
        assert_eq!(
            queues.batch(1).iter().collect::<std::vec::Vec<_>>(),
            [0x501]
        );
    }
}
