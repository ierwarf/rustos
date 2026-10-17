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

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct HandoffQueueFull;

pub(super) trait HandoffEntry: Copy {
    fn slot(self) -> usize;
}

impl HandoffEntry for usize {
    fn slot(self) -> usize {
        self
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
}

pub(super) struct SlotHandoffQueue<const CAPACITY: usize, T: HandoffEntry = usize> {
    slots: [Option<T>; CAPACITY],
    head: usize,
    len: usize,
}

impl<const CAPACITY: usize, T: HandoffEntry> SlotHandoffQueue<CAPACITY, T> {
    pub(super) const fn new() -> Self {
        Self {
            slots: [None; CAPACITY],
            head: 0,
            len: 0,
        }
    }

    pub(super) const fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Returns `Ok(true)` for a new slot and `Ok(false)` for a duplicate
    /// slot, even if its recorded task ID differs. Retirement must first
    /// remove stale entries before reuse. Capacity failure is an internal task-accounting
    /// contradiction when `CAPACITY == MAX_TASK`.
    pub(super) fn enqueue(&mut self, entry: T) -> Result<bool, HandoffQueueFull> {
        if (0..self.len).any(|offset| {
            let index = (self.head + offset) % CAPACITY;
            self.slots[index].is_some_and(|candidate| candidate.slot() == entry.slot())
        }) {
            return Ok(false);
        }
        if self.len >= CAPACITY {
            return Err(HandoffQueueFull);
        }
        let tail = (self.head + self.len) % CAPACITY;
        self.slots[tail] = Some(entry);
        self.len += 1;
        Ok(true)
    }

    pub(super) fn remove(&mut self, slot: usize) -> bool {
        let mut compact = [None; CAPACITY];
        let mut retained = 0_usize;
        for offset in 0..self.len {
            let index = (self.head + offset) % CAPACITY;
            if let Some(candidate) = self.slots[index]
                && candidate.slot() != slot
            {
                compact[retained] = Some(candidate);
                retained += 1;
            }
        }
        let removed = retained != self.len;
        self.slots = compact;
        self.head = 0;
        self.len = retained;
        removed
    }

    /// Discharge one exact ordering promise after an independently owned claim.
    /// A different incarnation of the same slot is not the completed promise.
    /// Shift only the occupied suffix: empty/missed removals do no array copy.
    pub(super) fn remove_exact(&mut self, entry: T) -> bool
    where T: PartialEq {
        let Some(offset) = (0..self.len).find(|offset| {
            self.slots[(self.head + offset) % CAPACITY] == Some(entry)
        }) else { return false; };
        for next in offset + 1..self.len {
            let from = (self.head + next) % CAPACITY;
            let to = (self.head + next - 1) % CAPACITY;
            self.slots[to] = self.slots[from];
        }
        self.slots[(self.head + self.len - 1) % CAPACITY] = None;
        self.len -= 1;
        true
    }

    /// Observe without consuming: incomplete original cohort/frame custody must
    /// retain its FIFO place and pending prefix until qualification completes.
    pub(super) fn front(&self) -> Option<T> {
        if self.len == 0 { None } else { self.slots[self.head] }
    }

    pub(super) fn pop(&mut self) -> Option<T> {
        if self.len == 0 {
            return None;
        }
        let index = self.head;
        let slot = self.slots[index].take();
        self.head = (index + 1) % CAPACITY;
        self.len -= 1;
        slot
    }

    #[cfg(test)]
    pub(super) fn len(&self) -> usize {
        self.len
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
        let mut queue: SlotHandoffQueue<3> = SlotHandoffQueue::new();
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
        let mut exact = SlotHandoffQueue::<3, super::FirstTurnHandoff>::new();
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
