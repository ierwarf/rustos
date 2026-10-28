/// Two terminators of one process, the root supervisor and the memory-pressure
/// watchdog. A request publishes the termination with one compare-exchange, a
/// request that finds one queued joins it, and only the publisher runs the
/// once-per-process follow-up. The bounded model excludes the exit worker and
/// the exit receipt, and shows the follow-up count is one for every schedule.
#[test]
fn concurrent_terminators_finalize_a_process_exactly_once() {
    loom::model(|| {
        let queued = Arc::new(AtomicBool::new(false));
        let finalized = Arc::new(AtomicUsize::new(0));

        let terminate = |queued: Arc<AtomicBool>, finalized: Arc<AtomicUsize>| {
            // request_marked: join a queued request, otherwise publish one.
            if queued
                .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
            {
                finalized.fetch_add(1, Ordering::AcqRel);
            }
        };
        let (q, f) = (queued.clone(), finalized.clone());
        let supervisor = thread::spawn(move || terminate(q, f));
        let (q, f) = (queued.clone(), finalized.clone());
        let watchdog = thread::spawn(move || terminate(q, f));

        supervisor.join().unwrap();
        watchdog.join().unwrap();
        assert_eq!(finalized.load(Ordering::Acquire), 1);
        assert!(queued.load(Ordering::Acquire));
    });
}
