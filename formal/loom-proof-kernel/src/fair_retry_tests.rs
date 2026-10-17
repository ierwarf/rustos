/// A torn fair epoch may be retried, but neither that observation nor a
/// later Local-to-Local republish can grant a claim for a different owner.
/// Models one writer and four bounded reader observations, not all candidates
/// or the catalog-independent policy comparator.
#[test]
fn incomplete_fair_retry_only_claims_its_observed_generation() {
    loom::model(|| {
        let version = Arc::new(AtomicUsize::new(2));
        let owner = Arc::new(AtomicUsize::new(1));
        let accepted = Arc::new(AtomicUsize::new(0));
        let writer_version = version.clone();
        let writer_owner = owner.clone();
        let writer = thread::spawn(move || {
            writer_version.fetch_add(1, Ordering::AcqRel); // odd
            let _ = writer_owner.compare_exchange(1, 2, Ordering::AcqRel, Ordering::Acquire);
            writer_version.fetch_add(1, Ordering::Release); // even
        });
        let reader_version = version.clone();
        let reader_owner = owner.clone();
        let reader_accepted = accepted.clone();
        let reader = thread::spawn(move || {
            for _ in 0..4 {
                let before = reader_version.load(Ordering::Acquire);
                if before & 1 != 0 {
                    continue;
                }
                let generation = reader_owner.load(Ordering::Acquire);
                let after = reader_version.load(Ordering::Acquire);
                if before != after || after & 1 != 0 {
                    continue;
                }
                if reader_owner.compare_exchange(
                    generation, generation + 10, Ordering::AcqRel, Ordering::Acquire,
                ).is_ok() {
                    reader_accepted.store(generation, Ordering::Release);
                }
                break;
            }
        });
        writer.join().unwrap();
        reader.join().unwrap();
        match accepted.load(Ordering::Acquire) {
            0 => assert!(matches!(owner.load(Ordering::Acquire), 1 | 2)),
            1 => assert_eq!(owner.load(Ordering::Acquire), 11),
            2 => assert_eq!(owner.load(Ordering::Acquire), 12),
            other => panic!("claimed unobserved generation {other}"),
        }
    });
}

/// An early strict-class observation starts inside an odd publication. It
/// may claim the exact System generation only after a stable even epoch;
/// bounded exhaustion must remain guarded rather than selecting later User
/// work from an absent catalog comparator.
#[test]
fn incomplete_early_policy_never_authorizes_a_later_class() {
    loom::model(|| {
        let epoch = Arc::new(AtomicUsize::new(3));
        let owner = Arc::new(AtomicUsize::new(1));
        let decision = Arc::new(AtomicUsize::new(0)); // 2: exact System, 3: guarded
        let writer_epoch = epoch.clone();
        let writer_owner = owner.clone();
        let writer = thread::spawn(move || {
            writer_owner.store(2, Ordering::Release);
            writer_epoch.store(4, Ordering::Release);
        });
        let reader_epoch = epoch.clone();
        let reader_owner = owner.clone();
        let reader_decision = decision.clone();
        let reader = thread::spawn(move || {
            for _ in 0..4 {
                let before = reader_epoch.load(Ordering::Acquire);
                if before & 1 != 0 { continue; }
                let generation = reader_owner.load(Ordering::Acquire);
                let after = reader_epoch.load(Ordering::Acquire);
                if before != after || after & 1 != 0 { continue; }
                if reader_owner.compare_exchange(generation, generation + 10,
                    Ordering::AcqRel, Ordering::Acquire).is_ok() {
                    reader_decision.store(generation, Ordering::Release);
                }
                return;
            }
            reader_decision.store(3, Ordering::Release);
        });
        writer.join().unwrap();
        reader.join().unwrap();
        match decision.load(Ordering::Acquire) {
            2 => assert_eq!(owner.load(Ordering::Acquire), 12),
            3 => assert_eq!(owner.load(Ordering::Acquire), 2),
            other => panic!("unresolved early policy granted a later peer: {other}"),
        }
    });
}
