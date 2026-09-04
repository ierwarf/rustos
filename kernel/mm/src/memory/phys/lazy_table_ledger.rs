//! IRQ-safe explicit ownership for fault-installed page tables.
//!
//! The descriptor backing is reserved at physical-allocator bootstrap.  A root
//! frame owns an intrusive stack of lazy table frames; table records retain the
//! root identity until retirement drains the inactive root.  No operation in
//! this module allocates or acquires the physical allocator lock.

use core::ptr;
use core::sync::atomic::{AtomicPtr, AtomicU64, AtomicUsize, Ordering};

use super::PAGE_SIZE;

/// One descriptor for each physical frame.
///
/// A root frame uses `owner_root_or_head` as the intrusive-list head. A lazy
/// table frame stores its owning root there and its successor in `next`.
#[repr(C)]
pub struct LazyTableLedgerRecord {
    owner_root_or_head: AtomicU64,
    next: AtomicU64,
}

#[cfg(test)]
impl LazyTableLedgerRecord {
    const fn empty() -> Self {
        Self {
            owner_root_or_head: AtomicU64::new(0),
            next: AtomicU64::new(0),
        }
    }
}

/// Immutable boot-sized descriptor catalog.
pub struct LazyTableLedger {
    records: AtomicPtr<LazyTableLedgerRecord>,
    frame_count: AtomicUsize,
}

impl LazyTableLedger {
    pub const fn empty() -> Self {
        Self {
            records: AtomicPtr::new(ptr::null_mut()),
            frame_count: AtomicUsize::new(0),
        }
    }

    fn record(&self, frame_phys: u64) -> Option<&LazyTableLedgerRecord> {
        if !frame_phys.is_multiple_of(PAGE_SIZE) {
            return None;
        }
        let index = usize::try_from(frame_phys / PAGE_SIZE).ok()?;
        // ORDERING: acquire observes the one-time bootstrap backing before a
        // fault dereferences an entry from it.
        let frame_count = self.frame_count.load(Ordering::Acquire);
        let records = self.records.load(Ordering::Acquire);
        if records.is_null() || index >= frame_count {
            return None;
        }
        // SAFETY: bootstrap reserves direct-mapped descriptor storage for the
        // full frame count before publishing this pointer; the checked index is
        // within that allocation and it outlives every process address space.
        Some(unsafe { &*records.add(index) })
    }

    pub fn install(&self, records: *mut LazyTableLedgerRecord, frame_count: usize) {
        assert!(
            !records.is_null() && frame_count != 0,
            "lazy table ledger backing is invalid"
        );
        // ORDERING: release publishes fully zeroed backing before the nonzero
        // frame count lets an exception-side reader dereference it.
        self.records.store(records, Ordering::Release);
        self.frame_count.store(frame_count, Ordering::Release);
    }
}

pub static LAZY_TABLE_LEDGER: LazyTableLedger = LazyTableLedger::empty();

/// Registers a newly allocated root; a residual head is fail-stop root ABA.
pub fn register_lazy_table_root(root_phys: u64) {
    let root = LAZY_TABLE_LEDGER
        .record(root_phys)
        .expect("new lazy table root is outside metadata");
    // ORDERING: acquire pairs with retirement's release clear before this
    // physical root frame can re-enter allocation.
    assert_eq!(
        root.owner_root_or_head.load(Ordering::Acquire),
        0,
        "reused root retained lazy table ownership"
    );
}

/// Claims an unpublished table record without allocation or locking.
pub fn claim_lazy_table_record(root_phys: u64, table_phys: u64) -> bool {
    if root_phys == 0 || root_phys == table_phys {
        return false;
    }
    if LAZY_TABLE_LEDGER.record(root_phys).is_none() {
        return false;
    }
    let Some(table) = LAZY_TABLE_LEDGER.record(table_phys) else {
        return false;
    };
    // ORDERING: successful AcqRel reserves the descriptor before its PTE CAS;
    // acquire on failure sees a conflicting owner and fails closed.
    table
        .owner_root_or_head
        .compare_exchange(0, root_phys, Ordering::AcqRel, Ordering::Acquire)
        .is_ok()
}

/// Links a successful table PTE publication to its already-claimed root.
pub fn publish_lazy_table_record(root_phys: u64, table_phys: u64) {
    let root = LAZY_TABLE_LEDGER
        .record(root_phys)
        .expect("lazy table root is outside metadata");
    let table = LAZY_TABLE_LEDGER
        .record(table_phys)
        .expect("lazy table is outside metadata");
    // ORDERING: acquire validates that the reservation is visible before the
    // table becomes reachable through the root's release-published list head.
    assert_eq!(
        table.owner_root_or_head.load(Ordering::Acquire),
        root_phys,
        "lazy table publication lost its reserved owner record"
    );
    // ORDERING: acquire observes prior appenders; AcqRel makes this table's
    // stable next pointer and owner visible before a retirement drain sees it.
    let mut head = root.owner_root_or_head.load(Ordering::Acquire);
    loop {
        table.next.store(head, Ordering::Relaxed);
        match root.owner_root_or_head.compare_exchange_weak(
            head,
            table_phys,
            Ordering::AcqRel,
            Ordering::Acquire,
        ) {
            Ok(_) => return,
            Err(observed) => head = observed,
        }
    }
}

/// Cancels a descriptor when the table-entry CAS loses before publication.
pub fn cancel_lazy_table_record(root_phys: u64, table_phys: u64) {
    let table = LAZY_TABLE_LEDGER
        .record(table_phys)
        .expect("lazy table cancellation is outside metadata");
    assert_eq!(
        table.next.load(Ordering::Relaxed),
        0,
        "published lazy table record cannot be cancelled"
    );
    // ORDERING: AcqRel releases the reservation before the caller returns the
    // unpublished frame; acquire on failure detects an ownership violation.
    assert_eq!(
        table.owner_root_or_head.compare_exchange(
            root_phys,
            0,
            Ordering::AcqRel,
            Ordering::Acquire,
        ),
        Ok(root_phys),
        "lazy table cancellation lost exact owner identity"
    );
}

/// Drains the inactive root before its frame can be returned to `phys`.
pub fn drain_lazy_table_records(root_phys: u64) -> alloc::vec::Vec<u64> {
    let root = LAZY_TABLE_LEDGER
        .record(root_phys)
        .expect("retired lazy table root is outside metadata");
    // ORDERING: AcqRel freezes the root list after the retirement barrier and
    // synchronizes with every successful exception-side append.
    let mut current = root.owner_root_or_head.swap(0, Ordering::AcqRel);
    let maximum = LAZY_TABLE_LEDGER.frame_count.load(Ordering::Acquire);
    let mut tables = alloc::vec::Vec::new();
    while current != 0 {
        assert!(
            tables.len() < maximum,
            "lazy table ledger cycle while draining root={root_phys:#x}"
        );
        let table = LAZY_TABLE_LEDGER
            .record(current)
            .expect("lazy table ledger points outside metadata");
        // ORDERING: acquire validates the exact claim released by the fault
        // path before this normal-context owner clears and frees the record.
        assert_eq!(
            table.owner_root_or_head.load(Ordering::Acquire),
            root_phys,
            "lazy table ledger owner mismatch root={root_phys:#x} table={current:#x}"
        );
        let next = table.next.swap(0, Ordering::AcqRel);
        table.owner_root_or_head.store(0, Ordering::Release);
        tables.push(current);
        current = next;
    }
    tables
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn records_link_and_drain_exactly_once() {
        let mut records = [
            LazyTableLedgerRecord::empty(),
            LazyTableLedgerRecord::empty(),
            LazyTableLedgerRecord::empty(),
            LazyTableLedgerRecord::empty(),
        ];
        let ledger = LazyTableLedger::empty();
        ledger.install(records.as_mut_ptr(), records.len());

        let root_phys = PAGE_SIZE;
        let first_table = PAGE_SIZE * 2;
        let second_table = PAGE_SIZE * 3;
        let root = ledger.record(root_phys).expect("root record");
        let first = ledger.record(first_table).expect("first table record");
        let second = ledger.record(second_table).expect("second table record");

        assert!(first
            .owner_root_or_head
            .compare_exchange(0, root_phys, Ordering::AcqRel, Ordering::Acquire)
            .is_ok());
        assert!(second
            .owner_root_or_head
            .compare_exchange(0, root_phys, Ordering::AcqRel, Ordering::Acquire)
            .is_ok());
        first.next.store(0, Ordering::Relaxed);
        assert!(root
            .owner_root_or_head
            .compare_exchange(0, first_table, Ordering::AcqRel, Ordering::Acquire)
            .is_ok());
        second.next.store(first_table, Ordering::Relaxed);
        assert!(root
            .owner_root_or_head
            .compare_exchange(
                first_table,
                second_table,
                Ordering::AcqRel,
                Ordering::Acquire
            )
            .is_ok());

        let mut current = root.owner_root_or_head.swap(0, Ordering::AcqRel);
        let mut drained = alloc::vec::Vec::new();
        while current != 0 {
            let record = ledger.record(current).expect("linked record");
            assert_eq!(record.owner_root_or_head.load(Ordering::Acquire), root_phys);
            let next = record.next.swap(0, Ordering::AcqRel);
            record.owner_root_or_head.store(0, Ordering::Release);
            drained.push(current);
            current = next;
        }
        assert_eq!(drained, alloc::vec![second_table, first_table]);
        assert_eq!(root.owner_root_or_head.load(Ordering::Acquire), 0);
        assert_eq!(first.owner_root_or_head.load(Ordering::Acquire), 0);
        assert_eq!(second.owner_root_or_head.load(Ordering::Acquire), 0);
    }
}
