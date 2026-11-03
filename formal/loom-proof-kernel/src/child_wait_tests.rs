/// One previous slot occupant is withdrawn and a new waiter arms while a
/// producer publishes under the process-table lock. The wake scan deliberately
/// reads task and parent separately, so stale/spurious hints are permitted.
/// Scheduler wake/commit and multiple child consumers are outside this model.
#[test]
fn child_wait_reuse_and_recheck_cannot_lose_published_status() {
    loom::model(|| {
        struct State {
            task: AtomicUsize,
            parent: AtomicUsize,
            status: Mutex<bool>,
            pending: AtomicBool,
            woke_new: AtomicBool,
        }
        let s = Arc::new(State {
            task: AtomicUsize::new(1), parent: AtomicUsize::new(1),
            status: Mutex::new(false), pending: AtomicBool::new(false),
            woke_new: AtomicBool::new(false),
        });
        let w = s.clone();
        let waiter = thread::spawn(move || {
            w.task.compare_exchange(1, 0, Ordering::Release, Ordering::Relaxed).unwrap();
            w.task.compare_exchange(0, 2, Ordering::Acquire, Ordering::Relaxed).unwrap();
            w.parent.store(2, Ordering::Release);
            fence(Ordering::SeqCst);
            w.pending.store(!*w.status.lock().unwrap(), Ordering::Relaxed);
        });
        let p = s.clone();
        let producer = thread::spawn(move || {
            *p.status.lock().unwrap() = true;
            fence(Ordering::SeqCst);
            let task = p.task.load(Ordering::Acquire);
            if task != 0 && p.parent.load(Ordering::Acquire) == 2 && task == 2 {
                p.woke_new.store(true, Ordering::Relaxed);
            }
        });
        waiter.join().unwrap();
        producer.join().unwrap();
        assert!(!s.pending.load(Ordering::Relaxed) || s.woke_new.load(Ordering::Relaxed));
    });
}
