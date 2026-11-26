//! Allocation-free FIFO for exact scheduler handoff custody.
//!
//! - **Owner:** `kernel-ps` owns exact task-slot handoff retention for
//!   supervisor-committed first turns and committed synchronous IPC peers.
//! - **Boundary:** Only a schedulable slot with explicit activation or live
//!   call/reply authority may enter.
//! - **Lifecycle:** Enqueue once, retain across fairness turns, dispatch FIFO,
//!   or remove the exact slot on retirement.
//! - **Concurrency:** The owning per-CPU Policy leaf serializes queue mutations;
//!   complete dispatch choices additionally retain independent priority custody.
//! - **Failure:** Capacity contradiction is returned to the scheduler, which
//!   panics rather than losing committed execution authority.
//! - **Forbidden:** No allocation, overwrite, duplicate capacity use, or
//!   survivor reordering.
//! - **Evidence:** `bootstrap-activation-handoff` and
//!   `synchronous-ipc-handoff`.
use crate::multitask::irq_pool::{SlabPool, SlabQueue};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct HandoffQueueFull;

pub(super) trait HandoffEntry: Copy + 'static {
    fn slot(self) -> usize;
    fn slab_pool() -> &'static SlabPool<Self>;
}

impl HandoffEntry for usize {
    fn slot(self) -> usize {
        self
    }

    fn slab_pool() -> &'static SlabPool<Self> {
        static POOL: SlabPool<usize> = SlabPool::new();
        &POOL
    }
}

/// Exact first-turn task identity for an ordinary spawn or an atomically
/// activated cohort; a reused slot cannot inherit the former owner's priority.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct FirstTurnHandoff {
    pub(super) slot: usize,
    pub(super) task_id: u64,
}

impl HandoffEntry for FirstTurnHandoff {
    fn slot(self) -> usize {
        self.slot
    }

    fn slab_pool() -> &'static SlabPool<Self> {
        static POOL: SlabPool<FirstTurnHandoff> = SlabPool::new();
        &POOL
    }
}

pub(super) fn preallocate_first_turn_pool(min_free: usize) {
    SlabQueue::<FirstTurnHandoff>::preallocate_for_pool(FirstTurnHandoff::slab_pool(), min_free);
}

pub(super) struct SlotHandoffQueue<T: HandoffEntry = usize> {
    queue: SlabQueue<T>,
    capacity: usize,
}

impl<T: HandoffEntry> SlotHandoffQueue<T> {
    /// An unbounded slab-backed queue; grows in normal task creation without
    /// allocating while policy/IRQ locks are held.
    pub(super) const fn new() -> Self {
        Self {
            queue: SlabQueue::new(),
            capacity: usize::MAX,
        }
    }

    #[cfg(test)]
    pub(super) fn with_capacity(capacity: usize) -> Self {
        let mut queue = Self::new();
        queue.resize(capacity);
        queue
    }

    /// Sets an explicit cap on queued entries (e.g. bounded atomic activation bursts).
    pub(super) fn resize(&mut self, capacity: usize) {
        assert!(self.queue.is_empty(), "handoff queue resized while holding entries");
        self.capacity = capacity;
    }

    /// Drops every entry and releases its slab blocks back to the pool.
    pub(super) fn clear(&mut self) {
        self.queue.clear(T::slab_pool());
    }

    pub(super) const fn is_empty(&self) -> bool {
        self.queue.is_empty()
    }

    /// Returns `Ok(true)` for a new slot and `Ok(false)` for a duplicate
    /// slot, even if its recorded task ID differs. Retirement must first
    /// remove stale entries before reuse.
    pub(super) fn enqueue(&mut self, entry: T) -> Result<bool, HandoffQueueFull> {
        let pool = T::slab_pool();
        if self.queue.any(pool, |candidate| candidate.slot() == entry.slot()) {
            return Ok(false);
        }
        if self.queue.len() >= self.capacity || !self.queue.push_back(pool, entry) {
            return Err(HandoffQueueFull);
        }
        Ok(true)
    }

    pub(super) fn remove(&mut self, slot: usize) -> bool {
        let pool = T::slab_pool();
        self.queue.retain(pool, |candidate| candidate.slot() != slot) != 0
    }

    /// Discharge one exact ordering promise after an independently owned claim.
    /// A different incarnation of the same slot is not the completed promise.
    pub(super) fn remove_exact(&mut self, entry: T) -> bool
    where T: PartialEq {
        let pool = T::slab_pool();
        self.queue.pop_first_matching(pool, |candidate| *candidate == entry).is_some()
    }

    /// Observe without consuming: incomplete original cohort/frame custody must
    /// retain its FIFO place and pending prefix until qualification completes.
    pub(super) fn front(&self) -> Option<T> {
        self.queue.front(T::slab_pool())
    }

    pub(super) fn pop(&mut self) -> Option<T> {
        self.queue.pop_front(T::slab_pool())
    }

    #[cfg(test)]
    pub(super) fn len(&self) -> usize {
        self.queue.len()
    }
}

#[cfg(test)]
mod tests {
    use super::{HandoffQueueFull, SlotHandoffQueue};

    /// Direct white-box coverage for the generic FIFO custody primitive,
    /// independent of any one scheduler-level handoff queue instantiation.
    /// Integration paths (spawn/sync/atomic-activation handoffs) exercise
    /// this type indirectly but do not necessarily drive it to capacity or
    /// inspect its length after every `remove`, so this test owns those
    /// exact boundary and post-compaction invariants.
    #[test]
    fn slot_handoff_queue_is_fifo_deduplicated_bounded_and_compacts_on_remove() {
        let mut queue: SlotHandoffQueue = SlotHandoffQueue::with_capacity(3);
        assert!(queue.is_empty());

        assert_eq!(queue.enqueue(10), Ok(true));
        assert_eq!(queue.enqueue(10), Ok(false));
        assert_eq!(queue.len(), 1);
        assert_eq!(queue.enqueue(20), Ok(true));
        assert_eq!(queue.enqueue(30), Ok(true));
        assert_eq!(queue.len(), 3);

        // Capacity exhaustion is reported, not silently dropped or overwritten.
        assert_eq!(queue.enqueue(40), Err(HandoffQueueFull));
        assert_eq!(queue.len(), 3);

        assert_eq!(queue.pop(), Some(10));
        assert_eq!(queue.len(), 2);

        // `remove` must compact both the retained entries and the length so a
        // long-running retire/re-admit cycle cannot leak phantom occupancy
        // that eventually starves every future handoff with a false "full".
        assert_eq!(queue.enqueue(40), Ok(true));
        queue.remove(30);
        assert_eq!(queue.len(), 2);
        assert_eq!(queue.pop(), Some(20));
        assert_eq!(queue.pop(), Some(40));
        assert_eq!(queue.pop(), None);
        assert!(queue.is_empty());

        // Exact removal preserves wrap-around order, head and tail. In contrast
        // to terminal slot removal it cannot discharge a foreign incarnation.
        let mut exact = SlotHandoffQueue::<super::FirstTurnHandoff>::with_capacity(3);
        let record = |slot, task_id| super::FirstTurnHandoff { slot, task_id };
        assert!(!exact.remove_exact(record(1, 10)));
        exact.enqueue(record(1, 10)).unwrap();
        exact.enqueue(record(2, 20)).unwrap();
        exact.enqueue(record(3, 30)).unwrap();
        assert_eq!(exact.pop(), Some(record(1, 10)));
        exact.enqueue(record(4, 40)).unwrap();
        assert!(!exact.remove_exact(record(2, 21)));
        assert_eq!(exact.len(), 3);
        assert!(exact.remove_exact(record(3, 30)));
        assert_eq!(exact.front(), Some(record(2, 20)));
        assert!(exact.remove_exact(record(4, 40)));
        assert!(exact.remove_exact(record(2, 20)));
        assert!(exact.is_empty());
        assert!(!exact.remove_exact(record(2, 20)));

        // Removing an absent slot is a no-op, not a corruption.
        assert_eq!(queue.enqueue(50), Ok(true));
        queue.remove(999);
        assert_eq!(queue.len(), 1);
        assert_eq!(queue.pop(), Some(50));
    }
}
