    /// Mirrors `PublishedPagerVma`: a seqlock payload plus a count of
    /// exception-time installers that hold a permit against it.
    struct PagerVmaPublication {
        sequence: AtomicUsize,
        installers: AtomicUsize,
        /// Stands for the prepared leaf a permit holder may write. The writer
        /// may only touch it once every permit is gone.
        leaf: AtomicUsize,
        writer_touched_leaf: AtomicBool,
    }

    impl PagerVmaPublication {
        fn published() -> Self {
            Self {
                sequence: AtomicUsize::new(2),
                installers: AtomicUsize::new(0),
                leaf: AtomicUsize::new(0),
                writer_touched_leaf: AtomicBool::new(false),
            }
        }
    }

    /// The decisive property of the ring0 anonymous fault path.
    ///
    /// An installer takes no lock: it reads an even sequence, registers
    /// itself, re-reads, and only then writes the prepared leaf. A withdrawing
    /// writer publishes an odd sequence, drains the installer count, and only
    /// then owns the leaf. If those two can overlap, a `munmap` reclaims a
    /// frame an exception-time CAS is still installing - a use-after-free that
    /// no test in the tree covered, because the fault path had no Loom row at
    /// all until this one.
    #[test]
    fn a_withdrawing_writer_never_owns_a_leaf_an_installer_still_holds() {
        loom::model(|| {
            // Already published, with no concurrent initialization: in the
            // kernel a withdrawal and the publication it withdraws are
            // serialized by the global VMA writer lock, so a writer never
            // races the store that established the sequence it reads.
            let vma = Arc::new(PagerVmaPublication::published());

            let installer_vma = Arc::clone(&vma);
            let installer = thread::spawn(move || {
                let before = installer_vma.sequence.load(Ordering::Acquire);
                if before == 0 || before & 1 != 0 {
                    return;
                }
                installer_vma.installers.fetch_add(1, Ordering::AcqRel);
                // The store-buffer barrier. Without it this load may be
                // reordered before the registration above, and the writer's
                // mirror-image pair may be reordered too, so both sides can
                // conclude the other is absent.
                fence(Ordering::SeqCst);
                let after = installer_vma.sequence.load(Ordering::Acquire);
                if before != after {
                    installer_vma.installers.fetch_sub(1, Ordering::Release);
                    return;
                }
                // Permit held: the one leaf write this path is allowed.
                let touched = installer_vma.writer_touched_leaf.load(Ordering::Acquire);
                let seq_now = installer_vma.sequence.load(Ordering::SeqCst);
                let count_now = installer_vma.installers.load(Ordering::SeqCst);
                assert!(
                    !touched,
                    "writer took the leaf while a permit was held: before={before} after={after} seq_now={seq_now} installers={count_now}"
                );
                installer_vma.leaf.store(1, Ordering::Release);
                installer_vma.installers.fetch_sub(1, Ordering::Release);
            });

            let writer_vma = Arc::clone(&vma);
            let writer = thread::spawn(move || {
                let before = writer_vma.sequence.load(Ordering::Relaxed);
                writer_vma.sequence.store(before + 1, Ordering::Release);
                // The matching half of the installer's barrier.
                fence(Ordering::SeqCst);
                // Bounded drain. In the kernel this is wall-clock bounded; in
                // the model every installer is finite, so a load suffices.
                while writer_vma.installers.load(Ordering::Acquire) != 0 {
                    loom::thread::yield_now();
                }
                writer_vma.writer_touched_leaf.store(true, Ordering::Release);
                writer_vma.leaf.store(0, Ordering::Release);
            });

            installer.join().unwrap();
            writer.join().unwrap();
        });
    }

    /// Mirrors `FaultFramePool`: an availability count that authorizes a claim
    /// and a slot array the claim then scans.
    #[derive(Default)]
    struct FaultFrameReserve {
        available: AtomicUsize,
        slot_a: AtomicUsize,
        slot_b: AtomicUsize,
    }

    /// The count is authority, not a census.
    ///
    /// A claimer decrements before it scans, so two claimers can never be
    /// promised the same frame, and a claimer that wins the decrement always
    /// finds one. The previous shape recomputed the depth by sweeping the
    /// array on every fault, which is both O(pool) on the fault path and
    /// unable to state this property at all.
    #[test]
    fn two_claimers_never_receive_the_same_reserve_frame() {
        loom::model(|| {
            let pool = Arc::new(FaultFrameReserve::default());
            pool.slot_a.store(11, Ordering::Release);
            pool.slot_b.store(22, Ordering::Release);
            pool.available.store(2, Ordering::Release);

            let claim = |pool: Arc<FaultFrameReserve>| {
                thread::spawn(move || {
                    let granted = pool
                        .available
                        .fetch_update(Ordering::AcqRel, Ordering::Acquire, |available| {
                            available.checked_sub(1)
                        })
                        .is_ok();
                    if !granted {
                        return 0;
                    }
                    let taken = pool.slot_a.swap(0, Ordering::AcqRel);
                    if taken != 0 {
                        return taken;
                    }
                    let taken = pool.slot_b.swap(0, Ordering::AcqRel);
                    assert_ne!(
                        taken, 0,
                        "a claimer won the count but the array held no frame"
                    );
                    taken
                })
            };
            let first = claim(Arc::clone(&pool));
            let second = claim(Arc::clone(&pool));
            let first = first.join().unwrap();
            let second = second.join().unwrap();
            assert!(
                first == 0 || second == 0 || first != second,
                "two claimers received the same frame: {first} and {second}"
            );
        });
    }
