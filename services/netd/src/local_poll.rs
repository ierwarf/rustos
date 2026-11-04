//! Deferred AF_UNIX poll waiters and the pending-slot reservation counters shared with the worker pools.
//!
//! - **Owner:** `netd` owns the waiter queue and its global reservation counter.
//! - **Boundary:** Poll requests and their reply capabilities are untrusted until evaluated.
//! - **Lifecycle:** Reserve, defer with reply authority, service in serialized passes, reply once on ready/expiry/poison, release the reservation.
//! - **Concurrency:** Queue mutex plus a run/request flag pair that serializes passes so a commit during a pass is never missed.
//! - **Failure:** Queue poison drains into bounded `EIO` replies; expired waiters reply `ETIMEDOUT`.
//! - **Forbidden:** No waiter without a reservation, no second reply, no pass that can strand a requested rerun.
//! - **Evidence:** `local_socket_poll_tests`, `hardening_tests`, `netd-deferred-reply-lifecycle`.

use super::*;

/// Milliseconds until the soonest deferred local poll must be answered, or
/// `None` when nothing is deferred.
///
/// # Why the service loop cannot block forever while this is `Some`
/// A deferred local poll is answered only by `service_local_poll_waiters`, and
/// that runs only around a received request. With the endpoint idle, a waiter
/// whose deadline has passed was therefore neither replied to nor timed out -
/// it simply waited for unrelated traffic to arrive and remind netd it existed.
/// AF_UNIX is how Wayland clients reach uiserver, so "unrelated traffic" is not
/// a latency bound anyone should depend on. While any poll is deferred the loop
/// waits with this budget instead.
pub(super) fn next_local_poll_budget_ms() -> Option<u64> {
    let waiters = local_poll_waiters().lock().ok()?;
    let earliest = waiters
        .iter()
        .map(|waiter| waiter.request.deadline_ns)
        .min()?;
    let now_ns = rustos_svc_runtime::syscall::monotonic_nanos();
    Some(local_poll_budget_ms_for_deadline(earliest, now_ns))
}

pub(super) fn local_poll_budget_ms_for_deadline(earliest_ns: u64, now_ns: u64) -> u64 {
    // Round up and floor at one: the kernel rejects a zero budget, and a
    // deadline that has already passed still needs one pass to be answered.
    earliest_ns
        .saturating_sub(now_ns)
        .div_ceil(1_000_000)
        .clamp(1, NETD_MAX_IDLE_WAIT_MS)
}

pub(super) fn local_poll_waiters() -> &'static Mutex<VecDeque<DeferredLocalPoll>> {
    static WAITERS: OnceLock<Mutex<VecDeque<DeferredLocalPoll>>> = OnceLock::new();
    WAITERS.get_or_init(|| Mutex::new(VecDeque::new()))
}

pub(super) fn defer_local_poll_request(request: NetdIpcRequest, reply_cap: u64) -> Result<(), i32> {
    defer_local_poll_request_at(
        request,
        reply_cap,
        rustos_svc_runtime::syscall::monotonic_nanos(),
    )
}

pub(super) fn defer_local_poll_request_at(
    request: NetdIpcRequest,
    reply_cap: u64,
    now_ns: u64,
) -> Result<(), i32> {
    // Keep the expiry check ahead of the reservation so an old request cannot
    // exhaust the detached local-poll bound without receiving a reply.
    request_deadline_remaining_ns_at(&request, now_ns)?;
    if !reserve_pending_slot(&PENDING_LOCAL_POLLS, MAX_PENDING_BLOCKING_REQUESTS) {
        return Err(libc::EAGAIN);
    }
    let Ok(mut waiters) = local_poll_waiters().lock() else {
        release_pending_slot(&PENDING_LOCAL_POLLS);
        return Err(libc::EAGAIN);
    };
    waiters.push_back(DeferredLocalPoll {
        request: Box::new(request),
        reply_cap,
    });
    Ok(())
}

/// Local AF_UNIX state is owned by this service loop, so readiness can be
/// completed here without creating or waking an unrelated worker task. This
/// preserves the IPC reply handoff from netd directly to the waiting client.
///
/// # Lost-wakeup protocol
/// A pass takes the whole queue, evaluates it unlocked and re-queues the
/// still-waiting entries. A state change committed by another worker while a
/// pass is in flight would then find an empty queue and its own pass would see
/// nothing to wake; the entry re-queued afterwards would sleep until unrelated
/// traffic or its deadline. Passes are therefore serialized: a caller that finds
/// a pass running records a rerun request and returns, and the running pass
/// repeats until no request is outstanding, so every commit that precedes a
/// call is observed by a pass that starts after it.
pub(super) fn service_local_poll_waiters() {
    static PASS_REQUESTED: AtomicBool = AtomicBool::new(false);
    static PASS_RUNNING: AtomicBool = AtomicBool::new(false);
    run_serialized_passes(&PASS_REQUESTED, &PASS_RUNNING, service_local_poll_waiters_pass);
}

/// Runs `pass` at least once after every call, never concurrently with itself.
pub(super) fn run_serialized_passes(requested: &AtomicBool, running: &AtomicBool, mut pass: impl FnMut()) {
    // ORDERING: the request flag publishes "state changed before this call";
    // the running pass consumes it with AcqRel, so it observes that state.
    requested.store(true, Ordering::Release);
    loop {
        // ORDERING: Acquire pairs with the Release that ends the previous pass.
        if running
            .compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
            .is_err()
        {
            return;
        }
        // ORDERING: AcqRel consumes the requester's Release and orders the pass after it.
        while requested.swap(false, Ordering::AcqRel) {
            pass();
        }
        // ORDERING: Release publishes the finished pass to the next runner.
        running.store(false, Ordering::Release);
        // A request that raced with the release above found the pass running
        // and returned; take it over rather than strand it.
        // ORDERING: Acquire pairs with the Release store of a racing requester.
        if !requested.load(Ordering::Acquire) {
            return;
        }
    }
}

pub(super) fn service_local_poll_waiters_pass() {
    let (pending, queue_poisoned) = take_deferred_queue(local_poll_waiters().lock());
    if pending.is_empty() {
        return;
    }

    let mut still_waiting = VecDeque::new();
    for waiter in pending {
        if let Some(waiter) = service_one_local_poll_waiter(
            waiter,
            queue_poisoned,
            rustos_svc_runtime::syscall::monotonic_nanos(),
            |reply_cap, response| {
                let _ = publish_reply(reply_cap, response);
            },
            || release_pending_slot(&PENDING_LOCAL_POLLS),
        ) {
            still_waiting.push_back(waiter);
        }
    }
    if !still_waiting.is_empty() {
        match local_poll_waiters().lock() {
            Ok(mut waiters) => waiters.extend(still_waiting),
            Err(_) => {
                // The queue is no longer usable. Resolve every detached
                // request and release its reservation rather than leaking
                // reply capabilities or permanently exhausting admission.
                for waiter in still_waiting {
                    let response = NetdIpcResponse {
                        version: NETD_IPC_ABI_VERSION,
                        op: waiter.request.op,
                        status: libc::EIO,
                        ..NetdIpcResponse::default()
                    };
                    let _ = publish_reply(waiter.reply_cap, &response);
                    release_pending_slot(&PENDING_LOCAL_POLLS);
                }
            }
        }
    }
}

/// Returns a waiter only when it remains live.  All terminal paths invoke the
/// reply and reservation-release callbacks together, making a detached wait
/// incapable of leaking one half of its lifecycle transaction.
pub(super) fn service_one_local_poll_waiter<F, R>(
    waiter: DeferredLocalPoll,
    queue_poisoned: bool,
    now_ns: u64,
    reply: F,
    release: R,
) -> Option<DeferredLocalPoll>
where
    F: FnOnce(u64, &NetdIpcResponse),
    R: FnOnce(),
{
    let mut response = NetdIpcResponse {
        version: NETD_IPC_ABI_VERSION,
        op: waiter.request.op,
        ..NetdIpcResponse::default()
    };
    response.status = if queue_poisoned {
        libc::EIO
    } else if let Err(errno) = request_deadline_remaining_ns_at(&waiter.request, now_ns) {
        errno
    } else {
        handle_poll_socket(&waiter.request, &mut response)
    };
    if response.status == 0 && response.value == 0 {
        return Some(waiter);
    }
    if response.status == 0 {
        response.reserved1 = NETD_IPC_RESPONSE_FLAG_LATENCY_HANDOFF;
    }
    reply(waiter.reply_cap, &response);
    release();
    None
}

pub(super) fn take_deferred_queue<T>(locked: LockResult<MutexGuard<'_, VecDeque<T>>>) -> (VecDeque<T>, bool) {
    match locked {
        Ok(mut waiters) => (std::mem::take(&mut *waiters), false),
        Err(poisoned) => {
            // Rust poisoning reports that a previous owner unwound, not that
            // VecDeque memory is invalid. Drain the structurally valid queue
            // and fail every reserved request instead of leaking the global
            // bound and reply capabilities forever.
            let mut waiters = poisoned.into_inner();
            (std::mem::take(&mut *waiters), true)
        }
    }
}

pub(super) fn reserve_pending_slot(counter: &AtomicUsize, limit: usize) -> bool {
    // ORDERING: admission produces a slot reservation; reply completion and
    // error cleanup consume it by releasing the counter, so AcqRel prevents
    // two concurrent admissions from exceeding the shared bound.
    counter
        .fetch_update(Ordering::AcqRel, Ordering::Acquire, |pending| {
            if pending >= limit {
                return None;
            }
            Some(pending + 1)
        })
        .is_ok()
}

pub(super) fn release_pending_slot(counter: &AtomicUsize) {
    // ORDERING: completion produces an available slot; a later admission
    // consumes that availability with an acquire failure-order load, so this
    // AcqRel decrement publishes the release before reuse.
    let released = counter.fetch_update(Ordering::AcqRel, Ordering::Acquire, |pending| {
        pending.checked_sub(1)
    });
    debug_assert!(released.is_ok(), "pending request counter underflow");
}
