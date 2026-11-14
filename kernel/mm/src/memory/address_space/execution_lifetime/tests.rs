extern crate std;
use super::*;
use alloc::sync::Arc;
static QUEUE: Queue = Queue::new();
static SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());
#[test]
fn sealing_refuses_new_execution_but_captured_refs_retain_the_root_until_cold_drain() {
    let _serial = SERIAL.lock().unwrap();
    let owner = Owner::with_queue(0x1000, false, &QUEUE);
    let first = owner.pin().unwrap();
    let second = first.try_clone().unwrap();
    owner.seal();
    assert!(owner.pin().is_none());
    assert!(!first.is_open());
    assert!(first.try_clone().is_none());
    drop(owner);
    assert!(QUEUE.head.load(Ordering::Acquire).is_null());
    assert_eq!(QUEUE.drain(8, |_, _| panic!("live MM reclaimed")), 0);
    drop(first);
    assert!(QUEUE.head.load(Ordering::Acquire).is_null());
    assert_eq!(QUEUE.drain(8, |_, _| panic!("last MM reader ignored")), 0);
    drop(second);
    let mut roots = alloc::vec::Vec::new();
    assert_eq!(QUEUE.drain(8, |root, _| roots.push(root)), 1);
    assert_eq!(roots, [0x1000]);
    assert_eq!(QUEUE.drain(8, |_, _| panic!("MM retired twice")), 0);
}
#[test]
fn owner_drop_and_pin_release_never_run_finalization_inline() {
    let _serial = SERIAL.lock().unwrap();
    let owner = Owner::with_queue(0x2000, false, &QUEUE);
    let pin = owner.pin().unwrap();
    drop(owner);
    // Observe queue authority before a consumer could destroy a live node.
    assert!(QUEUE.head.load(Ordering::Acquire).is_null());
    assert_eq!(pin.root_phys(), 0x2000);
    drop(pin);
    assert!(!QUEUE.head.load(Ordering::Acquire).is_null());
    assert_eq!(
        QUEUE.drain(1, |root, reclaim| {
            assert_eq!(root, 0x2000);
            assert!(!reclaim);
        }),
        1
    );
}

#[test]
fn old_mapping_backing_payload_survives_until_last_execution_ref_and_cold_finalizer() {
    let _serial = SERIAL.lock().unwrap();
    static DROPPED: AtomicUsize = AtomicUsize::new(0);
    struct Payload;
    impl Drop for Payload {
        fn drop(&mut self) {
            DROPPED.fetch_add(1, Ordering::Release);
        }
    }
    DROPPED.store(0, Ordering::Release);
    let owner = Owner::with_queue(0x9000, false, &QUEUE);
    let pin = owner.pin().unwrap();
    RetirementOwner { owner }.retire(Payload);
    assert_eq!(DROPPED.load(Ordering::Acquire), 0);
    assert_eq!(
        QUEUE.drain(8, |_, _| panic!("backing retired during execution")),
        0
    );
    drop(pin);
    assert_eq!(DROPPED.load(Ordering::Acquire), 0);
    assert_eq!(
        QUEUE.drain(8, |_, _| assert_eq!(DROPPED.load(Ordering::Acquire), 0)),
        1
    );
    assert_eq!(DROPPED.load(Ordering::Acquire), 1);
}
#[test]
fn immutable_backing_custody_precedes_publication_and_survives_all_retirement_routes() {
    let _serial = SERIAL.lock().unwrap();
    static DROPPED: AtomicUsize = AtomicUsize::new(0);
    struct Backing;
    impl Drop for Backing {
        fn drop(&mut self) {
            DROPPED.fetch_add(1, Ordering::Release);
        }
    }
    DROPPED.store(0, Ordering::Release);
    for explicit_retirement in [false, true] {
        let mut owner = Owner::with_queue(0xa000, false, &QUEUE);
        owner.retain_backing(super::super::ImmutableBacking::new(1, 1, Arc::new(Backing))).unwrap();
        let pin = owner.pin().unwrap();
        if explicit_retirement {
            RetirementOwner { owner }.retire(());
        } else {
            drop(owner);
        }
        let before = usize::from(explicit_retirement);
        assert_eq!(DROPPED.load(Ordering::Acquire), before);
        assert_eq!(QUEUE.drain(8, |_, _| panic!("root still pinned")), 0);
        drop(pin);
        assert_eq!(
            QUEUE.drain(8, |_, _| assert_eq!(
                DROPPED.load(Ordering::Acquire),
                before
            )),
            1
        );
        assert_eq!(DROPPED.load(Ordering::Acquire), before + 1);
    }
}

#[test]
fn concurrent_last_releases_enqueue_once_and_pin_cannot_cross_close() {
    let _serial = SERIAL.lock().unwrap();
    let owner = Owner::with_queue(0x3000, false, &QUEUE);
    let pins: alloc::vec::Vec<_> = (0..8).map(|_| owner.pin().unwrap()).collect();
    owner.seal();
    drop(owner);
    std::thread::scope(|scope| {
        for pin in pins {
            scope.spawn(move || {
                assert!(pin.try_clone().is_none());
                drop(pin);
            });
        }
    });
    assert_eq!(QUEUE.drain(8, |root, _| assert_eq!(root, 0x3000)), 1);
}
#[test]
fn bounded_single_consumer_drain_survives_reentry_and_concurrent_producers() {
    let _serial = SERIAL.lock().unwrap();
    for root in [0x4000, 0x5000, 0x6000] {
        drop(Owner::with_queue(root, false, &QUEUE));
    }
    assert_eq!(
        QUEUE.drain(1, |_, _| {
            assert_eq!(QUEUE.drain(1, |_, _| panic!("consumer reentry")), 0);
            drop(Owner::with_queue(0x7000, false, &QUEUE));
        }),
        1
    );
    assert_eq!(QUEUE.drain(8, |_, _| {}), 3);
}
#[test]
fn checked_reference_saturation_refuses_without_spending_owner_custody() {
    let _serial = SERIAL.lock().unwrap();
    let owner = Owner::with_queue(0x8000, false, &QUEUE);
    let node = unsafe { owner.root.as_ref() };
    node.state.store(COUNT, Ordering::Release);
    assert!(owner.pin().is_none());
    node.state.store(1, Ordering::Release);
    drop(owner);
    assert_eq!(QUEUE.drain(1, |_, _| {}), 1);
}

#[test]
fn immutable_backing_quota_is_unique_per_root_and_survives_pin_until_cold_reclaim() {
    let _serial = SERIAL.lock().unwrap();
    use crate::memory::charge;
    charge::init_group(19, None);
    charge::set_limit(19, 3);
    let mut owner = Owner::with_queue(0xb000, false, &QUEUE);
    let lease = super::super::ImmutableBacking::new(7, 3, Arc::new(()));
    assert!(owner.retain_backing(lease.clone()).unwrap());
    assert!(!owner.retain_backing(lease.clone()).unwrap());
    assert!(owner.bind_backings(19));
    assert_eq!(charge::usage(19), 3);
    let other = super::super::ImmutableBacking::new(8, 1, Arc::new(()));
    assert_eq!(owner.retain_backing(other), Err(super::super::AddressSpaceError::OutOfFrames));
    let mut second = Owner::with_queue(0xc000, false, &QUEUE);
    assert!(second.retain_backing(lease).unwrap());
    assert!(!second.bind_backings(19));
    assert_eq!(charge::usage(19), 3);
    drop(second);
    let pin = owner.pin().unwrap();
    drop(owner);
    assert_eq!(charge::usage(19), 3);
    QUEUE.drain(8, |_, _| {});
    assert_eq!(charge::usage(19), 3);
    drop(pin);
    QUEUE.drain(8, |_, _| {});
    assert_eq!(charge::usage(19), 0);
}

#[test]
fn backing_census_mirror_counts_unique_live_custody_and_failed_map_rollback() {
    let _serial = SERIAL.lock().unwrap();
    let mut owner = Owner::with_queue(0xf000, false, &QUEUE);
    let pin = owner.pin().unwrap();
    let first = super::super::ImmutableBacking::new(1, 8, Arc::new(()));
    assert!(owner.retain_backing(first.clone()).unwrap());
    assert!(!owner.retain_backing(first).unwrap());
    assert_eq!(pin.retained_backing_pages(), 8);
    let staged = super::super::ImmutableBacking::new(2, 3, Arc::new(()));
    assert!(owner.retain_backing(staged.clone()).unwrap());
    assert_eq!(pin.retained_backing_pages(), 11);
    owner.cancel_backing(staged);
    assert_eq!(pin.retained_backing_pages(), 8);
    drop(owner);
    assert_eq!(pin.retained_backing_pages(), 8);
    assert_eq!(QUEUE.drain(8, |_, _| panic!("census pin lost backing custody")), 0);
    drop(pin);
    assert_eq!(QUEUE.drain(8, |_, _| {}), 1);
}

#[test]
fn sibling_roots_atomically_admit_full_backing_against_ancestor() {
    let _serial = SERIAL.lock().unwrap();
    use crate::memory::charge;
    use std::sync::{Arc as StdArc, Barrier};
    charge::init_group(22, None);
    charge::init_group(23, Some(22));
    charge::init_group(24, Some(22));
    charge::set_limit(22, 4);
    let barrier = StdArc::new(Barrier::new(3));
    let release = StdArc::new(Barrier::new(3));
    let mut workers = alloc::vec::Vec::new();
    for (root, group) in [(0xd000, 23), (0xe000, 24)] {
        let barrier = barrier.clone();
        let release = release.clone();
        workers.push(std::thread::spawn(move || {
            let mut owner = Owner::with_queue(root, false, &QUEUE);
            owner.retain_backing(super::super::ImmutableBacking::new(1, 4, Arc::new(()))).unwrap();
            let admitted = owner.bind_backings(group);
            barrier.wait();
            release.wait();
            drop(owner);
            admitted
        }));
    }
    barrier.wait();
    assert_eq!(charge::usage(22), 4);
    release.wait();
    assert_eq!(workers.into_iter().map(|t| t.join().unwrap()).filter(|won| *won).count(), 1);
    QUEUE.drain(8, |_, _| {});
    assert_eq!((charge::usage(22), charge::usage(23), charge::usage(24)), (0, 0, 0));
}

#[test]
fn mapping_meter_stays_charged_after_owner_drop_until_cold_reclaim() {
    let _serial = SERIAL.lock().unwrap();
    use crate::memory::kmem_charge::{self, Class, SharedMeter};
    kmem_charge::init_group(22, None);
    let meter = SharedMeter::new(Class::Vma);
    assert!(meter.bind(22, 80));
    let mut owner = Owner::with_queue(0xf000, false, &QUEUE);
    owner.set_mapping_meter(meter.clone());
    let pin = owner.pin().unwrap();
    drop(meter);
    drop(owner);
    assert_eq!(kmem_charge::usage(Class::Vma, 22), 80);
    assert_eq!(QUEUE.drain(8, |_, _| {}), 0);
    drop(pin);
    assert_eq!(QUEUE.drain(8, |_, _| {}), 1);
    assert_eq!(kmem_charge::usage(Class::Vma, 22), 0);
}
