extern crate std;
use super::*;
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
