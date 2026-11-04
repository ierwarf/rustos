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
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Condvar, LockResult, Mutex, MutexGuard, OnceLock};
use std::thread;
use std::time::{Duration, Instant as StdInstant};

mod readiness;
use readiness::{NetStateGuard, SocketTable};

use rustos_user_abi::linux as linux_abi;
use rustos_user_abi::syscall::{
    COMMERCIAL_MAX_NETD_OP_ADDRESS_BIND, COMMERCIAL_MAX_NETD_OP_FD_TRANSFER,
    COMMERCIAL_MAX_NETD_OP_PACKET_LEASE, COMMERCIAL_MAX_NETD_OP_ROUTE_POLICY,
    COMMERCIAL_MAX_NETD_OP_SOCKET_NAMESPACE, COMMERCIAL_MAX_NETD_OP_SOCKET_OPTIONS,
    COMMERCIAL_MAX_PROTOCOL_ABI_VERSION, COMMERCIAL_MAX_PROTOCOL_MAX_DESCRIPTORS,
    COMMERCIAL_MAX_PROTOCOL_NETD, CommercialMaxCapabilityLeaseWire,
    CommercialMaxProtocolDescriptorWire, CommercialMaxProtocolRequest,
    CommercialMaxProtocolResponse, IPC_SERVICE_INPUTD, IPC_SERVICE_NETD, IpcRecvWithSenderArgs,
    NET_BROKER_OP_PACKET_LEASE_GRANT, NET_BROKER_OP_PACKET_LEASE_RESET,
    NET_BROKER_OP_PACKET_LEASE_REVOKE, NET_BROKER_OP_PACKET_RX, NET_BROKER_OP_PACKET_STATUS,
    NET_BROKER_OP_PACKET_TX, NET_BROKER_OP_PREPARE_SOCKET_PUBLICATION, NET_BROKER_PACKET_MTU,
    NET_BROKER_PACKET_STATUS_ACTIVE, NET_BROKER_PACKET_STATUS_AWAITING_AUTHENTICATED_CONTROL,
    NET_BROKER_PACKET_STATUS_UNAVAILABLE, NET_BROKER_SOCKET_PUBLICATION_VERSION,
    NETD_DVM_SESSION_GRANT, NETD_DVM_SESSION_REVOKE, NETD_IPC_ABI_VERSION, NETD_IPC_OP_DVM_SESSION,
    NETD_IPC_OP_POLL_BATCH, NETD_IPC_OP_REF_ACK, NETD_IPC_REQUEST_HEADER_SIZE, NETD_IPC_RESPONSE_FLAG_LATENCY_HANDOFF,
    NETD_IPC_RESPONSE_HEADER_SIZE, NETD_POLL_MODE_QUERY, NETD_POLL_MODE_WAIT,
    NETD_POLL_BATCH_MAX, NETD_RECVMSG_PAYLOAD_HEADER_SIZE, NETD_SENDMSG_PAYLOAD_HEADER_SIZE,
    NetBrokerPrepareSocketPublication, NetdIpcRequest, NetdIpcResponse, NetdPollQueryWire, NetdPollResultWire, RustosNetBrokerArgs,
    SYS_RUSTOS_IPC_RECV_WITH_SENDER,
    SYS_RUSTOS_IPC_RECV_WITH_SENDER_BOUNDED, SYS_RUSTOS_IPC_REPLY, SYS_RUSTOS_NET_BROKER,
    SYSCALL_OFFLOAD_OP_LINUX_ACCEPT, SYSCALL_OFFLOAD_OP_LINUX_BIND, SYSCALL_OFFLOAD_OP_LINUX_CLOSE,
    SYSCALL_OFFLOAD_OP_LINUX_CONNECT, SYSCALL_OFFLOAD_OP_LINUX_DUP,
    SYSCALL_OFFLOAD_OP_LINUX_GETPEERNAME, SYSCALL_OFFLOAD_OP_LINUX_GETSOCKNAME,
    SYSCALL_OFFLOAD_OP_LINUX_GETSOCKOPT, SYSCALL_OFFLOAD_OP_LINUX_LISTEN,
    SYSCALL_OFFLOAD_OP_LINUX_POLL_SOCKET, SYSCALL_OFFLOAD_OP_LINUX_RECVFROM,
    SYSCALL_OFFLOAD_OP_LINUX_RECVMSG, SYSCALL_OFFLOAD_OP_LINUX_SENDMSG,
    SYSCALL_OFFLOAD_OP_LINUX_SENDTO, SYSCALL_OFFLOAD_OP_LINUX_SETSOCKOPT,
    SYSCALL_OFFLOAD_OP_LINUX_SHUTDOWN, SYSCALL_OFFLOAD_OP_LINUX_SOCKET,
    SYSCALL_OFFLOAD_OP_LINUX_SOCKETPAIR,
};
#[cfg(not(test))]
use rustos_user_abi::syscall::{
    SYS_RUSTOS_ENTROPY_BROKER, SYS_RUSTOS_WAITSET_SIGNAL_BROKER, WAITSET_ABI_VERSION,
    WAITSET_PROVIDER_NETD, WaitSetSignalBrokerArgs,
};
use smoltcp::iface::{
    Config as SmolConfig, Interface, PollIngressSingleResult, PollResult, SocketHandle, SocketSet,
};
use smoltcp::phy::{Device, DeviceCapabilities, Medium, RxToken, TxToken};
use smoltcp::socket::{tcp, udp, icmp};
use smoltcp::time::Instant as SmolInstant;
use smoltcp::wire::{EthernetAddress, HardwareAddress, IpAddress, IpCidr, IpEndpoint, Ipv4Address};

mod inet_datagram;

mod commercial_protocol;
mod provider_accounting;
mod blocking;
use blocking::*;
mod deadline;
use deadline::*;
mod dispatch;
use dispatch::*;
mod inet_stack;
use inet_stack::*;
mod inet_tcp;
use inet_tcp::*;
mod local_poll;
use local_poll::*;
mod packet_provider;
use packet_provider::*;
mod poll;
use poll::*;
mod ref_replay;
use ref_replay::*;
mod service_loop;
use service_loop::*;
mod socket_info;
use socket_info::*;
mod socket_lifecycle;
use socket_lifecycle::*;
mod sys;
use sys::*;
mod unix_stream;
use unix_stream::*;
use commercial_protocol::{dispatch_commercial_request, validate_commercial_request};

const RECV_BACKOFF: Duration = Duration::from_millis(1);
static REPLY_FAILURE_DIAGNOSTICS: rustos_svc_runtime::ipc::ReplyFailureDiagnostics =
    rustos_svc_runtime::ipc::ReplyFailureDiagnostics::new();
/// Replies the kernel rejected with `EINVAL`: a well-formed netd response (its
/// length is validated before the syscall) is refused only when the one-shot
/// reply capability is no longer valid, i.e. the caller timed out or exited
/// after its request was delivered. That is an expected race, not a netd
/// fault, so it is counted apart from genuine reply failures.
static ABANDONED_REPLY_DIAGNOSTICS: rustos_svc_runtime::ipc::ReplyFailureDiagnostics =
    rustos_svc_runtime::ipc::ReplyFailureDiagnostics::new();
/// Fixed endpoint front-end pool. Short local-socket requests from independent
/// clients must not queue behind one receiver, while shared protocol state
/// remains serialized by `NetState` and blocking INET work stays in its
/// separate bounded pool.
const NETD_REQUEST_WORKERS: usize = 4;
const BLOCKING_WORKER_COUNT: usize = 8;
const MAX_PENDING_BLOCKING_REQUESTS: usize = 32;
/// Dedicated pool for `socket(AF_INET)` calls that must wait for the DVM
/// control relay. It is separate from the blocking-I/O pool on purpose: blocking
/// receives can hold all of those workers for their whole (up to 30 s)
/// deadline, and socket creation must not queue behind them.
const PROVIDER_WAIT_WORKER_COUNT: usize = 2;
const MAX_PENDING_PROVIDER_WAITS: usize = 16;
static PENDING_PROVIDER_WAITS: AtomicUsize = AtomicUsize::new(0);
static ACTIVE_PROVIDER_WAIT_WORKERS: AtomicUsize = AtomicUsize::new(0);
const REF_REPLAY_CAPACITY: usize = 4096;
/// A completed reference result is kept for replay at least this long even
/// when the log is full. It is far above the caller's 5 s state-mutation lease
/// plus its bounded retries, so only results nobody can still retry (the
/// caller crashed or abandoned the operation without sending `REF_ACK`) are
/// ever reclaimed. Without reclamation those leak until every later dup/close
/// fails with `ENOSPC`.
const REF_REPLAY_MIN_RETENTION: Duration = Duration::from_secs(60);
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
    /// Admission time, used only to decide when a never-acknowledged result may
    /// be reclaimed.
    admitted: StdInstant,
}

const MAX_LISTEN_BACKLOG: usize = 128;
/// Table-wide socket ceilings. Per-process Job billing bounds one client, but
/// netd's heap is shared by every client: each AF_UNIX socket may queue
/// `SOCKET_BUFFER_CAPACITY` bytes and each TCP socket pins two
/// `INET_TCP_BUFFER_CAPACITY` rings, so an unbounded table lets one service
/// bug exhaust memory and abort all networking. Exceeding a ceiling reports
/// `ENFILE`, the system-wide analogue of `EMFILE`.
const MAX_UNIX_SOCKETS: usize = 8192;
const MAX_INET_SOCKETS: usize = 1024;
const SOCKET_BUFFER_CAPACITY: usize = 1024 * 1024;
// Every queued ancillary byte must fit in one kernel-consumed release trailer
// when read/close discards it. This is a lifecycle bound, not a tuning knob.
const SOCKET_CONTROL_BUFFER_CAPACITY: usize = 16 * 1024;
const INET_TCP_BUFFER_CAPACITY: usize = 16 * 1024;
const INET_CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const INET_IO_POLL_BUDGET: usize = 256;
/// Bound on how long a closed-by-owner connection may linger to finish its FIN
/// exchange, and on how many may do so at once (each pins 32 KiB of buffers).
const INET_CLOSE_LINGER: Duration = Duration::from_secs(5);
const INET_CLOSING_CAPACITY: usize = 64;
const BLOCKING_RETRY_BACKOFF: Duration = Duration::from_millis(1);
/// Retry delay doubles per unsuccessful attempt up to this many doublings
/// (1, 2, 4 ms). Up to `BLOCKING_WORKER_COUNT` workers retry against the same
/// global state lock and packet-broker syscall; a flat 1 ms cadence made eight
/// idle blocking waiters a steady load on a single CPU.
const BLOCKING_RETRY_MAX_DOUBLINGS: u32 = 2;

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
/// Upper bound on how long a timer-only smoltcp socket transition may go
/// unobserved by the readiness table. Twice the worker cadence, so a silent
/// transition is seen by at most the second worker tick after it happens.
const INET_TIMER_SWEEP_INTERVAL: Duration = Duration::from_millis(20);
const QEMU_USERNET_ADDR: Ipv4Address = Ipv4Address::new(10, 0, 2, 15);
const QEMU_USERNET_GATEWAY: Ipv4Address = Ipv4Address::new(10, 0, 2, 2);
const QEMU_USERNET_MAC: EthernetAddress = EthernetAddress([0x52, 0x54, 0x00, 0x12, 0x34, 0x56]);

fn main() {
    run();
}

mod receive_turn;
use receive_turn::{
    ReceiveTurn, ReceivedNetdRequest, dispatch_admitted_request, service_received_request_with,
};

#[cfg(test)]
#[path = "tests/receive_custody_tests.rs"]
mod receive_custody_tests;

struct BlockingRequest {
    request: Box<NetdIpcRequest>,
    reply_cap: u64,
}

struct BlockingRequestQueue {
    requests: Mutex<VecDeque<BlockingRequest>>,
    available: Condvar,
}

struct DeferredLocalPoll {
    request: Box<NetdIpcRequest>,
    reply_cap: u64,
}

/// Ceiling on one bounded receive, so a far-future waiter deadline cannot be
/// handed to the kernel as an out-of-range budget.
const NETD_MAX_IDLE_WAIT_MS: u64 =
    rustos_user_abi::performance::IPC_INTERACTIVE_CONTROL_HARD_LIMIT_MS;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum DvmSessionTransition {
    Grant,
    Revoke,
}

#[cfg(test)]
mod dvm_session_policy_tests;

enum RefReplayAction {
    Execute,
    Replay(i32),
}

#[cfg(test)]
#[path = "tests/ref_replay_tests.rs"]
mod ref_replay_tests;

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
    /// Aligned wire charge, including padding needed when controls are joined.
    incoming_control_bytes: usize,
    /// Listener custody survives queue removal until accept publication settles.
    pending_listener: Option<u64>,
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
    transport: InetTransport,
    local: Option<IpEndpoint>,
    peer: Option<IpEndpoint>,
    received: Option<inet_datagram::ReceivedPacket>,
    errors: Option<inet_datagram::ErrorState>,
    /// Armed while an active open is in flight. smoltcp reports both an RST
    /// and its own connect timeout as a closed socket; the deadline tells
    /// them apart (`ETIMEDOUT` vs `ECONNREFUSED`) and `SO_ERROR` consumes it.
    connect_deadline: Option<StdInstant>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum InetTransport {
    Tcp(SocketHandle),
    Udp(SocketHandle),
    Icmp(SocketHandle),
}

impl InetTransport {
    fn is_datagram(self) -> bool { !matches!(self, Self::Tcp(_)) }
    fn handle(self) -> SocketHandle {
        match self { Self::Tcp(h) | Self::Udp(h) | Self::Icmp(h) => h }
    }
    fn socket_type(self) -> i32 {
        if self.is_datagram() { libc::SOCK_DGRAM } else { libc::SOCK_STREAM }
    }
    fn protocol(self) -> i32 {
        match self { Self::Tcp(_) => libc::IPPROTO_TCP, Self::Udp(_) => libc::IPPROTO_UDP, Self::Icmp(_) => libc::IPPROTO_ICMP }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct InetReadiness {
    can_recv: bool,
    can_send: bool,
    open: bool,
    /// The peer's FIN was received: reads return EOF and `POLLRDHUP` is set.
    rx_closed: bool,
    error: bool,
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
    sockets: SocketTable<UnixSocket>,
    inet_sockets: SocketTable<InetSocket>,
    bindings: BTreeMap<String, u64>,
    inet: Option<InetStack>,
    /// Per-socket readiness generation: the newest value published for that
    /// socket. Entries exist only for live sockets, so the map is bounded by
    /// the socket tables; a missing entry reads as generation 1.
    readiness_generations: BTreeMap<u64, u64>,
    readiness_observations: BTreeMap<u64, readiness::Readiness>,
    readiness_publications: Vec<(u64, u64)>,
}

impl NetState {
    fn new() -> Self {
        Self {
            sockets: SocketTable::new(),
            inet_sockets: SocketTable::new(),
            bindings: BTreeMap::new(),
            inet: None,
            readiness_generations: BTreeMap::new(),
            readiness_observations: BTreeMap::new(),
            readiness_publications: Vec::new(),
        }
    }

    fn socket_exists(&self, token: u64) -> bool {
        self.sockets.contains_key(&token) || self.inet_sockets.contains_key(&token)
    }

    fn socket_readiness_generation(&mut self, token: u64) -> u64 {
        // Poll responses must pair readiness with its stamped generation, not
        // wait for the guard's eventual drop to publish the mutation.
        self.record_readiness_changes();
        self.readiness_generations.get(&token).copied().unwrap_or(1)
    }

    /// Whether `additional` more AF_UNIX identities fit under the table ceiling.
    fn unix_capacity_available(&self, additional: usize) -> bool {
        self.sockets
            .len()
            .checked_add(additional)
            .is_some_and(|total| total <= MAX_UNIX_SOCKETS)
    }

    fn inet_capacity_available(&self) -> bool {
        self.inet_sockets.len() < MAX_INET_SOCKETS
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

}

/// Locks the global network state. The release profile aborts on panic, so a
/// poisoned lock can only be observed after unwinding (tests); a half-updated
/// socket table must not be served, so fail the process (the supervisor
/// restarts it) instead of letting every later handler unwrap-panic.
fn lock_net_state() -> NetStateGuard<'static> {
    match net_state().lock() {
        Ok(guard) => NetStateGuard::new(guard),
        Err(_) => {
            eprintln!("netd: state lock poisoned; exiting");
            std::process::exit(101)
        }
    }
}

fn net_state() -> &'static Mutex<NetState> {
    static STATE: OnceLock<Mutex<NetState>> = OnceLock::new();
    STATE.get_or_init(|| Mutex::new(NetState::new()))
}

struct SocketReceive {
    read: usize,
    control: Vec<u8>,
    discarded: Vec<u8>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum PacketProviderState {
    Unavailable,
    AwaitingAuthenticatedControl,
    Active,
}

#[cfg(test)]
#[path = "tests/packet_provider_state_tests.rs"]
mod packet_provider_state_tests;

#[cfg(test)]
#[path = "tests/local_socket_poll_tests.rs"]
mod local_socket_poll_tests;

#[cfg(test)]
#[path = "tests/hardening_tests.rs"]
mod hardening_tests;

#[cfg(test)]
#[path = "tests/unix_socket_lifecycle_tests.rs"]
mod unix_socket_lifecycle_tests;

#[cfg(test)]
mod idle_cadence_tests {
    use super::{
        INET_IDLE_POLL_INTERVAL, INET_READINESS_POLL_INTERVAL, NETD_MAX_IDLE_WAIT_MS,
        local_poll_budget_ms_for_deadline,
    };

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

    #[test]
    fn deferred_poll_deadline_always_wakes_the_request_owner() {
        assert_eq!(local_poll_budget_ms_for_deadline(10_000_000, 10_000_000), 1);
        assert_eq!(local_poll_budget_ms_for_deadline(9_000_000, 10_000_000), 1);
        assert_eq!(local_poll_budget_ms_for_deadline(12_500_000, 10_000_000), 3);
        assert_eq!(
            local_poll_budget_ms_for_deadline(u64::MAX, 0),
            NETD_MAX_IDLE_WAIT_MS
        );
        let source = include_str!("local_poll.rs");
        assert!(
            source
                .lines()
                .any(|line| line.trim()
                    == "Some(local_poll_budget_ms_for_deadline(earliest, now_ns))")
        );
    }

    #[test]
    fn ephemeral_port_selection_wraps_and_rejects_exhaustion() {
        let mut next = u16::MAX;
        assert_eq!(crate::select_ephemeral_port(&mut next, |port| port == u16::MAX), Some(49152));
        assert_eq!(next, 49153);
        let mut visited = 0;
        assert_eq!(crate::select_ephemeral_port(&mut next, |_| { visited += 1; true }), None);
        assert_eq!(visited, 16384);
        assert_eq!(next, 49153);
    }

    #[test]
    fn sendmsg_control_rejects_overflowing_lengths() {
        let mut control = vec![0; std::mem::size_of::<rustos_user_abi::linux::LinuxCmsghdr>()];
        control[..8].copy_from_slice(&u64::MAX.to_ne_bytes());
        assert_eq!(crate::validate_sendmsg_control(&control), Err(libc::EINVAL));

        let mut two_headers = vec![0; control.len() * 2];
        let header_len = control.len();
        two_headers[..8].copy_from_slice(&(header_len as u64).to_ne_bytes());
        two_headers[8..12].copy_from_slice(&(rustos_user_abi::linux::SOL_SOCKET as u32).to_ne_bytes());
        two_headers[12..16].copy_from_slice(&(rustos_user_abi::linux::SCM_RIGHTS as u32).to_ne_bytes());
        two_headers[header_len..header_len + 8].copy_from_slice(&u64::MAX.to_ne_bytes());
        assert_eq!(crate::validate_sendmsg_control(&two_headers), Err(libc::EINVAL));
    }

    #[test]
    fn sendmsg_sections_are_bounded_by_the_received_payload() {
        use crate::{NETD_SENDMSG_PAYLOAD_HEADER_SIZE, NetdIpcRequest, split_sendmsg_payload};
        let mut request = NetdIpcRequest::default();
        request.payload[0..4].copy_from_slice(&3_u32.to_ne_bytes());
        request.payload[4..8].copy_from_slice(&2_u32.to_ne_bytes());
        request.payload_len = (NETD_SENDMSG_PAYLOAD_HEADER_SIZE + 5) as u32;
        let (data, control) = split_sendmsg_payload(&request).unwrap();
        assert_eq!(data, NETD_SENDMSG_PAYLOAD_HEADER_SIZE..NETD_SENDMSG_PAYLOAD_HEADER_SIZE + 3);
        assert_eq!(control.len(), 2);
        request.payload_len -= 1;
        assert_eq!(split_sendmsg_payload(&request), Err(libc::EINVAL));
        request.payload[0..4].copy_from_slice(&u32::MAX.to_ne_bytes());
        assert_eq!(split_sendmsg_payload(&request), Err(libc::EINVAL));
    }
}
