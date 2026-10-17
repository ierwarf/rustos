/// PCT permutations of an incomplete FIFO identity and a newer Direct owner
/// against exact-generation materialization of the consumed record.
#[test]
fn incomplete_fifo_identity_restores_only_its_direct_generation_pct() {
    shuttle::check_pct(
        || {
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
        },
        iterations(),
        pct_depth(),
    );
}
