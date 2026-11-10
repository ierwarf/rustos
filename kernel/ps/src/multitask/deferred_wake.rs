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

/// Appends one unique task identity to the exact CPU-owned batch and makes the
/// deferral visible to every check that may skip a scheduler entry.
///
/// The batch is drained only by `DispatchEntry` (a scheduler entry on this
/// CPU). The periodic continuation fast return (`irq::retain_periodic_continuation`)
/// and the syscall tail (`irq::claim_interruptible_syscall_reschedule`) decide to
/// skip that entry from the CPU's durable reschedule request bit, not from this
/// batch, so a deferral that published no request could wait indefinitely on a
/// CPU running one compute-bound task or idling. Every deferral therefore
/// publishes this CPU's request after its append, duplicate or not: a request
/// claimed since the original append is always followed by a drain (see
/// `DispatchEntry::from_scope`), but re-publishing keeps that argument local.
///
/// `publish_request` is one CAS loop on this CPU's own cache-padded request
/// word plus a lock-free lifecycle snapshot: no lock, no allocation and no lock
/// order, so it is legal under a raw leaf, with IRQs masked and inside a
/// `DispatchPreemption` scope - every context that defers.
///
/// Callers provide the local-IRQ exclusion that makes the mutable queue access
/// safe. Keeping the batch operation separate makes the static CPU-local
/// storage testable without executing CLI on a host process.
fn defer_and_request(queue: &mut DeferredWakeBatch, cpu: usize, task_id: u64) -> bool {
    let queued = defer_into_queue(queue, cpu, task_id);
    super::irq::request_local_reschedule_on(cpu);
    queued
}

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
        defer_and_request(queue, cpu, task_id)
    })
}

/// Consume only this original masked CPU's mailbox; the scope, not a caller's
/// scalar CPU index, excludes nested writers through the entire take.
/// `None` when nothing is queued: every scheduler entry takes this, and the
/// batch is a kilobyte that an empty take used to zero and copy each time.
pub(super) fn take_masked(
    scope: &nucleus_core::util::lockdep::DispatchPreemption<'_>,
) -> Option<DeferredWakeBatch> {
    assert!(scope.is_masked(), "deferred wake take requires original masked scope");
    let cpu = scope.cpu();
    assert!(cpu < CPU_COUNT, "deferred wake take CPU exceeds capacity");
    // SAFETY: original same-CPU masked custody excludes local queue writers.
    let queue = unsafe { &mut *DEFERRED_WAKES.0[cpu].get() };
    (queue.len != 0).then(|| take_queue(queue))
}

/// Whether this original masked CPU's batch holds a deferral the entry has not
/// taken yet (a wake published by the entry itself deferred another).
pub(super) fn queued_masked(scope: &nucleus_core::util::lockdep::DispatchPreemption<'_>) -> bool {
    assert!(scope.is_masked(), "deferred wake probe requires original masked scope");
    let cpu = scope.cpu();
    assert!(cpu < CPU_COUNT, "deferred wake probe CPU exceeds capacity");
    // SAFETY: original same-CPU masked custody excludes local queue writers;
    // this reads one word of the CPU-owned batch.
    unsafe { (*DEFERRED_WAKES.0[cpu].get()).len != 0 }
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
        defer_and_request(queue, cpu, task_id)
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

    /// A deferral is invisible to the periodic continuation fast return and
    /// the syscall tail unless it publishes this CPU's durable request; before
    /// that publication a compute-bound or idle CPU kept the wake forever.
    #[test]
    fn deferral_publishes_the_request_that_defeats_every_scheduler_skip() {
        let mut queues = TestDeferredWakeQueuesRestore::new();
        let cpu = CPU_COUNT - 3;
        let _ = crate::multitask::reschedule_observation::claim_pending(cpu);
        assert!(!crate::multitask::reschedule_observation::request_pending(cpu));
        assert!(queues.defer(cpu, 0x601));
        assert!(
            crate::multitask::reschedule_observation::request_pending(cpu),
            "a deferred wake must force the next periodic tick / syscall tail into the scheduler"
        );
        let _ = crate::multitask::reschedule_observation::claim_pending(cpu);
    }

    /// The drain and a new deferral are ordered on one CPU: a deferral made by
    /// work running after the drain read the batch (a nested wake inside the
    /// same scheduler entry) re-publishes the request even when the earlier one
    /// was already claimed, so the claim cannot have consumed it unseen.
    #[test]
    fn deferral_racing_after_a_drain_and_claim_republishes_its_request() {
        let mut queues = TestDeferredWakeQueuesRestore::new();
        let cpu = CPU_COUNT - 3;
        assert!(queues.defer(cpu, 0x701));
        let drained = queues.take(cpu);
        assert_eq!(drained.len(), 1);
        assert!(crate::multitask::reschedule_observation::claim_pending(cpu).is_some());
        // A wake deferred after the drain and after the claim (or a duplicate
        // of an entry whose request was claimed) must leave a pending edge.
        assert!(queues.defer(cpu, 0x702));
        assert!(queues.defer(cpu, 0x702));
        assert!(crate::multitask::reschedule_observation::request_pending(cpu));
        assert_eq!(queues.batch(cpu).iter().collect::<std::vec::Vec<_>>(), [0x702]);
        let _ = crate::multitask::reschedule_observation::claim_pending(cpu);
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
