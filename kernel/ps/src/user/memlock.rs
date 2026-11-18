//! Per-process `mlock` ledger: which user ranges are locked, and what
//! `MCL_FUTURE` asked of mappings that do not exist yet.
//!
//! - **Owner:** `kernel-ps` owns the ledger in `UserProcessState`; the limit
//!   and the caller's privilege are policy and arrive as arguments.
//! - **Truthfulness:** a lock is a promise that the pages stay resident. This
//!   kernel never evicts, swaps or otherwise reclaims resident user pages (the
//!   only memory-pressure action is the OOM watchdog killing a whole process),
//!   so every resident anonymous, private-file or memfd page already keeps that
//!   promise; the ledger records the promise so `RLIMIT_MEMLOCK`, `VmLck` and
//!   `munlock` can behave as Linux's do.
//! - **Boundary:** ranges are page-aligned user addresses already validated as
//!   mapped by the caller; the ledger does not know the VMA table.
//! - **Lifecycle:** `munmap` and a moving `mremap` call [`MemlockLedger::unlock`]
//!   / [`MemlockLedger::relocate`] under the same process-state lock as the PTE
//!   edit, so a lock never outlives or trails its mapping. Fork starts empty
//!   (Linux clears `VM_LOCKED` in the child) and exec replaces the state.
//! - **Concurrency:** mutated only under the process-state lock.
//! - **Failure:** admission checks the limit and reserves storage before any
//!   change, so a refused lock changes nothing.
//! - **Forbidden:** no accounting for pages that were never admitted, no
//!   double counting of an already locked page, no limit bypass but privilege.

use alloc::vec::Vec;

/// Most disjoint locked islands one process may hold. Linux bounds the same
/// thing through `vm.max_map_count` (a lock splits VMAs); without a bound a
/// process could lock every other page and make the ledger as large as its
/// address space.
pub const MAX_LOCKED_ISLANDS: usize = 16 * 1024;

/// `mlockall` bookkeeping for mappings created later.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct FutureLock {
    /// `MCL_ONFAULT`: lock on first touch rather than at creation.
    pub on_fault: bool,
    /// `RLIMIT_MEMLOCK` in bytes as of the `mlockall` call; `u64::MAX` when the
    /// caller was privileged.
    pub limit_bytes: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LockRefusal {
    /// The cumulative locked total would exceed the limit (`ENOMEM`).
    Limit,
    /// The ledger could not allocate its own storage (`ENOMEM`).
    NoMemory,
}

#[derive(Clone, Debug, Default)]
pub struct MemlockLedger {
    /// Ascending, disjoint, page-aligned half-open ranges; adjacent ranges are
    /// kept merged so the count is bounded by the number of locked islands.
    ranges: Vec<(u64, u64)>,
    locked_bytes: u64,
    future: Option<FutureLock>,
}

impl MemlockLedger {
    pub const fn new() -> Self {
        Self {
            ranges: Vec::new(),
            locked_bytes: 0,
            future: None,
        }
    }

    pub fn locked_bytes(&self) -> u64 {
        self.locked_bytes
    }

    pub fn ranges(&self) -> &[(u64, u64)] {
        &self.ranges
    }

    pub fn future(&self) -> Option<FutureLock> {
        self.future
    }

    pub fn set_future(&mut self, future: Option<FutureLock>) {
        self.future = future;
    }

    /// Bytes of `[start, end)` that are not locked yet.
    pub fn unlocked_bytes_in(&self, start: u64, end: u64) -> u64 {
        if start >= end {
            return 0;
        }
        let first = self.ranges.partition_point(|&(_, range_end)| range_end <= start);
        let mut covered = 0_u64;
        for &(range_start, range_end) in &self.ranges[first..] {
            if range_start >= end {
                break;
            }
            covered += range_end.min(end) - range_start.max(start);
        }
        (end - start) - covered
    }

    pub fn locked_bytes_in(&self, start: u64, end: u64) -> u64 {
        if start >= end {
            0
        } else {
            (end - start) - self.unlocked_bytes_in(start, end)
        }
    }

    /// Whether the page containing `address` is locked.
    pub fn is_locked(&self, address: u64) -> bool {
        let index = self.ranges.partition_point(|&(_, range_end)| range_end <= address);
        self.ranges
            .get(index)
            .is_some_and(|&(range_start, _)| range_start <= address)
    }

    /// Locks `[start, end)`; returns the bytes newly locked. Atomic: either the
    /// whole range is locked and charged, or nothing changes.
    pub fn lock(&mut self, start: u64, end: u64, limit_bytes: u64) -> Result<u64, LockRefusal> {
        if start >= end {
            return Ok(0);
        }
        let newly = self.unlocked_bytes_in(start, end);
        if newly == 0 {
            return Ok(0);
        }
        if self.locked_bytes.saturating_add(newly) > limit_bytes {
            return Err(LockRefusal::Limit);
        }
        if self.ranges.len() + 2 > MAX_LOCKED_ISLANDS {
            return Err(LockRefusal::NoMemory);
        }
        self.ranges
            .try_reserve(2)
            .map_err(|_| LockRefusal::NoMemory)?;
        self.insert_merged(start, end);
        self.locked_bytes += newly;
        Ok(newly)
    }

    /// Unlocks `[start, end)`; returns the bytes released. Never allocates more
    /// than one split record, reserved before any change.
    pub fn unlock(&mut self, start: u64, end: u64) -> Result<u64, LockRefusal> {
        let held = self.locked_bytes_in(start, end);
        if held == 0 {
            return Ok(0);
        }
        if self.ranges.len() + 1 > MAX_LOCKED_ISLANDS {
            return Err(LockRefusal::NoMemory);
        }
        self.ranges
            .try_reserve(1)
            .map_err(|_| LockRefusal::NoMemory)?;
        self.remove_range(start, end);
        self.locked_bytes -= held;
        Ok(held)
    }

    /// `munmap` of `[start, end)`: the mapping is gone, so its locks are too. A
    /// split needs one more record than the ledger may be able to allocate; the
    /// lock is dropped regardless (a lock without a mapping is meaningless), so
    /// a failed reserve falls back to dropping the whole intersecting island.
    pub fn release_mapping(&mut self, start: u64, end: u64) {
        if self.locked_bytes_in(start, end) == 0 {
            return;
        }
        if self.ranges.try_reserve(1).is_err() {
            // Cannot split: also drop the surviving remainder of any island that
            // straddles a boundary, over-releasing rather than leaving a lock
            // on memory that is gone. Never reached below the kernel heap limit.
            let doomed: Vec<(u64, u64)> = self
                .ranges
                .iter()
                .copied()
                .filter(|&(s, e)| s < end && start < e)
                .collect();
            self.ranges.retain(|&(s, e)| !(s < end && start < e));
            self.locked_bytes -= doomed.iter().map(|&(s, e)| e - s).sum::<u64>();
            return;
        }
        let held = self.locked_bytes_in(start, end);
        self.remove_range(start, end);
        self.locked_bytes -= held;
    }

    /// Reserves the records a later [`Self::relocate`] of `[old_start, old_end)`
    /// needs, so that call cannot fail once page tables have moved.
    pub fn reserve_relocation(&mut self, old_start: u64, old_end: u64) -> Result<(), LockRefusal> {
        let pieces = self
            .ranges
            .iter()
            .filter(|&&(s, e)| s < old_end && old_start < e)
            .count();
        if pieces == 0 {
            return Ok(());
        }
        if self.ranges.len() + pieces + 2 > MAX_LOCKED_ISLANDS {
            return Err(LockRefusal::NoMemory);
        }
        self.ranges
            .try_reserve(pieces + 2)
            .map_err(|_| LockRefusal::NoMemory)
    }

    /// `mremap` of `[old_start, old_end)` to `new_start`: the locked part of the
    /// moved range follows it, keeping the cumulative total unchanged. The
    /// destination must be disjoint from the source and hold no lock.
    pub fn relocate(
        &mut self,
        old_start: u64,
        old_end: u64,
        new_start: u64,
    ) -> Result<(), LockRefusal> {
        let pieces: Vec<(u64, u64)> = self
            .ranges
            .iter()
            .copied()
            .filter(|&(s, e)| s < old_end && old_start < e)
            .map(|(s, e)| (s.max(old_start), e.min(old_end)))
            .collect();
        if pieces.is_empty() {
            return Ok(());
        }
        // One split at each edge of the source plus every translated piece.
        if self.ranges.len() + pieces.len() + 2 > MAX_LOCKED_ISLANDS {
            return Err(LockRefusal::NoMemory);
        }
        self.ranges
            .try_reserve(pieces.len() + 2)
            .map_err(|_| LockRefusal::NoMemory)?;
        self.remove_range(old_start, old_end);
        for (s, e) in pieces {
            let (ns, ne) = if new_start >= old_start {
                (s + (new_start - old_start), e + (new_start - old_start))
            } else {
                (s - (old_start - new_start), e - (old_start - new_start))
            };
            self.insert_merged(ns, ne);
        }
        Ok(())
    }

    fn remove_range(&mut self, start: u64, end: u64) {
        // The caller reserved the split records on `self.ranges`, so the
        // in-place removals and insertions below never allocate.
        let mut index = 0;
        while index < self.ranges.len() {
            let (s, e) = self.ranges[index];
            if e <= start || s >= end {
                index += 1;
                continue;
            }
            let left = (s < start).then_some((s, start));
            let right = (e > end).then_some((end, e));
            self.ranges.remove(index);
            let mut at = index;
            if let Some(piece) = left {
                self.ranges.insert(at, piece);
                at += 1;
            }
            if let Some(piece) = right {
                self.ranges.insert(at, piece);
                at += 1;
            }
            index = at;
        }
    }

    fn insert_merged(&mut self, start: u64, end: u64) {
        let mut merged_start = start;
        let mut merged_end = end;
        let first = self.ranges.partition_point(|&(_, e)| e < start);
        let mut last = first;
        while last < self.ranges.len() && self.ranges[last].0 <= end {
            merged_start = merged_start.min(self.ranges[last].0);
            merged_end = merged_end.max(self.ranges[last].1);
            last += 1;
        }
        self.ranges.drain(first..last);
        self.ranges.insert(first, (merged_start, merged_end));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const P: u64 = 4096;

    #[test]
    fn admission_is_cumulative_and_never_double_counts() {
        let mut ledger = MemlockLedger::new();
        assert_eq!(ledger.lock(0x10 * P, 0x14 * P, 8 * P), Ok(4 * P));
        // Overlapping re-lock only charges the new tail.
        assert_eq!(ledger.lock(0x12 * P, 0x16 * P, 8 * P), Ok(2 * P));
        assert_eq!(ledger.locked_bytes(), 6 * P);
        assert_eq!(ledger.ranges(), [(0x10 * P, 0x16 * P)]);
        // Locking what is already locked is free even at the limit.
        assert_eq!(ledger.lock(0x10 * P, 0x16 * P, 6 * P), Ok(0));
        // Exceeding the limit refuses and changes nothing.
        assert_eq!(ledger.lock(0x30 * P, 0x33 * P, 8 * P), Err(LockRefusal::Limit));
        assert_eq!(ledger.locked_bytes(), 6 * P);
        assert_eq!(ledger.ranges(), [(0x10 * P, 0x16 * P)]);
        // Exactly at the limit is allowed; privileged callers pass u64::MAX.
        assert_eq!(ledger.lock(0x30 * P, 0x32 * P, 8 * P), Ok(2 * P));
        assert_eq!(ledger.lock(0x40 * P, 0x80 * P, u64::MAX), Ok(0x40 * P));
    }

    #[test]
    fn adjacent_locks_merge_and_unlock_splits() {
        let mut ledger = MemlockLedger::new();
        ledger.lock(0x10 * P, 0x12 * P, u64::MAX).unwrap();
        ledger.lock(0x12 * P, 0x14 * P, u64::MAX).unwrap();
        assert_eq!(ledger.ranges(), [(0x10 * P, 0x14 * P)]);
        assert_eq!(ledger.unlock(0x11 * P, 0x13 * P), Ok(2 * P));
        assert_eq!(ledger.ranges(), [(0x10 * P, 0x11 * P), (0x13 * P, 0x14 * P)]);
        assert_eq!(ledger.locked_bytes(), 2 * P);
        assert!(ledger.is_locked(0x10 * P));
        assert!(!ledger.is_locked(0x11 * P));
        assert!(ledger.is_locked(0x13 * P + 5));
        // Unlocking unlocked memory (or a hole) is a successful no-op.
        assert_eq!(ledger.unlock(0x50 * P, 0x60 * P), Ok(0));
        assert_eq!(ledger.unlock(0x10 * P, 0x14 * P), Ok(2 * P));
        assert_eq!(ledger.locked_bytes(), 0);
        assert!(ledger.ranges().is_empty());
    }

    #[test]
    fn unmap_releases_exactly_the_unmapped_locks() {
        let mut ledger = MemlockLedger::new();
        ledger.lock(0x10 * P, 0x18 * P, u64::MAX).unwrap();
        ledger.release_mapping(0x12 * P, 0x14 * P);
        assert_eq!(ledger.ranges(), [(0x10 * P, 0x12 * P), (0x14 * P, 0x18 * P)]);
        assert_eq!(ledger.locked_bytes(), 6 * P);
        ledger.release_mapping(0x0 * P, 0x100 * P);
        assert_eq!(ledger.locked_bytes(), 0);
        assert!(ledger.ranges().is_empty());
    }

    #[test]
    fn relocation_carries_the_locked_part_and_keeps_the_total() {
        let mut ledger = MemlockLedger::new();
        ledger.lock(0x12 * P, 0x14 * P, u64::MAX).unwrap();
        ledger.lock(0x30 * P, 0x31 * P, u64::MAX).unwrap();
        // Move pages [0x10, 0x18) to 0x80: only the locked pair inside follows.
        assert_eq!(ledger.relocate(0x10 * P, 0x18 * P, 0x80 * P), Ok(()));
        assert_eq!(ledger.ranges(), [(0x30 * P, 0x31 * P), (0x82 * P, 0x84 * P)]);
        assert_eq!(ledger.locked_bytes(), 3 * P);
        // Downward move, and a source holding no lock changes nothing.
        assert_eq!(ledger.relocate(0x82 * P, 0x84 * P, 0x20 * P), Ok(()));
        assert_eq!(ledger.ranges(), [(0x20 * P, 0x22 * P), (0x30 * P, 0x31 * P)]);
        assert_eq!(ledger.relocate(0x500 * P, 0x510 * P, 0x600 * P), Ok(()));
        assert_eq!(ledger.locked_bytes(), 3 * P);
    }

    #[test]
    fn island_count_is_bounded() {
        let mut ledger = MemlockLedger::new();
        let mut accepted = 0_u64;
        // Every other page: no merging, one island each.
        while ledger
            .lock((2 * accepted + 16) * P, (2 * accepted + 17) * P, u64::MAX)
            .is_ok()
        {
            accepted += 1;
            assert!(accepted <= MAX_LOCKED_ISLANDS as u64);
        }
        assert!(ledger.ranges().len() <= MAX_LOCKED_ISLANDS);
        assert_eq!(ledger.locked_bytes(), accepted * P);
        // A refusal changed nothing, and unmapping always works (it only shrinks).
        let before = ledger.locked_bytes();
        ledger.release_mapping(0, u64::MAX);
        assert_eq!(ledger.locked_bytes(), 0);
        assert!(before > 0);
    }

    #[test]
    fn future_lock_is_recorded_and_clearable() {
        let mut ledger = MemlockLedger::new();
        assert_eq!(ledger.future(), None);
        let future = FutureLock { on_fault: true, limit_bytes: 16 * P };
        ledger.set_future(Some(future));
        assert_eq!(ledger.future(), Some(future));
        ledger.set_future(None);
        assert_eq!(ledger.future(), None);
    }
}
