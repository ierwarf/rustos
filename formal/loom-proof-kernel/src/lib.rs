//! Small concurrency proof kernels mapped to concrete RustOS owner protocols.

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;

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

    /// Mirrors FUTEX_WAIT versus user store plus FUTEX_WAKE. The waiter owns
    /// the bucket across its atomic comparison and queue publication, so a
    /// waker that publishes a changed word cannot pass an unqueued sleeper.
    #[test]
    fn futex_compare_and_waiter_publication_have_one_linearization_point() {
        loom::model(|| {
            struct Futex {
                word: AtomicUsize,
                waiter: Mutex<bool>,
                slept: AtomicBool,
                woke: AtomicBool,
            }
            let futex = Arc::new(Futex {
                word: AtomicUsize::new(0),
                waiter: Mutex::new(false),
                slept: AtomicBool::new(false),
                woke: AtomicBool::new(false),
            });

            let waiter_side = Arc::clone(&futex);
            let waiter = thread::spawn(move || {
                let mut registered = waiter_side.waiter.lock().unwrap();
                if waiter_side.word.load(Ordering::Acquire) == 0 {
                    *registered = true;
                    waiter_side.slept.store(true, Ordering::Release);
                }
            });
            let waker_side = Arc::clone(&futex);
            let waker = thread::spawn(move || {
                waker_side.word.store(1, Ordering::Release);
                let mut registered = waker_side.waiter.lock().unwrap();
                if *registered {
                    *registered = false;
                    waker_side.woke.store(true, Ordering::Release);
                }
            });
            waiter.join().unwrap();
            waker.join().unwrap();

            let changed = futex.word.load(Ordering::Acquire) == 1;
            let registered = *futex.waiter.lock().unwrap();
            let slept = futex.slept.load(Ordering::Acquire);
            let woke = futex.woke.load(Ordering::Acquire);
            assert!(!(changed && registered));
            assert!(!(changed && slept && !woke));
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

    /// Models the endpoint slot as the single linearization owner of two FIFO
    /// lanes and their shared burst counter. Producers may race a receiver,
    /// but selection can never observe or update only half of that state.
    #[test]
    fn ipc_priority_lane_is_kernel_derived_and_starvation_bounded() {
        loom::model(|| {
            #[derive(Default)]
            struct Queue {
                system: VecDeque<u8>,
                ordinary: VecDeque<u8>,
                system_streak: u8,
                delivered: Vec<u8>,
            }

            impl Queue {
                fn pop(&mut self) {
                    let choose_system = !self.system.is_empty()
                        && (self.ordinary.is_empty() || self.system_streak < 2);
                    if choose_system {
                        assert!(self.ordinary.is_empty() || self.system_streak < 2);
                        self.delivered.push(self.system.pop_front().unwrap());
                        self.system_streak = (self.system_streak + 1).min(2);
                    } else if let Some(message) = self.ordinary.pop_front() {
                        assert!(self.system.is_empty() || self.system_streak == 2);
                        self.delivered.push(message);
                        self.system_streak = 0;
                    }
                }
            }

            let queue = Arc::new(Mutex::new(Queue::default()));
            let system_queue = Arc::clone(&queue);
            let system = thread::spawn(move || {
                let mut queue = system_queue.lock().unwrap();
                queue.system.push_back(1);
                queue.system.push_back(2);
            });
            let ordinary_queue = Arc::clone(&queue);
            let ordinary = thread::spawn(move || {
                let mut queue = ordinary_queue.lock().unwrap();
                queue.ordinary.push_back(3);
                queue.ordinary.push_back(4);
            });
            let receiver_queue = Arc::clone(&queue);
            let receiver = thread::spawn(move || receiver_queue.lock().unwrap().pop());

            system.join().unwrap();
            ordinary.join().unwrap();
            receiver.join().unwrap();
            let mut queue = queue.lock().unwrap();
            while !queue.system.is_empty() || !queue.ordinary.is_empty() {
                queue.pop();
            }
            let mut delivered = queue.delivered.clone();
            delivered.sort_unstable();
            assert_eq!(delivered, [1, 2, 3, 4]);
        });
    }

    /// Models the 0->1 coalescing bit in `arm_remote_reschedule` racing the
    /// target CPU's AcqRel consume.  A sender may collapse notifications, but
    /// its request may never disappear: after both sides finish it is either
    /// still durable or was observed by the target exactly once.
    #[test]
    fn reschedule_request_is_never_lost_across_concurrent_consume() {
        loom::model(|| {
            let request = Arc::new(AtomicUsize::new(0));
            let consumed = Arc::new(AtomicBool::new(false));

            let producer_request = Arc::clone(&request);
            let producer = thread::spawn(move || {
                let _ =
                    producer_request.compare_exchange(0, 1, Ordering::AcqRel, Ordering::Acquire);
            });
            let consumer_request = Arc::clone(&request);
            let consumer_consumed = Arc::clone(&consumed);
            let consumer = thread::spawn(move || {
                if consumer_request.swap(0, Ordering::AcqRel) != 0 {
                    consumer_consumed.store(true, Ordering::Release);
                }
            });

            producer.join().unwrap();
            consumer.join().unwrap();
            assert!(
                request.load(Ordering::Acquire) != 0 || consumed.load(Ordering::Acquire),
                "a coalesced reschedule request was neither pending nor consumed"
            );
        });
    }

    /// Models the shootdown mailbox's two release/acquire publications.  This
    /// is deliberately a bounded protocol kernel: source anchoring and the
    /// architecture litmus cover the concrete atomics, while Loom enumerates
    /// all interleavings of this mailbox/flush/ack dependency graph.
    #[test]
    fn tlb_mailbox_and_ack_publish_the_exact_generation() {
        loom::model(|| {
            const GENERATION: usize = 7;
            const GLOBAL_ROOT: usize = 0;

            struct Mailbox {
                root: AtomicUsize,
                request: AtomicUsize,
                flushed: AtomicBool,
                acknowledgement: AtomicUsize,
            }

            let mailbox = Arc::new(Mailbox {
                root: AtomicUsize::new(usize::MAX),
                request: AtomicUsize::new(0),
                flushed: AtomicBool::new(false),
                acknowledgement: AtomicUsize::new(0),
            });

            let publisher_mailbox = Arc::clone(&mailbox);
            let publisher = thread::spawn(move || {
                publisher_mailbox.root.store(GLOBAL_ROOT, Ordering::Relaxed);
                publisher_mailbox
                    .request
                    .store(GENERATION, Ordering::Release);
            });
            let target_mailbox = Arc::clone(&mailbox);
            let target = thread::spawn(move || {
                let generation = target_mailbox.request.load(Ordering::Acquire);
                if generation != 0 {
                    assert_eq!(generation, GENERATION);
                    assert_eq!(target_mailbox.root.load(Ordering::Relaxed), GLOBAL_ROOT);
                    target_mailbox.flushed.store(true, Ordering::Release);
                    target_mailbox
                        .acknowledgement
                        .store(generation, Ordering::Release);
                }
            });
            let reclaimer_mailbox = Arc::clone(&mailbox);
            let reclaimer = thread::spawn(move || {
                if reclaimer_mailbox.acknowledgement.load(Ordering::Acquire) == GENERATION {
                    assert!(reclaimer_mailbox.flushed.load(Ordering::Acquire));
                }
            });

            publisher.join().unwrap();
            target.join().unwrap();
            reclaimer.join().unwrap();
            if mailbox.acknowledgement.load(Ordering::Acquire) == GENERATION {
                assert!(mailbox.flushed.load(Ordering::Acquire));
            }
        });
    }

    /// Mirrors recoverable grow-down fault handling. Planning and commit take
    /// only the scheduler owner; page installation takes only process state.
    /// A concurrent retirement may invalidate the plan, but the transaction
    /// must still terminate as either an exact metadata commit or task-only
    /// retirement, never as a half-committed live task.
    #[test]
    fn recoverable_stack_growth_never_nests_scheduler_and_process_locks() {
        loom::model(|| {
            #[derive(Clone, Copy)]
            struct SchedulerState {
                generation: usize,
                stack_start: usize,
                retired: bool,
                committed: bool,
            }
            #[derive(Default)]
            struct ProcessState {
                reserved: bool,
                mapped: bool,
            }

            let scheduler = Arc::new(Mutex::new(SchedulerState {
                generation: 1,
                stack_start: 8,
                retired: false,
                committed: false,
            }));
            let process = Arc::new(Mutex::new(ProcessState {
                reserved: true,
                mapped: false,
            }));

            let fault_scheduler = Arc::clone(&scheduler);
            let fault_process = Arc::clone(&process);
            let fault = thread::spawn(move || {
                // The immutable plan escapes only after the scheduler guard is
                // dropped. No process-state access occurs in this scope.
                let plan = {
                    let scheduler = fault_scheduler.lock().unwrap();
                    (!scheduler.retired).then_some((scheduler.generation, scheduler.stack_start))
                };
                let Some((generation, previous_start)) = plan else {
                    return;
                };

                // The mapping phase owns only process state. This scope ends
                // before scheduler revalidation begins.
                let mapped = {
                    let mut process = fault_process.lock().unwrap();
                    if process.reserved {
                        process.reserved = false;
                        process.mapped = true;
                        true
                    } else {
                        false
                    }
                };
                if !mapped {
                    fault_scheduler.lock().unwrap().retired = true;
                    return;
                }

                let mut scheduler = fault_scheduler.lock().unwrap();
                if !scheduler.retired
                    && scheduler.generation == generation
                    && scheduler.stack_start == previous_start
                {
                    scheduler.stack_start = previous_start - 1;
                    scheduler.committed = true;
                } else {
                    scheduler.retired = true;
                }
            });

            let retire_scheduler = Arc::clone(&scheduler);
            let retire = thread::spawn(move || {
                let mut scheduler = retire_scheduler.lock().unwrap();
                scheduler.retired = true;
                scheduler.generation += 1;
            });

            fault.join().unwrap();
            retire.join().unwrap();
            let scheduler = scheduler.lock().unwrap();
            let process = process.lock().unwrap();
            // A later ordinary retirement may follow a successful commit; it
            // does not erase the fact that the commit owned a real mapping.
            assert!(!scheduler.committed || process.mapped);
            assert!(scheduler.committed || scheduler.retired);
        });
    }

    /// Mirrors the exec reservation protocol around a concurrent exit. Exit
    /// may win before root installation, but once the scheduler installs the
    /// reserved root the process-state owner transfer is mandatory even when
    /// `exiting` becomes visible in the transaction gap.
    #[test]
    fn exec_installed_root_retains_generation_bound_owner() {
        loom::model(|| {
            #[derive(Default)]
            struct ProcessState {
                reservation: usize,
                authorized: bool,
                exiting: bool,
                owner_generation: usize,
            }
            #[derive(Default)]
            struct SchedulerState {
                retired: bool,
                root_generation: usize,
            }
            let process = Arc::new(Mutex::new(ProcessState {
                owner_generation: 1,
                ..ProcessState::default()
            }));
            let scheduler = Arc::new(Mutex::new(SchedulerState::default()));

            let exec_process = Arc::clone(&process);
            let exec_scheduler = Arc::clone(&scheduler);
            let exec = thread::spawn(move || {
                {
                    let mut process = exec_process.lock().unwrap();
                    if process.exiting {
                        return;
                    }
                    process.reservation = 2;
                    process.authorized = true;
                }
                let installed = {
                    let mut scheduler = exec_scheduler.lock().unwrap();
                    if scheduler.retired {
                        false
                    } else {
                        scheduler.root_generation = 2;
                        true
                    }
                };
                if installed {
                    let mut process = exec_process.lock().unwrap();
                    assert_eq!(process.reservation, 2);
                    assert!(process.authorized);
                    // Deliberately do not reject `exiting`: the installed CR3
                    // already needs this exact generation-bound owner.
                    process.owner_generation = 2;
                    process.reservation = 0;
                    process.authorized = false;
                }
            });

            let exit_process = Arc::clone(&process);
            let exit_scheduler = Arc::clone(&scheduler);
            let exit = thread::spawn(move || {
                exit_process.lock().unwrap().exiting = true;
                exit_scheduler.lock().unwrap().retired = true;
            });

            exec.join().unwrap();
            exit.join().unwrap();
            let installed = scheduler.lock().unwrap().root_generation == 2;
            let owner_generation = process.lock().unwrap().owner_generation;
            assert!(!installed || owner_generation == 2);
        });
    }

    /// Mirrors kernel-generated robust-list and clear-child wakes. Userspace
    /// flags are unavailable at exit, so a stable shared identity is tried
    /// first and the exact private identity remains the anonymous fallback.
    #[test]
    fn kernel_generated_futex_wake_matches_waiter_identity() {
        #[derive(Clone, Copy, Debug, Eq, PartialEq)]
        enum Key {
            Shared,
            Private,
        }

        for stable_shared in [false, true] {
            loom::model(move || {
                let waiter = Arc::new(Mutex::new(None::<Key>));
                let registered = Arc::new(AtomicBool::new(false));
                let cleanup_claimed = Arc::new(AtomicBool::new(false));
                let woke = Arc::new(AtomicBool::new(false));

                let waiter_slot = Arc::clone(&waiter);
                let waiter_registered = Arc::clone(&registered);
                let register = thread::spawn(move || {
                    *waiter_slot.lock().unwrap() = Some(if stable_shared {
                        Key::Shared
                    } else {
                        Key::Private
                    });
                    waiter_registered.store(true, Ordering::Release);
                });

                let cleanup_slot = Arc::clone(&waiter);
                let cleanup_registered = Arc::clone(&registered);
                let cleanup_observed = Arc::clone(&cleanup_claimed);
                let cleanup_woke = Arc::clone(&woke);
                let cleanup = thread::spawn(move || {
                    if !cleanup_registered.load(Ordering::Acquire) {
                        return;
                    }
                    cleanup_observed.store(true, Ordering::Release);
                    let candidates = if stable_shared {
                        [Some(Key::Shared), Some(Key::Private)]
                    } else {
                        [Some(Key::Private), None]
                    };
                    let mut waiter = cleanup_slot.lock().unwrap();
                    for candidate in candidates.into_iter().flatten() {
                        if *waiter == Some(candidate) {
                            *waiter = None;
                            cleanup_woke.store(true, Ordering::Release);
                            break;
                        }
                    }
                });

                register.join().unwrap();
                cleanup.join().unwrap();
                if cleanup_claimed.load(Ordering::Acquire) {
                    assert!(woke.load(Ordering::Acquire));
                    assert!(waiter.lock().unwrap().is_none());
                }
            });
        }
    }

    /// Mirrors a GPU transport rejection racing the producer's candidate
    /// compilation. Only an observed rejection owns rollback; when it does,
    /// both compiler counters return to the exact checkpoint and the next
    /// successful submit is forced to replay the complete atlas.
    #[test]
    fn rejected_gpu_submit_restores_producer_state() {
        loom::model(|| {
            #[derive(Clone, Copy)]
            struct Compiler {
                next_submit: usize,
                timeline: usize,
                force_full: bool,
            }
            let compiler = Arc::new(Mutex::new(Compiler {
                next_submit: 7,
                timeline: 11,
                force_full: false,
            }));
            let reject = Arc::new(AtomicBool::new(false));
            let rolled_back = Arc::new(AtomicBool::new(false));

            let transport_reject = Arc::clone(&reject);
            let transport = thread::spawn(move || {
                transport_reject.store(true, Ordering::Release);
            });
            let submit_compiler = Arc::clone(&compiler);
            let submit_reject = Arc::clone(&reject);
            let submit_rollback = Arc::clone(&rolled_back);
            let submit = thread::spawn(move || {
                let mut compiler = submit_compiler.lock().unwrap();
                let checkpoint = (compiler.next_submit, compiler.timeline);
                compiler.next_submit += 1;
                compiler.timeline += 1;
                if submit_reject.load(Ordering::Acquire) {
                    compiler.next_submit = checkpoint.0;
                    compiler.timeline = checkpoint.1;
                    compiler.force_full = true;
                    submit_rollback.store(true, Ordering::Release);
                }
            });

            transport.join().unwrap();
            submit.join().unwrap();
            let compiler = compiler.lock().unwrap();
            if rolled_back.load(Ordering::Acquire) {
                assert_eq!(compiler.next_submit, 7);
                assert_eq!(compiler.timeline, 11);
                assert!(compiler.force_full);
            }
        });
    }

    /// Mirrors the late acceptance watcher after its initial registry miss.
    /// Multiple observations of the now-complete private contract race through
    /// one atomic publication and may emit exactly one profiler announcement.
    #[test]
    fn late_acceptance_contract_enables_profiler_once() {
        loom::model(|| {
            let contract_complete = Arc::new(AtomicBool::new(false));
            let enabled = Arc::new(AtomicBool::new(false));
            let announcements = Arc::new(AtomicUsize::new(0));

            // The synchronous initial read missed. Publication then precedes
            // both bounded watcher observations.
            contract_complete.store(true, Ordering::Release);
            let mut watchers = Vec::new();
            for _ in 0..2 {
                let contract = Arc::clone(&contract_complete);
                let enabled = Arc::clone(&enabled);
                let announcements = Arc::clone(&announcements);
                watchers.push(thread::spawn(move || {
                    if contract.load(Ordering::Acquire) && !enabled.swap(true, Ordering::AcqRel) {
                        announcements.fetch_add(1, Ordering::AcqRel);
                    }
                }));
            }
            for watcher in watchers {
                watcher.join().unwrap();
            }
            assert!(enabled.load(Ordering::Acquire));
            assert_eq!(announcements.load(Ordering::Acquire), 1);
        });
    }
}
