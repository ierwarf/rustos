/// One replacement edge shared by the immutable membership and cached policy
/// word protocols. This models the atomic publication, not production Current,
/// table locking, MM teardown or the complete 256-bit snapshot.
fn job_policy_publication_case(payload_order: Ordering, old: usize, new: usize, expected: usize) {
    let incarnation = Arc::new(AtomicUsize::new(1));
    let payload = Arc::new(AtomicUsize::new(old));
    let (id, data) = (incarnation.clone(), payload.clone());
    let writer = thread::spawn(move || {
        // ORDERING: revoke the old incarnation before replacing its payload.
        id.store(0, Ordering::Release);
        // ORDERING: production Release joins this replacement to revocation;
        // the Relaxed variant is used only by the sensitivity test below.
        data.store(new, payload_order);
        // ORDERING: Release commits the new incarnation after initialization.
        id.store(2, Ordering::Release);
    });
    let (id, data) = (incarnation.clone(), payload.clone());
    let reader = thread::spawn(move || {
        // ORDERING: Acquire resolves the caller's exact requested incarnation.
        let before = id.load(Ordering::Acquire);
        if before == expected {
            // ORDERING: Acquire of replacement data observes its prior revoke.
            let seen = data.load(Ordering::Acquire);
            // ORDERING: Acquire revalidates the incarnation after payload.
            let after = id.load(Ordering::Acquire);
            if before == after {
                assert_eq!(seen, if expected == 1 { old } else { new },
                           "replacement payload under previous incarnation");
            }
        }
    });
    writer.join().unwrap();
    reader.join().unwrap();
}

#[test]
fn job_policy_membership_generation_and_payload_have_one_publication() {
    for expected in 1..=2 {
        // ORDERING: the membership payload uses the production Release bridge.
        loom::model(move || job_policy_publication_case(Ordering::Release, 11, 22, expected));
    }
}

#[test]
fn job_policy_word_generation_and_payload_have_one_publication() {
    for expected in 1..=2 {
        // ORDERING: the policy word uses the production Release bridge.
        loom::model(move || job_policy_publication_case(Ordering::Release, 0, 1, expected));
    }
}

#[test]
#[should_panic(expected = "replacement payload under previous incarnation")]
fn job_policy_replacement_requires_the_payload_release_bridge() {
    // Negative sensitivity fixture: removing the payload synchronization lets
    // a reader observe new permissive data while both identity reads are old.
    loom::model(|| job_policy_publication_case(Ordering::Relaxed, 0, 1, 1));
}

#[test]
fn job_policy_tightening_is_visible_after_acknowledged_completion() {
    loom::model(|| {
        let parent = Arc::new(AtomicBool::new(true));
        let child = Arc::new(AtomicBool::new(true));
        let returned = Arc::new(AtomicBool::new(false));
        let (p, c, done) = (parent.clone(), child.clone(), returned.clone());
        let writer = thread::spawn(move || {
            // ORDERING: Release publishes each affected operation word.
            p.store(false, Ordering::Release);
            // ORDERING: Release publishes the existing descendant too.
            c.store(false, Ordering::Release);
            // ORDERING: the caller's completion acknowledgment follows every
            // descendant update; this is not a wall-clock-only freshness claim.
            done.store(true, Ordering::Release);
        });
        let (c, done) = (child.clone(), returned.clone());
        let reader = thread::spawn(move || {
            // ORDERING: Acquire establishes that SET's response was observed.
            let after_return = done.load(Ordering::Acquire);
            // ORDERING: Acquire reads the descendant's current operation word.
            let allowed = c.load(Ordering::Acquire);
            if after_return { assert!(!allowed); }
        });
        writer.join().unwrap();
        reader.join().unwrap();
        // ORDERING: joins precede these final post-completion observations.
        assert!(!parent.load(Ordering::Acquire));
        // ORDERING: the descendant must also be denied after completion.
        assert!(!child.load(Ordering::Acquire));
    });
}
