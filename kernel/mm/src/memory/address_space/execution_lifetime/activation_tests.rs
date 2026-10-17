use super::*;
extern crate std;

#[test]
fn activation_refuses_close_without_partial_sealing_until_last_grant() {
    let queue = Box::leak(Box::new(Queue::new()));
    let owner = Owner::with_queue(0x44000, false, queue);
    let physical = owner.pin().unwrap();
    let first = physical.try_activate().unwrap();
    let second = physical.try_activate().unwrap();
    assert!(!owner.try_seal_activation());
    assert!(physical.is_open());
    drop(first);
    assert!(!owner.try_seal_activation());
    assert!(physical.is_open());
    drop(second);
    assert!(owner.try_seal_activation());
    assert!(!physical.is_open());
    assert!(physical.try_activate().is_none());
    assert!(owner.try_seal_activation());
    drop(owner);
    drop(physical);
    assert_eq!(queue.drain(1, |root, _| assert_eq!(root, 0x44000)), 1);
}
#[test]
fn concurrent_activation_and_close_have_one_admission_winner() {
    for _ in 0..64 {
        let queue = Box::leak(Box::new(Queue::new()));
        let owner = Owner::with_queue(0x45000, false, queue);
        let physical = owner.pin().unwrap();
        let start = std::sync::Barrier::new(3);
        let observed = std::sync::Barrier::new(3);
        let admitted = std::sync::atomic::AtomicBool::new(false);
        let closed = std::sync::atomic::AtomicBool::new(false);
        std::thread::scope(|scope| {
            scope.spawn(|| {
                start.wait();
                let grant = physical.try_activate();
                admitted.store(grant.is_some(), Ordering::Release);
                observed.wait();
                drop(grant);
            });
            scope.spawn(|| {
                start.wait();
                closed.store(owner.try_seal_activation(), Ordering::Release);
                observed.wait();
            });
            start.wait();
            observed.wait();
        });
        assert_ne!(
            admitted.load(Ordering::Acquire),
            closed.load(Ordering::Acquire)
        );
        assert!(owner.try_seal_activation());
        drop(owner);
        drop(physical);
        assert_eq!(queue.drain(1, |_, _| {}), 1);
    }
}
#[test]
fn owner_drop_retains_activation_backing_and_hot_release_only_enqueues() {
    let queue = Box::leak(Box::new(Queue::new()));
    let owner = Owner::with_queue(0x46000, false, queue);
    let physical = owner.pin().unwrap();
    let grant = physical.try_activate().unwrap();
    drop(physical);
    drop(owner);
    assert!(!grant.is_open());
    assert!(queue.head.load(Ordering::Acquire).is_null());
    assert_eq!(
        queue.drain(1, |_, _| panic!("live activation backing reclaimed")),
        0
    );
    drop(grant);
    assert!(!queue.head.load(Ordering::Acquire).is_null());
    assert_eq!(queue.drain(1, |root, _| assert_eq!(root, 0x46000)), 1);
}
#[test]
fn activation_saturation_and_unwind_preserve_exact_existing_custody() {
    let queue = Box::leak(Box::new(Queue::new()));
    let owner = Owner::with_queue(0x47000, false, queue);
    let physical = owner.pin().unwrap();
    let node = unsafe { owner.root.as_ref() };
    node.activation.store(COUNT, Ordering::Release);
    assert!(physical.try_activate().is_none());
    assert_eq!(node.activation.load(Ordering::Acquire), COUNT);
    node.activation.store(0, Ordering::Release);
    assert!(
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _grant = physical.try_activate().unwrap();
            panic!("modeled activation abort");
        }))
        .is_err()
    );
    assert_eq!(node.activation.load(Ordering::Acquire), 0);
    assert!(owner.try_seal_activation());
    drop(owner);
    drop(physical);
    assert_eq!(queue.drain(1, |_, _| {}), 1);
}

#[test]
fn closed_activation_gate_refuses_even_before_physical_close_publication() {
    let queue = Box::leak(Box::new(Queue::new()));
    let owner = Owner::with_queue(0x48000, false, queue);
    let physical = owner.pin().unwrap();
    let node = unsafe { owner.root.as_ref() };
    // Isolate the first close publication, before the physical-ref close bit.
    node.activation.store(CLOSED, Ordering::Release);
    assert!(physical.is_open());
    assert!(physical.try_activate().is_none());
    assert_eq!(node.activation.load(Ordering::Acquire), CLOSED);
    assert!(owner.try_seal_activation());
    assert!(!physical.is_open());
    drop(owner);
    drop(physical);
    assert_eq!(queue.drain(1, |_, _| {}), 1);
}
