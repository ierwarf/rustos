//! Hierarchical kernel-object byte charge groups, one counter set per class.
//!
//! - **Owner:** `kernel-mm` holds the counters; `kernel-ps` maps each job to one
//!   group (the same index as `charge`) and each kernel object owner charges
//!   and refunds its own allocation.
//! - **Design:** the same lock-free shape as Linux's `page_counter`: a charge
//!   speculatively adds to the group and every ancestor and, if any limit is
//!   exceeded, subtracts what it added and fails. There is no lock, so a
//!   charge or refund is legal under any lock order and in drop glue. Admission
//!   may let concurrent charges overshoot a limit transiently by the bytes in
//!   flight (bounded by the CPU count times one object); refund never fails.
//! - **Lifecycle:** a group stays valid after its job dies until usage drains
//!   to zero, which the job layer waits for before reusing the slot, so a
//!   late refund always finds the group it charged.
//! - **Forbidden:** allocating, taking a lock, sleeping, or refunding bytes
//!   that were not charged to the same group.
use core::sync::atomic::{AtomicU32, AtomicU64, Ordering};

use super::charge::{CHARGE_GROUPS, UNLIMITED};

/// Kernel allocation classes with their own limit.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(usize)]
pub enum Class {
    /// Slots of a process's dynamic descriptor (handle) table.
    HandleTable = 0,
    /// Reserved queue storage of a process-owned IPC endpoint.
    IpcEndpoint = 1,
}
pub const CLASSES: usize = 2;
const NO_PARENT: u32 = u32::MAX;
const MAX_DEPTH: usize = 9;

struct Group {
    usage: AtomicU64,
    limit: AtomicU64,
    parent: AtomicU32,
}

impl Group {
    const fn new() -> Self {
        Self {
            usage: AtomicU64::new(0),
            limit: AtomicU64::new(UNLIMITED),
            parent: AtomicU32::new(NO_PARENT),
        }
    }
}

static GROUPS: [[Group; CHARGE_GROUPS]; CLASSES] =
    [const { [const { Group::new() }; CHARGE_GROUPS] }; CLASSES];

fn group(class: Class, index: usize) -> Option<&'static Group> {
    GROUPS.get(class as usize)?.get(index)
}

/// Makes `index` a fresh group below `parent` for every class, with no limit.
/// Usage is left alone: bytes of a previous tenant may still be draining.
pub fn init_group(index: usize, parent: Option<usize>) {
    for class in 0..CLASSES {
        let Some(cell) = GROUPS[class].get(index) else { continue };
        cell.limit.store(UNLIMITED, Ordering::Relaxed);
        cell.parent
            .store(parent.map_or(NO_PARENT, |p| p as u32), Ordering::Relaxed);
    }
}

pub fn set_limit(class: Class, index: usize, bytes: u64) {
    if let Some(cell) = group(class, index) {
        cell.limit.store(bytes, Ordering::Relaxed);
    }
}

pub fn limit(class: Class, index: usize) -> u64 {
    group(class, index).map_or(UNLIMITED, |cell| cell.limit.load(Ordering::Relaxed))
}

pub fn usage(class: Class, index: usize) -> u64 {
    group(class, index).map_or(0, |cell| cell.usage.load(Ordering::Relaxed))
}

/// Total bytes charged in any class; the job layer reuses a slot only at zero.
pub fn total_usage(index: usize) -> u64 {
    (0..CLASSES)
        .filter_map(|class| GROUPS[class].get(index))
        .map(|cell| cell.usage.load(Ordering::Relaxed))
        .fold(0, u64::saturating_add)
}

/// Charges `bytes` to the group and every ancestor, or charges nothing.
pub fn try_charge(class: Class, index: usize, bytes: u64) -> bool {
    if bytes == 0 {
        return true;
    }
    let mut visited: [Option<&'static Group>; MAX_DEPTH] = [None; MAX_DEPTH];
    let mut count = 0;
    let mut cursor = index;
    let mut ok = true;
    for _ in 0..MAX_DEPTH {
        let Some(cell) = group(class, cursor) else { break };
        let old = cell.usage.fetch_add(bytes, Ordering::Relaxed);
        visited[count] = Some(cell);
        count += 1;
        let limit = cell.limit.load(Ordering::Relaxed);
        if limit != UNLIMITED && old.saturating_add(bytes) > limit {
            ok = false;
            break;
        }
        let parent = cell.parent.load(Ordering::Relaxed);
        if parent == NO_PARENT {
            break;
        }
        cursor = parent as usize;
    }
    if !ok {
        for cell in visited[..count].iter().flatten() {
            cell.usage.fetch_sub(bytes, Ordering::Relaxed);
        }
    }
    ok
}

/// Returns `bytes` to the group and every ancestor.
pub fn uncharge(class: Class, index: usize, bytes: u64) {
    if bytes == 0 {
        return;
    }
    let mut cursor = index;
    for _ in 0..MAX_DEPTH {
        let Some(cell) = group(class, cursor) else { return };
        let previous = cell.usage.fetch_sub(bytes, Ordering::Relaxed);
        assert!(previous >= bytes, "kmem charge group usage underflow");
        let parent = cell.parent.load(Ordering::Relaxed);
        if parent == NO_PARENT {
            return;
        }
        cursor = parent as usize;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    // Process-wide statics: every test owns distinct group indices.
    const C: Class = Class::HandleTable;

    #[test]
    fn charge_propagates_and_refund_restores_every_ancestor() {
        init_group(50, None);
        init_group(51, Some(50));
        assert!(try_charge(C, 51, 96));
        assert_eq!((usage(C, 51), usage(C, 50)), (96, 96));
        assert_eq!(total_usage(50), 96);
        uncharge(C, 51, 96);
        assert_eq!((usage(C, 51), usage(C, 50), total_usage(50)), (0, 0, 0));
    }

    #[test]
    fn a_refused_charge_rolls_back_every_level_it_touched() {
        init_group(52, None);
        init_group(53, Some(52));
        init_group(54, Some(53));
        set_limit(C, 52, 100);
        assert!(try_charge(C, 54, 60));
        assert!(!try_charge(C, 54, 60), "the root's limit binds the leaf");
        assert_eq!((usage(C, 54), usage(C, 53), usage(C, 52)), (60, 60, 60));
        assert!(try_charge(C, 54, 40));
        uncharge(C, 54, 100);
        assert_eq!(total_usage(52), 0);
    }

    #[test]
    fn a_child_limit_binds_without_touching_the_parent() {
        init_group(55, None);
        init_group(56, Some(55));
        set_limit(C, 56, 10);
        assert!(!try_charge(C, 56, 11));
        assert_eq!((usage(C, 56), usage(C, 55)), (0, 0));
        assert_eq!(limit(C, 56), 10);
    }

    #[test]
    fn reinitialising_a_group_drops_its_limit_but_keeps_draining_usage() {
        init_group(57, None);
        assert!(try_charge(C, 57, 8));
        set_limit(C, 57, 4);
        init_group(57, None);
        assert_eq!(limit(C, 57), UNLIMITED);
        assert_eq!(usage(C, 57), 8);
        uncharge(C, 57, 8);
    }
}
