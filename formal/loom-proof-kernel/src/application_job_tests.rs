// Bounded protocol projection, not production IPC or the job-table spinlock.
// The mutex stands for the existing job-operation linearization boundary.
fn application_job_retirement_race() {
    // members, killing, loader descriptor owned, number of closes
    let state = Arc::new(Mutex::new((2_u32, false, true, 0_u32)));
    let publisher_state = state.clone();
    let publisher = thread::spawn(move || {
        let mut state = publisher_state.lock().unwrap();
        if !state.1 { state.0 += 1; }
    });
    let retire_state = state.clone();
    let retire = thread::spawn(move || {
        let remaining = {
            let mut state = retire_state.lock().unwrap();
            state.1 = true;
            state.0 = state.0.saturating_sub(1); // bounded kill may be partial
            state.0
        };
        if remaining == 0 {
            let mut state = retire_state.lock().unwrap();
            assert_eq!(state.0, 0, "Killing must refuse later publication");
            state.2 = false;
            state.3 += 1;
        }
    });
    let rootd_state = state.clone();
    let rootd = thread::spawn(move || {
        let mut state = rootd_state.lock().unwrap();
        state.1 = true;
        state.0 = 0; // ancestor retirement can complete the child's kill
    });
    publisher.join().unwrap();
    retire.join().unwrap();
    rootd.join().unwrap();
    let mut state = state.lock().unwrap();
    assert!(state.2 || state.0 == 0, "partial retirement must retain custody");
    if state.2 {
        assert_eq!(state.0, 0);
        state.2 = false; // next loader request collects the drained child
        state.3 += 1;
    }
    assert_eq!(state.3, 1, "the descriptor has one final closer");
}

#[test]
fn application_job_partial_retirement_and_ancestor_kill_keep_one_closer() {
    loom::model(application_job_retirement_race);
}
