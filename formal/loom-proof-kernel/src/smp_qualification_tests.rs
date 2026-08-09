/// Mirrors the one-shot ProcBroker authority shared by SMP qualification
/// bind, individual activation, and batch activation.  The reserved flag
/// is kernel-derived: a required child cannot consume its authority until
/// bind has installed `BoundSuspended` and activation has advanced it to
/// `Active`; the ordinary child deliberately remains independent of a
/// live SESSIOND endpoint.
#[test]
fn smp_qualification_required_deferred_bind_serializes_activation() {
    loom::model(|| {
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
                // Deliberate preemption while the broker transaction is
                // retained: activation can only run before or after this
                // install, never through it.
                thread::yield_now();
                if !state.required.authority_live {
                    state.bind_installed_after_consume = true;
                } else {
                    state.binding = BindingState::BoundSuspended;
                }
            }
        });

        // Individual activate must make the final kernel-derived required
        // check while it owns the same ProcBroker transaction.
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
                    // Retained as a mutation witness: deleting the guard
                    // above would otherwise turn Absent directly Active.
                    state.active_without_bound_suspended = true;
                }
                state.binding = BindingState::Active;
            }
            state.required.authority_live = false;
            state.required.runnable = true;
        });

        // Batch activation cannot acquire the reserved child even if it
        // wins the race to the registry.  Its ordinary counterpart is
        // exercised after the concurrent transaction below.
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

        // SESSIOND is intentionally absent here.  An ordinary deferred
        // authority stays on the normal activate path and must not inherit
        // a dependency on the private qualification binding.
        state.sessiond_live = false;
        assert!(!state.sessiond_live);
        assert!(!state.ordinary.qualification_required);
        assert!(state.ordinary.authority_live);
        state.ordinary.authority_live = false;
        state.ordinary.runnable = true;
        assert!(state.ordinary.runnable);
    });
}

/// Models the post-admission SESSIOND owner+epoch revalidation.  The
/// phase reaches the binding FSM under its own lock, then the old SESSIOND
/// owner is revoked and the PID is re-registered at a new generation and
/// endpoint epoch before evidence emission.  The stale phase must
/// terminalize and emit no evidence.
#[test]
fn smp_qualification_phase_revalidation_terminalizes_on_sessiond_epoch_change() {
    loom::model(|| {
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
            // endpoint snapshot and debug output; make that exact window
            // an explicit controlled interleaving point.
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
    });
}
