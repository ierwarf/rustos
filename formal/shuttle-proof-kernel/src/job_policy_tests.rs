/// PCT projection of the same exact-incarnation protocol as the Loom witness.
/// Shuttle covers scheduled interleavings, not weak-memory reorderings.
fn job_policy_publication_case(old: usize, new: usize, expected: usize) {
    let incarnation = Arc::new(AtomicUsize::new(1));
    let payload = Arc::new(AtomicUsize::new(old));
    let (id, data) = (incarnation.clone(), payload.clone());
    let writer = thread::spawn(move || {
        // ORDERING: revoke the old incarnation before replacement.
        id.store(0, Ordering::Release);
        // ORDERING: Release binds the payload to that revocation.
        data.store(new, Ordering::Release);
        // ORDERING: Release commits the new initialized incarnation.
        id.store(2, Ordering::Release);
    });
    let (id, data) = (incarnation.clone(), payload.clone());
    let reader = thread::spawn(move || {
        // ORDERING: Acquire resolves the exact requested incarnation.
        let before = id.load(Ordering::Acquire);
        if before == expected {
            // ORDERING: Acquire observes initialization and prior revocation.
            let seen = data.load(Ordering::Acquire);
            // ORDERING: Acquire rejects a replacement during this read.
            let after = id.load(Ordering::Acquire);
            if before == after {
                assert_eq!(seen, if expected == 1 { old } else { new });
            }
        }
    });
    writer.join().unwrap();
    reader.join().unwrap();
}

#[test]
fn job_policy_membership_generation_and_payload_have_one_publication_pct() {
    for expected in 1..=2 {
        shuttle::check_pct(move || job_policy_publication_case(11, 22, expected),
                           iterations(), pct_depth());
    }
}

#[test]
fn job_policy_word_generation_and_payload_have_one_publication_pct() {
    for expected in 1..=2 {
        shuttle::check_pct(move || job_policy_publication_case(0, 1, expected),
                           iterations(), pct_depth());
    }
}

#[test]
fn job_policy_tightening_is_visible_after_acknowledged_completion_pct() {
    shuttle::check_pct(|| {
        let parent = Arc::new(AtomicBool::new(true));
        let child = Arc::new(AtomicBool::new(true));
        let returned = Arc::new(AtomicBool::new(false));
        let (p, c, done) = (parent.clone(), child.clone(), returned.clone());
        let writer = thread::spawn(move || {
            // ORDERING: Release publishes the parent's changed operation word.
            p.store(false, Ordering::Release);
            // ORDERING: Release updates the existing descendant before return.
            c.store(false, Ordering::Release);
            // ORDERING: the explicit caller acknowledgment follows all updates.
            done.store(true, Ordering::Release);
        });
        let (c, done) = (child.clone(), returned.clone());
        let reader = thread::spawn(move || {
            // ORDERING: Acquire observes acknowledged SET completion, not time.
            let after_return = done.load(Ordering::Acquire);
            // ORDERING: Acquire observes the affected descendant word.
            let allowed = c.load(Ordering::Acquire);
            if after_return { assert!(!allowed); }
        });
        writer.join().unwrap();
        reader.join().unwrap();
        // ORDERING: joins precede final post-completion observations.
        assert!(!parent.load(Ordering::Acquire));
        // ORDERING: completion includes the descendant's update too.
        assert!(!child.load(Ordering::Acquire));
    }, iterations(), pct_depth());
}
