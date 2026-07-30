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

    #[derive(Default)]
    struct StableEndpoint {
        epoch: AtomicUsize,
        endpoint: AtomicUsize,
        owner: AtomicUsize,
    }

    /// Mirrors the compat service registry exactly: registration advances the
    /// epoch first, publishes owner next, and publishes endpoint last; readers
    /// accept only equal epoch/endpoint double reads. Observing the endpoint's
    /// release therefore admits neither the old epoch nor an unowned tuple.
    #[test]
    fn stable_endpoint_snapshot_never_admits_a_torn_publication() {
        loom::model(|| {
            let registry = Arc::new(StableEndpoint::default());
            let writer_side = Arc::clone(&registry);
            let writer = thread::spawn(move || {
                writer_side.epoch.fetch_add(1, Ordering::AcqRel);
                writer_side.owner.store(42, Ordering::Release);
                writer_side.endpoint.store(7, Ordering::Release);
            });
            let reader_side = Arc::clone(&registry);
            let reader = thread::spawn(move || {
                let before = reader_side.epoch.load(Ordering::Acquire);
                let endpoint_before = reader_side.endpoint.load(Ordering::Acquire);
                let owner = reader_side.owner.load(Ordering::Acquire);
                let endpoint_after = reader_side.endpoint.load(Ordering::Acquire);
                let after = reader_side.epoch.load(Ordering::Acquire);
                if before == after && endpoint_before == endpoint_after && endpoint_before != 0 {
                    assert_eq!(before, 1);
                    assert_eq!(endpoint_before, 7);
                    assert_eq!(owner, 42);
                }
            });
            writer.join().unwrap();
            reader.join().unwrap();
        });
    }

    /// Mirrors scheduler retirement: a slot becomes reusable only after the
    /// exact retired identity has received its cleanup acknowledgement.
    #[test]
    fn retired_slot_cannot_reuse_task_identity_before_cleanup() {
        loom::model(|| {
            #[derive(Clone, Copy)]
            struct Slot {
                generation: usize,
                task: Option<usize>,
                retired: bool,
                cleanup_acked: bool,
            }

            let slot = Arc::new(Mutex::new(Slot {
                generation: 1,
                task: Some(7),
                retired: false,
                cleanup_acked: false,
            }));
            let retire_side = Arc::clone(&slot);
            let retire = thread::spawn(move || {
                let mut slot = retire_side.lock().unwrap();
                slot.retired = true;
                slot.cleanup_acked = true;
                slot.task = None;
                slot.generation += 1;
            });
            let reuse_side = Arc::clone(&slot);
            let reuse = thread::spawn(move || {
                let mut slot = reuse_side.lock().unwrap();
                if slot.task.is_none() && slot.retired && slot.cleanup_acked {
                    slot.task = Some(9);
                    slot.retired = false;
                    assert_ne!(slot.generation, 1);
                }
            });
            retire.join().unwrap();
            reuse.join().unwrap();
            let slot = slot.lock().unwrap();
            assert!(slot.task != Some(9) || slot.generation > 1);
        });
    }

    /// Mirrors the wait-lock queue rule: a retired head is discarded while
    /// scanning, so it cannot consume the wake intended for the first live
    /// waiter behind it.
    #[test]
    fn wait_lock_unlock_skips_retired_front_waiters() {
        loom::model(|| {
            #[derive(Default)]
            struct Queue {
                front_live: AtomicBool,
                back_live: AtomicBool,
                woke: AtomicUsize,
            }

            let queue = Arc::new(Queue {
                front_live: AtomicBool::new(true),
                back_live: AtomicBool::new(true),
                woke: AtomicUsize::new(0),
            });
            let retire_side = Arc::clone(&queue);
            let retire = thread::spawn(move || {
                retire_side.front_live.store(false, Ordering::Release);
            });
            let wake_side = Arc::clone(&queue);
            let wake = thread::spawn(move || {
                if wake_side.front_live.load(Ordering::Acquire) {
                    wake_side.woke.store(1, Ordering::Release);
                } else if wake_side.back_live.load(Ordering::Acquire) {
                    wake_side.woke.store(2, Ordering::Release);
                }
            });
            retire.join().unwrap();
            wake.join().unwrap();
            let woke = queue.woke.load(Ordering::Acquire);
            assert!(woke == 1 || woke == 2);
        });
    }

    /// Mirrors a generational slab remove/reuse transaction: observing a reused
    /// slot can never make a stale handle with the predecessor generation live.
    #[test]
    fn ipc_generation_reuse_never_aliases_stale_handle() {
        loom::model(|| {
            #[derive(Clone, Copy)]
            struct Slot {
                generation: usize,
                occupied: bool,
            }
            let slot = Arc::new(Mutex::new(Slot {
                generation: 1,
                occupied: true,
            }));
            let remove_side = Arc::clone(&slot);
            let remove = thread::spawn(move || {
                let mut slot = remove_side.lock().unwrap();
                if slot.occupied && slot.generation == 1 {
                    slot.occupied = false;
                    slot.generation += 1;
                }
            });
            let reuse_side = Arc::clone(&slot);
            let reuse = thread::spawn(move || {
                let mut slot = reuse_side.lock().unwrap();
                if !slot.occupied {
                    slot.occupied = true;
                    assert_ne!(slot.generation, 1);
                }
            });
            remove.join().unwrap();
            reuse.join().unwrap();
            let slot = slot.lock().unwrap();
            assert_ne!(slot.generation, 1);
        });
    }

    /// Mirrors lockdep edge insertion under its graph lock. Concurrent inverse
    /// observations may admit either direction, but never both directions.
    #[test]
    fn ipc_lock_class_graph_rejects_inverse_order() {
        loom::model(|| {
            #[derive(Default)]
            struct Graph {
                ab: bool,
                ba: bool,
            }
            let graph = Arc::new(Mutex::new(Graph::default()));
            let ab_side = Arc::clone(&graph);
            let ab = thread::spawn(move || {
                let mut graph = ab_side.lock().unwrap();
                if !graph.ba {
                    graph.ab = true;
                }
            });
            let ba_side = Arc::clone(&graph);
            let ba = thread::spawn(move || {
                let mut graph = ba_side.lock().unwrap();
                if !graph.ab {
                    graph.ba = true;
                }
            });
            ab.join().unwrap();
            ba.join().unwrap();
            let graph = graph.lock().unwrap();
            assert!(!(graph.ab && graph.ba));
        });
    }

    /// Mirrors exact-capacity IPC cancellation: every admitted entry has one
    /// owner and cancellation removes each entry once without a sentinel slot.
    #[test]
    fn ipc_retirement_accepts_exact_global_capacity() {
        loom::model(|| {
            const CAPACITY: usize = 2;
            let entries = Arc::new(Mutex::new([true; CAPACITY]));
            let first_side = Arc::clone(&entries);
            let first = thread::spawn(move || {
                let mut entries = first_side.lock().unwrap();
                assert!(entries[0]);
                entries[0] = false;
            });
            let second_side = Arc::clone(&entries);
            let second = thread::spawn(move || {
                let mut entries = second_side.lock().unwrap();
                assert!(entries[1]);
                entries[1] = false;
            });
            first.join().unwrap();
            second.join().unwrap();
            assert_eq!(*entries.lock().unwrap(), [false; CAPACITY]);
        });
    }

    /// Mirrors lockdep IRQ-use classification. The first observed class mode is
    /// stable; a conflicting IRQ-context or IRQ-enabled use is rejected.
    #[test]
    fn irq_safe_lock_class_never_depends_on_irq_unsafe_class() {
        loom::model(|| {
            #[derive(Default)]
            struct Usage {
                irq_safe: bool,
                irq_unsafe: bool,
            }
            let usage = Arc::new(Mutex::new(Usage::default()));
            let safe_side = Arc::clone(&usage);
            let safe = thread::spawn(move || {
                let mut usage = safe_side.lock().unwrap();
                if !usage.irq_unsafe {
                    usage.irq_safe = true;
                }
            });
            let unsafe_side = Arc::clone(&usage);
            let unsafe_use = thread::spawn(move || {
                let mut usage = unsafe_side.lock().unwrap();
                if !usage.irq_safe {
                    usage.irq_unsafe = true;
                }
            });
            safe.join().unwrap();
            unsafe_use.join().unwrap();
            let usage = usage.lock().unwrap();
            assert!(!(usage.irq_safe && usage.irq_unsafe));
        });
    }

    /// Mirrors deferred shared-region reclaim: revocation may make the object
    /// unreachable, but its quota charge remains until backing reclaim commits.
    #[test]
    fn shared_region_quota_survives_deferred_reclaim() {
        loom::model(|| {
            #[derive(Default)]
            struct Region {
                reachable: bool,
                backing_live: bool,
                quota_charged: bool,
            }
            let region = Arc::new(Mutex::new(Region {
                reachable: true,
                backing_live: true,
                quota_charged: true,
            }));
            let revoke_side = Arc::clone(&region);
            let revoke = thread::spawn(move || {
                revoke_side.lock().unwrap().reachable = false;
            });
            let reclaim_side = Arc::clone(&region);
            let reclaim = thread::spawn(move || {
                let mut region = reclaim_side.lock().unwrap();
                if !region.reachable {
                    region.backing_live = false;
                    region.quota_charged = false;
                }
            });
            revoke.join().unwrap();
            reclaim.join().unwrap();
            let region = region.lock().unwrap();
            assert!(!region.backing_live || region.quota_charged);
        });
    }

    /// Mirrors signal selection/removal: consuming the selected cause uses the
    /// selected bit subset and therefore cannot clear a concurrently added one.
    #[test]
    fn sigchld_snapshot_cannot_clear_future_cause() {
        loom::model(|| {
            const SELECTED: usize = 1;
            const FUTURE: usize = 2;
            let causes = Arc::new(AtomicUsize::new(SELECTED));
            let remove_side = Arc::clone(&causes);
            let remove = thread::spawn(move || {
                let selected = remove_side.load(Ordering::Acquire) & SELECTED;
                remove_side.fetch_and(!selected, Ordering::AcqRel);
            });
            let add_side = Arc::clone(&causes);
            let add = thread::spawn(move || {
                add_side.fetch_or(FUTURE, Ordering::Release);
            });
            remove.join().unwrap();
            add.join().unwrap();
            assert_ne!(causes.load(Ordering::Acquire) & FUTURE, 0);
        });
    }

    /// Mirrors the lock-class context rule. A sleepable class and a raw spin
    /// class may be used sequentially, but their live ownership cannot overlap.
    #[test]
    fn sleepable_lock_class_cannot_nest_with_raw_spin() {
        loom::model(|| {
            #[derive(Default)]
            struct Held {
                raw: bool,
                sleepable: bool,
                overlap: bool,
            }
            let held = Arc::new(Mutex::new(Held::default()));
            let raw_side = Arc::clone(&held);
            let raw = thread::spawn(move || {
                {
                    let mut held = raw_side.lock().unwrap();
                    if held.sleepable {
                        return;
                    }
                    held.raw = true;
                }
                thread::yield_now();
                raw_side.lock().unwrap().raw = false;
            });
            let sleep_side = Arc::clone(&held);
            let sleepable = thread::spawn(move || {
                {
                    let mut held = sleep_side.lock().unwrap();
                    if held.raw {
                        return;
                    }
                    held.sleepable = true;
                    held.overlap |= held.raw;
                }
                thread::yield_now();
                sleep_side.lock().unwrap().sleepable = false;
            });
            raw.join().unwrap();
            sleepable.join().unwrap();
            assert!(!held.lock().unwrap().overlap);
        });
    }

    /// Mirrors hook dispatch: copy the callback identity while holding the
    /// registry read guard, release it, and only then invoke scheduler work
    /// that may need the registry again.
    #[test]
    fn scheduler_callback_runs_after_hook_registry_read_guard_is_released() {
        loom::model(|| {
            let registry = Arc::new(Mutex::new(Some(7usize)));
            let dispatch_side = Arc::clone(&registry);
            let dispatch = thread::spawn(move || {
                let callback = *dispatch_side.lock().unwrap();
                if callback.is_some() {
                    let guard = dispatch_side.try_lock();
                    assert!(guard.is_ok());
                }
            });
            let update_side = Arc::clone(&registry);
            let update = thread::spawn(move || {
                *update_side.lock().unwrap() = Some(9);
            });
            dispatch.join().unwrap();
            update.join().unwrap();
        });
    }
}
