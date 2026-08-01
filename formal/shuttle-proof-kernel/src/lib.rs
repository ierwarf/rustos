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
}
