//! Small concurrency proof kernels mapped to concrete RustOS owner protocols.

#[cfg(test)]
mod tests {
    use loom::sync::atomic::{AtomicBool, Ordering};
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
}
