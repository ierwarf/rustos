/// Two growers race to publish the same directory chunk. The chunk body is
/// written before the release CAS that publishes it, a loser frees only its
/// own unpublished chunk, and a concurrent reader either finds no chunk or a
/// fully initialized one. Projection of `AtomicDir::ensure` / `get`: the chunk
/// pointer is an index into two candidate bodies, and the allocator is not
/// modelled.
#[test]
fn slot_chunk_publication_race_publishes_once_and_readers_see_initialized_chunks() {
    loom::model(|| {
        const NONE: usize = usize::MAX;
        let published = Arc::new(AtomicUsize::new(NONE));
        let bodies = Arc::new([AtomicUsize::new(0), AtomicUsize::new(0)]);
        let freed = Arc::new(AtomicUsize::new(0));

        let grow = |which: usize,
                    published: Arc<AtomicUsize>,
                    bodies: Arc<[AtomicUsize; 2]>,
                    freed: Arc<AtomicUsize>| {
            // build_chunk: initialize the whole body first.
            bodies[which].store(7, Ordering::Relaxed);
            // ORDERING: release publishes the initialized body.
            if published
                .compare_exchange(NONE, which, Ordering::AcqRel, Ordering::Acquire)
                .is_err()
            {
                // The loser drops its own unpublished body.
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
                assert_eq!(b[seen].load(Ordering::Relaxed), 7, "a published chunk is initialized");
            }
        });

        first.join().unwrap();
        second.join().unwrap();
        reader.join().unwrap();
        let winner = published.load(Ordering::Acquire);
        assert!(winner == 0 || winner == 1, "exactly one chunk is published");
        assert_eq!(freed.load(Ordering::Acquire), 1, "the loser freed exactly its own chunk");
    });
}

/// A slot becomes allocatable only after the side-array chunk addressing its
/// index is published. The grower publishes the side chunk, then raises the
/// table length with a release store; an allocator that observes the length
/// and then indexes the side array must find an initialized chunk. Projection
/// of `grow_slots` followed by `reserve_spawn`.
#[test]
fn a_slot_is_allocatable_only_after_its_side_chunks_are_published() {
    loom::model(|| {
        let side_chunk = Arc::new(AtomicUsize::new(0)); // 0 = unpublished, 7 = initialized
        let table_len = Arc::new(AtomicUsize::new(0));

        let (side, len) = (side_chunk.clone(), table_len.clone());
        let grower = thread::spawn(move || {
            // ensure_slot_storage for every side array first ...
            side.store(7, Ordering::Release);
            // ... then the table chunk, which is what makes a slot allocatable.
            len.store(1, Ordering::Release);
        });
        let (side, len) = (side_chunk.clone(), table_len.clone());
        let allocator = thread::spawn(move || {
            if len.load(Ordering::Acquire) >= 1 {
                assert_eq!(
                    side.load(Ordering::Acquire),
                    7,
                    "side-array storage exists before its slot can be reserved"
                );
            }
        });
        grower.join().unwrap();
        allocator.join().unwrap();
    });
}
