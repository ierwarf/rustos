/// PCT permutations of a saved-RSP republish that must withdraw Local
/// ownership before updating the frame and republishing its generation.
#[test]
fn selected_local_frame_republish_cannot_claim_the_old_saved_rsp_pct() {
    shuttle::check_pct(
        || {
            let owner = Arc::new(Mutex::new((1_usize, true, false)));
            let version = Arc::new(AtomicUsize::new(2));
            let saved_rsp = Arc::new(AtomicUsize::new(1));
            let writer_owner = owner.clone();
            let writer_version = version.clone();
            let writer_rsp = saved_rsp.clone();
            let writer = thread::spawn(move || {
                let mut state = writer_owner.lock().unwrap();
                if state.2 { return; }
                state.1 = false;
                writer_version.fetch_add(1, Ordering::AcqRel);
                writer_rsp.store(2, Ordering::Relaxed);
                writer_version.fetch_add(1, Ordering::Release);
                state.0 = 2;
                state.1 = true;
            });
            let reader_owner = owner.clone();
            let reader_version = version.clone();
            let reader_rsp = saved_rsp.clone();
            let reader = thread::spawn(move || {
                let (generation, local, claimed) = *reader_owner.lock().unwrap();
                if generation != 1 || !local || claimed { return; }
                let before = reader_version.load(Ordering::Acquire);
                let frame_rsp = reader_rsp.load(Ordering::Relaxed);
                let after = reader_version.load(Ordering::Acquire);
                if before & 1 != 0 || before != after || frame_rsp != 1 { return; }
                let mut state = reader_owner.lock().unwrap();
                if reader_version.load(Ordering::Acquire) != before
                    || reader_rsp.load(Ordering::Acquire) != frame_rsp
                    || state.0 != 1 || !state.1 || state.2
                { return; }
                assert_eq!(frame_rsp, 1);
                state.2 = true;
            });
            writer.join().unwrap();
            reader.join().unwrap();
            if owner.lock().unwrap().2 {
                assert_eq!(saved_rsp.load(Ordering::Acquire), 1);
            }
        },
        iterations(),
        pct_depth(),
    );
}

/// A bounded local-frame reader racing an exact-generation owner transition.
/// Models the production version/generation atomics and owner/claim gate; it
/// does not model saved-RSP validation or the entire scheduler pick.
#[test]
fn local_frame_claim_never_uses_stale_owner_generation_pct() {
    shuttle::check_pct(
        || {
            struct Owner {
                generation: usize,
                local: bool,
                claimed: bool,
            }
            let owner = Arc::new(Mutex::new(Owner {
                generation: 1,
                local: true,
                claimed: false,
            }));
            let version = Arc::new(AtomicUsize::new(2));
            let frame_generation = Arc::new(AtomicUsize::new(1));

            let writer_owner = owner.clone();
            let writer_version = version.clone();
            let writer_generation = frame_generation.clone();
            let writer = thread::spawn(move || {
                {
                    let mut state = writer_owner.lock().unwrap();
                    if state.claimed {
                        return;
                    }
                    state.local = false; // transition retains stack custody
                }
                writer_version.fetch_add(1, Ordering::AcqRel); // odd: begin_write
                writer_generation.store(2, Ordering::Release);
                writer_version.fetch_add(1, Ordering::Release); // even: finish_write
                let mut state = writer_owner.lock().unwrap();
                state.generation = 2;
                state.local = true;
            });

            let reader_owner = owner.clone();
            let reader_version = version.clone();
            let reader_generation = frame_generation.clone();
            let reader = thread::spawn(move || {
                let (first_generation, first_local) = {
                    let state = reader_owner.lock().unwrap();
                    (state.generation, state.local && !state.claimed)
                };
                if !first_local {
                    return;
                }
                let before = reader_version.load(Ordering::Acquire);
                let token_generation = reader_generation.load(Ordering::Acquire);
                let after = reader_version.load(Ordering::Acquire);
                if before & 1 != 0 || before != after || token_generation != first_generation {
                    return;
                }
                let mut state = reader_owner.lock().unwrap();
                if reader_version.load(Ordering::Acquire) != before
                    || reader_generation.load(Ordering::Acquire) != token_generation
                    || state.generation != first_generation
                    || !state.local
                    || state.claimed
                {
                    return;
                }
                // The claim linearizes while the exact owner is retained.
                assert_eq!(token_generation, state.generation);
                assert_eq!(before & 1, 0);
                state.claimed = true;
            });
            writer.join().unwrap();
            reader.join().unwrap();
            let state = owner.lock().unwrap();
            if state.claimed {
                assert!(state.local);
            }
        },
        iterations(),
        pct_depth(),
    );
}

/// PCT explores the same bounded generation/Current-custody gate; arbitrary
/// frame writers, lifecycle publication, budget preparation and teardown are
/// outside this model and remain separate production obligations.
#[test]
fn retained_current_frame_never_authorizes_a_republished_or_released_owner_pct() {
    shuttle::check_pct(|| {
        let owner = Arc::new(Mutex::new((7_u64, true)));
        let writer = owner.clone();
        let changed = shuttle::thread::spawn(move || {
            let mut state = writer.lock().unwrap();
            state.0 += 1;
            state.1 = false;
        });
        let selected = owner.lock().unwrap().0;
        shuttle::thread::yield_now();
        let state = owner.lock().unwrap();
        let admitted = rustos_scheduler_frame_gate::retained_current_frame_custody(
            selected, state.0, true, true, state.1,
        );
        if admitted {
            assert_eq!(selected, state.0);
            assert!(state.1);
        }
        assert!(!rustos_scheduler_frame_gate::retained_current_frame_custody(
            selected, state.0, true, true, false,
        ));
        drop(state);
        changed.join().unwrap();
    }, 128, 3);
}
