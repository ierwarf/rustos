/// Same bound as the Loom kernel with PCT permutations across both growers
/// and the reader.
#[test]
fn slot_chunk_publication_race_publishes_once_and_readers_see_initialized_chunks_pct() {
    shuttle::check_pct(
        || {
            const NONE: usize = usize::MAX;
            let published = Arc::new(AtomicUsize::new(NONE));
            let bodies = Arc::new([AtomicUsize::new(0), AtomicUsize::new(0)]);
            let freed = Arc::new(AtomicUsize::new(0));

            let grow = |which: usize,
                        published: Arc<AtomicUsize>,
                        bodies: Arc<[AtomicUsize; 2]>,
                        freed: Arc<AtomicUsize>| {
                bodies[which].store(7, Ordering::Relaxed);
                if published
                    .compare_exchange(NONE, which, Ordering::AcqRel, Ordering::Acquire)
                    .is_err()
                {
                    freed.fetch_add(1, Ordering::AcqRel);
                }
            };
            let (p, b, f) = (published.clone(), bodies.clone(), freed.clone());
            let first = thread::spawn(move || grow(0, p, b, f));
            let (p, b, f) = (published.clone(), bodies.clone(), freed.clone());
            let second = thread::spawn(move || grow(1, p, b, f));
            let (p, b) = (published.clone(), bodies.clone());
            let reader = thread::spawn(move || {
                let seen = p.load(Ordering::Acquire);
                if seen != NONE {
                    assert_eq!(b[seen].load(Ordering::Relaxed), 7);
                }
            });
            first.join().unwrap();
            second.join().unwrap();
            reader.join().unwrap();
            let winner = published.load(Ordering::Acquire);
            assert!(winner == 0 || winner == 1);
            assert_eq!(freed.load(Ordering::Acquire), 1);
        },
        iterations(),
        pct_depth(),
    );
}

/// PCT variant of the side-storage-before-slot ordering.
#[test]
fn a_slot_is_allocatable_only_after_its_side_chunks_are_published_pct() {
    shuttle::check_pct(
        || {
            let side_chunk = Arc::new(AtomicUsize::new(0));
            let table_len = Arc::new(AtomicUsize::new(0));
            let (side, len) = (side_chunk.clone(), table_len.clone());
            let grower = thread::spawn(move || {
                side.store(7, Ordering::Release);
                len.store(1, Ordering::Release);
            });
            let (side, len) = (side_chunk.clone(), table_len.clone());
            let allocator = thread::spawn(move || {
                if len.load(Ordering::Acquire) >= 1 {
                    assert_eq!(side.load(Ordering::Acquire), 7);
                }
            });
            grower.join().unwrap();
            allocator.join().unwrap();
        },
        iterations(),
        pct_depth(),
    );
}
