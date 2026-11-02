/// A tagged activation and a mutation's generation bump are ordered by SeqCst
/// publication on both sides: the sender cannot both miss the activating CPU in
/// its root sample and leave the activation reading the old generation.
#[test]
fn tlb_activation_and_generation_bump_cannot_both_miss() {
    loom::model(|| {
        let generation = Arc::new(AtomicUsize::new(0));
        let root = Arc::new(AtomicUsize::new(0));
        let sender_saw_active = Arc::new(AtomicBool::new(false));
        let activation_saw = Arc::new(AtomicUsize::new(usize::MAX));
        let (g, r, saw) = (generation.clone(), root.clone(), sender_saw_active.clone());
        let sender = thread::spawn(move || {
            g.fetch_add(1, Ordering::SeqCst);
            fence(Ordering::SeqCst);
            saw.store(r.load(Ordering::SeqCst) == 1, Ordering::Relaxed);
        });
        let (g, r, seen) = (generation.clone(), root.clone(), activation_saw.clone());
        let activator = thread::spawn(move || {
            r.store(1, Ordering::SeqCst);
            fence(Ordering::SeqCst);
            seen.store(g.load(Ordering::SeqCst), Ordering::Relaxed);
        });
        sender.join().unwrap();
        activator.join().unwrap();
        assert!(
            sender_saw_active.load(Ordering::Relaxed) || activation_saw.load(Ordering::Relaxed) >= 1,
            "a mutation must reach an activating CPU by request or by generation"
        );
    });
}

/// A global mutation's exclusive gate and a per-space mutation's shared slot
/// never overlap: both transitions are read-modify-writes of one word.
#[test]
fn tlb_gate_excludes_global_from_shared_mutations() {
    loom::model(|| {
        const EXCLUSIVE: usize = 1 << 20;
        let gate = Arc::new(AtomicUsize::new(0));
        let in_shared = Arc::new(AtomicUsize::new(0));
        let in_global = Arc::new(AtomicUsize::new(0));
        let mut workers = Vec::new();
        for _ in 0..1 {
            let (gate, in_shared, in_global) = (gate.clone(), in_shared.clone(), in_global.clone());
            workers.push(thread::spawn(move || {
                let observed = gate.load(Ordering::Acquire);
                if observed & EXCLUSIVE == 0
                    && gate
                        .compare_exchange(observed, observed + 1, Ordering::AcqRel, Ordering::Acquire)
                        .is_ok()
                {
                    in_shared.fetch_add(1, Ordering::SeqCst);
                    assert_eq!(in_global.load(Ordering::SeqCst), 0);
                    in_shared.fetch_sub(1, Ordering::SeqCst);
                    gate.fetch_sub(1, Ordering::Release);
                }
            }));
        }
        let (gate_g, in_shared_g, in_global_g) = (gate.clone(), in_shared.clone(), in_global.clone());
        workers.push(thread::spawn(move || {
            gate_g.fetch_or(EXCLUSIVE, Ordering::AcqRel);
            if gate_g.load(Ordering::Acquire) == EXCLUSIVE {
                in_global_g.store(1, Ordering::SeqCst);
                assert_eq!(in_shared_g.load(Ordering::SeqCst), 0);
                in_global_g.store(0, Ordering::SeqCst);
            }
            gate_g.fetch_and(!EXCLUSIVE, Ordering::Release);
        }));
        for worker in workers {
            worker.join().unwrap();
        }
    });
}

/// Each sender owns its own slot toward a target, so two concurrent senders
/// cannot overwrite each other's request, and the target acknowledges each with
/// exactly the generation that sender published.
#[test]
fn tlb_sender_slots_never_alias() {
    loom::model(|| {
        const GENERATION: [usize; 2] = [3, 5];
        let request = Arc::new([AtomicUsize::new(0), AtomicUsize::new(0)]);
        let acknowledgement = Arc::new([AtomicUsize::new(0), AtomicUsize::new(0)]);
        let pending = Arc::new(AtomicUsize::new(0));
        let mut senders = Vec::new();
        for sender in 0..2 {
            let (request, pending) = (request.clone(), pending.clone());
            senders.push(thread::spawn(move || {
                request[sender].store(GENERATION[sender], Ordering::Release);
                pending.fetch_or(1 << sender, Ordering::Release);
            }));
        }
        let (request_t, acknowledgement_t, pending_t) =
            (request.clone(), acknowledgement.clone(), pending.clone());
        let target = thread::spawn(move || {
            let mut mask = pending_t.swap(0, Ordering::Acquire);
            while mask != 0 {
                let sender = mask.trailing_zeros() as usize;
                mask &= mask - 1;
                let generation = request_t[sender].load(Ordering::Acquire);
                assert_eq!(generation, GENERATION[sender]);
                acknowledgement_t[sender].store(generation, Ordering::Release);
            }
        });
        for sender in senders {
            sender.join().unwrap();
        }
        target.join().unwrap();
        for sender in 0..2 {
            let ack = acknowledgement[sender].load(Ordering::Acquire);
            assert!(ack == 0 || ack == GENERATION[sender]);
        }
    });
}
