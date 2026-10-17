/// One committed child identity is held by Scheduler from FIFO selection
/// through claim. Independently, another CPU may republish the owner word.
/// This bounded model excludes saved-frame validation, budgets and the queue.
#[test]
fn activation_claim_rejects_republish_and_slot_reuse() {
    loom::model(|| {
        const LOCAL: usize = 1;
        const REMOTE: usize = 2;
        const RUNNING: usize = 3;
        let owner = Arc::new(AtomicUsize::new((1 << 2) | LOCAL));
        let catalog = Arc::new(Mutex::new(802_usize));

        let word = owner.clone();
        let republish = thread::spawn(move || {
            // The first CAS withdraws the old generation. A different writer
            // can overtake the second CAS without being overwritten.
            if word
                .compare_exchange(
                    (1 << 2) | LOCAL,
                    (2 << 2) | REMOTE,
                    Ordering::AcqRel,
                    Ordering::Acquire,
                )
                .is_ok()
            {
                let _ = word.compare_exchange(
                    (2 << 2) | REMOTE,
                    (2 << 2) | LOCAL,
                    Ordering::AcqRel,
                    Ordering::Acquire,
                );
            }
        });
        let word = owner.clone();
        let identity = catalog.clone();
        let reuse = thread::spawn(move || {
            let mut live = identity.lock().unwrap();
            let old = word.load(Ordering::Acquire);
            if old & 3 != RUNNING
                && word
                    .compare_exchange(
                        old,
                        (3 << 2) | LOCAL,
                        Ordering::AcqRel,
                        Ordering::Acquire,
                    )
                    .is_ok()
            {
                *live = 902;
            }
        });
        let word = owner.clone();
        let identity = catalog.clone();
        let dispatch = thread::spawn(move || {
            let live = identity.lock().unwrap(); // Scheduler, held until claim
            if *live != 802 {
                return;
            }
            let selected = word.load(Ordering::Acquire);
            if selected & 3 != LOCAL {
                return;
            }
            // Mirrors the generation-bound owner CAS, not an unbound retry.
            if word
                .compare_exchange(
                    selected,
                    (selected & !3) | RUNNING,
                    Ordering::AcqRel,
                    Ordering::Acquire,
                )
                .is_ok()
            {
                assert_eq!(*live, 802);
                assert_ne!(selected >> 2, 0);
            }
        });
        republish.join().unwrap();
        reuse.join().unwrap();
        dispatch.join().unwrap();
        if owner.load(Ordering::Acquire) & 3 == RUNNING {
            assert_eq!(*catalog.lock().unwrap(), 802);
        }
    });
}

/// A consumed Generic FIFO hint binds one Local generation. The final CAS
/// cannot claim a newer Local publication with the same task identity.
#[test]
fn consumed_generic_local_generation_rejects_republication() {
    loom::model(|| {
        const LOCAL: usize = 1;
        const REMOTE: usize = 2;
        const RUNNING: usize = 3;
        let owner = Arc::new(AtomicUsize::new((1 << 2) | LOCAL));
        let republished = owner.clone();
        let writer = thread::spawn(move || {
            if republished
                .compare_exchange(
                    (1 << 2) | LOCAL,
                    (2 << 2) | REMOTE,
                    Ordering::AcqRel,
                    Ordering::Acquire,
                )
                .is_ok()
            {
                let _ = republished.compare_exchange(
                    (2 << 2) | REMOTE,
                    (2 << 2) | LOCAL,
                    Ordering::AcqRel,
                    Ordering::Acquire,
                );
            }
        });
        let claiming = owner.clone();
        let dispatch = thread::spawn(move || {
            // Captured after FIFO consumption, never reloaded as a fresh
            // generation after a competing republish.
            let selected_generation = 1_usize;
            let observed = claiming.load(Ordering::Acquire);
            if rustos_scheduler_frame_gate::first_turn_claim_custody(
                802,
                802,
                selected_generation as u64,
                (observed >> 2) as u64,
                observed & 3 == LOCAL,
                true,
                true,
            ) {
                let _ = claiming.compare_exchange(
                    (selected_generation << 2) | LOCAL,
                    (selected_generation << 2) | RUNNING,
                    Ordering::AcqRel,
                    Ordering::Acquire,
                );
            }
        });
        writer.join().unwrap();
        dispatch.join().unwrap();
        let observed = owner.load(Ordering::Acquire);
        if observed & 3 == RUNNING {
            assert_eq!(observed >> 2, 1);
        }
    });
}
