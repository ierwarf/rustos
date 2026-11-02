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
    /// Page-table frames (the root and every lazily published table) of a
    /// bound address space.
    PageTable = 2,
    /// Kernel stack and scheduler record of a user thread.
    Thread = 3,
    /// Per-mapping bookkeeping nodes (shared memfd and shared-region mappings).
    Vma = 4,
    /// A queued synchronous-call message and its request/handle storage,
    /// billed to the sending process's Job.
    IpcMessage = 5,
}
pub const CLASSES: usize = 6;
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

/// Whether `bytes` more would fit under every limit on the path to the root,
/// charging nothing. Advisory, like `charge::would_admit`.
pub fn would_admit(class: Class, index: usize, bytes: u64) -> bool {
    let mut cursor = index;
    for _ in 0..MAX_DEPTH {
        let Some(cell) = group(class, cursor) else { return true };
        let limit = cell.limit.load(Ordering::Relaxed);
        if limit != UNLIMITED && cell.usage.load(Ordering::Relaxed).saturating_add(bytes) > limit {
            return false;
        }
        let parent = cell.parent.load(Ordering::Relaxed);
        if parent == NO_PARENT {
            return true;
        }
        cursor = parent as usize;
    }
    true
}

/// A kernel object owner's running charge in one class: the pattern every
/// owner of variable-size kernel state follows. The meter is bound to a Job
/// group once; `set_total`/`set_total_forced` keep the group's usage equal to
/// the owner's current byte total, and dropping the meter refunds whatever it
/// still holds. A clone starts unbound and holds nothing, so duplicating the
/// owner never double-refunds.
#[derive(Debug)]
pub struct Meter {
    class: Class,
    group: Option<u32>,
    charged: u64,
}

impl Meter {
    pub const fn new(class: Class) -> Self {
        Self { class, group: None, charged: 0 }
    }

    pub fn is_bound(&self) -> bool {
        self.group.is_some()
    }

    pub fn charged(&self) -> u64 {
        self.charged
    }

    /// Binds once to `group` and charges `total` bytes against its limits;
    /// refused (nothing charged, still unbound) if they do not fit.
    pub fn bind(&mut self, group: usize, total: u64) -> bool {
        if self.group.is_some() || !try_charge(self.class, group, total) {
            return false;
        }
        self.group = Some(group as u32);
        self.charged = total;
        true
    }

    /// Whether a total of `total` bytes would be admitted. Unbound meters
    /// admit everything.
    pub fn would_admit(&self, total: u64) -> bool {
        match self.group {
            Some(group) if total > self.charged => {
                would_admit(self.class, group as usize, total - self.charged)
            }
            _ => true,
        }
    }

    /// Moves the charge to `total` bytes. Growth is checked against limits and
    /// refused without change; shrinking cannot fail.
    pub fn set_total(&mut self, total: u64) -> bool {
        let Some(group) = self.group else { return true };
        if total > self.charged {
            if !try_charge(self.class, group as usize, total - self.charged) {
                return false;
            }
        } else {
            uncharge(self.class, group as usize, self.charged - total);
        }
        self.charged = total;
        true
    }

    /// As `set_total`, but growth ignores limits (state that already exists).
    pub fn set_total_forced(&mut self, total: u64) {
        let Some(group) = self.group else { return };
        if total > self.charged {
            charge_forced(self.class, group as usize, total - self.charged);
        } else {
            uncharge(self.class, group as usize, self.charged - total);
        }
        self.charged = total;
    }

    /// Takes over the binding and charge of `other`, leaving it unbound.
    pub fn adopt(&mut self, other: &mut Meter) {
        self.release();
        self.group = other.group.take();
        self.charged = core::mem::take(&mut other.charged);
    }

    fn release(&mut self) {
        if let Some(group) = self.group.take() {
            uncharge(self.class, group as usize, self.charged);
        }
        self.charged = 0;
    }
}

impl Clone for Meter {
    fn clone(&self) -> Self {
        Self::new(self.class)
    }
}

impl Drop for Meter {
    fn drop(&mut self) {
        self.release();
    }
}

/// Charges `bytes` without consulting any limit, like `page_counter_charge`.
/// Only for state that already exists and cannot be refused (catching a
/// bound group up to objects built before it was bound, or a split that an
/// unmap must not fail); the matching refund is the ordinary `uncharge`.
pub fn charge_forced(class: Class, index: usize, bytes: u64) {
    if bytes == 0 {
        return;
    }
    let mut cursor = index;
    for _ in 0..MAX_DEPTH {
        let Some(cell) = group(class, cursor) else { return };
        cell.usage.fetch_add(bytes, Ordering::Relaxed);
        let parent = cell.parent.load(Ordering::Relaxed);
        if parent == NO_PARENT {
            return;
        }
        cursor = parent as usize;
    }
}

/// Returns `bytes` to the group and every ancestor.
pub fn uncharge(class: Class, index: usize, bytes: u64) {
    if bytes == 0 {
        return;
    }
    let mut cursor = index;
    for _ in 0..MAX_DEPTH {
        let Some(cell) = group(class, cursor) else { return };
        // Checked update: an over-refund leaves the counter intact, never wrapped.
        let updated = cell.usage.fetch_update(Ordering::Relaxed, Ordering::Relaxed, |usage| {
            usage.checked_sub(bytes)
        });
        assert!(updated.is_ok(), "kmem charge group usage underflow");
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
    fn forced_charge_ignores_limits_and_every_class_is_summed() {
        init_group(62, None);
        init_group(63, Some(62));
        set_limit(Class::PageTable, 62, 4096);
        charge_forced(Class::PageTable, 63, 3 * 4096);
        assert_eq!((usage(Class::PageTable, 63), usage(Class::PageTable, 62)), (3 * 4096, 3 * 4096));
        assert!(!try_charge(Class::PageTable, 63, 1), "usage above the limit refuses");
        assert!(try_charge(Class::Vma, 63, 7), "classes are independent");
        assert_eq!(total_usage(62), 3 * 4096 + 7);
        uncharge(Class::PageTable, 63, 3 * 4096);
        uncharge(Class::Vma, 63, 7);
        assert_eq!(total_usage(62), 0);
    }

    #[test]
    fn a_meter_tracks_its_owner_total_and_refunds_on_drop() {
        init_group(48, None);
        set_limit(Class::Vma, 48, 100);
        let mut meter = Meter::new(Class::Vma);
        assert!(meter.set_total(500), "an unbound meter charges nothing");
        assert_eq!(usage(Class::Vma, 48), 0);
        assert!(!meter.bind(48, 101), "binding is limit checked");
        assert!(!meter.is_bound());
        assert!(meter.bind(48, 60));
        assert!(!meter.bind(48, 1), "binds once");
        assert!(meter.would_admit(100));
        assert!(!meter.would_admit(101));
        assert!(!meter.set_total(101), "growth is refused unchanged");
        assert_eq!((meter.charged(), usage(Class::Vma, 48)), (60, 60));
        assert!(meter.set_total(100));
        meter.set_total_forced(130);
        assert_eq!(usage(Class::Vma, 48), 130);
        assert!(meter.set_total(10), "shrinking never fails");
        let clone = meter.clone();
        assert_eq!((clone.is_bound(), clone.charged()), (false, 0));
        drop(clone);
        assert_eq!(usage(Class::Vma, 48), 10, "a clone refunds nothing");
        let mut heir = Meter::new(Class::Vma);
        heir.adopt(&mut meter);
        assert_eq!((meter.charged(), heir.charged()), (0, 10));
        drop(meter);
        assert_eq!(usage(Class::Vma, 48), 10);
        drop(heir);
        assert_eq!(usage(Class::Vma, 48), 0);
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
