//! Small concurrency proof kernels mapped to concrete RustOS owner protocols.

#[cfg(test)]
mod tests {
    use loom::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use loom::sync::{Arc, Mutex};
    use loom::thread;

    #[derive(Default)]
    struct Registry {
        exiting: AtomicBool,
        endpoint: Mutex<Option<u64>>,
    }

    /// Mirrors endpoint publication's lock-and-recheck rule: the exit marker is
    /// the revocation linearization point, and no publication can survive exit.
    #[test]
    fn exit_and_publication_never_leave_authority_live() {
        loom::model(|| {
            let registry = Arc::new(Registry::default());
            let publisher = Arc::clone(&registry);
            let publish = thread::spawn(move || {
                if publisher.exiting.load(Ordering::Acquire) {
                    return;
                }
                let mut endpoint = publisher.endpoint.lock().unwrap();
                if !publisher.exiting.load(Ordering::Acquire) {
                    *endpoint = Some(7);
                }
            });
            let exiting = Arc::clone(&registry);
            let revoke = thread::spawn(move || {
                exiting.exiting.store(true, Ordering::Release);
                *exiting.endpoint.lock().unwrap() = None;
            });
            publish.join().unwrap();
            revoke.join().unwrap();
            if registry.exiting.load(Ordering::Acquire) {
                assert!(registry.endpoint.lock().unwrap().is_none());
            }
        });
    }

    #[derive(Default)]
    struct WaitSet {
        generation: AtomicUsize,
        observed: AtomicUsize,
        waiter: Mutex<Option<u64>>,
        slept: AtomicBool,
        woke: AtomicBool,
    }

    /// Mirrors check-register-recheck: if a signal advances readiness across
    /// the arm window, a thread that committed to sleeping must be woken.
    #[test]
    fn readiness_change_cannot_be_lost_across_arm() {
        loom::model(|| {
            let waitset = Arc::new(WaitSet::default());
            let waiter_side = Arc::clone(&waitset);
            let waiter = thread::spawn(move || {
                let observed = waiter_side.generation.load(Ordering::Acquire);
                waiter_side.observed.store(observed, Ordering::Release);
                *waiter_side.waiter.lock().unwrap() = Some(7);

                let current = waiter_side.generation.load(Ordering::Acquire);
                let still_registered = waiter_side.waiter.lock().unwrap().is_some();
                if current == observed && still_registered {
                    waiter_side.slept.store(true, Ordering::Release);
                }
            });
            let signal_side = Arc::clone(&waitset);
            let signal = thread::spawn(move || {
                signal_side.generation.fetch_add(1, Ordering::AcqRel);
                if signal_side.waiter.lock().unwrap().take().is_some() {
                    signal_side.woke.store(true, Ordering::Release);
                }
            });
            waiter.join().unwrap();
            signal.join().unwrap();

            let slept = waitset.slept.load(Ordering::Acquire);
            let generation = waitset.generation.load(Ordering::Acquire);
            let observed = waitset.observed.load(Ordering::Acquire);
            let woke = waitset.woke.load(Ordering::Acquire);
            assert!(!(slept && generation > observed && !woke));
        });
    }
}
