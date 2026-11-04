//! Bounded worker pools for requests that may wait (blocking INET I/O, provider-gated socket creation).
//!
//! - **Owner:** `netd` owns both fixed pools; callers own only their absolute deadline.
//! - **Boundary:** Queued requests and their reply capabilities are untrusted client work until dispatched.
//! - **Lifecycle:** Reserve a pending slot, enqueue with the reply capability, run once or retry within the caller end, send exactly one terminal reply, release the slot.
//! - **Concurrency:** Queue mutex and atomic slot counters only; no `NetState` lock is held while sleeping between retries.
//! - **Failure:** Pool full or no worker: `EAGAIN` without reserving; expiry before provider entry replies `ETIMEDOUT`; a rejected reply is counted, never retried.
//! - **Forbidden:** No unbounded queue, thread-per-request fallback, refreshed deadline, or provider-gated socket creation on the blocking-I/O pool.
//! - **Evidence:** `hardening_tests`, `local_socket_poll_tests`, `netd-deferred-reply-lifecycle`.

use super::*;

pub(super) fn blocking_retry_backoff(attempt: usize) -> Duration {
    BLOCKING_RETRY_BACKOFF
        * (1_u32 << u32::try_from(attempt).unwrap_or(u32::MAX).min(BLOCKING_RETRY_MAX_DOUBLINGS))
}

pub(super) fn blocking_provider_started() {
    #[cfg(test)]
    {
        BLOCKING_PROVIDER_STARTS.fetch_add(1, Ordering::Relaxed);
    }
}

pub(super) fn is_deferred_local_poll_request(request: &NetdIpcRequest) -> bool {
    request.op == SYSCALL_OFFLOAD_OP_LINUX_POLL_SOCKET
        && request.arg2 == NETD_POLL_MODE_WAIT
        && !inet_socket_exists(request.socket_token)
}

pub(super) fn is_inet_socket_creation(request: &NetdIpcRequest) -> bool {
    request.op == SYSCALL_OFFLOAD_OP_LINUX_SOCKET && request.arg0 == linux_abi::AF_INET
}

/// `socket(AF_INET)` may wait up to `AUTHENTICATED_CONTROL_WAIT` for the DVM
/// control relay. That wait must never occupy one of the few fixed RPC
/// workers: four boot-time resolver/netprobe sockets would otherwise starve
/// every AF_UNIX request (the Wayland stream) behind it. Only the transitional
/// state waits; an active or unavailable provider answers inline.
pub(super) fn must_wait_for_packet_provider(request: &NetdIpcRequest) -> bool {
    is_inet_socket_creation(request)
        && matches!(
            packet_provider_state(),
            Ok(PacketProviderState::AwaitingAuthenticatedControl)
        )
}

pub(super) fn is_blocking_request(request: &NetdIpcRequest) -> bool {
    if request.op == SYSCALL_OFFLOAD_OP_LINUX_POLL_SOCKET && request.arg2 == NETD_POLL_MODE_WAIT {
        return inet_socket_exists(request.socket_token);
    }
    let inet_operation = match request.op {
        SYSCALL_OFFLOAD_OP_LINUX_CONNECT => sockaddr_family(request) == Some(linux_abi::AF_INET),
        SYSCALL_OFFLOAD_OP_LINUX_SENDTO
        | SYSCALL_OFFLOAD_OP_LINUX_SENDMSG
        | SYSCALL_OFFLOAD_OP_LINUX_RECVFROM
        | SYSCALL_OFFLOAD_OP_LINUX_RECVMSG => inet_socket_exists(request.socket_token),
        _ => false,
    };
    inet_operation
        && request.status_flags & linux_abi::O_NONBLOCK == 0
        && request_msg_flags(request) & (linux_abi::MSG_DONTWAIT | libc::MSG_ERRQUEUE as u64) == 0
}

pub(super) fn local_socket_reply_requests_latency_handoff(
    request: &NetdIpcRequest,
    response: &NetdIpcResponse,
    status: i32,
) -> bool {
    let is_send_or_recv = matches!(
        request.op,
        SYSCALL_OFFLOAD_OP_LINUX_SENDTO
            | SYSCALL_OFFLOAD_OP_LINUX_SENDMSG
            | SYSCALL_OFFLOAD_OP_LINUX_RECVFROM
            | SYSCALL_OFFLOAD_OP_LINUX_RECVMSG
    );
    // Only byte-moving operations can ask for a handoff; skip the table probe
    // for every other request.
    if !is_send_or_recv || inet_socket_exists(request.socket_token) {
        return false;
    }
    if status == 0 && response.value != 0 && is_send_or_recv {
        return true;
    }
    status == libc::EAGAIN
        && matches!(
            request.op,
            SYSCALL_OFFLOAD_OP_LINUX_RECVFROM | SYSCALL_OFFLOAD_OP_LINUX_RECVMSG
        )
        && consume_local_recv_drain_handoff(request.socket_token)
}

pub(super) fn consume_local_recv_drain_handoff(socket_token: u64) -> bool {
    let mut state = lock_net_state();
    let Some(socket) = state.sockets.get_mut(&socket_token) else {
        return false;
    };
    let UnixSocketState::Connected(connected) = &mut socket.state else {
        return false;
    };
    core::mem::take(&mut connected.recv_drain_handoff_armed)
}

pub(super) fn blocking_request_queue() -> &'static BlockingRequestQueue {
    static QUEUE: OnceLock<BlockingRequestQueue> = OnceLock::new();
    QUEUE.get_or_init(|| BlockingRequestQueue {
        requests: Mutex::new(VecDeque::new()),
        available: Condvar::new(),
    })
}

pub(super) fn provider_wait_queue() -> &'static BlockingRequestQueue {
    static QUEUE: OnceLock<BlockingRequestQueue> = OnceLock::new();
    QUEUE.get_or_init(|| BlockingRequestQueue {
        requests: Mutex::new(VecDeque::new()),
        available: Condvar::new(),
    })
}

pub(super) fn start_provider_wait_workers() {
    for index in 0..PROVIDER_WAIT_WORKER_COUNT {
        match thread::Builder::new()
            .name(format!("netd-provider-wait-{index}"))
            .spawn(|| {
                worker_loop(provider_wait_queue(), &PENDING_PROVIDER_WAITS);
            }) {
            Ok(_) => {
                // ORDERING: spawn produces pool availability that admission
                // consumes with Acquire before reserving a slot.
                ACTIVE_PROVIDER_WAIT_WORKERS.fetch_add(1, Ordering::AcqRel);
            }
            Err(error) => debug_line(&format!(
                "netd: provider-wait worker unavailable index={index} error={error}"
            )),
        }
    }
}

/// Parks a provider-gated `socket(AF_INET)` on the dedicated pool. The reply
/// capability travels with it; the worker sends the one terminal reply.
pub(super) fn enqueue_provider_wait_request(request: NetdIpcRequest, reply_cap: u64) -> Result<(), i32> {
    enqueue_to_pool(
        provider_wait_queue(),
        &PENDING_PROVIDER_WAITS,
        MAX_PENDING_PROVIDER_WAITS,
        &ACTIVE_PROVIDER_WAIT_WORKERS,
        request,
        reply_cap,
        rustos_svc_runtime::syscall::monotonic_nanos(),
    )
}

pub(super) fn start_blocking_workers() {
    start_provider_wait_workers();
    let queue = blocking_request_queue();
    let mut last_error = None;
    for index in 0..BLOCKING_WORKER_COUNT {
        match thread::Builder::new()
            .name(format!("netd-wait-{index}"))
            .spawn(blocking_worker_loop)
        {
            Ok(_) => {
                // ORDERING: successful worker spawn produces an active-worker
                // count; queue-admission consumers require this AcqRel update
                // to be visible before they reserve a blocking request slot.
                ACTIVE_BLOCKING_WORKERS.fetch_add(1, Ordering::AcqRel);
            }
            Err(error) => last_error = Some(error),
        }
    }
    // ORDERING: worker-spawn increments produce pool availability; this
    // startup-status consumer acquires the count before deciding whether to
    // publish the queue's initial wake-up.
    let active = ACTIVE_BLOCKING_WORKERS.load(Ordering::Acquire);
    if active == 0 {
        debug_line(&format!(
            "netd: blocking worker pool unavailable error={}",
            last_error
                .as_ref()
                .map_or_else(|| "unknown".to_owned(), ToString::to_string)
        ));
    } else {
        if let Some(error) = last_error {
            debug_line(&format!(
                "netd: blocking worker pool partially admitted active={} requested={} error={error}",
                active, BLOCKING_WORKER_COUNT,
            ));
        }
        queue.available.notify_all();
    }
}

pub(super) fn enqueue_blocking_request(request: NetdIpcRequest, reply_cap: u64) -> Result<(), i32> {
    enqueue_blocking_request_at(
        request,
        reply_cap,
        rustos_svc_runtime::syscall::monotonic_nanos(),
    )
}

pub(super) fn enqueue_blocking_request_at(
    request: NetdIpcRequest,
    reply_cap: u64,
    now_ns: u64,
) -> Result<(), i32> {
    enqueue_to_pool(
        blocking_request_queue(),
        &PENDING_BLOCKING_REQUESTS,
        MAX_PENDING_BLOCKING_REQUESTS,
        &ACTIVE_BLOCKING_WORKERS,
        request,
        reply_cap,
        now_ns,
    )
}

pub(super) fn enqueue_to_pool(
    queue: &'static BlockingRequestQueue,
    pending_counter: &AtomicUsize,
    pending_limit: usize,
    active_workers: &AtomicUsize,
    request: NetdIpcRequest,
    reply_cap: u64,
    now_ns: u64,
) -> Result<(), i32> {
    // This check intentionally precedes all reservation and worker checks:
    // queued work may age out before it reaches a provider, and expiry must
    // not consume a scarce queue slot or hide behind worker availability.
    request_deadline_remaining_ns_at(&request, now_ns)?;
    // ORDERING: worker startup produces the active count; request admission
    // consumes it so a request cannot reserve queue capacity for an
    // unavailable worker pool.
    if active_workers.load(Ordering::Acquire) == 0 {
        return Err(libc::EAGAIN);
    }
    // ORDERING: successful admission produces a reserved pending slot; worker
    // completion and rollback consume/release that slot, so AcqRel makes the
    // bounded-capacity decision linearizable against those releases.
    if pending_counter
        .fetch_update(Ordering::AcqRel, Ordering::Acquire, |pending| {
            if pending >= pending_limit {
                return None;
            }
            Some(pending + 1)
        })
        .is_err()
    {
        return Err(libc::EAGAIN);
    }
    let Ok(mut requests) = queue.requests.lock() else {
        // ORDERING: failed queue insertion produces no request; later
        // admission consumes this released slot, so the rollback must publish
        // the decrement before another bounded reservation succeeds.
        pending_counter.fetch_sub(1, Ordering::AcqRel);
        return Err(libc::EAGAIN);
    };
    requests.push_back(BlockingRequest {
        request: Box::new(request),
        reply_cap,
    });
    drop(requests);
    queue.available.notify_one();
    Ok(())
}

pub(super) fn blocking_worker_loop() {
    worker_loop(blocking_request_queue(), &PENDING_BLOCKING_REQUESTS)
}

pub(super) fn worker_loop(queue: &'static BlockingRequestQueue, pending_counter: &AtomicUsize) -> ! {
    loop {
        let job = {
            let mut requests = queue.requests.lock().unwrap();
            while requests.is_empty() {
                requests = queue.available.wait(requests).unwrap();
            }
            requests
                .pop_front()
                .expect("blocking request queue was nonempty")
        };
        let mut response = NetdIpcResponse {
            version: NETD_IPC_ABI_VERSION,
            op: job.request.op,
            ..NetdIpcResponse::default()
        };
        response.status = run_blocking_request(&job.request, &mut response, job.reply_cap);
        publish_reply(job.reply_cap, &response);
        release_pending_slot(pending_counter);
    }
}

pub(super) fn run_blocking_request(
    request: &NetdIpcRequest,
    response: &mut NetdIpcResponse,
    reply_cap: u64,
) -> i32 {
    run_blocking_request_at(
        request,
        response,
        reply_cap,
        rustos_svc_runtime::syscall::monotonic_nanos(),
    )
}

pub(super) fn run_blocking_request_at(
    request: &NetdIpcRequest,
    response: &mut NetdIpcResponse,
    reply_cap: u64,
    now_ns: u64,
) -> i32 {
    // A worker owns a queued reply capability.  Do not enter a provider once
    // the caller's absolute deadline has elapsed; the worker still sends its
    // one terminal reply and releases its queue reservation in its loop.
    if let Err(errno) = request_deadline_remaining_ns_at(request, now_ns) {
        return errno;
    }
    blocking_provider_started();
    if request.op == SYSCALL_OFFLOAD_OP_LINUX_SOCKET {
        // Socket creation publishes through this very reply capability and is
        // not retried: a second attempt would mint a second identity.
        return dispatch_request(request, response, reply_cap);
    }
    if request.op == SYSCALL_OFFLOAD_OP_LINUX_POLL_SOCKET {
        return wait_for_socket_readiness(request, response);
    }
    if request.op == SYSCALL_OFFLOAD_OP_LINUX_CONNECT {
        let mut status = handle_inet_connect(request);
        let mut attempt = 0_usize;
        while status == libc::EINPROGRESS {
            if let Err(errno) =
                sleep_within_request_deadline(request, blocking_retry_backoff(attempt))
            {
                return errno;
            }
            attempt = attempt.saturating_add(1);
            status = poll_inet_connect(request);
        }
        return status;
    }

    for attempt in 0..INET_IO_POLL_BUDGET {
        if let Err(errno) = request_deadline_remaining_ns(request) {
            return errno;
        }
        let status = dispatch_request(request, response, reply_cap);
        if status != libc::EAGAIN {
            return status;
        }
        if let Err(errno) = sleep_within_request_deadline(request, blocking_retry_backoff(attempt))
        {
            return errno;
        }
    }
    libc::EAGAIN
}
