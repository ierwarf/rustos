//! Allocation-free FIFO for exact scheduler handoff custody.
//!
//! - **Owner:** `kernel-ps` owns exact task-slot handoff retention for
//!   supervisor-committed first turns and committed synchronous IPC peers.
//! - **Boundary:** Only a schedulable slot with explicit activation or live
//!   call/reply authority may enter.
//! - **Lifecycle:** Enqueue once, retain across fairness turns, dispatch FIFO,
//!   or remove the exact slot on retirement.
//! - **Concurrency:** The enclosing interrupt-excluded scheduler mutation
//!   serializes every operation.
//! - **Failure:** Capacity contradiction is returned to the scheduler, which
//!   panics rather than losing committed execution authority.
//! - **Forbidden:** No allocation, overwrite, duplicate capacity use, or
//!   survivor reordering.
//! - **Evidence:** `bootstrap-activation-handoff` and
//!   `synchronous-ipc-handoff`.

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct HandoffQueueFull;

pub(super) struct SlotHandoffQueue<const CAPACITY: usize> {
    slots: [Option<usize>; CAPACITY],
    head: usize,
    len: usize,
}

impl<const CAPACITY: usize> SlotHandoffQueue<CAPACITY> {
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

    /// Returns `Ok(true)` for a new entry and `Ok(false)` for an exact
    /// duplicate. Capacity failure is an internal task-accounting
    /// contradiction when `CAPACITY == MAX_TASK`.
    pub(super) fn enqueue(&mut self, slot: usize) -> Result<bool, HandoffQueueFull> {
        if (0..self.len).any(|offset| {
            let index = (self.head + offset) % CAPACITY;
            self.slots[index] == Some(slot)
        }) {
            return Ok(false);
        }
        if self.len >= CAPACITY {
            return Err(HandoffQueueFull);
        }
        let tail = (self.head + self.len) % CAPACITY;
        self.slots[tail] = Some(slot);
        self.len += 1;
        Ok(true)
    }

    pub(super) fn remove(&mut self, slot: usize) {
        let mut compact = [None; CAPACITY];
        let mut retained = 0_usize;
        for offset in 0..self.len {
            let index = (self.head + offset) % CAPACITY;
            if let Some(candidate) = self.slots[index]
                && candidate != slot
            {
                compact[retained] = Some(candidate);
                retained += 1;
            }
        }
        self.slots = compact;
        self.head = 0;
        self.len = retained;
    }

    pub(super) fn pop(&mut self) -> Option<usize> {
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
