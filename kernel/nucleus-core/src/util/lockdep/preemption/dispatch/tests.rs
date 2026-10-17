use super::*;
use std::panic::{AssertUnwindSafe, catch_unwind};

#[test]
fn independent_dispatch_unit_is_unique_and_refusal_cannot_release_its_owner() {
    let cell = Cell::new();
    let owner = cell.acquire(3, false).unwrap();
    assert!(cell.acquire(3, false).is_none());
    assert!(cell.live.load(Ordering::Acquire));
    assert_eq!(owner.cpu(), 3);
    drop(owner);
    assert!(!cell.live.load(Ordering::Acquire));
    drop(cell.acquire(3, false).unwrap());
}
#[test]
fn switched_dispatch_cannot_restore_outgoing_irq_flags() {
    let cell = Cell::new();
    let mut owner = cell.acquire(3, true).unwrap();
    assert!(owner.restore_irq);
    assert!(!owner.is_masked()); // unmasked bounded wait still owns preemption
    owner.mask_irqs();
    assert!(owner.is_masked());
    owner.retain_irq_mask();
    assert!(!owner.restore_irq);
    drop(owner);
    assert!(!cell.live.load(Ordering::Acquire));
}
#[test]
fn scoped_unwind_releases_one_unit_and_duplicate_release_refuses() {
    let cell = Cell::new();
    assert!(
        catch_unwind(AssertUnwindSafe(|| {
            let _owner = cell.acquire(2, false).unwrap();
            panic!("modeled dispatch unwind");
        }))
        .is_err()
    );
    assert!(!cell.live.load(Ordering::Acquire));
    assert!(catch_unwind(AssertUnwindSafe(|| cell.release())).is_err());
    assert!(!cell.live.load(Ordering::Acquire));
}
#[test]
fn concurrent_dispatch_admission_has_one_non_lock_preemption_owner() {
    let cell = Cell::new();
    let start = std::sync::Barrier::new(3);
    let observed = std::sync::Barrier::new(3);
    let wins = core::sync::atomic::AtomicUsize::new(0);
    std::thread::scope(|scope| {
        for _ in 0..2 {
            scope.spawn(|| {
                start.wait();
                let owner = cell.acquire(1, false);
                if owner.is_some() {
                    wins.fetch_add(1, Ordering::Relaxed);
                }
                observed.wait();
                drop(owner);
            });
        }
        start.wait();
        observed.wait();
    });
    assert_eq!(wins.load(Ordering::Acquire), 1);
    assert!(!cell.live.load(Ordering::Acquire));
}

#[test]
fn effective_preemption_counts_dispatch_without_inventing_a_raw_guard() {
    let owner = DispatchPreemption::enter_model(0);
    assert_eq!(super::super::preemption_depth(), 1);
    assert!(super::super::preemption_disabled());
    let snapshot = super::super::preemption_snapshot();
    assert_eq!(snapshot.depth, 1);
    assert_eq!(snapshot.dispatch_depth, 1);
    assert_eq!(snapshot.held_depth, 0);
    assert_eq!(snapshot.pending_depth, 0);
    drop(owner);
    assert_eq!(super::super::preemption_depth(), 0);
}
