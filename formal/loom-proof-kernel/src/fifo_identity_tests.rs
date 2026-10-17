/// A consumed FIFO record without a complete identity cannot dispatch.
/// Exact-generation Direct materialization holds the target queue lock,
/// inserts before its owner CAS and rolls back if a newer owner wins.
/// This bounded model excludes lifecycle publication writers and budget gates.
#[test]
fn incomplete_fifo_identity_restores_only_its_direct_generation() {
    loom::model(|| {
        const OLD_DIRECT: usize = 1;
        const NEW_DIRECT: usize = 2;
        const OLD_LOCAL: usize = 3;
        let owner = Arc::new(AtomicUsize::new(OLD_DIRECT));
        let queued = Arc::new(Mutex::new(false));
        let new_owner = owner.clone();
        let republish = thread::spawn(move || {
            let _ = new_owner.compare_exchange(
                OLD_DIRECT, NEW_DIRECT, Ordering::AcqRel, Ordering::Acquire,
            );
        });
        let rejected_owner = owner.clone();
        let rejected_queue = queued.clone();
        let reject = thread::spawn(move || {
            // Missing identity is never permission to dispatch this record.
            let mut local = rejected_queue.lock().unwrap();
            *local = true;
            if rejected_owner.compare_exchange(
                OLD_DIRECT, OLD_LOCAL, Ordering::AcqRel, Ordering::Acquire,
            ).is_err() {
                *local = false;
            }
        });
        republish.join().unwrap();
        reject.join().unwrap();
        let final_owner = owner.load(Ordering::Acquire);
        assert!(final_owner == OLD_LOCAL || final_owner == NEW_DIRECT);
        assert_eq!(*queued.lock().unwrap(), final_owner == OLD_LOCAL);
    });
}
