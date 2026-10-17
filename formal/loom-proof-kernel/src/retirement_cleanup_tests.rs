/// A bounded independent cleanup cell: two consumers and one reuser, with
/// two monotonic task IDs. External cleanup success and execution/TLB/stack
/// quiescence are assumed, not established by this lock model.
#[test]
fn cleanup_claim_ack_and_reuse_have_one_exact_slot_owner() {
    loom::model(|| {
        // task, phase (pending=1, claimed=2, complete=3), claims, ACKs
        let cell = Arc::new(Mutex::new((1_u64, 1_u8, [0_u8; 2], [0_u8; 2])));
        let mut consumers = Vec::new();
        for _ in 0..2 {
            let cell = cell.clone();
            consumers.push(loom::thread::spawn(move || {
                let claimed = {
                    let mut state = cell.lock().unwrap();
                    if state.1 == 1 {
                        let task = state.0;
                        state.1 = 2;
                        state.2[(task - 1) as usize] += 1;
                        Some(task)
                    } else { None }
                };
                loom::thread::yield_now();
                if let Some(task) = claimed {
                    let mut state = cell.lock().unwrap();
                    if rustos_scheduler_frame_gate::retirement_cleanup_ack_custody(
                        state.0, task, state.1 == 2, true,
                    ) {
                        state.1 = 3;
                        state.3[(task - 1) as usize] += 1;
                    }
                }
            }));
        }
        let reuse_cell = cell.clone();
        let reuse = loom::thread::spawn(move || {
            let mut state = reuse_cell.lock().unwrap();
            if state.1 == 3 {
                assert_eq!(state.2[0], 1);
                assert_eq!(state.3[0], 1);
                state.0 = 2;
                state.1 = 1;
                assert!(!rustos_scheduler_frame_gate::retirement_cleanup_ack_custody(
                    state.0, 1, true, true,
                ));
            }
        });
        for consumer in consumers { consumer.join().unwrap(); }
        reuse.join().unwrap();
        let state = cell.lock().unwrap();
        for i in 0..2 {
            assert!(state.2[i] <= 1);
            assert!(state.3[i] <= state.2[i]);
        }
        if state.0 == 2 { assert_eq!(state.3[0], 1); }
    });
}
