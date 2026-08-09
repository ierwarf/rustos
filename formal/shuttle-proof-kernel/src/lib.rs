//! Bounded schedule exploration for source-anchored RustOS concurrency flows.
//!
//! These are intentionally protocol models, not a substitute for compiling the
//! ring0 target under Shuttle.  `concurrency-triangle.toml` binds every model
//! to an exact source symbol, ordering anchors, a Loom kernel, and, where the
//! source uses lock-free publication, an x86_64 herd7 litmus.

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;

    use shuttle::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use shuttle::sync::{Arc, Mutex};
    use shuttle::thread;

    const DEFAULT_ITERATIONS: usize = 128;
    const DEFAULT_PCT_DEPTH: usize = 3;

    fn bounded_env(name: &str, default: usize, minimum: usize, maximum: usize) -> usize {
        let value = std::env::var(name)
            .ok()
            .and_then(|value| value.parse::<usize>().ok())
            .unwrap_or(default);
        assert!(
            (minimum..=maximum).contains(&value),
            "{name} must be in {minimum}..={maximum}"
        );
        value
    }

    fn iterations() -> usize {
        bounded_env("SHUTTLE_ITERATIONS", DEFAULT_ITERATIONS, 16, 2_048)
    }

    fn pct_depth() -> usize {
        bounded_env("SHUTTLE_PCT_DEPTH", DEFAULT_PCT_DEPTH, 1, 4)
    }

    /// Models the endpoint registry's publication/revoke race.  The terminal
    /// state is intentionally checked only after all controlled tasks join;
    /// this avoids inventing a liveness failure from a schedule that simply
    /// has not run a legitimate waiter yet.
    #[test]
    fn endpoint_exit_and_publication_have_one_terminal_owner() {
        shuttle::check_pct(
            || {
                #[derive(Default)]
                struct Registry {
                    exiting: bool,
                    endpoint: Option<u64>,
                }
                let registry = Arc::new(Mutex::new(Registry::default()));
                let publisher = Arc::clone(&registry);
                let publish = thread::spawn(move || {
                    let mut state = publisher.lock().unwrap();
                    if !state.exiting {
                        state.endpoint = Some(7);
                    }
                });
                let revoker = Arc::clone(&registry);
                let revoke = thread::spawn(move || {
                    let mut state = revoker.lock().unwrap();
                    state.exiting = true;
                    state.endpoint = None;
                });
                publish.join().unwrap();
                revoke.join().unwrap();
                let state = registry.lock().unwrap();
                assert!(!state.exiting || state.endpoint.is_none());
            },
            iterations(),
            pct_depth(),
        );
    }

    /// Models reply, deadline, and endpoint-revoke contenders.  A terminal
    /// owner is an exact state transition, never a best-effort winner count.
    #[test]
    fn ipc_reply_timeout_and_revoke_have_one_terminal_owner() {
        shuttle::check_pct(
            || {
                #[derive(Clone, Copy, Debug, Eq, PartialEq)]
                enum Terminal {
                    Reply,
                    Timeout,
                    Revoked,
                }
                let terminal = Arc::new(Mutex::new(None::<Terminal>));
                let mut contenders = Vec::new();
                for outcome in [Terminal::Reply, Terminal::Timeout, Terminal::Revoked] {
                    let terminal = Arc::clone(&terminal);
                    contenders.push(thread::spawn(move || {
                        let mut state = terminal.lock().unwrap();
                        if state.is_none() {
                            *state = Some(outcome);
                        }
                    }));
                }
                for contender in contenders {
                    contender.join().unwrap();
                }
                assert!(terminal.lock().unwrap().is_some());
            },
            iterations(),
            pct_depth(),
        );
    }

    /// Models 0->1 IPI coalescing against the target safe-point consume.  The
    /// valid terminal alternatives are deliberately explicit: pending or
    /// consumed; no third "lost" alternative is permitted.
    #[test]
    fn reschedule_request_is_never_lost_across_target_consume() {
        shuttle::check_pct(
            || {
                #[derive(Default)]
                struct Request {
                    pending: bool,
                    consumed: bool,
                }
                let request = Arc::new(Mutex::new(Request::default()));
                let publisher = Arc::clone(&request);
                let publish = thread::spawn(move || {
                    let mut state = publisher.lock().unwrap();
                    state.pending = true;
                });
                let target = Arc::clone(&request);
                let consume = thread::spawn(move || {
                    let mut state = target.lock().unwrap();
                    if state.pending {
                        state.pending = false;
                        state.consumed = true;
                    }
                });
                publish.join().unwrap();
                consume.join().unwrap();
                let state = request.lock().unwrap();
                assert!(state.pending || state.consumed);
            },
            iterations(),
            pct_depth(),
        );
    }

    /// Models the mailbox ordering and exact-generation acknowledgement around
    /// a shootdown.  Reclaim is conditional because a valid schedule may run
    /// the target before publication; that is an incomplete protocol attempt,
    /// not a false liveness alarm.
    #[test]
    fn tlb_mailbox_flush_ack_and_reclaim_preserve_generation_order() {
        shuttle::check_pct(
            || {
                const GENERATION: u64 = 7;
                const GLOBAL_ROOT: u64 = 0;
                #[derive(Default)]
                struct Shootdown {
                    root: u64,
                    request: u64,
                    flushed: bool,
                    acknowledgement: u64,
                    reclaimed: bool,
                }
                let state = Arc::new(Mutex::new(Shootdown {
                    root: u64::MAX,
                    ..Shootdown::default()
                }));
                let publisher = Arc::clone(&state);
                let publish = thread::spawn(move || {
                    let mut state = publisher.lock().unwrap();
                    state.root = GLOBAL_ROOT;
                    state.request = GENERATION;
                });
                let target = Arc::clone(&state);
                let flush_and_ack = thread::spawn(move || {
                    let mut state = target.lock().unwrap();
                    if state.request == GENERATION {
                        assert_eq!(state.root, GLOBAL_ROOT);
                        state.flushed = true;
                        state.acknowledgement = GENERATION;
                    }
                });
                let reclaimer = Arc::clone(&state);
                let reclaim = thread::spawn(move || {
                    let mut state = reclaimer.lock().unwrap();
                    if state.acknowledgement == GENERATION {
                        assert!(state.flushed);
                        state.reclaimed = true;
                    }
                });
                publish.join().unwrap();
                flush_and_ack.join().unwrap();
                reclaim.join().unwrap();
                let state = state.lock().unwrap();
                assert!(!state.reclaimed || (state.flushed && state.acknowledgement == GENERATION));
            },
            iterations(),
            pct_depth(),
        );
    }

    /// Models check-register-recheck with a real third controlled task.  A
    /// signal that crosses the arm window must either be observed by recheck or
    /// leave a wake token for the armed waiter.
    #[test]
    fn waitset_generation_crossing_arm_is_not_lost() {
        shuttle::check_pct(
            || {
                #[derive(Default)]
                struct WaitSet {
                    generation: u64,
                    observed: u64,
                    armed: bool,
                    slept: bool,
                    woke: bool,
                }
                let waitset = Arc::new(Mutex::new(WaitSet::default()));
                let waiter = Arc::clone(&waitset);
                let arm = thread::spawn(move || {
                    let mut state = waiter.lock().unwrap();
                    state.observed = state.generation;
                    state.armed = true;
                    if state.generation == state.observed && state.armed {
                        state.slept = true;
                    }
                });
                let signaler = Arc::clone(&waitset);
                let signal = thread::spawn(move || {
                    let mut state = signaler.lock().unwrap();
                    state.generation += 1;
                    if state.armed {
                        state.woke = true;
                    }
                });
                arm.join().unwrap();
                signal.join().unwrap();
                let state = waitset.lock().unwrap();
                assert!(!(state.slept && state.generation > state.observed && !state.woke));
            },
            iterations(),
            pct_depth(),
        );
    }

    #[test]
    fn ipc_priority_lane_remains_fifo_and_burst_bounded() {
        shuttle::check_pct(
            || {
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
            },
            iterations(),
            pct_depth(),
        );
    }

    #[test]
    fn futex_store_wake_cannot_pass_compare_and_queue_publication() {
        shuttle::check_pct(
            || {
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
            },
            iterations(),
            pct_depth(),
        );
    }

    #[test]
    fn scheduler_switch_never_exposes_incoming_without_outgoing_owner() {
        shuttle::check_pct(
            || {
                const OUTGOING: usize = 17;
                const INCOMING: usize = 29;

                let current = Arc::new(AtomicUsize::new(OUTGOING));
                let transition = Arc::new(AtomicUsize::new(0));
                let transition_active = Arc::new(AtomicBool::new(false));

                let writer_current = Arc::clone(&current);
                let writer_transition = Arc::clone(&transition);
                let writer_active = Arc::clone(&transition_active);
                let writer = thread::spawn(move || {
                    writer_transition.store(OUTGOING, Ordering::Release);
                    writer_active.store(true, Ordering::Release);
                    writer_current.store(INCOMING, Ordering::Release);
                    writer_active.store(false, Ordering::Release);
                });

                let reader_current = Arc::clone(&current);
                let reader_transition = Arc::clone(&transition);
                let reader_active = Arc::clone(&transition_active);
                let reader = thread::spawn(move || {
                    let observed_current = reader_current.load(Ordering::Acquire);
                    let observed_active = reader_active.load(Ordering::Acquire);
                    let observed_transition = reader_transition.load(Ordering::Acquire);
                    if observed_current == INCOMING || observed_active {
                        assert_eq!(observed_transition, OUTGOING);
                    }
                });

                writer.join().unwrap();
                reader.join().unwrap();
                assert_eq!(current.load(Ordering::Acquire), INCOMING);
                assert!(!transition_active.load(Ordering::Acquire));
                assert_eq!(transition.load(Ordering::Acquire), OUTGOING);
            },
            iterations(),
            pct_depth(),
        );
    }

    #[test]
    fn scheduler_transition_wake_has_one_mailbox_owner_until_commit() {
        shuttle::check_pct(
            || {
                const BLOCKED: usize = 0;
                const REMOTE_QUEUED: usize = 1;
                const LOCAL: usize = 2;
                const RUNNING: usize = 3;

                struct WakeState {
                    transition_active: AtomicBool,
                    owner: AtomicUsize,
                    mailbox: Mutex<bool>,
                    notified: AtomicBool,
                    claims: AtomicUsize,
                    claim_before_commit: AtomicBool,
                    wake_published: AtomicBool,
                    target_claim_attempted: AtomicBool,
                }

                let state = Arc::new(WakeState {
                    transition_active: AtomicBool::new(true),
                    owner: AtomicUsize::new(BLOCKED),
                    mailbox: Mutex::new(false),
                    notified: AtomicBool::new(false),
                    claims: AtomicUsize::new(0),
                    claim_before_commit: AtomicBool::new(false),
                    wake_published: AtomicBool::new(false),
                    target_claim_attempted: AtomicBool::new(false),
                });

                let wake_state = Arc::clone(&state);
                let wake = thread::spawn(move || {
                    assert!(
                        wake_state
                            .owner
                            .compare_exchange(
                                BLOCKED,
                                REMOTE_QUEUED,
                                Ordering::AcqRel,
                                Ordering::Acquire,
                            )
                            .is_ok()
                    );
                    *wake_state.mailbox.lock().unwrap() = true;
                    wake_state.notified.store(true, Ordering::Release);
                    wake_state.wake_published.store(true, Ordering::Release);
                });

                let target_state = Arc::clone(&state);
                let target = thread::spawn(move || {
                    while !target_state.wake_published.load(Ordering::Acquire) {
                        thread::yield_now();
                    }
                    {
                        let mut mailbox = target_state.mailbox.lock().unwrap();
                        if *mailbox {
                            target_state.notified.store(false, Ordering::Release);
                            *mailbox = false;
                            assert!(
                                target_state
                                    .owner
                                    .compare_exchange(
                                        REMOTE_QUEUED,
                                        LOCAL,
                                        Ordering::AcqRel,
                                        Ordering::Acquire,
                                    )
                                    .is_ok()
                            );
                        }
                    }
                    // Preserve the observed transition state at the owner-CAS
                    // claim boundary. A guard-removal mutant must leave this
                    // latch set; post-join recovery may complete lost work but
                    // must never hide an early claim.
                    let transition_active_at_claim =
                        target_state.transition_active.load(Ordering::Acquire);
                    if !transition_active_at_claim
                        && target_state
                            .owner
                            .compare_exchange(LOCAL, RUNNING, Ordering::AcqRel, Ordering::Acquire)
                            .is_ok()
                    {
                        if transition_active_at_claim {
                            target_state
                                .claim_before_commit
                                .store(true, Ordering::Release);
                        }
                        target_state.claims.fetch_add(1, Ordering::AcqRel);
                    }
                    target_state
                        .target_claim_attempted
                        .store(true, Ordering::Release);
                });

                let commit_state = Arc::clone(&state);
                let commit = thread::spawn(move || {
                    while !commit_state.target_claim_attempted.load(Ordering::Acquire) {
                        thread::yield_now();
                    }
                    commit_state
                        .transition_active
                        .store(false, Ordering::Release);
                });

                wake.join().unwrap();
                target.join().unwrap();
                commit.join().unwrap();

                assert!(
                    !state.claim_before_commit.load(Ordering::Acquire),
                    "a target claim before assembly commit must not be masked by recovery"
                );

                {
                    let mut mailbox = state.mailbox.lock().unwrap();
                    if *mailbox {
                        *mailbox = false;
                        assert!(
                            state
                                .owner
                                .compare_exchange(
                                    REMOTE_QUEUED,
                                    LOCAL,
                                    Ordering::AcqRel,
                                    Ordering::Acquire,
                                )
                                .is_ok()
                        );
                    }
                    state.notified.store(false, Ordering::Release);
                }
                let transition_active_at_claim = state.transition_active.load(Ordering::Acquire);
                if !transition_active_at_claim
                    && state
                        .owner
                        .compare_exchange(LOCAL, RUNNING, Ordering::AcqRel, Ordering::Acquire)
                        .is_ok()
                {
                    if transition_active_at_claim {
                        state.claim_before_commit.store(true, Ordering::Release);
                    }
                    state.claims.fetch_add(1, Ordering::AcqRel);
                }

                assert!(!state.transition_active.load(Ordering::Acquire));
                assert!(
                    !state.claim_before_commit.load(Ordering::Acquire),
                    "every successful claim must follow assembly commit"
                );
                assert!(!*state.mailbox.lock().unwrap());
                assert_eq!(state.owner.load(Ordering::Acquire), RUNNING);
                assert_eq!(state.claims.load(Ordering::Acquire), 1);
            },
            iterations(),
            pct_depth(),
        );
    }

    /// Mirrors the one-shot ProcBroker authority shared by SMP qualification
    /// bind, individual activation, and batch activation. The reserved flag
    /// is kernel-derived: a required child cannot consume its authority until
    /// bind has installed `BoundSuspended` and activation has advanced it to
    /// `Active`; the ordinary child deliberately remains independent of a
    /// live SESSIOND endpoint.
    #[test]
    fn smp_qualification_required_deferred_bind_serializes_activation() {
        shuttle::check_pct(
            || {
                #[derive(Clone, Copy, Debug, Eq, PartialEq)]
                enum BindingState {
                    Absent,
                    BoundSuspended,
                    Active,
                }

                #[derive(Clone, Copy, Debug)]
                struct DeferredChild {
                    authority_live: bool,
                    qualification_required: bool,
                    runnable: bool,
                }

                struct State {
                    sessiond_live: bool,
                    required: DeferredChild,
                    ordinary: DeferredChild,
                    binding: BindingState,
                    active_without_bound_suspended: bool,
                    bind_installed_after_consume: bool,
                    batch_consumed_required: bool,
                }

                let state = Arc::new(Mutex::new(State {
                    sessiond_live: true,
                    required: DeferredChild {
                        authority_live: true,
                        qualification_required: true,
                        runnable: false,
                    },
                    ordinary: DeferredChild {
                        authority_live: true,
                        qualification_required: false,
                        runnable: false,
                    },
                    binding: BindingState::Absent,
                    active_without_bound_suspended: false,
                    bind_installed_after_consume: false,
                    batch_consumed_required: false,
                }));

                // This is the qualification bind callback, invoked while the
                // ProcBroker deferred-authority guard is still retained.
                let bind_state = Arc::clone(&state);
                let bind = thread::spawn(move || {
                    thread::yield_now();
                    let mut state = bind_state.lock().unwrap();
                    if !state.required.authority_live {
                        // A late bind observes one-shot consumption and cannot
                        // install a new binding for a now-runnable child.
                        return;
                    }
                    if state.required.qualification_required
                        && state.sessiond_live
                        && state.binding == BindingState::Absent
                    {
                        // Deliberate preemption while the broker transaction
                        // is retained: activation can only run before or after
                        // this install, never through it.
                        thread::yield_now();
                        if !state.required.authority_live {
                            state.bind_installed_after_consume = true;
                        } else {
                            state.binding = BindingState::BoundSuspended;
                        }
                    }
                });

                // Individual activate must make the final kernel-derived
                // required check while it owns the same ProcBroker transaction.
                let individual_state = Arc::clone(&state);
                let individual = thread::spawn(move || {
                    thread::yield_now();
                    let mut state = individual_state.lock().unwrap();
                    if !state.required.authority_live {
                        return;
                    }
                    if state.required.qualification_required
                        && state.binding != BindingState::BoundSuspended
                    {
                        return;
                    }
                    if state.required.qualification_required {
                        if state.binding != BindingState::BoundSuspended {
                            // Retained as a mutation witness: deleting the
                            // guard above would otherwise turn Absent directly
                            // Active.
                            state.active_without_bound_suspended = true;
                        }
                        state.binding = BindingState::Active;
                    }
                    state.required.authority_live = false;
                    state.required.runnable = true;
                });

                // Batch activation cannot acquire the reserved child even if
                // it wins the race to the registry. Its ordinary counterpart
                // is exercised after the concurrent transaction below.
                let batch_state = Arc::clone(&state);
                let batch = thread::spawn(move || {
                    thread::yield_now();
                    let mut state = batch_state.lock().unwrap();
                    if !state.required.authority_live {
                        return;
                    }
                    if state.required.qualification_required {
                        return;
                    }
                    state.batch_consumed_required = true;
                    state.required.authority_live = false;
                    state.required.runnable = true;
                });

                bind.join().unwrap();
                individual.join().unwrap();
                batch.join().unwrap();

                let mut state = state.lock().unwrap();
                assert!(
                    !state.active_without_bound_suspended,
                    "a required child must not advance Absent directly to Active"
                );
                assert!(
                    !state.bind_installed_after_consume,
                    "a consumed deferred authority must reject a late qualification bind"
                );
                assert!(
                    !state.batch_consumed_required,
                    "batch activation must reject the kernel-derived qualification-required child"
                );
                if state.required.runnable {
                    assert_eq!(state.binding, BindingState::Active);
                }

                // SESSIOND is intentionally absent here. An ordinary deferred
                // authority stays on the normal activate path and must not
                // inherit a dependency on the private qualification binding.
                state.sessiond_live = false;
                assert!(!state.sessiond_live);
                assert!(!state.ordinary.qualification_required);
                assert!(state.ordinary.authority_live);
                state.ordinary.authority_live = false;
                state.ordinary.runnable = true;
                assert!(state.ordinary.runnable);
            },
            iterations(),
            pct_depth(),
        );
    }

    /// Models the post-admission SESSIOND owner+epoch revalidation. The phase
    /// reaches the binding FSM under its own lock, then the old SESSIOND owner
    /// is revoked and the PID is re-registered at a new generation and
    /// endpoint epoch before evidence emission. The stale phase must
    /// terminalize and emit no evidence.
    #[test]
    fn smp_qualification_phase_revalidation_terminalizes_on_sessiond_epoch_change() {
        shuttle::check_pct(
            || {
                #[derive(Clone, Copy, Debug, Eq, PartialEq)]
                struct EndpointIdentity {
                    process_id: usize,
                    process_generation: usize,
                    epoch: usize,
                    live: bool,
                }

                #[derive(Clone, Copy, Debug, Eq, PartialEq)]
                enum BindingState {
                    Active,
                    Terminal,
                }

                struct Binding {
                    expected_endpoint: EndpointIdentity,
                    state: BindingState,
                    phase_admitted: bool,
                    post_revalidation_mismatch: bool,
                    evidence_published: bool,
                }

                const SESSIOND_PID: usize = 17;
                let expected = EndpointIdentity {
                    process_id: SESSIOND_PID,
                    process_generation: 3,
                    epoch: 7,
                    live: true,
                };
                let endpoint = Arc::new(Mutex::new(expected));
                let binding = Arc::new(Mutex::new(Binding {
                    expected_endpoint: expected,
                    state: BindingState::Active,
                    phase_admitted: false,
                    post_revalidation_mismatch: false,
                    evidence_published: false,
                }));
                let phase_admitted = Arc::new(AtomicBool::new(false));
                let replacement_complete = Arc::new(AtomicBool::new(false));

                let admission_endpoint = Arc::clone(&endpoint);
                let admission_binding = Arc::clone(&binding);
                let admission_phase_admitted = Arc::clone(&phase_admitted);
                let admission_replacement_complete = Arc::clone(&replacement_complete);
                let admit = thread::spawn(move || {
                    let pre_admission = *admission_endpoint.lock().unwrap();
                    {
                        let mut binding = admission_binding.lock().unwrap();
                        if binding.state != BindingState::Active
                            || pre_admission != binding.expected_endpoint
                        {
                            return;
                        }
                        binding.phase_admitted = true;
                    }
                    admission_phase_admitted.store(true, Ordering::Release);
                    // Production releases the binding lock before the second
                    // endpoint snapshot and debug output; make that exact
                    // window an explicit controlled interleaving point.
                    thread::yield_now();
                    while !admission_replacement_complete.load(Ordering::Acquire) {
                        thread::yield_now();
                    }

                    let post_admission = *admission_endpoint.lock().unwrap();
                    let mut binding = admission_binding.lock().unwrap();
                    if post_admission != binding.expected_endpoint {
                        binding.post_revalidation_mismatch = true;
                        binding.state = BindingState::Terminal;
                        return;
                    }
                    binding.evidence_published = true;
                });

                let revoke_endpoint = Arc::clone(&endpoint);
                let revoke_phase_admitted = Arc::clone(&phase_admitted);
                let revoke_replacement_complete = Arc::clone(&replacement_complete);
                let revoke_and_reregister = thread::spawn(move || {
                    while !revoke_phase_admitted.load(Ordering::Acquire) {
                        thread::yield_now();
                    }
                    let mut endpoint = revoke_endpoint.lock().unwrap();
                    // PID reuse alone is deliberately insufficient: both the
                    // process generation and endpoint epoch differ from the
                    // compound admission identity.
                    *endpoint = EndpointIdentity {
                        process_id: SESSIOND_PID,
                        process_generation: 4,
                        epoch: 8,
                        live: true,
                    };
                    drop(endpoint);
                    revoke_replacement_complete.store(true, Ordering::Release);
                    thread::yield_now();
                });

                admit.join().unwrap();
                revoke_and_reregister.join().unwrap();

                let binding = binding.lock().unwrap();
                assert!(binding.phase_admitted);
                assert!(binding.post_revalidation_mismatch);
                assert_eq!(binding.state, BindingState::Terminal);
                assert!(
                    !binding.evidence_published,
                    "a phase whose post-admission SESSIOND owner+epoch changed must not emit evidence"
                );
            },
            iterations(),
            pct_depth(),
        );
    }
}
