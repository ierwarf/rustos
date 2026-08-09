//! Socket namespace, packet admission, queues, and deferred-reply policy.
//!
//! - **Owner:** `netd` owns Linux socket semantics and DVM network policy;
//!   ring0 owns only bounded transport and fd substrate.
//! - **Boundary:** Client envelopes, addresses, payloads, DVM frames, sender
//!   identity, and control epochs are untrusted.
//! - **Lifecycle:** Create/bind/connect, enqueue bounded work, detach one
//!   deferred reply owner, complete/timeout/close, revoke on client/provider
//!   exit, and restart with a new epoch.
//! - **Concurrency:** No socket/pending lock is held across synchronous IPC;
//!   every pending slot has exactly one terminal reply.
//! - **Failure:** Queue/capacity, malformed packet, timeout, cancellation, peer
//!   close, client death, and DVM revoke cannot poison unrelated clients.
//! - **Forbidden:** No socket policy in ring0, unbounded queue, single-client
//!   global stall, stale reply, or native NIC fallback.
//! - **Evidence:** `dvm-network-ingress`, `netd-deferred-reply-lifecycle`, and
//!   `commercial-envelope`.
// Provider-state tests sit beside the state machine they exercise; production
// items intentionally continue below that test-only module.
#![cfg_attr(test, allow(clippy::items_after_test_module))]

use std::collections::{BTreeMap, VecDeque};
use std::io::Write;
use std::mem::size_of;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Condvar, LockResult, Mutex, MutexGuard, OnceLock};
use std::thread;
use std::time::{Duration, Instant as StdInstant};

use rustos_user_abi::linux as linux_abi;
use rustos_user_abi::syscall::{
    CommercialMaxCapabilityLeaseWire, CommercialMaxProtocolDescriptorWire,
    CommercialMaxProtocolRequest, CommercialMaxProtocolResponse, NetBrokerPrepareSocketPublication,
    NetdIpcRequest, NetdIpcResponse, RustosNetBrokerArgs, COMMERCIAL_MAX_NETD_OP_ADDRESS_BIND,
    COMMERCIAL_MAX_NETD_OP_FD_TRANSFER, COMMERCIAL_MAX_NETD_OP_PACKET_LEASE,
    COMMERCIAL_MAX_NETD_OP_ROUTE_POLICY, COMMERCIAL_MAX_NETD_OP_SOCKET_NAMESPACE,
    COMMERCIAL_MAX_NETD_OP_SOCKET_OPTIONS, COMMERCIAL_MAX_PROTOCOL_ABI_VERSION,
    COMMERCIAL_MAX_PROTOCOL_MAX_DESCRIPTORS, COMMERCIAL_MAX_PROTOCOL_NETD, IPC_SERVICE_INPUTD,
    IPC_SERVICE_NETD, NETD_DVM_SESSION_GRANT, NETD_DVM_SESSION_REVOKE, NETD_IPC_ABI_VERSION,
    NETD_IPC_OP_DVM_SESSION, NETD_IPC_OP_REF_ACK, NETD_IPC_REQUEST_HEADER_SIZE,
    NETD_IPC_RESPONSE_FLAG_LATENCY_HANDOFF, NETD_IPC_RESPONSE_HEADER_SIZE, NETD_POLL_MODE_QUERY,
    NETD_POLL_MODE_WAIT, NETD_RECVMSG_PAYLOAD_HEADER_SIZE, NETD_SENDMSG_PAYLOAD_HEADER_SIZE,
    NET_BROKER_OP_PACKET_LEASE_GRANT, NET_BROKER_OP_PACKET_LEASE_RESET,
    NET_BROKER_OP_PACKET_LEASE_REVOKE, NET_BROKER_OP_PACKET_RX, NET_BROKER_OP_PACKET_STATUS,
    NET_BROKER_OP_PACKET_TX, NET_BROKER_OP_PREPARE_SOCKET_PUBLICATION, NET_BROKER_PACKET_MTU,
    NET_BROKER_PACKET_STATUS_ACTIVE, NET_BROKER_PACKET_STATUS_AWAITING_AUTHENTICATED_CONTROL,
    NET_BROKER_PACKET_STATUS_UNAVAILABLE, NET_BROKER_SOCKET_PUBLICATION_VERSION,
    SYSCALL_OFFLOAD_OP_LINUX_ACCEPT, SYSCALL_OFFLOAD_OP_LINUX_BIND, SYSCALL_OFFLOAD_OP_LINUX_CLOSE,
    SYSCALL_OFFLOAD_OP_LINUX_CONNECT, SYSCALL_OFFLOAD_OP_LINUX_DUP,
    SYSCALL_OFFLOAD_OP_LINUX_GETPEERNAME, SYSCALL_OFFLOAD_OP_LINUX_GETSOCKNAME,
    SYSCALL_OFFLOAD_OP_LINUX_GETSOCKOPT, SYSCALL_OFFLOAD_OP_LINUX_LISTEN,
    SYSCALL_OFFLOAD_OP_LINUX_POLL_SOCKET, SYSCALL_OFFLOAD_OP_LINUX_RECVFROM,
    SYSCALL_OFFLOAD_OP_LINUX_RECVMSG, SYSCALL_OFFLOAD_OP_LINUX_SENDMSG,
    SYSCALL_OFFLOAD_OP_LINUX_SENDTO, SYSCALL_OFFLOAD_OP_LINUX_SETSOCKOPT,
    SYSCALL_OFFLOAD_OP_LINUX_SHUTDOWN, SYSCALL_OFFLOAD_OP_LINUX_SOCKET,
    SYSCALL_OFFLOAD_OP_LINUX_SOCKETPAIR, SYS_RUSTOS_DEBUG_PRINT, SYS_RUSTOS_IPC_ENDPOINT_CREATE,
    SYS_RUSTOS_IPC_RECV_WITH_SENDER, SYS_RUSTOS_IPC_REPLY, SYS_RUSTOS_NET_BROKER,
};
#[cfg(not(test))]
use rustos_user_abi::syscall::{
    WaitSetSignalBrokerArgs, SYS_RUSTOS_ENTROPY_BROKER, SYS_RUSTOS_WAITSET_SIGNAL_BROKER,
    WAITSET_ABI_VERSION, WAITSET_PROVIDER_NETD,
};
use smoltcp::iface::{
    Config as SmolConfig, Interface, PollIngressSingleResult, PollResult, SocketHandle, SocketSet,
};
use smoltcp::phy::{Device, DeviceCapabilities, Medium, RxToken, TxToken};
use smoltcp::socket::tcp;
use smoltcp::time::Instant as SmolInstant;
use smoltcp::wire::{EthernetAddress, HardwareAddress, IpAddress, IpCidr, Ipv4Address};

mod commercial_protocol;
use commercial_protocol::{dispatch_commercial_request, validate_commercial_request};

const RECV_BACKOFF: Duration = Duration::from_millis(1);
static REPLY_FAILURE_DIAGNOSTICS: rustos_svc_runtime::ipc::ReplyFailureDiagnostics =
    rustos_svc_runtime::ipc::ReplyFailureDiagnostics::new();
/// Fixed endpoint front-end pool. Short local-socket requests from independent
/// clients must not queue behind one receiver, while shared protocol state
/// remains serialized by `NetState` and blocking INET work stays in its
/// separate bounded pool.
const NETD_REQUEST_WORKERS: usize = 4;
const BLOCKING_WORKER_COUNT: usize = 8;
const MAX_PENDING_BLOCKING_REQUESTS: usize = 32;
const REF_REPLAY_CAPACITY: usize = 4096;
static PENDING_BLOCKING_REQUESTS: AtomicUsize = AtomicUsize::new(0);
static PENDING_LOCAL_POLLS: AtomicUsize = AtomicUsize::new(0);
static ACTIVE_BLOCKING_WORKERS: AtomicUsize = AtomicUsize::new(0);
static READINESS_GENERATION: AtomicU64 = AtomicU64::new(1);
#[cfg(test)]
static BLOCKING_PROVIDER_STARTS: AtomicU64 = AtomicU64::new(0);

fn dvm_session_epoch() -> &'static Mutex<u32> {
    static EPOCH: OnceLock<Mutex<u32>> = OnceLock::new();
    EPOCH.get_or_init(|| Mutex::new(0))
}

#[derive(Clone)]
struct RefReplayEntry {
    operation_hi: u64,
    operation_lo: u64,
    op: u16,
    socket_token: u64,
    status: i32,
    value: u64,
    release: Vec<u8>,
    complete: bool,
}

fn ref_replay_log() -> &'static Mutex<VecDeque<RefReplayEntry>> {
    static LOG: OnceLock<Mutex<VecDeque<RefReplayEntry>>> = OnceLock::new();
    LOG.get_or_init(|| Mutex::new(VecDeque::with_capacity(REF_REPLAY_CAPACITY)))
}
const MAX_LISTEN_BACKLOG: usize = 128;
const SOCKET_BUFFER_CAPACITY: usize = 1024 * 1024;
// Every queued ancillary byte must fit in one kernel-consumed release trailer
// when read/close discards it. This is a lifecycle bound, not a tuning knob.
const SOCKET_CONTROL_BUFFER_CAPACITY: usize = 16 * 1024;
const INET_TCP_BUFFER_CAPACITY: usize = 16 * 1024;
const INET_CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const INET_IO_POLL_BUDGET: usize = 256;
const BLOCKING_RETRY_BACKOFF: Duration = Duration::from_millis(1);
// A mapped aperture can appear before the L0-authenticated control relay has
// delivered SESSION_START. Delay only that explicitly transitional state so a
// boot-time client does not race into a fabricated permanent ENODEV. A truly
// absent/invalid provider still fails immediately, and the wait is bounded.
// The DVM agent, L0 HMAC handshake, and RustOS session-start record boot in
// parallel with runtimed clients. Use the same five-second setup budget as the
// authenticated relay; 250 ms was shorter than a normal cold KVM handshake and
// turned a healthy transitional aperture into a permanent ENODEV for netprobe.
const AUTHENTICATED_CONTROL_WAIT: Duration = Duration::from_secs(5);
const AUTHENTICATED_CONTROL_RETRY: Duration = Duration::from_millis(4);
/// The DVM packet ring has no interrupt edge exposed to userspace. Keep the
/// provider-side ingress check bounded and substantially below an interactive
/// deadline; only an actual smoltcp socket-state transition advances the
/// public wait-set generation. Each turn processes at most the fixed ingress
/// budget before yielding, even if a hostile producer keeps refilling the ring.
// The DVM ring has no userspace eventfd; 10 ms avoids unchanged-readiness wakeup churn.
const INET_READINESS_POLL_INTERVAL: Duration = Duration::from_millis(10);
/// Cadence when no socket exists to service.
///
/// This worker takes the global net lock every tick to drive TCP timers and
/// retransmits, which is work only a live socket can need. With no sockets
/// there is none, yet the tick still acquired the lock a hundred times a
/// second for the whole life of the session, contending with the request loop
/// that shares it. The stack exposes no next-deadline hint to sleep against
/// (smoltcp's `poll_delay` has no equivalent here), so back off on the one
/// condition that is certain instead, and return to the tight cadence the
/// moment a socket exists.
const INET_IDLE_POLL_INTERVAL: Duration = Duration::from_millis(100);
const INET_READINESS_POLL_BUDGET: usize = 32;
const QEMU_USERNET_ADDR: Ipv4Address = Ipv4Address::new(10, 0, 2, 15);
const QEMU_USERNET_GATEWAY: Ipv4Address = Ipv4Address::new(10, 0, 2, 2);
const QEMU_USERNET_MAC: EthernetAddress = EthernetAddress([0x52, 0x54, 0x00, 0x12, 0x34, 0x56]);

fn main() {
    debug_line("netd: service start");
    let endpoint = syscall0(SYS_RUSTOS_IPC_ENDPOINT_CREATE);
    if endpoint < 0 {
        let _ = writeln!(
            std::io::stderr(),
            "netd: endpoint create failed errno={}",
            -endpoint
        );
        return;
    }

    let register =
        rustos_svc_runtime::ipc::register_service_endpoint(IPC_SERVICE_NETD, endpoint as u64);
    if register < 0 {
        let _ = writeln!(
            std::io::stderr(),
            "netd: endpoint register failed errno={}",
            -register
        );
        return;
    }
    if call_packet_broker(NET_BROKER_OP_PACKET_LEASE_RESET, 0, 0).is_err() {
        debug_line("netd: failed to reset inherited DVM transport lease");
        return;
    }

    debug_line("netd: network policy endpoint registered");
    serve(endpoint as u64);
}

fn serve(endpoint: u64) {
    start_blocking_workers();
    start_inet_readiness_worker();
    for worker_index in 1..NETD_REQUEST_WORKERS {
        let name = format!("netd-rpc-{worker_index}");
        if let Err(error) = thread::Builder::new()
            .name(name)
            .spawn(move || serve_request_loop(endpoint))
        {
            debug_line(&format!(
                "netd: request worker unavailable index={worker_index} error={error}"
            ));
        }
    }
    serve_request_loop(endpoint);
}

fn start_inet_readiness_worker() {
    if let Err(error) = thread::Builder::new()
        .name("netd-inet-readiness".to_owned())
        .spawn(inet_readiness_worker_loop)
    {
        debug_line(&format!(
            "netd: INET readiness worker unavailable error={error}"
        ));
        std::process::exit(134);
    }
}

fn inet_readiness_worker_loop() {
    loop {
        let changed_tokens = {
            let mut state = net_state().lock().unwrap();
            let before = state.inet_readiness_snapshot();
            let readiness_changed = state
                .inet
                .as_mut()
                .is_some_and(|stack| stack.poll_budget(INET_READINESS_POLL_BUDGET));
            if readiness_changed {
                let after = state.inet_readiness_snapshot();
                after
                    .into_iter()
                    .filter_map(|(token, readiness)| {
                        (before
                            .iter()
                            .find(|(before_token, _)| *before_token == token)
                            .is_none_or(|(_, previous)| *previous != readiness))
                        .then_some(token)
                    })
                    .collect::<Vec<_>>()
            } else {
                Vec::new()
            }
        };
        advance_readiness_generation(&changed_tokens);
        let has_sockets = !net_state().lock().unwrap().inet_sockets.is_empty();
        thread::sleep(if has_sockets {
            INET_READINESS_POLL_INTERVAL
        } else {
            INET_IDLE_POLL_INTERVAL
        });
    }
}

fn serve_request_loop(endpoint: u64) {
    loop {
        service_local_poll_waiters();
        let mut request = NetdIpcRequest::default();
        let mut reply_cap = 0_u64;
        let mut sender_pid = 0_u64;
        let mut sender_tid = 0_u64;
        let received = syscall6(
            SYS_RUSTOS_IPC_RECV_WITH_SENDER,
            endpoint,
            (&mut request as *mut NetdIpcRequest) as u64,
            size_of::<NetdIpcRequest>() as u64,
            (&mut reply_cap as *mut u64) as u64,
            (&mut sender_pid as *mut u64) as u64,
            (&mut sender_tid as *mut u64) as u64,
        );
        if received < 0 {
            thread::sleep(RECV_BACKOFF);
            continue;
        }
        if received == 0 {
            thread::sleep(RECV_BACKOFF);
            continue;
        }

        if received as usize == size_of::<CommercialMaxProtocolRequest>() {
            let commercial_request = unsafe {
                &*((&request as *const NetdIpcRequest).cast::<CommercialMaxProtocolRequest>())
            };
            if commercial_request.header.version == COMMERCIAL_MAX_PROTOCOL_ABI_VERSION
                && commercial_request.header.protocol == COMMERCIAL_MAX_PROTOCOL_NETD
            {
                let reply = if commercial_request.header.subject_pid == sender_pid
                    && commercial_request.header.subject_tid == sender_tid
                {
                    reply_commercial_request(reply_cap, commercial_request)
                } else {
                    reply_commercial_error(reply_cap, commercial_request, libc::EACCES)
                };
                if reply < 0 {
                    REPLY_FAILURE_DIAGNOSTICS.record("netd", "ipc", -reply);
                }
                continue;
            }
        }

        let mut response = NetdIpcResponse {
            version: NETD_IPC_ABI_VERSION,
            op: request.op,
            ..NetdIpcResponse::default()
        };
        response.status = match validate_request(received as usize, &request) {
            Ok(()) if request.pid != sender_pid || request.tid != sender_tid => libc::EACCES,
            Ok(()) => {
                // Admission is the one point that derives a service class end
                // from the caller's immutable wire end. Queued/retried work
                // only consumes this stamped end; it may never refresh it.
                if let Err(errno) = clamp_request_deadline_at(
                    &mut request,
                    rustos_svc_runtime::syscall::monotonic_nanos(),
                ) {
                    errno
                } else if request.op == NETD_IPC_OP_DVM_SESSION {
                    let status = handle_dvm_session(&request, sender_pid);
                    if status != 0 {
                        let _ = writeln!(
                            std::io::stderr(),
                            "netd: DVM session transition rejected status={status} epoch={} action={}",
                            request.arg0,
                            request.arg1
                        );
                    }
                    status
                } else if is_deferred_local_poll_request(&request) {
                    let status = handle_poll_socket(&request, &mut response);
                    if status == 0 && response.value == 0 {
                        match defer_local_poll_request(request, reply_cap) {
                            Ok(()) => continue,
                            Err(errno) => errno,
                        }
                    } else {
                        if status == 0 {
                            response.reserved1 = NETD_IPC_RESPONSE_FLAG_LATENCY_HANDOFF;
                        }
                        status
                    }
                } else if is_blocking_request(&request) {
                    match enqueue_blocking_request(request, reply_cap) {
                        Ok(()) => continue,
                        Err(errno) => errno,
                    }
                } else {
                    let status = dispatch_request(&request, &mut response, reply_cap);
                    if local_socket_reply_requests_latency_handoff(&request, &response, status) {
                        response.reserved1 = NETD_IPC_RESPONSE_FLAG_LATENCY_HANDOFF;
                    }
                    status
                }
            }
            Err(errno) => errno,
        };
        let reply = reply_netd_response(reply_cap, &response);
        if reply < 0 {
            REPLY_FAILURE_DIAGNOSTICS.record("netd", "ipc", -reply);
        }
        service_local_poll_waiters();
    }
}

fn reply_netd_response(reply_cap: u64, response: &NetdIpcResponse) -> i64 {
    let Some(response_len) = NETD_IPC_RESPONSE_HEADER_SIZE
        .checked_add(response.payload_len as usize)
        .filter(|len| *len <= size_of::<NetdIpcResponse>())
    else {
        return -(libc::EINVAL as i64);
    };
    syscall3(
        SYS_RUSTOS_IPC_REPLY,
        reply_cap,
        (response as *const NetdIpcResponse) as u64,
        response_len as u64,
    )
}

/// The caller owns one absolute `CLOCK_MONOTONIC` end instant.  Netd may
/// subdivide the remaining time for a provider operation or retry, but never
/// begins a fresh service-local timeout for a request that has already waited
/// in one of its bounded queues.
fn request_deadline_remaining_ns_at(request: &NetdIpcRequest, now_ns: u64) -> Result<u64, i32> {
    if request.deadline_ns == 0 || now_ns >= request.deadline_ns {
        return Err(libc::ETIMEDOUT);
    }
    Ok(request.deadline_ns - now_ns)
}

fn request_deadline_remaining_ns(request: &NetdIpcRequest) -> Result<u64, i32> {
    request_deadline_remaining_ns_at(request, rustos_svc_runtime::syscall::monotonic_nanos())
}

/// Netd derives its local authority once, at admission. The caller's wire end
/// stays immutable except for this tightening: a class cap never gives a
/// queued operation a later deadline than the caller originally supplied.
fn request_deadline_class_cap(request: &NetdIpcRequest) -> Duration {
    match request.op {
        SYSCALL_OFFLOAD_OP_LINUX_POLL_SOCKET => {
            Duration::from_millis(rustos_user_abi::performance::IPC_READINESS_QUERY_HARD_LIMIT_MS)
        }
        NETD_IPC_OP_DVM_SESSION
        | SYSCALL_OFFLOAD_OP_LINUX_DUP
        | SYSCALL_OFFLOAD_OP_LINUX_CLOSE
        | NETD_IPC_OP_REF_ACK => Duration::from_millis(
            rustos_user_abi::performance::IPC_INTERACTIVE_CONTROL_HARD_LIMIT_MS,
        ),
        _ => Duration::from_millis(rustos_user_abi::performance::IPC_BULK_DATA_HARD_LIMIT_MS),
    }
}

fn clamp_request_deadline_at(request: &mut NetdIpcRequest, now_ns: u64) -> Result<(), i32> {
    request_deadline_remaining_ns_at(request, now_ns)?;
    let cap_ns = request_deadline_class_cap(request)
        .as_nanos()
        .min(u128::from(u64::MAX)) as u64;
    request.deadline_ns = request.deadline_ns.min(now_ns.saturating_add(cap_ns));
    request_deadline_remaining_ns_at(request, now_ns).map(|_| ())
}

fn capped_request_wait_at(
    request: &NetdIpcRequest,
    now_ns: u64,
    cap: Duration,
) -> Result<Duration, i32> {
    let remaining_ns = request_deadline_remaining_ns_at(request, now_ns)?;
    let cap_ns = cap.as_nanos().min(u128::from(u64::MAX)) as u64;
    Ok(Duration::from_nanos(remaining_ns.min(cap_ns)))
}

fn capped_request_wait(request: &NetdIpcRequest, cap: Duration) -> Result<Duration, i32> {
    capped_request_wait_at(request, rustos_svc_runtime::syscall::monotonic_nanos(), cap)
}

fn sleep_within_request_deadline(request: &NetdIpcRequest, cap: Duration) -> Result<(), i32> {
    thread::sleep(capped_request_wait(request, cap)?);
    Ok(())
}

fn blocking_provider_started() {
    #[cfg(test)]
    {
        BLOCKING_PROVIDER_STARTS.fetch_add(1, Ordering::Relaxed);
    }
}

fn is_deferred_local_poll_request(request: &NetdIpcRequest) -> bool {
    request.op == SYSCALL_OFFLOAD_OP_LINUX_POLL_SOCKET
        && request.arg2 == NETD_POLL_MODE_WAIT
        && !inet_socket_exists(request.socket_token)
}

fn is_blocking_request(request: &NetdIpcRequest) -> bool {
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
        && request_msg_flags(request) & linux_abi::MSG_DONTWAIT == 0
}

fn local_socket_reply_requests_latency_handoff(
    request: &NetdIpcRequest,
    response: &NetdIpcResponse,
    status: i32,
) -> bool {
    if inet_socket_exists(request.socket_token) {
        return false;
    }
    let is_send_or_recv = matches!(
        request.op,
        SYSCALL_OFFLOAD_OP_LINUX_SENDTO
            | SYSCALL_OFFLOAD_OP_LINUX_SENDMSG
            | SYSCALL_OFFLOAD_OP_LINUX_RECVFROM
            | SYSCALL_OFFLOAD_OP_LINUX_RECVMSG
    );
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

fn consume_local_recv_drain_handoff(socket_token: u64) -> bool {
    let mut state = net_state().lock().unwrap();
    let Some(socket) = state.sockets.get_mut(&socket_token) else {
        return false;
    };
    let UnixSocketState::Connected(connected) = &mut socket.state else {
        return false;
    };
    core::mem::take(&mut connected.recv_drain_handoff_armed)
}

struct BlockingRequest {
    request: Box<NetdIpcRequest>,
    reply_cap: u64,
}

struct BlockingRequestQueue {
    requests: Mutex<VecDeque<BlockingRequest>>,
    available: Condvar,
}

fn blocking_request_queue() -> &'static BlockingRequestQueue {
    static QUEUE: OnceLock<BlockingRequestQueue> = OnceLock::new();
    QUEUE.get_or_init(|| BlockingRequestQueue {
        requests: Mutex::new(VecDeque::new()),
        available: Condvar::new(),
    })
}

fn start_blocking_workers() {
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

fn enqueue_blocking_request(request: NetdIpcRequest, reply_cap: u64) -> Result<(), i32> {
    enqueue_blocking_request_at(
        request,
        reply_cap,
        rustos_svc_runtime::syscall::monotonic_nanos(),
    )
}

fn enqueue_blocking_request_at(
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
    if ACTIVE_BLOCKING_WORKERS.load(Ordering::Acquire) == 0 {
        return Err(libc::EAGAIN);
    }
    // ORDERING: successful admission produces a reserved pending slot; worker
    // completion and rollback consume/release that slot, so AcqRel makes the
    // bounded-capacity decision linearizable against those releases.
    if PENDING_BLOCKING_REQUESTS
        .fetch_update(Ordering::AcqRel, Ordering::Acquire, |pending| {
            if pending >= MAX_PENDING_BLOCKING_REQUESTS {
                return None;
            }
            Some(pending + 1)
        })
        .is_err()
    {
        return Err(libc::EAGAIN);
    }
    let queue = blocking_request_queue();
    let Ok(mut requests) = queue.requests.lock() else {
        // ORDERING: failed queue insertion produces no request; later
        // admission consumes this released slot, so the rollback must publish
        // the decrement before another bounded reservation succeeds.
        PENDING_BLOCKING_REQUESTS.fetch_sub(1, Ordering::AcqRel);
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

fn blocking_worker_loop() {
    let queue = blocking_request_queue();
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
        response.status = run_blocking_request(&job.request, &mut response);
        let _ = reply_netd_response(job.reply_cap, &response);
        release_pending_slot(&PENDING_BLOCKING_REQUESTS);
    }
}

struct DeferredLocalPoll {
    request: Box<NetdIpcRequest>,
    reply_cap: u64,
}

fn local_poll_waiters() -> &'static Mutex<VecDeque<DeferredLocalPoll>> {
    static WAITERS: OnceLock<Mutex<VecDeque<DeferredLocalPoll>>> = OnceLock::new();
    WAITERS.get_or_init(|| Mutex::new(VecDeque::new()))
}

fn defer_local_poll_request(request: NetdIpcRequest, reply_cap: u64) -> Result<(), i32> {
    defer_local_poll_request_at(
        request,
        reply_cap,
        rustos_svc_runtime::syscall::monotonic_nanos(),
    )
}

fn defer_local_poll_request_at(
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
fn service_local_poll_waiters() {
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
                let _ = reply_netd_response(reply_cap, response);
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
                    let _ = reply_netd_response(waiter.reply_cap, &response);
                    release_pending_slot(&PENDING_LOCAL_POLLS);
                }
            }
        }
    }
}

/// Returns a waiter only when it remains live.  All terminal paths invoke the
/// reply and reservation-release callbacks together, making a detached wait
/// incapable of leaking one half of its lifecycle transaction.
fn service_one_local_poll_waiter<F, R>(
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

fn take_deferred_queue<T>(locked: LockResult<MutexGuard<'_, VecDeque<T>>>) -> (VecDeque<T>, bool) {
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

fn reserve_pending_slot(counter: &AtomicUsize, limit: usize) -> bool {
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

fn release_pending_slot(counter: &AtomicUsize) {
    // ORDERING: completion produces an available slot; a later admission
    // consumes that availability with an acquire failure-order load, so this
    // AcqRel decrement publishes the release before reuse.
    let released = counter.fetch_update(Ordering::AcqRel, Ordering::Acquire, |pending| {
        pending.checked_sub(1)
    });
    debug_assert!(released.is_ok(), "pending request counter underflow");
}

fn run_blocking_request(request: &NetdIpcRequest, response: &mut NetdIpcResponse) -> i32 {
    run_blocking_request_at(
        request,
        response,
        rustos_svc_runtime::syscall::monotonic_nanos(),
    )
}

fn run_blocking_request_at(
    request: &NetdIpcRequest,
    response: &mut NetdIpcResponse,
    now_ns: u64,
) -> i32 {
    // A worker owns a queued reply capability.  Do not enter a provider once
    // the caller's absolute deadline has elapsed; the worker still sends its
    // one terminal reply and releases its queue reservation in its loop.
    if let Err(errno) = request_deadline_remaining_ns_at(request, now_ns) {
        return errno;
    }
    blocking_provider_started();
    if request.op == SYSCALL_OFFLOAD_OP_LINUX_POLL_SOCKET {
        return wait_for_socket_readiness(request, response);
    }
    if request.op == SYSCALL_OFFLOAD_OP_LINUX_CONNECT {
        let mut status = begin_inet_connect(request);
        while status == libc::EINPROGRESS {
            if let Err(errno) = sleep_within_request_deadline(request, BLOCKING_RETRY_BACKOFF) {
                return errno;
            }
            status = poll_inet_connect(request);
        }
        if status == 0 {
            advance_readiness_generation(&[request.socket_token]);
        }
        return status;
    }

    for _ in 0..INET_IO_POLL_BUDGET {
        if let Err(errno) = request_deadline_remaining_ns(request) {
            return errno;
        }
        let status = dispatch_request(request, response, 0);
        if status != libc::EAGAIN {
            return status;
        }
        if let Err(errno) = sleep_within_request_deadline(request, BLOCKING_RETRY_BACKOFF) {
            return errno;
        }
    }
    libc::EAGAIN
}

fn reply_commercial_request(reply_cap: u64, request: &CommercialMaxProtocolRequest) -> i64 {
    let mut response = CommercialMaxProtocolResponse {
        header: request.header,
        ..CommercialMaxProtocolResponse::default()
    };
    response.header.version = COMMERCIAL_MAX_PROTOCOL_ABI_VERSION;
    response.status = match validate_commercial_request(request) {
        Ok(()) => dispatch_commercial_request(request, &mut response),
        Err(errno) => errno,
    };
    syscall3(
        SYS_RUSTOS_IPC_REPLY,
        reply_cap,
        (&response as *const CommercialMaxProtocolResponse) as u64,
        size_of::<CommercialMaxProtocolResponse>() as u64,
    )
}

fn reply_commercial_error(
    reply_cap: u64,
    request: &CommercialMaxProtocolRequest,
    status: i32,
) -> i64 {
    let response = CommercialMaxProtocolResponse {
        header: request.header,
        status,
        ..CommercialMaxProtocolResponse::default()
    };
    syscall3(
        SYS_RUSTOS_IPC_REPLY,
        reply_cap,
        (&response as *const CommercialMaxProtocolResponse) as u64,
        size_of::<CommercialMaxProtocolResponse>() as u64,
    )
}

fn dispatch_request(
    request: &NetdIpcRequest,
    response: &mut NetdIpcResponse,
    reply_cap: u64,
) -> i32 {
    if request.op == NETD_IPC_OP_REF_ACK {
        return acknowledge_ref_result(request);
    }
    let replay_safe_ref = matches!(
        request.op,
        SYSCALL_OFFLOAD_OP_LINUX_DUP | SYSCALL_OFFLOAD_OP_LINUX_CLOSE
    );
    if replay_safe_ref {
        match begin_ref_result(request, response) {
            Ok(RefReplayAction::Replay(status)) => return status,
            Ok(RefReplayAction::Execute) => {}
            Err(errno) => return errno,
        }
    }
    let mutates_readiness = request_mutates_readiness(request.op);
    let readiness_targets = if mutates_readiness {
        readiness_targets_for_request(request)
    } else {
        Vec::new()
    };
    let status = match request.op {
        SYSCALL_OFFLOAD_OP_LINUX_SOCKET => handle_socket(request, response, reply_cap),
        SYSCALL_OFFLOAD_OP_LINUX_SOCKETPAIR => handle_socketpair(request, response),
        SYSCALL_OFFLOAD_OP_LINUX_DUP => handle_dup(request),
        SYSCALL_OFFLOAD_OP_LINUX_CLOSE => handle_close(request, response),
        SYSCALL_OFFLOAD_OP_LINUX_BIND => handle_bind(request),
        SYSCALL_OFFLOAD_OP_LINUX_LISTEN => handle_listen(request),
        SYSCALL_OFFLOAD_OP_LINUX_ACCEPT => handle_accept(request, response),
        SYSCALL_OFFLOAD_OP_LINUX_CONNECT => handle_connect(request),
        SYSCALL_OFFLOAD_OP_LINUX_SENDTO | SYSCALL_OFFLOAD_OP_LINUX_SENDMSG => {
            handle_send(request, response)
        }
        SYSCALL_OFFLOAD_OP_LINUX_RECVFROM | SYSCALL_OFFLOAD_OP_LINUX_RECVMSG => {
            handle_recv(request, response)
        }
        SYSCALL_OFFLOAD_OP_LINUX_POLL_SOCKET => handle_poll_socket(request, response),
        SYSCALL_OFFLOAD_OP_LINUX_GETSOCKNAME => handle_getsockname(request, response),
        SYSCALL_OFFLOAD_OP_LINUX_GETPEERNAME => handle_getpeername(request, response),
        SYSCALL_OFFLOAD_OP_LINUX_SETSOCKOPT => handle_setsockopt(request),
        SYSCALL_OFFLOAD_OP_LINUX_GETSOCKOPT => handle_getsockopt(request, response),
        SYSCALL_OFFLOAD_OP_LINUX_SHUTDOWN => handle_shutdown(request),
        _ => libc::EINVAL,
    };
    let final_close = request.op != SYSCALL_OFFLOAD_OP_LINUX_CLOSE || response.value == 0;
    if status == 0 && mutates_readiness && final_close {
        advance_readiness_generation(&readiness_targets);
    }
    if replay_safe_ref {
        complete_ref_result(request, response, status);
    }
    status
}

fn handle_dvm_session(request: &NetdIpcRequest, sender_pid: u64) -> i32 {
    if rustos_svc_runtime::ipc::validate_service_owner(IPC_SERVICE_INPUTD, sender_pid) < 0 {
        return libc::EACCES;
    }
    let Ok(epoch) = u32::try_from(request.arg0) else {
        return libc::EINVAL;
    };
    if epoch == 0
        || request.arg2 != 0
        || request.arg3 != 0
        || request.arg4 != 0
        || request.arg5 != 0
    {
        return libc::EINVAL;
    }
    let Ok(mut active) = dvm_session_epoch().lock() else {
        return libc::EIO;
    };
    match admit_dvm_session_transition(*active, epoch, request.arg1) {
        Ok(DvmSessionTransition::Grant) => {
            match call_packet_broker(NET_BROKER_OP_PACKET_LEASE_GRANT, u64::from(epoch), 0) {
                Ok(_) => {
                    *active = epoch;
                    0
                }
                Err(errno) => errno,
            }
        }
        Ok(DvmSessionTransition::Revoke) => {
            match call_packet_broker(NET_BROKER_OP_PACKET_LEASE_REVOKE, u64::from(epoch), 0) {
                Ok(_) => {
                    *active = 0;
                    0
                }
                Err(errno) => errno,
            }
        }
        Err(errno) => errno,
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum DvmSessionTransition {
    Grant,
    Revoke,
}

fn admit_dvm_session_transition(
    active_epoch: u32,
    requested_epoch: u32,
    action: u64,
) -> Result<DvmSessionTransition, i32> {
    if requested_epoch == 0 {
        return Err(libc::EINVAL);
    }
    match action {
        NETD_DVM_SESSION_GRANT if active_epoch == 0 || active_epoch == requested_epoch => {
            Ok(DvmSessionTransition::Grant)
        }
        NETD_DVM_SESSION_GRANT => Err(libc::EBUSY),
        NETD_DVM_SESSION_REVOKE if active_epoch == requested_epoch => {
            Ok(DvmSessionTransition::Revoke)
        }
        NETD_DVM_SESSION_REVOKE => Err(libc::ESTALE),
        _ => Err(libc::EINVAL),
    }
}

#[cfg(test)]
mod dvm_session_policy_tests;

enum RefReplayAction {
    Execute,
    Replay(i32),
}

fn begin_ref_result(
    request: &NetdIpcRequest,
    response: &mut NetdIpcResponse,
) -> Result<RefReplayAction, i32> {
    if request.operation_hi == 0 && request.operation_lo == 0 {
        return Err(libc::EINVAL);
    }
    let mut log = ref_replay_log().lock().map_err(|_| libc::EIO)?;
    if let Some(entry) = log.iter().find(|entry| {
        entry.operation_hi == request.operation_hi && entry.operation_lo == request.operation_lo
    }) {
        if entry.op != request.op || entry.socket_token != request.socket_token {
            return Err(libc::EPROTO);
        }
        if !entry.complete {
            return Err(libc::EBUSY);
        }
        response.value = entry.value;
        response.payload[..entry.release.len()].copy_from_slice(entry.release.as_slice());
        response.payload_len = entry.release.len() as u32;
        response.reserved0 = entry.release.len() as u32;
        return Ok(RefReplayAction::Replay(entry.status));
    }
    if log.len() == REF_REPLAY_CAPACITY {
        return Err(libc::ENOSPC);
    }
    log.push_back(RefReplayEntry {
        operation_hi: request.operation_hi,
        operation_lo: request.operation_lo,
        op: request.op,
        socket_token: request.socket_token,
        status: libc::EINPROGRESS,
        value: 0,
        release: Vec::new(),
        complete: false,
    });
    Ok(RefReplayAction::Execute)
}

fn complete_ref_result(request: &NetdIpcRequest, response: &NetdIpcResponse, status: i32) {
    let Ok(mut log) = ref_replay_log().lock() else {
        debug_line("netd: ref replay completion lock poisoned");
        std::process::exit(134);
    };
    let Some(entry) = log.iter_mut().find(|entry| {
        entry.operation_hi == request.operation_hi && entry.operation_lo == request.operation_lo
    }) else {
        debug_line("netd: ref replay reservation lost");
        std::process::exit(134);
    };
    entry.status = status;
    entry.value = response.value;
    let release_len = response.reserved0 as usize;
    entry.release.clear();
    entry
        .release
        .extend_from_slice(&response.payload[..release_len]);
    entry.complete = true;
}

fn acknowledge_ref_result(request: &NetdIpcRequest) -> i32 {
    if request.operation_hi == 0 && request.operation_lo == 0
        || !matches!(
            request.arg0 as u16,
            SYSCALL_OFFLOAD_OP_LINUX_DUP | SYSCALL_OFFLOAD_OP_LINUX_CLOSE
        )
    {
        return libc::EINVAL;
    }
    let Ok(mut log) = ref_replay_log().lock() else {
        return libc::EIO;
    };
    let Some(index) = log.iter().position(|entry| {
        entry.operation_hi == request.operation_hi && entry.operation_lo == request.operation_lo
    }) else {
        return 0;
    };
    let entry = &log[index];
    if entry.op != request.arg0 as u16 || entry.socket_token != request.socket_token {
        return libc::EPROTO;
    }
    if !entry.complete {
        return libc::EBUSY;
    }
    log.remove(index);
    0
}

#[cfg(test)]
#[path = "tests/ref_replay_tests.rs"]
mod ref_replay_tests;

fn request_mutates_readiness(op: u16) -> bool {
    matches!(
        op,
        SYSCALL_OFFLOAD_OP_LINUX_CLOSE
            | SYSCALL_OFFLOAD_OP_LINUX_LISTEN
            | SYSCALL_OFFLOAD_OP_LINUX_ACCEPT
            | SYSCALL_OFFLOAD_OP_LINUX_CONNECT
            | SYSCALL_OFFLOAD_OP_LINUX_SENDTO
            | SYSCALL_OFFLOAD_OP_LINUX_SENDMSG
            | SYSCALL_OFFLOAD_OP_LINUX_RECVFROM
            | SYSCALL_OFFLOAD_OP_LINUX_RECVMSG
            | SYSCALL_OFFLOAD_OP_LINUX_SHUTDOWN
    )
}

fn readiness_targets_for_request(request: &NetdIpcRequest) -> Vec<u64> {
    let state = net_state().lock().unwrap();
    readiness_targets_in_state(request, &state)
}

fn readiness_targets_in_state(request: &NetdIpcRequest, state: &NetState) -> Vec<u64> {
    let mut targets = Vec::with_capacity(3);
    if request.socket_token != 0 {
        targets.push(request.socket_token);
    }
    if let Some(socket) = state.sockets.get(&request.socket_token) {
        if let UnixSocketState::Connected(connected) = &socket.state {
            targets.push(connected.peer);
        }
    }
    if request.op == SYSCALL_OFFLOAD_OP_LINUX_CONNECT
        && sockaddr_family(request) == Some(linux_abi::AF_UNIX)
    {
        if let Ok(path) = sockaddr_path_from_payload(request) {
            if let Some(listener) = state.bindings.get(&path) {
                targets.push(*listener);
            }
        }
    }
    targets.sort_unstable();
    targets.dedup();
    targets
}

fn advance_readiness_generation(object_ids: &[u64]) {
    if object_ids.is_empty() {
        return;
    }
    // ORDERING: socket-state mutation produces a new readiness generation;
    // polling clients consume the generation after the matching per-object
    // publication, so AcqRel serializes successive readiness transitions.
    let generation = READINESS_GENERATION
        .fetch_update(Ordering::AcqRel, Ordering::Acquire, |generation| {
            generation.checked_add(1)
        })
        .unwrap_or_else(|_| {
            debug_line("netd: readiness generation exhausted");
            std::process::exit(134);
        })
        + 1;
    for object_id in object_ids.iter().copied() {
        if object_id != 0 {
            publish_readiness_generation(object_id, generation);
        }
    }
}

fn publish_readiness_generation(object_id: u64, generation: u64) {
    #[cfg(test)]
    let _ = (object_id, generation);
    #[cfg(not(test))]
    {
        let args = WaitSetSignalBrokerArgs {
            abi_version: WAITSET_ABI_VERSION,
            provider: WAITSET_PROVIDER_NETD,
            flags: 0,
            object_id,
            generation,
            reserved0: 0,
        };
        let result = syscall1(
            SYS_RUSTOS_WAITSET_SIGNAL_BROKER,
            (&args as *const WaitSetSignalBrokerArgs) as u64,
        );
        if result < 0 {
            debug_line("netd: readiness generation publication failed");
            std::process::exit(134);
        }
    }
}

fn validate_request(received: usize, request: &NetdIpcRequest) -> Result<(), i32> {
    let expected_len = NETD_IPC_REQUEST_HEADER_SIZE.checked_add(request.payload_len as usize);
    if received < NETD_IPC_REQUEST_HEADER_SIZE
        || expected_len != Some(received)
        || received > size_of::<NetdIpcRequest>()
        || request.version != NETD_IPC_ABI_VERSION
        || request.flags != 0
        || request.reserved1 != 0
        || request.deadline_ns == 0
        || request.pid == 0
        || request.tid == 0
        || request.payload_len as usize > request.payload.len()
    {
        return Err(libc::EINVAL);
    }
    if request.op == SYSCALL_OFFLOAD_OP_LINUX_POLL_SOCKET
        && !matches!(request.arg2, NETD_POLL_MODE_QUERY | NETD_POLL_MODE_WAIT)
    {
        return Err(libc::EINVAL);
    }
    if matches!(
        request.op,
        SYSCALL_OFFLOAD_OP_LINUX_DUP | SYSCALL_OFFLOAD_OP_LINUX_CLOSE | NETD_IPC_OP_REF_ACK
    ) && request.operation_hi == 0
        && request.operation_lo == 0
    {
        return Err(libc::EINVAL);
    }
    match request.op {
        SYSCALL_OFFLOAD_OP_LINUX_SOCKET
        | SYSCALL_OFFLOAD_OP_LINUX_SOCKETPAIR
        | SYSCALL_OFFLOAD_OP_LINUX_DUP
        | SYSCALL_OFFLOAD_OP_LINUX_CLOSE
        | SYSCALL_OFFLOAD_OP_LINUX_BIND
        | SYSCALL_OFFLOAD_OP_LINUX_LISTEN
        | SYSCALL_OFFLOAD_OP_LINUX_ACCEPT
        | SYSCALL_OFFLOAD_OP_LINUX_CONNECT
        | SYSCALL_OFFLOAD_OP_LINUX_SENDTO
        | SYSCALL_OFFLOAD_OP_LINUX_GETSOCKNAME
        | SYSCALL_OFFLOAD_OP_LINUX_GETPEERNAME
        | SYSCALL_OFFLOAD_OP_LINUX_SETSOCKOPT
        | SYSCALL_OFFLOAD_OP_LINUX_GETSOCKOPT
        | SYSCALL_OFFLOAD_OP_LINUX_SHUTDOWN
        | SYSCALL_OFFLOAD_OP_LINUX_SENDMSG
        | SYSCALL_OFFLOAD_OP_LINUX_RECVMSG
        | SYSCALL_OFFLOAD_OP_LINUX_RECVFROM
        | SYSCALL_OFFLOAD_OP_LINUX_POLL_SOCKET
        | NETD_IPC_OP_DVM_SESSION
        | NETD_IPC_OP_REF_ACK => Ok(()),
        _ => Err(libc::EINVAL),
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
struct Credentials {
    pid: i32,
    uid: u32,
    gid: u32,
}

#[derive(Debug)]
enum UnixSocketState {
    Idle,
    Listening {
        backlog: usize,
        pending: VecDeque<u64>,
    },
    Connected(ConnectedState),
}

#[derive(Debug)]
struct ConnectedState {
    incoming: VecDeque<UnixStreamSegment>,
    incoming_bytes: usize,
    incoming_control_bytes: usize,
    channel_id: u64,
    peer: u64,
    peer_closed: bool,
    peer_read_closed: bool,
    peer_write_closed: bool,
    peer_credentials: Credentials,
    /// A successful nonblocking read arms exactly one follow-up EAGAIN
    /// handoff so drain-until-empty event loops can return to userspace
    /// without granting a boost to an arbitrary empty-read spin loop.
    recv_drain_handoff_armed: bool,
    recv_closed: bool,
    send_closed: bool,
}

#[derive(Debug)]
struct UnixStreamSegment {
    bytes: VecDeque<u8>,
    control: Vec<u8>,
}

#[derive(Debug)]
struct UnixSocket {
    owner: Credentials,
    refs: usize,
    options: SocketOptions,
    bound_path: Option<String>,
    local_path: Option<String>,
    peer_path: Option<String>,
    state: UnixSocketState,
}

struct InetSocket {
    refs: usize,
    options: SocketOptions,
    tcp: SocketHandle,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct InetReadiness {
    can_recv: bool,
    can_send: bool,
    open: bool,
}

#[derive(Clone, Copy, Debug)]
struct SocketOptions {
    reuse_addr: bool,
    reuse_port: bool,
    keepalive: bool,
    send_buffer: i32,
    recv_buffer: i32,
    passcred: bool,
}

impl Default for SocketOptions {
    fn default() -> Self {
        Self {
            reuse_addr: false,
            reuse_port: false,
            keepalive: false,
            send_buffer: SOCKET_BUFFER_CAPACITY as i32,
            recv_buffer: SOCKET_BUFFER_CAPACITY as i32,
            passcred: false,
        }
    }
}

struct NetState {
    sockets: BTreeMap<u64, UnixSocket>,
    inet_sockets: BTreeMap<u64, InetSocket>,
    bindings: BTreeMap<String, u64>,
    inet: Option<InetStack>,
}

impl NetState {
    fn new() -> Self {
        Self {
            sockets: BTreeMap::new(),
            inet_sockets: BTreeMap::new(),
            bindings: BTreeMap::new(),
            inet: None,
        }
    }

    fn token_available(&self, token: u64) -> bool {
        token != 0 && !self.sockets.contains_key(&token) && !self.inet_sockets.contains_key(&token)
    }

    fn inet_stack(&mut self) -> Result<&mut InetStack, i32> {
        if self.inet.is_none() {
            self.inet = Some(InetStack::new()?);
        }
        Ok(self.inet.as_mut().unwrap())
    }

    fn inet_readiness_snapshot(&self) -> Vec<(u64, InetReadiness)> {
        let Some(stack) = self.inet.as_ref() else {
            return Vec::new();
        };
        self.inet_sockets
            .iter()
            .map(|(token, inet)| {
                let socket = stack.sockets.get::<tcp::Socket>(inet.tcp);
                (
                    *token,
                    InetReadiness {
                        can_recv: socket.can_recv(),
                        can_send: socket.can_send(),
                        open: socket.is_open(),
                    },
                )
            })
            .collect()
    }
}

fn mint_socket_token() -> Result<u64, i32> {
    for _ in 0..16 {
        let mut token = 0_u64;
        #[cfg(not(test))]
        let read = syscall2(
            SYS_RUSTOS_ENTROPY_BROKER,
            (&mut token as *mut u64).cast::<u8>() as u64,
            size_of::<u64>() as u64,
        );
        // Host tests do not implement RustOS-private syscalls. Exercise the
        // same rejection/collision logic with the host CSPRNG, never a
        // deterministic fallback.
        #[cfg(test)]
        let read = unsafe {
            libc::syscall(
                libc::SYS_getrandom,
                (&mut token as *mut u64).cast::<u8>(),
                size_of::<u64>(),
                0,
            )
        };
        if read == size_of::<u64>() as i64 && token != 0 {
            return Ok(token);
        }
        if read < 0 {
            return Err(last_errno());
        }
    }
    Err(libc::EAGAIN)
}

fn net_state() -> &'static Mutex<NetState> {
    static STATE: OnceLock<Mutex<NetState>> = OnceLock::new();
    STATE.get_or_init(|| Mutex::new(NetState::new()))
}

struct InetStack {
    iface: Interface,
    sockets: SocketSet<'static>,
    device: BrokerDevice,
    next_port: u16,
}

impl InetStack {
    fn new() -> Result<Self, i32> {
        if packet_provider_state()? != PacketProviderState::Active {
            return Err(libc::ENODEV);
        }
        let mut device = BrokerDevice;
        let mut config = SmolConfig::new(HardwareAddress::Ethernet(QEMU_USERNET_MAC));
        config.random_seed = 0x5255_0001;
        let mut iface = Interface::new(config, &mut device, smol_now());
        iface.update_ip_addrs(|ip_addrs| {
            let _ = ip_addrs.push(IpCidr::new(IpAddress::Ipv4(QEMU_USERNET_ADDR), 24));
        });
        iface
            .routes_mut()
            .add_default_ipv4_route(QEMU_USERNET_GATEWAY)
            .map_err(|_| libc::ENOSPC)?;
        Ok(Self {
            iface,
            sockets: SocketSet::new(Vec::new()),
            device,
            next_port: 49152,
        })
    }

    fn add_tcp_socket(&mut self) -> SocketHandle {
        let rx = tcp::SocketBuffer::new(vec![0; INET_TCP_BUFFER_CAPACITY]);
        let tx = tcp::SocketBuffer::new(vec![0; INET_TCP_BUFFER_CAPACITY]);
        self.sockets.add(tcp::Socket::new(rx, tx))
    }

    fn remove(&mut self, handle: SocketHandle) {
        self.sockets.remove(handle);
    }

    fn poll_one(&mut self) -> (bool, bool) {
        let now = smol_now();
        self.iface.poll_maintenance(now);
        let ingress = self
            .iface
            .poll_ingress_single(now, &mut self.device, &mut self.sockets);
        let egress = self
            .iface
            .poll_egress(now, &mut self.device, &mut self.sockets);
        (
            !matches!(ingress, PollIngressSingleResult::None),
            poll_turn_changes_readiness(ingress, egress),
        )
    }

    fn poll_budget(&mut self, budget: usize) -> bool {
        let mut readiness_changed = false;
        for _ in 0..budget {
            let (processed_ingress, changed) = self.poll_one();
            readiness_changed |= changed;
            if !processed_ingress {
                break;
            }
        }
        readiness_changed
    }

    fn next_ephemeral_port(&mut self) -> u16 {
        let port = self.next_port;
        self.next_port = if self.next_port == 65535 {
            49152
        } else {
            self.next_port + 1
        };
        port
    }
}

fn poll_turn_changes_readiness(ingress: PollIngressSingleResult, egress: PollResult) -> bool {
    matches!(ingress, PollIngressSingleResult::SocketStateChanged)
        || matches!(egress, PollResult::SocketStateChanged)
}

struct BrokerDevice;

struct BrokerRxToken {
    frame: Vec<u8>,
}

struct BrokerTxToken;

impl Device for BrokerDevice {
    type RxToken<'a> = BrokerRxToken;
    type TxToken<'a> = BrokerTxToken;

    fn receive(
        &mut self,
        _timestamp: SmolInstant,
    ) -> Option<(Self::RxToken<'_>, Self::TxToken<'_>)> {
        let mut frame = vec![0_u8; NET_BROKER_PACKET_MTU];
        match packet_rx(frame.as_mut_slice()) {
            Ok(0) => None,
            Ok(len) => {
                frame.truncate(len);
                Some((BrokerRxToken { frame }, BrokerTxToken))
            }
            Err(errno) if errno == libc::EAGAIN => None,
            Err(_) => None,
        }
    }

    fn transmit(&mut self, _timestamp: SmolInstant) -> Option<Self::TxToken<'_>> {
        Some(BrokerTxToken)
    }

    fn capabilities(&self) -> DeviceCapabilities {
        let mut caps = DeviceCapabilities::default();
        caps.medium = Medium::Ethernet;
        caps.max_transmission_unit = NET_BROKER_PACKET_MTU;
        caps
    }
}

impl RxToken for BrokerRxToken {
    fn consume<R, F>(self, f: F) -> R
    where
        F: FnOnce(&[u8]) -> R,
    {
        f(self.frame.as_slice())
    }
}

impl TxToken for BrokerTxToken {
    fn consume<R, F>(self, len: usize, f: F) -> R
    where
        F: FnOnce(&mut [u8]) -> R,
    {
        let mut frame = vec![0_u8; len.min(NET_BROKER_PACKET_MTU)];
        let result = f(frame.as_mut_slice());
        let _ = packet_tx(frame.as_slice());
        result
    }
}

fn smol_now() -> SmolInstant {
    static START: OnceLock<StdInstant> = OnceLock::new();
    let start = START.get_or_init(StdInstant::now);
    SmolInstant::from_millis(start.elapsed().as_millis().min(i64::MAX as u128) as i64)
}

fn handle_socket(request: &NetdIpcRequest, response: &mut NetdIpcResponse, reply_cap: u64) -> i32 {
    let domain = request.arg0;
    let socket_type = request.arg1;
    let base_type = socket_type & linux_abi::SOCK_TYPE_MASK;
    if domain == linux_abi::AF_INET && base_type == linux_abi::SOCK_STREAM {
        // The provider gate is the final fallible step before publication of
        // a new socket token. It receives the admitted request end directly,
        // so expiration cannot mint or publish a socket identity.
        if let Err(errno) = await_authenticated_packet_provider(request) {
            return errno;
        }
        // The successful broker observation can age out before this local
        // publication turn. Recheck the same admitted end before minting any
        // externally usable socket identity.
        if let Err(errno) = request_deadline_remaining_ns(request) {
            return errno;
        }
        let token = match mint_socket_token() {
            Ok(token) => token,
            Err(errno) => return errno,
        };
        let mut state = net_state().lock().unwrap();
        let token_available = state.token_available(token);
        let tcp = match start_inet_socket_provider_at(
            request,
            rustos_svc_runtime::syscall::monotonic_nanos(),
            token_available,
            || state.inet_stack().map(|stack| stack.add_tcp_socket()),
        ) {
            Ok(tcp) => tcp,
            Err(errno) => return errno,
        };
        state.inet_sockets.insert(
            token,
            InetSocket {
                refs: 1,
                options: SocketOptions::default(),
                tcp,
            },
        );
        drop(state);
        return prepare_inet_socket_publication(request, response, reply_cap, token);
    }
    if domain != linux_abi::AF_UNIX || base_type != linux_abi::SOCK_STREAM {
        return libc::EAFNOSUPPORT;
    }

    let token = match mint_socket_token() {
        Ok(token) => token,
        Err(errno) => return errno,
    };
    let mut state = net_state().lock().unwrap();
    if !state.token_available(token) {
        return libc::EAGAIN;
    }
    state.sockets.insert(
        token,
        UnixSocket {
            owner: request_credentials(request),
            refs: 1,
            options: SocketOptions::default(),
            bound_path: None,
            local_path: None,
            peer_path: None,
            state: UnixSocketState::Idle,
        },
    );
    drop(state);
    call_net_broker(request, response, token, 0, 0, true)
}

fn handle_socketpair(request: &NetdIpcRequest, response: &mut NetdIpcResponse) -> i32 {
    if request.arg0 != linux_abi::AF_UNIX
        || request.arg1 & linux_abi::SOCK_TYPE_MASK != linux_abi::SOCK_STREAM
    {
        return libc::EAFNOSUPPORT;
    }
    let credentials = request_credentials(request);
    let left = match mint_socket_token() {
        Ok(token) => token,
        Err(errno) => return errno,
    };
    let right = match mint_socket_token() {
        Ok(token) => token,
        Err(errno) => return errno,
    };
    let mut state = net_state().lock().unwrap();
    if left == right || !state.token_available(left) || !state.token_available(right) {
        return libc::EAGAIN;
    }
    state.sockets.insert(
        left,
        UnixSocket {
            owner: credentials,
            refs: 1,
            options: SocketOptions::default(),
            bound_path: None,
            local_path: None,
            peer_path: None,
            state: UnixSocketState::Connected(ConnectedState {
                incoming: VecDeque::new(),
                incoming_bytes: 0,
                incoming_control_bytes: 0,
                channel_id: 0,
                peer: right,
                peer_closed: false,
                peer_read_closed: false,
                peer_write_closed: false,
                peer_credentials: credentials,
                recv_drain_handoff_armed: false,
                recv_closed: false,
                send_closed: false,
            }),
        },
    );
    state.sockets.insert(
        right,
        UnixSocket {
            owner: credentials,
            refs: 1,
            options: SocketOptions::default(),
            bound_path: None,
            local_path: None,
            peer_path: None,
            state: UnixSocketState::Connected(ConnectedState {
                incoming: VecDeque::new(),
                incoming_bytes: 0,
                incoming_control_bytes: 0,
                channel_id: 0,
                peer: left,
                peer_closed: false,
                peer_read_closed: false,
                peer_write_closed: false,
                peer_credentials: credentials,
                recv_drain_handoff_armed: false,
                recv_closed: false,
                send_closed: false,
            }),
        },
    );
    drop(state);
    call_net_broker(request, response, 0, left, right, true)
}

fn handle_dup(request: &NetdIpcRequest) -> i32 {
    let mut state = net_state().lock().unwrap();
    if let Some(socket) = state.inet_sockets.get_mut(&request.socket_token) {
        socket.refs = socket.refs.saturating_add(1).max(1);
        return 0;
    }
    let Some(socket) = state.sockets.get_mut(&request.socket_token) else {
        return libc::EBADF;
    };
    socket.refs = socket.refs.saturating_add(1).max(1);
    0
}

fn handle_close(request: &NetdIpcRequest, response: &mut NetdIpcResponse) -> i32 {
    let mut state = net_state().lock().unwrap();
    if let Some(socket) = state.inet_sockets.get_mut(&request.socket_token) {
        if socket.refs > 1 {
            socket.refs -= 1;
            response.value = socket.refs as u64;
            return 0;
        }
        let Some(socket) = state.inet_sockets.remove(&request.socket_token) else {
            return libc::EBADF;
        };
        if let Some(stack) = state.inet.as_mut() {
            stack.remove(socket.tcp);
            stack.poll_budget(8);
        }
        return 0;
    }
    let Some(socket) = state.sockets.get_mut(&request.socket_token) else {
        return libc::EBADF;
    };
    if socket.refs > 1 {
        socket.refs -= 1;
        response.value = socket.refs as u64;
        return 0;
    }
    let Some(socket) = state.sockets.remove(&request.socket_token) else {
        return libc::EBADF;
    };
    if let Some(path) = socket.bound_path {
        if state.bindings.get(&path).copied() == Some(request.socket_token) {
            state.bindings.remove(&path);
        }
    }
    if let UnixSocketState::Connected(mut connected) = socket.state {
        let mut released = Vec::new();
        while let Some(segment) = connected.incoming.pop_front() {
            if segment.control.is_empty() {
                continue;
            }
            let start = cmsg_align(released.len());
            released.resize(start, 0);
            released.extend_from_slice(segment.control.as_slice());
        }
        if released.len() > response.payload.len() {
            debug_line("netd: ancillary release bound violated");
            std::process::exit(134);
        }
        response.payload[..released.len()].copy_from_slice(released.as_slice());
        response.payload_len = released.len() as u32;
        response.reserved0 = released.len() as u32;
        if let Some(peer) = state.sockets.get_mut(&connected.peer) {
            if let UnixSocketState::Connected(peer_connected) = &mut peer.state {
                peer_connected.peer_closed = true;
                peer_connected.peer_read_closed = true;
                peer_connected.peer_write_closed = true;
            }
        }
    }
    drop(state);
    0
}

/// Performs the final under-lock admission immediately before smoltcp gains a
/// socket. The earlier provider wait and pre-mint checks avoid wasted work;
/// this check is the provider-start linearization point and uses the same
/// immutable request end rather than a refreshed relative timeout.
fn start_inet_socket_provider_at<T>(
    request: &NetdIpcRequest,
    now_ns: u64,
    token_available: bool,
    start: impl FnOnce() -> Result<T, i32>,
) -> Result<T, i32> {
    request_deadline_remaining_ns_at(request, now_ns)?;
    if !token_available {
        return Err(libc::EAGAIN);
    }
    start()
}

fn handle_bind(request: &NetdIpcRequest) -> i32 {
    let path = match sockaddr_path_from_payload(request) {
        Ok(path) => path,
        Err(errno) => return errno,
    };
    if is_wayland_path(path.as_str()) {
        debug_line("netd: wayland bind");
    }
    let mut state = net_state().lock().unwrap();
    if state.bindings.contains_key(&path) {
        return libc::EADDRINUSE;
    }
    let Some(socket) = state.sockets.get_mut(&request.socket_token) else {
        return libc::EBADF;
    };
    if socket.bound_path.is_some() || !matches!(socket.state, UnixSocketState::Idle) {
        return libc::EINVAL;
    }
    socket.bound_path = Some(path.clone());
    socket.local_path = Some(path.clone());
    state.bindings.insert(path, request.socket_token);
    0
}

fn handle_listen(request: &NetdIpcRequest) -> i32 {
    let mut state = net_state().lock().unwrap();
    let Some(socket) = state.sockets.get_mut(&request.socket_token) else {
        return libc::EBADF;
    };
    let is_wayland = socket.bound_path.as_deref().is_some_and(is_wayland_path);
    if socket.bound_path.is_none() || !matches!(socket.state, UnixSocketState::Idle) {
        return libc::EINVAL;
    }
    let backlog = usize::try_from(request.arg1)
        .unwrap_or(usize::MAX)
        .clamp(1, MAX_LISTEN_BACKLOG);
    socket.state = UnixSocketState::Listening {
        backlog,
        pending: VecDeque::new(),
    };
    if is_wayland {
        debug_line("netd: wayland listen");
    }
    0
}

fn handle_connect(request: &NetdIpcRequest) -> i32 {
    if sockaddr_family(request) == Some(linux_abi::AF_INET) {
        return handle_inet_connect(request);
    }
    let path = match sockaddr_path_from_payload(request) {
        Ok(path) => path,
        Err(errno) => return errno,
    };
    let credentials = request_credentials(request);
    let accepted = match mint_socket_token() {
        Ok(token) => token,
        Err(errno) => return errno,
    };
    let mut state = net_state().lock().unwrap();
    let Some(listener_token) = state.bindings.get(&path).copied() else {
        return libc::ENOENT;
    };
    if !state.token_available(accepted) {
        return libc::EAGAIN;
    }
    let Some(client) = state.sockets.get(&request.socket_token) else {
        return libc::EBADF;
    };
    if client.bound_path.is_some() || !matches!(client.state, UnixSocketState::Idle) {
        return libc::EINVAL;
    }
    let Some(listener) = state.sockets.get(&listener_token) else {
        return libc::ECONNREFUSED;
    };
    let UnixSocketState::Listening { backlog, pending } = &listener.state else {
        return libc::ECONNREFUSED;
    };
    if pending.len() >= *backlog {
        return libc::EAGAIN;
    }
    let channel_id = match bind_unix_connection(request, accepted) {
        Ok(channel_id) => channel_id,
        Err(errno) => return errno,
    };
    let is_wayland = is_wayland_path(path.as_str());
    let client = state
        .sockets
        .get_mut(&request.socket_token)
        .expect("validated client socket disappeared under net state lock");
    let client_local_path = client.local_path.clone();
    client.peer_path = Some(path.clone());
    client.state = UnixSocketState::Connected(ConnectedState {
        incoming: VecDeque::new(),
        incoming_bytes: 0,
        incoming_control_bytes: 0,
        channel_id,
        peer: accepted,
        peer_closed: false,
        peer_read_closed: false,
        peer_write_closed: false,
        peer_credentials: credentials,
        recv_drain_handoff_armed: false,
        recv_closed: false,
        send_closed: false,
    });
    let (listener_owner, listener_path) = {
        let Some(listener) = state.sockets.get_mut(&listener_token) else {
            return libc::ECONNREFUSED;
        };
        let UnixSocketState::Listening { pending, .. } = &mut listener.state else {
            return libc::ECONNREFUSED;
        };
        pending.push_back(accepted);
        if is_wayland {
            debug_line("netd: wayland connect queued");
        }
        (listener.owner, listener.local_path.clone())
    };
    state.sockets.insert(
        accepted,
        UnixSocket {
            owner: listener_owner,
            refs: 1,
            options: SocketOptions::default(),
            bound_path: None,
            local_path: listener_path,
            peer_path: client_local_path,
            state: UnixSocketState::Connected(ConnectedState {
                incoming: VecDeque::new(),
                incoming_bytes: 0,
                incoming_control_bytes: 0,
                channel_id,
                peer: request.socket_token,
                peer_closed: false,
                peer_read_closed: false,
                peer_write_closed: false,
                peer_credentials: credentials,
                recv_drain_handoff_armed: false,
                recv_closed: false,
                send_closed: false,
            }),
        },
    );
    drop(state);
    0
}

fn bind_unix_connection(request: &NetdIpcRequest, accepted: u64) -> Result<u64, i32> {
    let args = RustosNetBrokerArgs {
        process_id: request.pid,
        op: rustos_user_abi::syscall::NET_BROKER_OP_UNIX_CONNECT_BIND,
        reserved0: 0,
        reserved1: 0,
        arg0: request.arg0,
        arg1: request.socket_token,
        arg2: accepted,
        arg3: 0,
        arg4: 0,
        arg5: 0,
    };
    let result = syscall1(
        SYS_RUSTOS_NET_BROKER,
        (&args as *const RustosNetBrokerArgs) as u64,
    );
    if result < 0 {
        Err(last_errno())
    } else if result == 0 {
        Err(libc::EPROTO)
    } else {
        Ok(result as u64)
    }
}

fn handle_inet_connect(request: &NetdIpcRequest) -> i32 {
    begin_inet_connect(request)
}

fn begin_inet_connect(request: &NetdIpcRequest) -> i32 {
    let connect_timeout = match capped_request_wait(request, INET_CONNECT_TIMEOUT) {
        Ok(timeout) => timeout,
        Err(errno) => return errno,
    };
    // smoltcp's timeout setter uses millisecond granularity.  A sub-millisecond
    // remainder cannot safely be encoded as zero because zero means no timeout
    // in that API, so fail closed instead of widening the caller's deadline.
    let connect_timeout_ms = connect_timeout.as_millis();
    if connect_timeout_ms == 0 {
        return libc::ETIMEDOUT;
    }
    let (remote, port) = match sockaddr_in_from_payload(request) {
        Ok(endpoint) => endpoint,
        Err(errno) => return errno,
    };
    let mut state = net_state().lock().unwrap();
    let Some(inet) = state.inet_sockets.get(&request.socket_token) else {
        return libc::EBADF;
    };
    let tcp_handle = inet.tcp;
    let stack = match state.inet_stack() {
        Ok(stack) => stack,
        Err(errno) => return errno,
    };
    let local_port = stack.next_ephemeral_port();
    {
        let socket = stack.sockets.get_mut::<tcp::Socket>(tcp_handle);
        if socket.may_send() {
            return 0;
        }
        if socket.is_open() {
            return libc::EINPROGRESS;
        }
        socket.set_timeout(Some(smoltcp::time::Duration::from_millis(
            connect_timeout_ms.min(u128::from(u64::MAX)) as u64,
        )));
        if socket
            .connect(
                stack.iface.context(),
                (IpAddress::Ipv4(remote), port),
                local_port,
            )
            .is_err()
        {
            return libc::EINVAL;
        }
    }
    stack.poll_budget(8);
    let socket = stack.sockets.get_mut::<tcp::Socket>(tcp_handle);
    if socket.may_send() {
        debug_line("netd: inet connect ok");
        0
    } else if !socket.is_open() {
        libc::ECONNREFUSED
    } else {
        libc::EINPROGRESS
    }
}

fn poll_inet_connect(request: &NetdIpcRequest) -> i32 {
    if let Err(errno) = request_deadline_remaining_ns(request) {
        return errno;
    }
    let mut state = net_state().lock().unwrap();
    let Some(inet) = state.inet_sockets.get(&request.socket_token) else {
        return libc::EBADF;
    };
    let tcp_handle = inet.tcp;
    let stack = match state.inet_stack() {
        Ok(stack) => stack,
        Err(errno) => return errno,
    };
    stack.poll_budget(8);
    let socket = stack.sockets.get_mut::<tcp::Socket>(tcp_handle);
    if socket.may_send() {
        debug_line("netd: inet connect ok");
        0
    } else if !socket.is_open() {
        libc::ECONNREFUSED
    } else {
        libc::EINPROGRESS
    }
}

fn handle_accept(request: &NetdIpcRequest, response: &mut NetdIpcResponse) -> i32 {
    let nonblocking = request.status_flags & linux_abi::O_NONBLOCK != 0
        || request.arg3 & linux_abi::SOCK_NONBLOCK != 0;
    let accepted = {
        let mut state = net_state().lock().unwrap();
        let Some(listener) = state.sockets.get_mut(&request.socket_token) else {
            return libc::EBADF;
        };
        let is_wayland = listener.local_path.as_deref().is_some_and(is_wayland_path);
        match &mut listener.state {
            UnixSocketState::Listening { pending, .. } => match pending.pop_front() {
                Some(token) => {
                    if is_wayland {
                        debug_line("netd: wayland accept dequeued");
                    }
                    token
                }
                None if nonblocking => return libc::EAGAIN,
                None => return libc::EAGAIN,
            },
            _ => return libc::EINVAL,
        }
    };
    let channel_id = {
        let state = net_state().lock().unwrap();
        let Some(socket) = state.sockets.get(&accepted) else {
            return libc::ECONNABORTED;
        };
        let UnixSocketState::Connected(connected) = &socket.state else {
            return libc::ECONNABORTED;
        };
        connected.channel_id
    };
    if channel_id == 0 {
        return libc::EPROTO;
    }
    if request.arg1 != 0 && request.arg2 != 0 {
        let state = net_state().lock().unwrap();
        if let Some(socket) = state.sockets.get(&accepted) {
            if let Err(errno) =
                sockaddr_payload(socket.peer_path.as_deref().unwrap_or(""), response)
            {
                return errno;
            }
        }
    }
    let result = call_net_broker(request, response, 0, accepted, channel_id, false);
    if result != 0 {
        // The kernel did not publish the accepted open description. Restore
        // the listener's ownership so a transient broker failure cannot lose
        // a connection that is still live in netd.
        let mut state = net_state().lock().unwrap();
        if state.sockets.contains_key(&accepted) {
            if let Some(listener) = state.sockets.get_mut(&request.socket_token) {
                if let UnixSocketState::Listening { pending, .. } = &mut listener.state {
                    if !pending.contains(&accepted) {
                        pending.push_front(accepted);
                    }
                }
            }
        }
    }
    result
}

fn is_wayland_path(path: &str) -> bool {
    path.ends_with("/wayland-0") || path == "wayland-0"
}

fn handle_send(request: &NetdIpcRequest, response: &mut NetdIpcResponse) -> i32 {
    if inet_socket_exists(request.socket_token) {
        return handle_inet_send(request, response);
    }
    if request.op == SYSCALL_OFFLOAD_OP_LINUX_SENDMSG {
        return handle_sendmsg(request, response);
    }
    let len = request.payload_len as usize;
    let bytes = &request.payload[..len];
    match send_socket_bytes(request, bytes) {
        Ok(sent) => {
            response.value = sent as u64;
            0
        }
        Err(errno) => errno,
    }
}

fn handle_sendmsg(request: &NetdIpcRequest, response: &mut NetdIpcResponse) -> i32 {
    let payload_len = request.payload_len as usize;
    if payload_len < NETD_SENDMSG_PAYLOAD_HEADER_SIZE {
        return libc::EINVAL;
    }
    let data_len = u32::from_ne_bytes(request.payload[0..4].try_into().unwrap_or([0; 4])) as usize;
    let control_len =
        u32::from_ne_bytes(request.payload[4..8].try_into().unwrap_or([0; 4])) as usize;
    let data_start = NETD_SENDMSG_PAYLOAD_HEADER_SIZE;
    let control_start = match data_start.checked_add(data_len) {
        Some(value) => value,
        None => return libc::EINVAL,
    };
    let end = match control_start.checked_add(control_len) {
        Some(value) => value,
        None => return libc::EINVAL,
    };
    if end > payload_len {
        return libc::EINVAL;
    }
    if let Err(errno) = validate_sendmsg_control(&request.payload[control_start..end]) {
        return errno;
    }
    match send_socket_message(
        request,
        &request.payload[data_start..control_start],
        &request.payload[control_start..end],
    ) {
        Ok(sent) => {
            response.value = sent as u64;
            0
        }
        Err(errno) => errno,
    }
}

fn handle_recv(request: &NetdIpcRequest, response: &mut NetdIpcResponse) -> i32 {
    if inet_socket_exists(request.socket_token) {
        return handle_inet_recv(request, response);
    }
    if request.op == SYSCALL_OFFLOAD_OP_LINUX_RECVMSG {
        return handle_recvmsg(request, response);
    }
    let requested = if request.op == SYSCALL_OFFLOAD_OP_LINUX_RECVMSG {
        request.payload_len as usize
    } else {
        usize::try_from(request.arg2).unwrap_or(usize::MAX)
    };
    let limit = requested.min(response.payload.len() - SOCKET_CONTROL_BUFFER_CAPACITY);
    match recv_socket_bytes(request, &mut response.payload[..limit], false) {
        Ok(received) => {
            let release_start = received.read;
            let release_end = release_start + received.discarded.len();
            response.payload[release_start..release_end]
                .copy_from_slice(received.discarded.as_slice());
            response.reserved0 = received.discarded.len() as u32;
            response.value = received.read as u64;
            response.payload_len = release_end as u32;
            0
        }
        Err(errno) => errno,
    }
}

fn handle_recvmsg(request: &NetdIpcRequest, response: &mut NetdIpcResponse) -> i32 {
    let requested = request.payload_len as usize;
    let control_len = match pending_recvmsg_control_len(request, requested) {
        Ok(control_len) => control_len,
        Err(errno) => return errno,
    };
    let available = response
        .payload
        .len()
        .saturating_sub(NETD_RECVMSG_PAYLOAD_HEADER_SIZE)
        .saturating_sub(control_len);
    let data_len = requested.min(available);
    let data_start = NETD_RECVMSG_PAYLOAD_HEADER_SIZE;
    let data_end = data_start + data_len;
    match recv_socket_bytes(request, &mut response.payload[data_start..data_end], true) {
        Ok(received) => {
            let control = match append_recvmsg_credentials(request, received.control) {
                Ok(control) => control,
                Err(errno) => return errno,
            };
            let control_start = data_start + received.read;
            let control_end = control_start + control.len();
            response.payload[control_start..control_end].copy_from_slice(&control);
            response.payload[0..4].copy_from_slice(&(received.read as u32).to_ne_bytes());
            response.payload[4..8].copy_from_slice(&(control.len() as u32).to_ne_bytes());
            response.payload[8..12].copy_from_slice(&0_u32.to_ne_bytes());
            response.payload[12..16].copy_from_slice(&0_u32.to_ne_bytes());
            response.payload_len = control_end as u32;
            response.value = received.read as u64;
            0
        }
        Err(errno) => errno,
    }
}

fn handle_inet_send(request: &NetdIpcRequest, response: &mut NetdIpcResponse) -> i32 {
    let bytes = &request.payload[..request.payload_len as usize];
    if bytes.is_empty() {
        response.value = 0;
        return 0;
    }
    let mut state = net_state().lock().unwrap();
    let Some(inet) = state.inet_sockets.get(&request.socket_token) else {
        return libc::EBADF;
    };
    let tcp_handle = inet.tcp;
    let stack = match state.inet_stack() {
        Ok(stack) => stack,
        Err(errno) => return errno,
    };
    stack.poll_budget(4);
    let socket = stack.sockets.get_mut::<tcp::Socket>(tcp_handle);
    if socket.can_send() {
        match socket.send_slice(bytes) {
            Ok(sent) => {
                response.value = sent as u64;
                stack.poll_budget(16);
                return 0;
            }
            Err(_) => return libc::EPIPE,
        }
    }
    if !socket.may_send() {
        libc::EPIPE
    } else {
        libc::EAGAIN
    }
}

fn handle_inet_recv(request: &NetdIpcRequest, response: &mut NetdIpcResponse) -> i32 {
    let requested = usize::try_from(request.arg2).unwrap_or(usize::MAX);
    let limit = requested.min(response.payload.len());
    if limit == 0 {
        response.value = 0;
        response.payload_len = 0;
        return 0;
    }
    let mut state = net_state().lock().unwrap();
    let Some(inet) = state.inet_sockets.get(&request.socket_token) else {
        return libc::EBADF;
    };
    let tcp_handle = inet.tcp;
    let stack = match state.inet_stack() {
        Ok(stack) => stack,
        Err(errno) => return errno,
    };
    stack.poll_budget(8);
    let socket = stack.sockets.get_mut::<tcp::Socket>(tcp_handle);
    if socket.can_recv() {
        match socket.recv_slice(&mut response.payload[..limit]) {
            Ok(read) => {
                response.value = read as u64;
                response.payload_len = read as u32;
                stack.poll_budget(8);
                return 0;
            }
            Err(_) => return libc::ECONNRESET,
        }
    }
    if !socket.may_recv() {
        response.value = 0;
        response.payload_len = 0;
        0
    } else {
        libc::EAGAIN
    }
}

fn handle_inet_poll(request: &NetdIpcRequest, response: &mut NetdIpcResponse) -> i32 {
    let requested = request.arg1 as u32;
    let mut state = net_state().lock().unwrap();
    let Some(inet) = state.inet_sockets.get(&request.socket_token) else {
        return libc::EBADF;
    };
    let tcp_handle = inet.tcp;
    let stack = match state.inet_stack() {
        Ok(stack) => stack,
        Err(errno) => return errno,
    };
    stack.poll_budget(8);
    let socket = stack.sockets.get_mut::<tcp::Socket>(tcp_handle);
    let mut revents = 0_u32;
    if requested & linux_abi::POLLIN as u32 != 0 && socket.can_recv() {
        revents |= linux_abi::POLLIN as u32;
    }
    if requested & linux_abi::POLLOUT as u32 != 0 && socket.can_send() {
        revents |= linux_abi::POLLOUT as u32;
    }
    if !socket.is_open() {
        revents |= linux_abi::POLLHUP as u32;
    }
    response.value = revents as u64;
    0
}

fn handle_poll_socket(request: &NetdIpcRequest, response: &mut NetdIpcResponse) -> i32 {
    let status = if inet_socket_exists(request.socket_token) {
        handle_inet_poll(request, response)
    } else {
        let state = net_state().lock().unwrap();
        match unix_socket_revents(&state, request.socket_token, request.arg1 as u32) {
            Ok(revents) => {
                response.value = revents as u64;
                0
            }
            Err(errno) => errno,
        }
    };
    if status == 0 {
        // ORDERING: readiness mutation produces an increment before publishing
        // the object generation; this successful-poll consumer acquires it so
        // its response carries a generation no older than that publication.
        response.payload[..8]
            .copy_from_slice(&READINESS_GENERATION.load(Ordering::Acquire).to_le_bytes());
        response.payload_len = 8;
    }
    status
}

fn unix_socket_revents(state: &NetState, token: u64, requested: u32) -> Result<u32, i32> {
    let socket = state.sockets.get(&token).ok_or(libc::EBADF)?;
    let mut revents = 0_u32;
    match &socket.state {
        UnixSocketState::Listening { pending, .. } => {
            if requested & linux_abi::POLLIN as u32 != 0 && !pending.is_empty() {
                revents |= linux_abi::POLLIN as u32;
            }
        }
        UnixSocketState::Connected(connected) => {
            let peer_connected =
                state
                    .sockets
                    .get(&connected.peer)
                    .and_then(|peer| match &peer.state {
                        UnixSocketState::Connected(peer_connected) => Some(peer_connected),
                        _ => None,
                    });
            if requested & linux_abi::POLLIN as u32 != 0
                && (connected.incoming_bytes != 0
                    || connected.peer_write_closed
                    || connected.peer_closed)
            {
                revents |= linux_abi::POLLIN as u32;
            }
            if requested & linux_abi::POLLOUT as u32 != 0 {
                let writable = !connected.send_closed
                    && !connected.peer_read_closed
                    && !connected.peer_closed
                    && peer_connected.is_some_and(|peer| {
                        !peer.recv_closed
                            && peer.incoming_bytes < SOCKET_BUFFER_CAPACITY
                            && peer.incoming_control_bytes < SOCKET_CONTROL_BUFFER_CAPACITY
                    });
                if writable {
                    revents |= linux_abi::POLLOUT as u32;
                }
            }
            if connected.peer_closed {
                revents |= linux_abi::POLLHUP as u32;
            }
            if peer_connected.is_none() {
                revents |= linux_abi::POLLERR as u32;
            }
        }
        UnixSocketState::Idle => {
            revents |= linux_abi::POLLERR as u32;
        }
    }
    Ok(revents)
}

fn wait_for_socket_readiness(request: &NetdIpcRequest, response: &mut NetdIpcResponse) -> i32 {
    if !inet_socket_exists(request.socket_token) {
        return libc::EBADF;
    }
    for _ in 0..INET_IO_POLL_BUDGET {
        if let Err(errno) = request_deadline_remaining_ns(request) {
            return errno;
        }
        let status = handle_inet_poll(request, response);
        if status != 0 || response.value != 0 {
            return status;
        }
        if let Err(errno) = sleep_within_request_deadline(request, BLOCKING_RETRY_BACKOFF) {
            return errno;
        }
    }
    libc::EAGAIN
}

fn handle_getsockname(request: &NetdIpcRequest, response: &mut NetdIpcResponse) -> i32 {
    let state = net_state().lock().unwrap();
    let Some(socket) = state.sockets.get(&request.socket_token) else {
        return libc::EBADF;
    };
    sockaddr_payload(socket.local_path.as_deref().unwrap_or(""), response)
        .unwrap_or_else(|errno| errno)
}

fn handle_getpeername(request: &NetdIpcRequest, response: &mut NetdIpcResponse) -> i32 {
    let state = net_state().lock().unwrap();
    let Some(socket) = state.sockets.get(&request.socket_token) else {
        return libc::EBADF;
    };
    let Some(path) = socket.peer_path.as_deref() else {
        return libc::ENOTCONN;
    };
    sockaddr_payload(path, response).unwrap_or_else(|errno| errno)
}

fn handle_getsockopt(request: &NetdIpcRequest, response: &mut NetdIpcResponse) -> i32 {
    if request.arg1 != linux_abi::SOL_SOCKET {
        return libc::EOPNOTSUPP;
    }
    let state = net_state().lock().unwrap();
    if let Some(socket) = state.inet_sockets.get(&request.socket_token) {
        let value = match request.arg2 {
            linux_abi::SO_ERROR => 0_i32,
            linux_abi::SO_TYPE => linux_abi::SOCK_STREAM as i32,
            linux_abi::SO_DOMAIN => linux_abi::AF_INET as i32,
            linux_abi::SO_PROTOCOL => 0_i32,
            linux_abi::SO_SNDBUF => socket.options.send_buffer,
            linux_abi::SO_RCVBUF => socket.options.recv_buffer,
            linux_abi::SO_KEEPALIVE => socket.options.keepalive as i32,
            _ => return libc::EOPNOTSUPP,
        };
        response.payload[..size_of::<i32>()].copy_from_slice(&value.to_ne_bytes());
        response.payload_len = size_of::<i32>() as u32;
        response.value = 0;
        return 0;
    }
    let Some(socket) = state.sockets.get(&request.socket_token) else {
        return libc::EBADF;
    };
    let value = match request.arg2 {
        linux_abi::SO_ERROR => 0_i32,
        linux_abi::SO_TYPE => linux_abi::SOCK_STREAM as i32,
        linux_abi::SO_DOMAIN => linux_abi::AF_UNIX as i32,
        linux_abi::SO_PROTOCOL => 0_i32,
        linux_abi::SO_ACCEPTCONN => {
            if matches!(socket.state, UnixSocketState::Listening { .. }) {
                1
            } else {
                0
            }
        }
        linux_abi::SO_REUSEADDR => socket.options.reuse_addr as i32,
        linux_abi::SO_REUSEPORT => socket.options.reuse_port as i32,
        linux_abi::SO_KEEPALIVE => socket.options.keepalive as i32,
        linux_abi::SO_SNDBUF => socket.options.send_buffer,
        linux_abi::SO_RCVBUF => socket.options.recv_buffer,
        linux_abi::SO_PASSCRED => socket.options.passcred as i32,
        linux_abi::SO_PEERCRED => return peercred_payload_from_socket(socket, response),
        _ => return libc::EOPNOTSUPP,
    };
    response.payload[..size_of::<i32>()].copy_from_slice(&value.to_ne_bytes());
    response.payload_len = size_of::<i32>() as u32;
    response.value = 0;
    0
}

fn handle_setsockopt(request: &NetdIpcRequest) -> i32 {
    if request.arg1 != linux_abi::SOL_SOCKET {
        return libc::EOPNOTSUPP;
    }
    let Some(value) = request_i32_payload(request) else {
        return libc::EINVAL;
    };
    let mut state = net_state().lock().unwrap();
    if let Some(socket) = state.inet_sockets.get_mut(&request.socket_token) {
        match request.arg2 {
            linux_abi::SO_KEEPALIVE => socket.options.keepalive = value != 0,
            linux_abi::SO_SNDBUF => socket.options.send_buffer = clamp_socket_buffer(value),
            linux_abi::SO_RCVBUF => socket.options.recv_buffer = clamp_socket_buffer(value),
            linux_abi::SO_REUSEADDR | linux_abi::SO_REUSEPORT => {}
            _ => return libc::EOPNOTSUPP,
        }
        return 0;
    }
    let Some(socket) = state.sockets.get_mut(&request.socket_token) else {
        return libc::EBADF;
    };
    match request.arg2 {
        linux_abi::SO_REUSEADDR => socket.options.reuse_addr = value != 0,
        linux_abi::SO_REUSEPORT => socket.options.reuse_port = value != 0,
        linux_abi::SO_KEEPALIVE => socket.options.keepalive = value != 0,
        linux_abi::SO_PASSCRED => socket.options.passcred = value != 0,
        linux_abi::SO_SNDBUF => socket.options.send_buffer = clamp_socket_buffer(value),
        linux_abi::SO_RCVBUF => socket.options.recv_buffer = clamp_socket_buffer(value),
        _ => return libc::EOPNOTSUPP,
    }
    0
}

fn handle_shutdown(request: &NetdIpcRequest) -> i32 {
    let mut state = net_state().lock().unwrap();
    if let Some(inet) = state.inet_sockets.get(&request.socket_token) {
        let tcp_handle = inet.tcp;
        let stack = match state.inet_stack() {
            Ok(stack) => stack,
            Err(errno) => return errno,
        };
        let socket = stack.sockets.get_mut::<tcp::Socket>(tcp_handle);
        socket.close();
        stack.poll_budget(8);
        return 0;
    }
    let peer = {
        let Some(socket) = state.sockets.get_mut(&request.socket_token) else {
            return libc::EBADF;
        };
        let UnixSocketState::Connected(connected) = &mut socket.state else {
            return libc::ENOTCONN;
        };
        match request.arg1 {
            linux_abi::SHUT_RD => connected.recv_closed = true,
            linux_abi::SHUT_WR => connected.send_closed = true,
            linux_abi::SHUT_RDWR => {
                connected.recv_closed = true;
                connected.send_closed = true;
            }
            _ => return libc::EINVAL,
        }
        connected.peer
    };
    if let Some(peer_socket) = state.sockets.get_mut(&peer) {
        if let UnixSocketState::Connected(peer_connected) = &mut peer_socket.state {
            match request.arg1 {
                linux_abi::SHUT_RD => peer_connected.peer_read_closed = true,
                linux_abi::SHUT_WR => peer_connected.peer_write_closed = true,
                linux_abi::SHUT_RDWR => {
                    peer_connected.peer_read_closed = true;
                    peer_connected.peer_write_closed = true;
                }
                _ => unreachable!(),
            }
        }
    }
    drop(state);
    0
}

fn send_socket_bytes(request: &NetdIpcRequest, bytes: &[u8]) -> Result<usize, i32> {
    send_socket_message(request, bytes, &[])
}

fn send_socket_message(
    request: &NetdIpcRequest,
    bytes: &[u8],
    control: &[u8],
) -> Result<usize, i32> {
    if bytes.is_empty() {
        return Ok(0);
    }
    let mut state = net_state().lock().unwrap();
    let peer = {
        let socket = state
            .sockets
            .get(&request.socket_token)
            .ok_or(libc::EBADF)?;
        let UnixSocketState::Connected(connected) = &socket.state else {
            return Err(libc::ENOTCONN);
        };
        if connected.send_closed || connected.peer_read_closed || connected.peer_closed {
            return Err(libc::EPIPE);
        }
        connected.peer
    };
    let peer_socket = state.sockets.get_mut(&peer).ok_or(libc::EPIPE)?;
    let UnixSocketState::Connected(peer_connected) = &mut peer_socket.state else {
        return Err(libc::ENOTCONN);
    };
    if peer_connected.recv_closed {
        return Err(libc::EPIPE);
    }
    let room = SOCKET_BUFFER_CAPACITY.saturating_sub(peer_connected.incoming_bytes);
    if room == 0 {
        return Err(libc::EAGAIN);
    }
    if !control.is_empty() && room < bytes.len() {
        return Err(libc::EAGAIN);
    }
    if !control.is_empty()
        && peer_connected
            .incoming_control_bytes
            .saturating_add(control.len())
            > SOCKET_CONTROL_BUFFER_CAPACITY
    {
        return Err(libc::EAGAIN);
    }
    let write_len = room.min(bytes.len());
    peer_connected.incoming_bytes += write_len;
    if !control.is_empty() {
        peer_connected.incoming_control_bytes = peer_connected
            .incoming_control_bytes
            .saturating_add(control.len());
    }
    peer_connected.incoming.push_back(UnixStreamSegment {
        bytes: bytes[..write_len].iter().copied().collect(),
        control: control.to_vec(),
    });
    drop(state);
    Ok(write_len)
}

struct SocketReceive {
    read: usize,
    control: Vec<u8>,
    discarded: Vec<u8>,
}

fn recv_socket_bytes(
    request: &NetdIpcRequest,
    dest: &mut [u8],
    collect_control: bool,
) -> Result<SocketReceive, i32> {
    if dest.is_empty() {
        return Ok(SocketReceive {
            read: 0,
            control: Vec::new(),
            discarded: Vec::new(),
        });
    }
    let mut state = net_state().lock().unwrap();
    let socket = state
        .sockets
        .get_mut(&request.socket_token)
        .ok_or(libc::EBADF)?;
    let UnixSocketState::Connected(connected) = &mut socket.state else {
        return Err(libc::ENOTCONN);
    };
    if connected.incoming_bytes == 0 {
        if connected.peer_closed || connected.peer_write_closed {
            return Ok(SocketReceive {
                read: 0,
                control: Vec::new(),
                discarded: Vec::new(),
            });
        }
        return Err(libc::EAGAIN);
    }
    let mut read = 0usize;
    let mut control = Vec::new();
    let mut discarded = Vec::new();
    while read < dest.len() {
        let Some(segment) = connected.incoming.front_mut() else {
            break;
        };
        if collect_control && !control.is_empty() && !segment.control.is_empty() {
            break;
        }
        if !segment.control.is_empty() {
            let ancillary = core::mem::take(&mut segment.control);
            connected.incoming_control_bytes = connected
                .incoming_control_bytes
                .saturating_sub(ancillary.len());
            if collect_control {
                control = ancillary;
            } else {
                let start = cmsg_align(discarded.len());
                discarded.resize(start, 0);
                discarded.extend_from_slice(ancillary.as_slice());
            }
        }
        let count = (dest.len() - read).min(segment.bytes.len());
        for slot in &mut dest[read..read + count] {
            *slot = segment.bytes.pop_front().unwrap_or_default();
        }
        read += count;
        connected.incoming_bytes = connected.incoming_bytes.saturating_sub(count);
        if segment.bytes.is_empty() {
            connected.incoming.pop_front();
        }
        if count == 0 {
            break;
        }
    }
    connected.recv_drain_handoff_armed = true;
    drop(state);
    Ok(SocketReceive {
        read,
        control,
        discarded,
    })
}

fn pending_recvmsg_control_len(request: &NetdIpcRequest, requested: usize) -> Result<usize, i32> {
    let state = net_state().lock().unwrap();
    let socket = state
        .sockets
        .get(&request.socket_token)
        .ok_or(libc::EBADF)?;
    let UnixSocketState::Connected(connected) = &socket.state else {
        return Err(libc::ENOTCONN);
    };
    let mut remaining = requested;
    let mut queued_len = 0usize;
    for segment in &connected.incoming {
        if remaining == 0 {
            break;
        }
        if !segment.control.is_empty() {
            queued_len = segment.control.len();
            break;
        }
        remaining = remaining.saturating_sub(segment.bytes.len());
    }
    let credentials_len = if socket.options.passcred {
        size_of::<linux_abi::LinuxCmsghdr>() + size_of::<linux_abi::LinuxUCred>()
    } else {
        0
    };
    Ok(cmsg_align(queued_len) + credentials_len)
}

fn append_recvmsg_credentials(
    request: &NetdIpcRequest,
    mut control: Vec<u8>,
) -> Result<Vec<u8>, i32> {
    let state = net_state().lock().unwrap();
    let socket = state
        .sockets
        .get(&request.socket_token)
        .ok_or(libc::EBADF)?;
    let passcred = socket.options.passcred;
    let UnixSocketState::Connected(connected) = &socket.state else {
        return Err(libc::ENOTCONN);
    };
    let peer_credentials = connected.peer_credentials;
    drop(state);
    if !passcred {
        return Ok(control);
    }
    let cmsg_len = size_of::<linux_abi::LinuxCmsghdr>() + size_of::<linux_abi::LinuxUCred>();
    let start = cmsg_align(control.len());
    control.resize(start + cmsg_align(cmsg_len), 0);
    let header = linux_abi::LinuxCmsghdr {
        cmsg_len: cmsg_len as u64,
        cmsg_level: linux_abi::SOL_SOCKET as u32,
        cmsg_type: linux_abi::SCM_CREDENTIALS as u32,
    };
    let credentials = linux_abi::LinuxUCred {
        pid: peer_credentials.pid,
        uid: peer_credentials.uid,
        gid: peer_credentials.gid,
    };
    write_plain_old_data(
        &mut control[start..start + size_of::<linux_abi::LinuxCmsghdr>()],
        &header,
    );
    write_plain_old_data(
        &mut control[start + size_of::<linux_abi::LinuxCmsghdr>()..start + cmsg_len],
        &credentials,
    );
    control.truncate(start + cmsg_len);
    Ok(control)
}

fn validate_sendmsg_control(control: &[u8]) -> Result<(), i32> {
    let mut offset = 0usize;
    while offset + size_of::<linux_abi::LinuxCmsghdr>() <= control.len() {
        let header = read_cmsghdr(&control[offset..])?;
        let cmsg_len = usize::try_from(header.cmsg_len).map_err(|_| libc::EINVAL)?;
        if cmsg_len < size_of::<linux_abi::LinuxCmsghdr>() || offset + cmsg_len > control.len() {
            return Err(libc::EINVAL);
        }
        if header.cmsg_level != linux_abi::SOL_SOCKET as u32 {
            return Err(libc::EOPNOTSUPP);
        }
        match header.cmsg_type {
            value if value == linux_abi::SCM_CREDENTIALS as u32 => {}
            value if value == linux_abi::SCM_RIGHTS as u32 => {}
            _ => return Err(libc::EOPNOTSUPP),
        }
        let aligned_next = offset
            .checked_add(cmsg_align(cmsg_len))
            .ok_or(libc::EINVAL)?;
        let unaligned_next = offset.checked_add(cmsg_len).ok_or(libc::EINVAL)?;
        let next = if aligned_next <= control.len() {
            aligned_next
        } else if unaligned_next == control.len() {
            unaligned_next
        } else {
            return Err(libc::EINVAL);
        };
        if next <= offset {
            return Err(libc::EINVAL);
        }
        offset = next;
    }
    if offset != control.len() {
        return Err(libc::EINVAL);
    }
    Ok(())
}

fn read_cmsghdr(bytes: &[u8]) -> Result<linux_abi::LinuxCmsghdr, i32> {
    if bytes.len() < size_of::<linux_abi::LinuxCmsghdr>() {
        return Err(libc::EINVAL);
    }
    Ok(linux_abi::LinuxCmsghdr {
        cmsg_len: u64::from_ne_bytes(bytes[0..8].try_into().map_err(|_| libc::EINVAL)?),
        cmsg_level: u32::from_ne_bytes(bytes[8..12].try_into().map_err(|_| libc::EINVAL)?),
        cmsg_type: u32::from_ne_bytes(bytes[12..16].try_into().map_err(|_| libc::EINVAL)?),
    })
}

fn write_plain_old_data<T: Copy>(dest: &mut [u8], value: &T) {
    let bytes =
        unsafe { std::slice::from_raw_parts((value as *const T).cast::<u8>(), size_of::<T>()) };
    dest.copy_from_slice(bytes);
}

fn cmsg_align(len: usize) -> usize {
    let align = size_of::<usize>();
    (len + align - 1) & !(align - 1)
}

/// Publishes a newly-created AF_INET stream socket only through the reply that
/// requested it.  Netd keeps the initial `refs = 1` until this succeeds; on a
/// rejection the broker has removed its invisible descriptor and this service
/// performs the single provider-side discard.
fn prepare_inet_socket_publication(
    request: &NetdIpcRequest,
    response: &mut NetdIpcResponse,
    reply_cap: u64,
    socket_token: u64,
) -> i32 {
    if reply_cap == 0 || socket_token == 0 {
        discard_unpublished_socket_tokens(socket_token, 0, 0);
        return libc::EINVAL;
    }
    let publication = NetBrokerPrepareSocketPublication {
        version: NET_BROKER_SOCKET_PUBLICATION_VERSION,
        reply_cap,
        caller_pid: request.pid,
        socket_token,
        domain: request.arg0,
        socket_type: request.arg1,
        protocol: request.arg2,
        ..NetBrokerPrepareSocketPublication::default()
    };
    let args = RustosNetBrokerArgs {
        process_id: request.pid,
        op: NET_BROKER_OP_PREPARE_SOCKET_PUBLICATION,
        reserved0: 0,
        reserved1: 0,
        arg0: (&publication as *const NetBrokerPrepareSocketPublication) as u64,
        arg1: 0,
        arg2: 0,
        arg3: 0,
        arg4: 0,
        arg5: 0,
    };
    let prepared = settle_inet_socket_publication(
        socket_token,
        || {
            let result = syscall1(
                SYS_RUSTOS_NET_BROKER,
                (&args as *const RustosNetBrokerArgs) as u64,
            );
            if result < 0 {
                Err(last_errno())
            } else {
                Ok(())
            }
        },
        |token| discard_unpublished_socket_tokens(token, 0, 0),
    );
    if let Err(errno) = prepared {
        return errno;
    }
    // The compat receiver verifies this token against the one transferred by
    // the kernel, then overwrites it with the fd it installed after a timely
    // reply take.  No numeric fd crosses this broker boundary.
    response.value = socket_token;
    0
}

/// Settles ownership after a prepared-reply bind attempt.  The prepare closure
/// owns no provider cleanup; its failure is the one point at which netd still
/// owns and must discard the unpublished initial reference.
fn settle_inet_socket_publication(
    socket_token: u64,
    prepare: impl FnOnce() -> Result<(), i32>,
    discard: impl FnOnce(u64),
) -> Result<(), i32> {
    match prepare() {
        Ok(()) => Ok(()),
        Err(errno) => {
            // LIFECYCLE: The kernel reclaims only the non-visible descriptor
            // on a bind failure.  Exactly this transition closes netd's
            // original `refs = 1` provider ownership.
            discard(socket_token);
            Err(errno)
        }
    }
}

fn call_net_broker(
    request: &NetdIpcRequest,
    response: &mut NetdIpcResponse,
    socket_token: u64,
    token_a: u64,
    token_b: u64,
    discard_tokens_on_failure: bool,
) -> i32 {
    let args = RustosNetBrokerArgs {
        process_id: request.pid,
        op: request.op,
        reserved0: 0,
        reserved1: 0,
        arg0: request.arg0,
        arg1: request.arg1,
        arg2: request.arg2,
        arg3: if socket_token != 0 {
            socket_token
        } else {
            request.arg3
        },
        arg4: token_a,
        arg5: token_b,
    };
    let result = syscall1(
        SYS_RUSTOS_NET_BROKER,
        (&args as *const RustosNetBrokerArgs) as u64,
    );
    if result < 0 {
        let errno = last_errno();
        if discard_tokens_on_failure {
            discard_unpublished_socket_tokens(socket_token, token_a, token_b);
        }
        return errno;
    }
    response.value = result as u64;
    0
}

fn discard_unpublished_socket_tokens(socket_token: u64, token_a: u64, token_b: u64) {
    let tokens = [socket_token, token_a, token_b];
    for (index, token) in tokens.iter().copied().enumerate() {
        if token == 0 || tokens[..index].contains(&token) {
            continue;
        }
        let request = NetdIpcRequest {
            socket_token: token,
            ..NetdIpcRequest::default()
        };
        let _ = handle_close(&request, &mut NetdIpcResponse::default());
    }
}

fn request_msg_flags(request: &NetdIpcRequest) -> u64 {
    match request.op {
        SYSCALL_OFFLOAD_OP_LINUX_SENDMSG | SYSCALL_OFFLOAD_OP_LINUX_RECVMSG => request.arg2,
        _ => request.arg3,
    }
}

fn request_credentials(request: &NetdIpcRequest) -> Credentials {
    Credentials {
        pid: request.pid as i32,
        uid: request.euid,
        gid: request.egid,
    }
}

fn inet_socket_exists(token: u64) -> bool {
    net_state()
        .lock()
        .unwrap()
        .inet_sockets
        .contains_key(&token)
}

fn sockaddr_family(request: &NetdIpcRequest) -> Option<u64> {
    if request.payload_len as usize >= size_of::<u16>() {
        Some(u16::from_ne_bytes([request.payload[0], request.payload[1]]) as u64)
    } else {
        None
    }
}

fn sockaddr_in_from_payload(request: &NetdIpcRequest) -> Result<(Ipv4Address, u16), i32> {
    let len = request.payload_len as usize;
    if len < size_of::<linux_abi::LinuxSockaddrIn>() {
        return Err(libc::EINVAL);
    }
    if sockaddr_family(request) != Some(linux_abi::AF_INET) {
        return Err(libc::EAFNOSUPPORT);
    }
    let port = u16::from_be_bytes([request.payload[2], request.payload[3]]);
    if port == 0 {
        return Err(libc::EINVAL);
    }
    let addr = Ipv4Address::new(
        request.payload[4],
        request.payload[5],
        request.payload[6],
        request.payload[7],
    );
    if addr.is_unspecified() {
        return Err(libc::EINVAL);
    }
    Ok((addr, port))
}

fn packet_status() -> Result<u64, i32> {
    call_packet_broker(NET_BROKER_OP_PACKET_STATUS, 0, 0)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum PacketProviderState {
    Unavailable,
    AwaitingAuthenticatedControl,
    Active,
}

fn packet_provider_state() -> Result<PacketProviderState, i32> {
    packet_provider_state_from_wire(packet_status()?)
}

fn packet_provider_state_from_wire(value: u64) -> Result<PacketProviderState, i32> {
    match value {
        NET_BROKER_PACKET_STATUS_UNAVAILABLE => Ok(PacketProviderState::Unavailable),
        NET_BROKER_PACKET_STATUS_AWAITING_AUTHENTICATED_CONTROL => {
            Ok(PacketProviderState::AwaitingAuthenticatedControl)
        }
        NET_BROKER_PACKET_STATUS_ACTIVE => Ok(PacketProviderState::Active),
        _ => Err(libc::EPROTO),
    }
}

fn await_authenticated_packet_provider_with<ProviderState, Now, LifecycleRemaining, Sleep>(
    request: &NetdIpcRequest,
    mut provider_state: ProviderState,
    mut now_ns: Now,
    mut lifecycle_remaining: LifecycleRemaining,
    mut sleep: Sleep,
) -> Result<(), i32>
where
    ProviderState: FnMut() -> Result<PacketProviderState, i32>,
    Now: FnMut() -> u64,
    LifecycleRemaining: FnMut() -> Option<Duration>,
    Sleep: FnMut(Duration),
{
    let mut logged_wait = false;
    loop {
        // Check the request and lifecycle before every status transaction.
        // In particular an already expired request cannot issue a status
        // query and then publish a socket from a stale successful result.
        request_deadline_remaining_ns_at(request, now_ns())?;
        if lifecycle_remaining()
            .filter(|remaining| !remaining.is_zero())
            .is_none()
        {
            debug_line("netd: authenticated DVM network control timed out");
            return Err(libc::ENODEV);
        }

        match provider_state()? {
            PacketProviderState::Active => {
                // A status result is not a publication permit by itself: the
                // request may have expired while the broker handled the query.
                request_deadline_remaining_ns_at(request, now_ns())?;
                if lifecycle_remaining()
                    .filter(|remaining| !remaining.is_zero())
                    .is_none()
                {
                    debug_line("netd: authenticated DVM network control timed out");
                    return Err(libc::ENODEV);
                }
                if logged_wait {
                    debug_line("netd: authenticated DVM network control active");
                }
                return Ok(());
            }
            PacketProviderState::Unavailable => {
                debug_line("netd: DVM network transport unavailable");
                return Err(libc::ENODEV);
            }
            PacketProviderState::AwaitingAuthenticatedControl => {
                if !logged_wait {
                    debug_line("netd: waiting for authenticated DVM network control");
                    logged_wait = true;
                }
            }
        }

        // The sleep is bounded simultaneously by the request's exact
        // monotonic end, the four-millisecond retry cadence, and the parent's
        // fresh five-second provider-lifecycle timer.
        let request_wait = capped_request_wait_at(request, now_ns(), AUTHENTICATED_CONTROL_RETRY)?;
        let lifecycle_left = lifecycle_remaining()
            .filter(|remaining| !remaining.is_zero())
            .ok_or_else(|| {
                debug_line("netd: authenticated DVM network control timed out");
                libc::ENODEV
            })?;
        sleep(request_wait.min(lifecycle_left));
    }
}

fn await_authenticated_packet_provider(request: &NetdIpcRequest) -> Result<(), i32> {
    let deadline = StdInstant::now() + AUTHENTICATED_CONTROL_WAIT;
    await_authenticated_packet_provider_with(
        request,
        packet_provider_state,
        rustos_svc_runtime::syscall::monotonic_nanos,
        || deadline.checked_duration_since(StdInstant::now()),
        thread::sleep,
    )
}

#[cfg(test)]
#[path = "tests/packet_provider_state_tests.rs"]
mod packet_provider_state_tests;

#[cfg(test)]
#[path = "tests/local_socket_poll_tests.rs"]
mod local_socket_poll_tests;

fn packet_tx(frame: &[u8]) -> Result<usize, i32> {
    call_packet_broker(
        NET_BROKER_OP_PACKET_TX,
        frame.as_ptr() as u64,
        frame.len() as u64,
    )
    .map(|value| value as usize)
}

fn packet_rx(frame: &mut [u8]) -> Result<usize, i32> {
    call_packet_broker(
        NET_BROKER_OP_PACKET_RX,
        frame.as_mut_ptr() as u64,
        frame.len() as u64,
    )
    .map(|value| value as usize)
}

fn call_packet_broker(op: u16, arg0: u64, arg1: u64) -> Result<u64, i32> {
    let args = RustosNetBrokerArgs {
        process_id: 1,
        op,
        reserved0: 0,
        reserved1: 0,
        arg0,
        arg1,
        arg2: 0,
        arg3: 0,
        arg4: 0,
        arg5: 0,
    };
    let result = syscall1(
        SYS_RUSTOS_NET_BROKER,
        (&args as *const RustosNetBrokerArgs) as u64,
    );
    if result < 0 {
        Err(last_errno())
    } else {
        Ok(result as u64)
    }
}

fn sockaddr_path_from_payload(request: &NetdIpcRequest) -> Result<String, i32> {
    let len = request.payload_len as usize;
    if len < size_of::<u16>() {
        return Err(libc::EINVAL);
    }
    let family = u16::from_ne_bytes([request.payload[0], request.payload[1]]) as u64;
    if family != linux_abi::AF_UNIX {
        return Err(libc::EAFNOSUPPORT);
    }
    let path_bytes = &request.payload[size_of::<u16>()..len];
    if path_bytes.first().copied() == Some(0) {
        return Err(libc::EINVAL);
    }
    let end = path_bytes
        .iter()
        .position(|byte| *byte == 0)
        .unwrap_or(path_bytes.len());
    if end == 0 || end >= linux_abi::UNIX_PATH_MAX {
        return Err(libc::EINVAL);
    }
    String::from_utf8(path_bytes[..end].to_vec()).map_err(|_| libc::EINVAL)
}

fn sockaddr_payload(path: &str, response: &mut NetdIpcResponse) -> Result<i32, i32> {
    let needed = size_of::<u16>()
        .checked_add(path.len())
        .and_then(|value| value.checked_add(1))
        .ok_or(libc::EINVAL)?;
    if needed > response.payload.len() || path.len() >= linux_abi::UNIX_PATH_MAX {
        return Err(libc::EINVAL);
    }
    response.payload[..2].copy_from_slice(&(linux_abi::AF_UNIX as u16).to_ne_bytes());
    response.payload[2..2 + path.len()].copy_from_slice(path.as_bytes());
    response.payload[2 + path.len()] = 0;
    response.payload_len = needed as u32;
    response.value = 0;
    Ok(0)
}

fn peercred_payload_from_socket(socket: &UnixSocket, response: &mut NetdIpcResponse) -> i32 {
    let UnixSocketState::Connected(connected) = &socket.state else {
        return libc::ENOTCONN;
    };
    let value = linux_abi::LinuxUCred {
        pid: connected.peer_credentials.pid,
        uid: connected.peer_credentials.uid,
        gid: connected.peer_credentials.gid,
    };
    let bytes = unsafe {
        std::slice::from_raw_parts(
            (&value as *const linux_abi::LinuxUCred).cast::<u8>(),
            size_of::<linux_abi::LinuxUCred>(),
        )
    };
    response.payload[..bytes.len()].copy_from_slice(bytes);
    response.payload_len = bytes.len() as u32;
    response.value = 0;
    0
}

fn request_i32_payload(request: &NetdIpcRequest) -> Option<i32> {
    if (request.payload_len as usize) < size_of::<i32>() {
        return None;
    }
    Some(i32::from_ne_bytes(request.payload[..4].try_into().ok()?))
}

fn clamp_socket_buffer(value: i32) -> i32 {
    value.clamp(4096, SOCKET_BUFFER_CAPACITY as i32)
}

fn syscall0(number: u64) -> i64 {
    unsafe { libc::syscall(number as libc::c_long) as i64 }
}

fn syscall1(number: u64, arg0: u64) -> i64 {
    unsafe { libc::syscall(number as libc::c_long, arg0) as i64 }
}

fn syscall2(number: u64, arg0: u64, arg1: u64) -> i64 {
    unsafe { libc::syscall(number as libc::c_long, arg0, arg1) as i64 }
}

fn syscall3(number: u64, arg0: u64, arg1: u64, arg2: u64) -> i64 {
    unsafe { libc::syscall(number as libc::c_long, arg0, arg1, arg2) as i64 }
}

fn syscall6(number: u64, arg0: u64, arg1: u64, arg2: u64, arg3: u64, arg4: u64, arg5: u64) -> i64 {
    unsafe { libc::syscall(number as libc::c_long, arg0, arg1, arg2, arg3, arg4, arg5) as i64 }
}

fn last_errno() -> i32 {
    std::io::Error::last_os_error()
        .raw_os_error()
        .unwrap_or(libc::EIO)
}

fn debug_line(message: &str) {
    let bytes = message.as_bytes();
    let len = bytes.len().min(1023);
    let mut line = [0_u8; 1024];
    line[..len].copy_from_slice(&bytes[..len]);
    line[len] = b'\n';
    let _ = syscall2(
        SYS_RUSTOS_DEBUG_PRINT,
        line.as_ptr() as u64,
        (len + 1) as u64,
    );
}

#[cfg(test)]
mod idle_cadence_tests {
    use super::{INET_IDLE_POLL_INTERVAL, INET_READINESS_POLL_INTERVAL};

    #[test]
    fn an_idle_stack_backs_off_without_relaxing_the_live_socket_cadence() {
        // A live socket keeps the timer/retransmit cadence it had.
        assert_eq!(
            INET_READINESS_POLL_INTERVAL,
            std::time::Duration::from_millis(10)
        );
        // And the idle cadence has to be a real back-off, or the global net
        // lock is still being taken at the same rate for no work.
        assert!(INET_IDLE_POLL_INTERVAL >= INET_READINESS_POLL_INTERVAL * 4);
    }
}
