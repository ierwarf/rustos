//! Kernel-local `AF_UNIX` socket object, pair channel, and bind namespace.
//!
//! - **Owner:** `kernel-ps` owns per-socket state, connected pair channels,
//!   the global `AF_UNIX` bind namespace, descriptor lifetime, and readiness;
//!   `kernel-compat` owns syscall ABI, user copy, blocking, wakeups, and `vfsd`
//!   filesystem-node creation/lookup/unlink.
//! - **Boundary:** Addresses, message lengths, control buffers, socket options,
//!   and fds are untrusted; every user copy and `vfsd` IPC completes outside
//!   every `LocalFdObject` and `UnixSocketNamespace` lock.
//! - **Lifecycle:** Each socket registers its `LocalDescription` in
//!   `local_wait::REGISTRY` on creation and unregisters on final descriptor
//!   release, removing its namespace binding, marking any connected peer closed,
//!   and returning all unread `PassedHandle`s for iterative release outside locks.
//! - **Concurrency:** `UnixSocketNamespace` (class 61) is a strict leaf lock.
//!   Per-socket and per-channel state use `LocalFdObject` (class 60). Connected
//!   pairs share one `Arc<UnixChannel>` locked once; an endpoint lock and a
//!   channel lock (or two endpoint locks) are never held simultaneously. Neither
//!   `PassedHandle` drops nor `wake_local_fd_object` calls happen under any lock.
//! - **Failure:** Empty receives, full sends, or full accept queues report
//!   `WouldBlock { generation }`; closed peers report `BrokenPipe` or EOF;
//!   capacity ceilings fail with `TooManyObjects` / `NoMemory` without corruption.
//! - **Accounting:** each endpoint bills `UNIX_SOCKET_OBJECT_PAGES` and each
//!   queued message `ceil(len / 4 KiB)` (min 1) resident pages to the Job
//!   charge group fixed at creation (`PageLease`, refunded exactly once on
//!   drop). Queued data bills the *sending* socket's group; an accepted socket
//!   bills the connector. Plain stream writes extend the tail chunk. Refusal:
//!   creation/accept `TooManyObjects` (`ENFILE`), send `NoBufferSpace`
//!   (`ENOBUFS`); queues also cap at `UNIX_MAX_QUEUED_MESSAGES` (`EAGAIN`).
//! - **Connect:** all-or-nothing. The client moves Unbound -> Connected before the
//!   accept-queue entry exists; a full/closed listener rolls the client back and
//!   releases the accepted-side socket, so a failed connect leaves no queued entry
//!   or charge. A full `SOCK_DGRAM` receiver reports `PeerWouldBlock` so the
//!   sender waits on the receiver's token.
//! - **Forbidden:** Nested `LocalFdObject` locks, holding `UnixSocketNamespace`
//!   across `LocalFdObject`/`vfsd`/user-copy/wakes, or dropping `PassedHandle`
//!   under a spinlock.
//! - **Evidence:** `userspace-wait-set`, `vfs-open-description`.

use alloc::collections::{BTreeMap, VecDeque};
use alloc::sync::{Arc, Weak};
use alloc::vec::Vec;

use kernel_object::api::handle::{HandleRights, SocketHandleRights};
use nucleus_core::util::lockdep::{LockClass, TrackedSpinLock};

use crate::user::handles::KernelHandle;
use crate::user::linux as linux_abi;
use crate::user::page_lease::PageLease;
use crate::user::local_wait::{
    self, LocalDescription, LocalReadiness, LocalRelease, LocalWaitable, LocalWake,
};
use crate::user::socket::{PassedHandle, SocketCredentials};

pub const UNIX_SOCK_SEQPACKET: u64 = 5;
pub const UNIX_MAX_PATH_LEN: usize = 108;
pub const UNIX_DEFAULT_BUF_SIZE: usize = 212_992;
pub const UNIX_MIN_BUF_SIZE: usize = 4096;
pub const UNIX_MAX_BUF_SIZE: usize = 1024 * 1024;
pub const UNIX_MAX_SCM_RIGHTS: usize = 253;
pub const UNIX_DEFAULT_BACKLOG: usize = 128;
pub const UNIX_MAX_BACKLOG: usize = 4096;
/// Most messages one receive queue holds (`SOCK_DGRAM`/`SOCK_SEQPACKET`
/// messages, and `SOCK_STREAM` chunks that cannot be coalesced). Zero-length
/// messages add no queued bytes, so the byte ceiling alone does not bound them.
pub const UNIX_MAX_QUEUED_MESSAGES: usize = 4096;
/// Pages billed for the socket object itself (endpoint, registry slot, wait
/// state), independent of queued data.
pub const UNIX_SOCKET_OBJECT_PAGES: u64 = 1;
const UNIX_PAGE_SIZE: usize = 4096;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum UnixSocketType {
    Stream,
    Dgram,
    SeqPacket,
}

impl UnixSocketType {
    pub fn from_linux_type(raw: u64) -> Option<Self> {
        match raw {
            linux_abi::SOCK_STREAM => Some(Self::Stream),
            linux_abi::SOCK_DGRAM => Some(Self::Dgram),
            UNIX_SOCK_SEQPACKET => Some(Self::SeqPacket),
            _ => None,
        }
    }

    pub const fn to_linux_type(self) -> u64 {
        match self {
            Self::Stream => linux_abi::SOCK_STREAM,
            Self::Dgram => linux_abi::SOCK_DGRAM,
            Self::SeqPacket => UNIX_SOCK_SEQPACKET,
        }
    }

    pub const fn is_connection_oriented(self) -> bool {
        matches!(self, Self::Stream | Self::SeqPacket)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum UnixAddress {
    Unnamed,
    /// Abstract namespace name (bytes after the leading NUL).
    Abstract(Vec<u8>),
    /// Filesystem path bytes (without trailing NUL).
    Pathname(Vec<u8>),
}

#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd)]
pub enum UnixNamespaceKey {
    Abstract(Vec<u8>),
    Inode { mount_id: u64, node_id: u64 },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum UnixSocketError {
    WouldBlock { generation: u64 },
    /// A datagram send found the *receiver's* queue full: wait on the
    /// receiver's `token` (its `recv` publishes the wake), not the sender's.
    PeerWouldBlock { token: u64, generation: u64 },
    BrokenPipe,
    ConnectionRefused,
    ConnectionReset,
    AddressInUse,
    AddressNotAvailable,
    AlreadyConnected,
    NotConnected,
    InvalidArgument,
    OperationNotSupported,
    MessageTooLong,
    TooManyObjects,
    NoMemory,
    /// The sender's Job charge group refused the pages for queued data
    /// (`ENOBUFS`); retrying cannot succeed until the Job frees memory.
    NoBufferSpace,
    DestinationRequired,
}

#[derive(Clone, Debug)]
pub struct UnixReceivedMessage {
    pub data: Vec<u8>,
    /// Original full message length (for `MSG_TRUNC` on DGRAM/SEQPACKET).
    pub original_len: usize,
    pub rights: Vec<PassedHandle>,
    pub creds: Option<SocketCredentials>,
    pub sender_addr: UnixAddress,
    pub wake: Option<LocalWake>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct UnixSendOutcome {
    pub bytes: usize,
    pub wake: Option<LocalWake>,
}

fn pages_for_bytes(len: usize) -> u64 {
    len.div_ceil(UNIX_PAGE_SIZE).max(1) as u64
}

#[derive(Debug)]
struct UnixQueuedMessage {
    data: Vec<u8>,
    /// Pages of `data` billed to the sending socket's Job; refunded when the
    /// message is dequeued, drained at close, or dropped with its queue.
    lease: PageLease,
    offset: usize,
    rights: Vec<PassedHandle>,
    creds: Option<SocketCredentials>,
    sender_addr: UnixAddress,
}

impl UnixQueuedMessage {
    fn remaining(&self) -> usize {
        self.data.len().saturating_sub(self.offset)
    }
}

struct UnixChannelSide {
    token: u64,
    recv_queue: VecDeque<UnixQueuedMessage>,
    queued_bytes: usize,
    max_bytes: usize,
    shut_rd: bool,
    shut_wr: bool,
    closed: bool,
    peer_creds: SocketCredentials,
    passcred: bool,
    generation: u64,
    in_generation: u64,
    out_generation: u64,
}

impl UnixChannelSide {
    fn new(token: u64, peer_creds: SocketCredentials, passcred: bool, max_bytes: usize) -> Self {
        Self {
            token,
            recv_queue: VecDeque::new(),
            queued_bytes: 0,
            max_bytes,
            shut_rd: false,
            shut_wr: false,
            closed: false,
            peer_creds,
            passcred,
            generation: 1,
            in_generation: 1,
            out_generation: 1,
        }
    }

    fn wake(&self) -> LocalWake {
        LocalWake {
            token: self.token,
            generation: self.generation,
        }
    }

    fn bump_in(&mut self) -> LocalWake {
        self.generation = self.generation.saturating_add(1);
        self.in_generation = self.in_generation.saturating_add(1);
        self.wake()
    }

    fn bump_out(&mut self) -> LocalWake {
        self.generation = self.generation.saturating_add(1);
        self.out_generation = self.out_generation.saturating_add(1);
        self.wake()
    }

    fn bump_both(&mut self) -> LocalWake {
        self.generation = self.generation.saturating_add(1);
        self.in_generation = self.in_generation.saturating_add(1);
        self.out_generation = self.out_generation.saturating_add(1);
        self.wake()
    }
}

struct UnixChannelState {
    socket_type: UnixSocketType,
    sides: [UnixChannelSide; 2],
}

struct UnixChannel {
    state: TrackedSpinLock<UnixChannelState, { LockClass::LocalFdObject as u8 }>,
}

impl UnixChannel {
    fn new(
        socket_type: UnixSocketType,
        side0: UnixChannelSide,
        side1: UnixChannelSide,
    ) -> Arc<Self> {
        Arc::new(Self {
            state: TrackedSpinLock::new(UnixChannelState {
                socket_type,
                sides: [side0, side1],
            }),
        })
    }

    fn readiness(&self, side: usize) -> LocalReadiness {
        let state = self.state.lock();
        let me = &state.sides[side];
        let peer = &state.sides[1 - side];
        // unix(7)/unix_poll: the receive direction is shut when this side
        // shut it or the peer shut/closed its write side; the send direction
        // is shut when this side shut it or the peer shut/closed its read
        // side. `EPOLLHUP` needs both; a shut send direction is still
        // `EPOLLOUT` because a write returns `EPIPE` immediately.
        let rcv_shut = me.shut_rd || peer.closed || peer.shut_wr;
        let snd_shut = me.shut_wr || peer.closed || peer.shut_rd;
        let mut events = 0_u32;
        if !me.recv_queue.is_empty() || rcv_shut {
            events |= linux_abi::EPOLLIN;
        }
        if rcv_shut {
            events |= linux_abi::EPOLLRDHUP;
        }
        if rcv_shut && snd_shut {
            events |= linux_abi::EPOLLHUP;
        }
        if snd_shut
            || (peer.queued_bytes < peer.max_bytes && peer.recv_queue.len() < UNIX_MAX_QUEUED_MESSAGES)
        {
            events |= linux_abi::EPOLLOUT;
        }
        LocalReadiness {
            events,
            generation: me.generation,
            in_generation: me.in_generation,
            out_generation: me.out_generation,
        }
    }

    fn bytes_available(&self, side: usize) -> usize {
        let state = self.state.lock();
        let me = &state.sides[side];
        if state.socket_type == UnixSocketType::SeqPacket {
            me.recv_queue.front().map_or(0, UnixQueuedMessage::remaining)
        } else {
            me.queued_bytes
        }
    }
}

enum EndpointRole {
    Unbound,
    Listening {
        backlog: usize,
        accept_queue: VecDeque<UnixSocketHandle>,
        generation: u64,
        in_generation: u64,
    },
    Connected {
        channel: Arc<UnixChannel>,
        side: usize,
        peer_addr: UnixAddress,
    },
    Dgram {
        recv_queue: VecDeque<UnixQueuedMessage>,
        queued_bytes: usize,
        peer: Option<Weak<UnixSocketEndpoint>>,
        peer_addr: UnixAddress,
        peer_creds: Option<SocketCredentials>,
        shut_rd: bool,
        shut_wr: bool,
        generation: u64,
        in_generation: u64,
        out_generation: u64,
    },
}

struct UnixSocketOptions {
    so_error: i32,
    passcred: bool,
    sndbuf: usize,
    rcvbuf: usize,
    rcvtimeo_us: Option<u64>,
    sndtimeo_us: Option<u64>,
    keepalive: bool,
    reuseaddr: bool,
    reuseport: bool,
}

impl UnixSocketOptions {
    const fn new() -> Self {
        Self {
            so_error: 0,
            passcred: false,
            sndbuf: UNIX_DEFAULT_BUF_SIZE,
            rcvbuf: UNIX_DEFAULT_BUF_SIZE,
            rcvtimeo_us: None,
            sndtimeo_us: None,
            keepalive: false,
            reuseaddr: false,
            reuseport: false,
        }
    }
}

struct UnixEndpointState {
    bound_addr: UnixAddress,
    bound_key: Option<UnixNamespaceKey>,
    creds: SocketCredentials,
    options: UnixSocketOptions,
    role: EndpointRole,
}

struct UnixSocketEndpoint {
    socket_type: UnixSocketType,
    /// Charge group of the creating Job; fixed for life and billed for data
    /// this socket sends. `None` bills nothing.
    charge_group: Option<usize>,
    /// The object's own pages; refunded when the endpoint is freed.
    _object_lease: PageLease,
    description: LocalDescription,
    state: TrackedSpinLock<UnixEndpointState, { LockClass::LocalFdObject as u8 }>,
}

impl Drop for UnixSocketEndpoint {
    fn drop(&mut self) {
        if !self.description.is_closed() {
            local_wait::unregister(self.description.token());
        }
    }
}

impl LocalWaitable for UnixSocketEndpoint {
    fn readiness(&self) -> Option<LocalReadiness> {
        if self.description.is_closed() {
            return None;
        }
        // Clone the channel handle (if connected) and release `self.state`
        // before locking `channel.state` so two `LocalFdObject` locks are never
        // held simultaneously.
        let connected = {
            let state = self.state.lock();
            match &state.role {
                EndpointRole::Connected { channel, side, .. } => Some((channel.clone(), *side)),
                EndpointRole::Listening {
                    accept_queue,
                    generation,
                    in_generation,
                    ..
                } => {
                    let events = if accept_queue.is_empty() {
                        0
                    } else {
                        linux_abi::EPOLLIN
                    };
                    return Some(LocalReadiness {
                        events,
                        generation: *generation,
                        in_generation: *in_generation,
                        out_generation: 1,
                    });
                }
                EndpointRole::Unbound => {
                    // Unconnected stream/seqpacket socket: Linux reports EPOLLOUT | EPOLLHUP.
                    return Some(LocalReadiness::uniform(
                        linux_abi::EPOLLOUT | linux_abi::EPOLLHUP,
                        1,
                    ));
                }
                EndpointRole::Dgram {
                    recv_queue,
                    shut_rd,
                    shut_wr,
                    generation,
                    in_generation,
                    out_generation,
                    ..
                } => {
                    let mut events = 0_u32;
                    if !recv_queue.is_empty() || *shut_rd {
                        events |= linux_abi::EPOLLIN;
                    }
                    if !*shut_wr {
                        events |= linux_abi::EPOLLOUT;
                    }
                    if *shut_rd && *shut_wr {
                        events |= linux_abi::EPOLLHUP;
                    }
                    return Some(LocalReadiness {
                        events,
                        generation: *generation,
                        in_generation: *in_generation,
                        out_generation: *out_generation,
                    });
                }
            }
        };
        connected.map(|(channel, side)| channel.readiness(side))
    }
}

// ---- Global AF_UNIX namespace ---------------------------------------------

struct NamespaceEntry {
    token: u64,
    endpoint: Weak<UnixSocketEndpoint>,
}

struct UnixNamespace {
    entries: BTreeMap<UnixNamespaceKey, NamespaceEntry>,
}

impl UnixNamespace {
    const fn new() -> Self {
        Self {
            entries: BTreeMap::new(),
        }
    }
}

static UNIX_NAMESPACE: TrackedSpinLock<UnixNamespace, { LockClass::UnixSocketNamespace as u8 }> =
    TrackedSpinLock::new(UnixNamespace::new());

fn namespace_insert(
    key: UnixNamespaceKey,
    token: u64,
    endpoint: Weak<UnixSocketEndpoint>,
) -> Result<(), UnixSocketError> {
    let stale_weak = {
        let mut ns = UNIX_NAMESPACE.lock();
        if let Some(existing) = ns.entries.get(&key) {
            if existing.endpoint.strong_count() > 0 {
                // Drop `endpoint` outside the lock.
                drop(ns);
                drop(endpoint);
                return Err(UnixSocketError::AddressInUse);
            }
            ns.entries.insert(key, NamespaceEntry { token, endpoint })
        } else {
            ns.entries.insert(key, NamespaceEntry { token, endpoint })
        }
    };
    drop(stale_weak);
    Ok(())
}

fn namespace_remove(key: &UnixNamespaceKey, token: u64) {
    let removed = {
        let mut ns = UNIX_NAMESPACE.lock();
        let matches = ns
            .entries
            .get(key)
            .is_some_and(|entry| entry.token == token);
        if matches {
            ns.entries.remove(key)
        } else {
            None
        }
    };
    drop(removed);
}

fn namespace_lookup(key: &UnixNamespaceKey) -> Option<Arc<UnixSocketEndpoint>> {
    let weak = {
        let ns = UNIX_NAMESPACE.lock();
        ns.entries.get(key).map(|entry| entry.endpoint.clone())
    }?;
    weak.upgrade()
}

// ---- Public handle --------------------------------------------------------

/// One `AF_UNIX` open socket description as held by descriptor tables.
#[derive(Clone)]
pub struct UnixSocketHandle {
    endpoint: Arc<UnixSocketEndpoint>,
}

impl core::fmt::Debug for UnixSocketHandle {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("UnixSocketHandle")
            .field("token", &self.token_id())
            .field("type", &self.endpoint.socket_type)
            .finish()
    }
}

impl PartialEq for UnixSocketHandle {
    fn eq(&self, other: &Self) -> bool {
        self.token_id() == other.token_id()
    }
}

impl Eq for UnixSocketHandle {}

fn new_endpoint(
    socket_type: UnixSocketType,
    token: u64,
    nonblocking: bool,
    creds: SocketCredentials,
    charge_group: Option<usize>,
    bound_addr: UnixAddress,
    role: EndpointRole,
) -> Result<Arc<UnixSocketEndpoint>, UnixSocketError> {
    let object_lease = PageLease::acquire(charge_group, UNIX_SOCKET_OBJECT_PAGES)
        .ok_or(UnixSocketError::TooManyObjects)?;
    Ok(Arc::new(UnixSocketEndpoint {
        socket_type,
        charge_group,
        _object_lease: object_lease,
        description: LocalDescription::new(token, nonblocking),
        state: TrackedSpinLock::new(UnixEndpointState {
            bound_addr,
            bound_key: None,
            creds,
            options: UnixSocketOptions::new(),
            role,
        }),
    }))
}

fn dgram_role(peer: Option<Weak<UnixSocketEndpoint>>, peer_creds: Option<SocketCredentials>) -> EndpointRole {
    EndpointRole::Dgram {
        recv_queue: VecDeque::new(),
        queued_bytes: 0,
        peer,
        peer_addr: UnixAddress::Unnamed,
        peer_creds,
        shut_rd: false,
        shut_wr: false,
        generation: 1,
        in_generation: 1,
        out_generation: 1,
    }
}

fn register_endpoint(endpoint: &Arc<UnixSocketEndpoint>) -> Result<(), UnixSocketError> {
    let waitable: Arc<dyn LocalWaitable> = endpoint.clone();
    if !local_wait::register(endpoint.description.token(), Arc::downgrade(&waitable)) {
        return Err(UnixSocketError::TooManyObjects);
    }
    Ok(())
}

impl UnixSocketHandle {
    /// Creates an unconnected socket whose memory (one page for the object
    /// plus every byte it queues for a peer) is billed to `charge_group`;
    /// `None` bills nothing. Refusal by the group is `TooManyObjects`.
    pub fn new(
        socket_type: UnixSocketType,
        nonblocking: bool,
        creds: SocketCredentials,
        charge_group: Option<usize>,
    ) -> Result<Self, UnixSocketError> {
        let token = local_wait::allocate_token().ok_or(UnixSocketError::TooManyObjects)?;
        let role = if socket_type == UnixSocketType::Dgram {
            dgram_role(None, None)
        } else {
            EndpointRole::Unbound
        };
        let endpoint = new_endpoint(
            socket_type,
            token,
            nonblocking,
            creds,
            charge_group,
            UnixAddress::Unnamed,
            role,
        )?;
        register_endpoint(&endpoint)?;
        Ok(Self { endpoint })
    }

    /// Creates a connected pair; both ends and their queues are billed to
    /// `charge_group`. Either both exist (and are charged) or neither.
    pub fn new_pair(
        socket_type: UnixSocketType,
        nonblocking: bool,
        creds: SocketCredentials,
        charge_group: Option<usize>,
    ) -> Result<(Self, Self), UnixSocketError> {
        let token0 = local_wait::allocate_token().ok_or(UnixSocketError::TooManyObjects)?;
        let token1 = local_wait::allocate_token().ok_or(UnixSocketError::TooManyObjects)?;

        let (ep0, ep1) = if socket_type == UnixSocketType::Dgram {
            let ep0 = new_endpoint(
                socket_type,
                token0,
                nonblocking,
                creds,
                charge_group,
                UnixAddress::Unnamed,
                dgram_role(None, Some(creds)),
            )?;
            let ep1 = new_endpoint(
                socket_type,
                token1,
                nonblocking,
                creds,
                charge_group,
                UnixAddress::Unnamed,
                dgram_role(Some(Arc::downgrade(&ep0)), Some(creds)),
            )?;
            {
                let mut state0 = ep0.state.lock();
                if let EndpointRole::Dgram { peer, .. } = &mut state0.role {
                    *peer = Some(Arc::downgrade(&ep1));
                }
            }
            (ep0, ep1)
        } else {
            let channel = UnixChannel::new(
                socket_type,
                UnixChannelSide::new(token0, creds, false, UNIX_DEFAULT_BUF_SIZE),
                UnixChannelSide::new(token1, creds, false, UNIX_DEFAULT_BUF_SIZE),
            );
            let connected = |side| EndpointRole::Connected {
                channel: channel.clone(),
                side,
                peer_addr: UnixAddress::Unnamed,
            };
            let ep0 = new_endpoint(
                socket_type,
                token0,
                nonblocking,
                creds,
                charge_group,
                UnixAddress::Unnamed,
                connected(0),
            )?;
            let ep1 = new_endpoint(
                socket_type,
                token1,
                nonblocking,
                creds,
                charge_group,
                UnixAddress::Unnamed,
                connected(1),
            )?;
            (ep0, ep1)
        };
        register_endpoint(&ep0)?;
        if let Err(err) = register_endpoint(&ep1) {
            local_wait::unregister(token0);
            return Err(err);
        }
        Ok((Self { endpoint: ep0 }, Self { endpoint: ep1 }))
    }

    pub fn token_id(&self) -> u64 {
        self.endpoint.description.token()
    }

    pub fn socket_type(&self) -> UnixSocketType {
        self.endpoint.socket_type
    }

    pub fn status_flags(&self) -> u64 {
        linux_abi::O_RDWR
            | if self.endpoint.description.nonblocking() {
                linux_abi::O_NONBLOCK
            } else {
                0
            }
    }

    pub fn nonblocking(&self) -> bool {
        self.endpoint.description.nonblocking()
    }

    pub fn set_nonblocking(&self, value: bool) {
        self.endpoint.description.set_nonblocking(value);
    }

    pub fn mark_epoll_registered(&self) {
        self.endpoint.description.mark_epoll_registered();
    }

    pub fn epoll_registered(&self) -> bool {
        self.endpoint.description.epoll_registered()
    }

    pub fn try_acquire_descriptor_reference(&self) -> bool {
        self.endpoint.description.try_acquire()
    }

    /// Drops one descriptor-table reference. On final release, unregisters from
    /// `local_wait`, removes any namespace bind, notifies any connected peer,
    /// and returns all unread `PassedHandle`s (including unaccepted sockets) to
    /// be released iteratively by the caller with no locks held.
    pub fn release_descriptor_reference(&self) -> (LocalRelease, Vec<PassedHandle>) {
        if !self.endpoint.description.release() {
            return (
                LocalRelease {
                    last: false,
                    wake: None,
                },
                Vec::new(),
            );
        }
        let token = self.token_id();
        local_wait::unregister(token);

        let (bound_key, old_role) = {
            let mut state = self.endpoint.state.lock();
            (
                state.bound_key.take(),
                core::mem::replace(&mut state.role, EndpointRole::Unbound),
            )
        };

        if let Some(key) = bound_key {
            namespace_remove(&key, token);
        }

        let mut drained = Vec::new();
        let mut wake = None;
        match old_role {
            EndpointRole::Unbound => {}
            EndpointRole::Listening {
                accept_queue,
                generation,
                ..
            } => {
                // Connectors blocked on a full backlog re-resolve the (now
                // removed) namespace entry and fail with ECONNREFUSED.
                wake = Some(LocalWake {
                    token,
                    generation: generation.saturating_add(1),
                });
                let rights = HandleRights::Socket(
                    SocketHandleRights::SEND
                        .union(SocketHandleRights::RECV)
                        .union(SocketHandleRights::PASS_FD)
                        .union(SocketHandleRights::TRANSFER),
                );
                for pending in accept_queue {
                    drained.push(PassedHandle::new_with_rights(
                        KernelHandle::UnixSocket(pending),
                        0,
                        rights,
                    ));
                }
            }
            EndpointRole::Connected { channel, side, .. } => {
                let mut ch = channel.state.lock();
                ch.sides[side].closed = true;
                let messages = core::mem::take(&mut ch.sides[side].recv_queue);
                ch.sides[side].queued_bytes = 0;
                wake = Some(ch.sides[1 - side].bump_both());
                drop(ch);
                for mut msg in messages {
                    drained.append(&mut msg.rights);
                }
            }
            EndpointRole::Dgram {
                recv_queue,
                generation,
                ..
            } => {
                wake = Some(LocalWake {
                    token,
                    generation: generation.saturating_add(1),
                });
                for mut msg in recv_queue {
                    drained.append(&mut msg.rights);
                }
            }
        }

        (LocalRelease { last: true, wake }, drained)
    }

    pub fn bind(&self, addr: UnixAddress, key: UnixNamespaceKey) -> Result<(), UnixSocketError> {
        {
            let state = self.endpoint.state.lock();
            if state.bound_key.is_some() {
                return Err(UnixSocketError::InvalidArgument);
            }
        }
        let token = self.token_id();
        namespace_insert(key.clone(), token, Arc::downgrade(&self.endpoint))?;
        let mut state = self.endpoint.state.lock();
        if state.bound_key.is_some() {
            drop(state);
            namespace_remove(&key, token);
            return Err(UnixSocketError::InvalidArgument);
        }
        state.bound_addr = addr;
        state.bound_key = Some(key);
        Ok(())
    }

    pub fn listen(&self, backlog: i32) -> Result<(), UnixSocketError> {
        if !self.endpoint.socket_type.is_connection_oriented() {
            return Err(UnixSocketError::OperationNotSupported);
        }
        let capped = if backlog <= 0 {
            UNIX_DEFAULT_BACKLOG
        } else {
            (backlog as usize).min(UNIX_MAX_BACKLOG)
        };
        let mut state = self.endpoint.state.lock();
        if state.bound_key.is_none() {
            return Err(UnixSocketError::InvalidArgument);
        }
        match &mut state.role {
            EndpointRole::Unbound => {
                state.role = EndpointRole::Listening {
                    backlog: capped,
                    accept_queue: VecDeque::new(),
                    generation: 1,
                    in_generation: 1,
                };
                Ok(())
            }
            EndpointRole::Listening { backlog: b, .. } => {
                *b = capped;
                Ok(())
            }
            EndpointRole::Connected { .. } | EndpointRole::Dgram { .. } => {
                Err(UnixSocketError::InvalidArgument)
            }
        }
    }

    pub fn try_accept(
        &self,
        nonblocking: bool,
    ) -> Result<(UnixSocketHandle, UnixAddress, LocalWake), UnixSocketError> {
        if !self.endpoint.socket_type.is_connection_oriented() {
            return Err(UnixSocketError::OperationNotSupported);
        }
        let (accepted, wake) = {
            let mut state = self.endpoint.state.lock();
            let EndpointRole::Listening {
                accept_queue,
                generation,
                ..
            } = &mut state.role
            else {
                return Err(UnixSocketError::InvalidArgument);
            };
            let Some(sock) = accept_queue.pop_front() else {
                return Err(UnixSocketError::WouldBlock {
                    generation: *generation,
                });
            };
            *generation = generation.saturating_add(1);
            // A freed backlog slot wakes connectors blocked on a full queue.
            let wake = LocalWake {
                token: self.endpoint.description.token(),
                generation: *generation,
            };
            (sock, wake)
        };
        accepted.set_nonblocking(nonblocking);
        let peer_addr = accepted.peer_name().unwrap_or(UnixAddress::Unnamed);
        Ok((accepted, peer_addr, wake))
    }

    /// Dissolves a `SOCK_DGRAM` default peer association (`connect(AF_UNSPEC)`).
    pub fn disconnect_dgram(&self) -> Result<(), UnixSocketError> {
        if self.endpoint.socket_type != UnixSocketType::Dgram {
            return Err(UnixSocketError::InvalidArgument);
        }
        let mut state = self.endpoint.state.lock();
        if let EndpointRole::Dgram {
            peer,
            peer_addr,
            peer_creds,
            ..
        } = &mut state.role
        {
            *peer = None;
            *peer_addr = UnixAddress::Unnamed;
            *peer_creds = None;
            Ok(())
        } else {
            Err(UnixSocketError::InvalidArgument)
        }
    }

    pub fn connect(
        &self,
        target_key: &UnixNamespaceKey,
        client_creds: SocketCredentials,
    ) -> Result<Option<LocalWake>, UnixSocketError> {
        let target = namespace_lookup(target_key).ok_or(UnixSocketError::ConnectionRefused)?;
        if target.socket_type != self.endpoint.socket_type {
            return Err(UnixSocketError::ConnectionRefused);
        }

        if self.endpoint.socket_type == UnixSocketType::Dgram {
            let (peer_addr, peer_creds) = {
                let t = target.state.lock();
                (t.bound_addr.clone(), t.creds)
            };
            let mut state = self.endpoint.state.lock();
            if let EndpointRole::Dgram {
                peer,
                peer_addr: pa,
                peer_creds: pc,
                ..
            } = &mut state.role
            {
                *peer = Some(Arc::downgrade(&target));
                *pa = peer_addr;
                *pc = Some(peer_creds);
                return Ok(None);
            }
            return Err(UnixSocketError::InvalidArgument);
        }

        // Connection-oriented (`SOCK_STREAM` / `SOCK_SEQPACKET`):
        let (client_addr, client_passcred, client_rcvbuf) = {
            let state = self.endpoint.state.lock();
            match &state.role {
                EndpointRole::Unbound => (
                    state.bound_addr.clone(),
                    state.options.passcred,
                    state.options.rcvbuf,
                ),
                EndpointRole::Connected { .. } => return Err(UnixSocketError::AlreadyConnected),
                EndpointRole::Listening { .. } | EndpointRole::Dgram { .. } => {
                    return Err(UnixSocketError::InvalidArgument);
                }
            }
        };

        let (server_creds, server_addr, server_passcred, server_rcvbuf) = {
            let t = target.state.lock();
            if !matches!(t.role, EndpointRole::Listening { .. }) {
                return Err(UnixSocketError::ConnectionRefused);
            }
            (
                t.creds,
                t.bound_addr.clone(),
                t.options.passcred,
                t.options.rcvbuf,
            )
        };

        let server_token = local_wait::allocate_token().ok_or(UnixSocketError::TooManyObjects)?;
        let channel = UnixChannel::new(
            self.endpoint.socket_type,
            UnixChannelSide::new(self.token_id(), server_creds, client_passcred, client_rcvbuf),
            UnixChannelSide::new(server_token, client_creds, server_passcred, server_rcvbuf),
        );
        // The accepted socket is allocated in the connecting task's context,
        // so it is billed to the connector's Job (Linux charges the same way).
        let server_ep = new_endpoint(
            self.endpoint.socket_type,
            server_token,
            false,
            server_creds,
            self.endpoint.charge_group,
            server_addr.clone(),
            EndpointRole::Connected {
                channel: channel.clone(),
                side: 1,
                peer_addr: client_addr,
            },
        )?;
        register_endpoint(&server_ep)?;
        let server_handle = UnixSocketHandle {
            endpoint: server_ep,
        };

        #[cfg(test)]
        connect_test_hook::run(self);

        // All-or-nothing: the client moves Unbound -> Connected first (the only
        // step another task can invalidate), then the accept-queue entry is
        // created. A failed enqueue rolls the client back and releases the
        // never-published accepted socket, so no orphan entry or charge
        // outlives a failed connect.
        {
            let mut state = self.endpoint.state.lock();
            if !matches!(state.role, EndpointRole::Unbound) {
                let err = match state.role {
                    EndpointRole::Connected { .. } => UnixSocketError::AlreadyConnected,
                    _ => UnixSocketError::InvalidArgument,
                };
                drop(state);
                let _ = server_handle.release_descriptor_reference();
                return Err(err);
            }
            state.role = EndpointRole::Connected {
                channel: channel.clone(),
                side: 0,
                peer_addr: server_addr,
            };
        }

        let rollback_client = |this: &Self| {
            let mut state = this.endpoint.state.lock();
            if let EndpointRole::Connected { channel: c, .. } = &state.role {
                if Arc::ptr_eq(c, &channel) {
                    state.role = EndpointRole::Unbound;
                }
            }
        };

        let listener_wake = {
            let mut t = target.state.lock();
            match &mut t.role {
                EndpointRole::Listening {
                    backlog,
                    accept_queue,
                    generation,
                    in_generation,
                } => {
                    if accept_queue.len() >= *backlog {
                        let observed = *generation;
                        drop(t);
                        rollback_client(self);
                        let _ = server_handle.release_descriptor_reference();
                        // The connector waits on the *listener's* token: its
                        // accept/close publishes the wake.
                        return Err(UnixSocketError::PeerWouldBlock {
                            token: target.description.token(),
                            generation: observed,
                        });
                    }
                    accept_queue.push_back(server_handle);
                    *generation = generation.saturating_add(1);
                    *in_generation = in_generation.saturating_add(1);
                    LocalWake {
                        token: target.description.token(),
                        generation: *generation,
                    }
                }
                _ => {
                    drop(t);
                    rollback_client(self);
                    let _ = server_handle.release_descriptor_reference();
                    return Err(UnixSocketError::ConnectionRefused);
                }
            }
        };
        Ok(Some(listener_wake))
    }

    pub fn try_send(
        &self,
        data: &[u8],
        rights: Vec<PassedHandle>,
        creds: SocketCredentials,
        target_key: Option<&UnixNamespaceKey>,
    ) -> Result<UnixSendOutcome, (UnixSocketError, Vec<PassedHandle>)> {
        if rights.len() > UNIX_MAX_SCM_RIGHTS {
            return Err((UnixSocketError::InvalidArgument, rights));
        }

        if self.endpoint.socket_type == UnixSocketType::Dgram {
            return self.try_send_dgram(data, rights, creds, target_key);
        }

        if target_key.is_some() {
            return Err((UnixSocketError::OperationNotSupported, rights));
        }

        let (channel, side) = {
            let state = self.endpoint.state.lock();
            match &state.role {
                EndpointRole::Connected { channel, side, .. } => (channel.clone(), *side),
                _ => return Err((UnixSocketError::NotConnected, rights)),
            }
        };

        let mut ch = channel.state.lock();
        let socket_type = ch.socket_type;
        let peer_side = 1 - side;
        if ch.sides[side].shut_wr || ch.sides[peer_side].closed || ch.sides[peer_side].shut_rd {
            return Err((UnixSocketError::BrokenPipe, rights));
        }

        let max_bytes = ch.sides[peer_side].max_bytes;
        if socket_type == UnixSocketType::SeqPacket {
            if data.len() > max_bytes {
                return Err((UnixSocketError::MessageTooLong, rights));
            }
            if ch.sides[peer_side].queued_bytes.saturating_add(data.len()) > max_bytes
                && ch.sides[peer_side].queued_bytes > 0
            {
                let observed = ch.sides[side].generation;
                return Err((
                    UnixSocketError::WouldBlock {
                        generation: observed,
                    },
                    rights,
                ));
            }
            if ch.sides[peer_side].recv_queue.len() >= UNIX_MAX_QUEUED_MESSAGES {
                let observed = ch.sides[side].generation;
                return Err((
                    UnixSocketError::WouldBlock {
                        generation: observed,
                    },
                    rights,
                ));
            }
            let Some(lease) =
                PageLease::acquire(self.endpoint.charge_group, pages_for_bytes(data.len()))
            else {
                return Err((UnixSocketError::NoBufferSpace, rights));
            };
            let mut owned = Vec::new();
            if owned.try_reserve_exact(data.len()).is_err() {
                return Err((UnixSocketError::NoMemory, rights));
            }
            owned.extend_from_slice(data);
            ch.sides[peer_side].queued_bytes =
                ch.sides[peer_side].queued_bytes.saturating_add(owned.len());
            ch.sides[peer_side].recv_queue.push_back(UnixQueuedMessage {
                data: owned,
                lease,
                offset: 0,
                rights,
                creds: Some(creds),
                sender_addr: UnixAddress::Unnamed,
            });
            let wake = ch.sides[peer_side].bump_in();
            return Ok(UnixSendOutcome {
                bytes: data.len(),
                wake: Some(wake),
            });
        }

        // SOCK_STREAM
        if data.is_empty() && rights.is_empty() {
            return Ok(UnixSendOutcome {
                bytes: 0,
                wake: None,
            });
        }
        let avail = max_bytes.saturating_sub(ch.sides[peer_side].queued_bytes);
        if avail == 0 && !data.is_empty() {
            let observed = ch.sides[side].generation;
            return Err((
                UnixSocketError::WouldBlock {
                    generation: observed,
                },
                rights,
            ));
        }
        let take = if data.is_empty() {
            0
        } else {
            data.len().min(avail)
        };
        // Plain bytes extend the tail chunk (the receiver merges equal-creds
        // chunks anyway), so many small writes bill by bytes, not one page each.
        if rights.is_empty() && take > 0 {
            let queue = &mut ch.sides[peer_side].recv_queue;
            if let Some(tail) = queue.back_mut() {
                if tail.rights.is_empty()
                    && tail.creds == Some(creds)
                    && tail.sender_addr == UnixAddress::Unnamed
                    && tail.data.len().saturating_add(take) <= UNIX_MAX_BUF_SIZE
                {
                    let new_len = tail.data.len() + take;
                    if tail.data.try_reserve(take).is_err() {
                        return Err((UnixSocketError::NoMemory, rights));
                    }
                    if !tail.lease.resize(pages_for_bytes(new_len)) {
                        return Err((UnixSocketError::NoBufferSpace, rights));
                    }
                    tail.data.extend_from_slice(&data[..take]);
                    ch.sides[peer_side].queued_bytes =
                        ch.sides[peer_side].queued_bytes.saturating_add(take);
                    let wake = ch.sides[peer_side].bump_in();
                    return Ok(UnixSendOutcome {
                        bytes: take,
                        wake: Some(wake),
                    });
                }
            }
        }
        if ch.sides[peer_side].recv_queue.len() >= UNIX_MAX_QUEUED_MESSAGES {
            let observed = ch.sides[side].generation;
            return Err((
                UnixSocketError::WouldBlock {
                    generation: observed,
                },
                rights,
            ));
        }
        let Some(lease) = PageLease::acquire(self.endpoint.charge_group, pages_for_bytes(take))
        else {
            return Err((UnixSocketError::NoBufferSpace, rights));
        };
        let mut owned = Vec::new();
        if owned.try_reserve_exact(take).is_err() {
            return Err((UnixSocketError::NoMemory, rights));
        }
        owned.extend_from_slice(&data[..take]);
        ch.sides[peer_side].queued_bytes = ch.sides[peer_side].queued_bytes.saturating_add(take);
        ch.sides[peer_side].recv_queue.push_back(UnixQueuedMessage {
            data: owned,
            lease,
            offset: 0,
            rights,
            creds: Some(creds),
            sender_addr: UnixAddress::Unnamed,
        });
        let wake = ch.sides[peer_side].bump_in();
        Ok(UnixSendOutcome {
            bytes: take,
            wake: Some(wake),
        })
    }

    fn try_send_dgram(
        &self,
        data: &[u8],
        rights: Vec<PassedHandle>,
        creds: SocketCredentials,
        target_key: Option<&UnixNamespaceKey>,
    ) -> Result<UnixSendOutcome, (UnixSocketError, Vec<PassedHandle>)> {
        let (sender_addr, default_peer, shut_wr) = {
            let state = self.endpoint.state.lock();
            match &state.role {
                EndpointRole::Dgram { peer, shut_wr, .. } => {
                    (state.bound_addr.clone(), peer.clone(), *shut_wr)
                }
                _ => return Err((UnixSocketError::InvalidArgument, rights)),
            }
        };
        if shut_wr {
            return Err((UnixSocketError::BrokenPipe, rights));
        }
        let target = if let Some(key) = target_key {
            match namespace_lookup(key) {
                Some(ep) => ep,
                None => return Err((UnixSocketError::ConnectionRefused, rights)),
            }
        } else if let Some(weak) = default_peer {
            match weak.upgrade() {
                Some(ep) => ep,
                None => return Err((UnixSocketError::ConnectionRefused, rights)),
            }
        } else {
            return Err((UnixSocketError::DestinationRequired, rights));
        };
        if target.socket_type != UnixSocketType::Dgram {
            return Err((UnixSocketError::ConnectionRefused, rights));
        }
        let Some(lease) =
            PageLease::acquire(self.endpoint.charge_group, pages_for_bytes(data.len()))
        else {
            return Err((UnixSocketError::NoBufferSpace, rights));
        };
        let mut owned = Vec::new();
        if owned.try_reserve_exact(data.len()).is_err() {
            return Err((UnixSocketError::NoMemory, rights));
        }
        owned.extend_from_slice(data);

        let mut t = target.state.lock();
        let max_bytes = t.options.rcvbuf;
        if owned.len() > max_bytes {
            return Err((UnixSocketError::MessageTooLong, rights));
        }
        let EndpointRole::Dgram {
            recv_queue,
            queued_bytes,
            shut_rd,
            generation,
            in_generation,
            ..
        } = &mut t.role
        else {
            return Err((UnixSocketError::ConnectionRefused, rights));
        };
        if *shut_rd {
            return Err((UnixSocketError::BrokenPipe, rights));
        }
        if (queued_bytes.saturating_add(owned.len()) > max_bytes && *queued_bytes > 0)
            || recv_queue.len() >= UNIX_MAX_QUEUED_MESSAGES
        {
            let observed = *generation;
            return Err((
                UnixSocketError::PeerWouldBlock {
                    token: target.description.token(),
                    generation: observed,
                },
                rights,
            ));
        }
        *queued_bytes = queued_bytes.saturating_add(owned.len());
        recv_queue.push_back(UnixQueuedMessage {
            data: owned,
            lease,
            offset: 0,
            rights,
            creds: Some(creds),
            sender_addr,
        });
        *generation = generation.saturating_add(1);
        *in_generation = in_generation.saturating_add(1);
        let wake = LocalWake {
            token: target.description.token(),
            generation: *generation,
        };
        Ok(UnixSendOutcome {
            bytes: data.len(),
            wake: Some(wake),
        })
    }

    pub fn try_recv(
        &self,
        max_len: usize,
        peek: bool,
    ) -> Result<UnixReceivedMessage, UnixSocketError> {
        if self.endpoint.socket_type == UnixSocketType::Dgram {
            return self.try_recv_dgram(max_len, peek);
        }

        let (channel, side) = {
            let state = self.endpoint.state.lock();
            match &state.role {
                EndpointRole::Connected { channel, side, .. } => (channel.clone(), *side),
                _ => return Err(UnixSocketError::NotConnected),
            }
        };

        let mut ch = channel.state.lock();
        let socket_type = ch.socket_type;
        let peer_side = 1 - side;
        let peer_closed_or_shut = ch.sides[peer_side].closed || ch.sides[peer_side].shut_wr;
        let self_shut_rd = ch.sides[side].shut_rd;

        if ch.sides[side].recv_queue.is_empty() {
            if self_shut_rd || peer_closed_or_shut {
                return Ok(UnixReceivedMessage {
                    data: Vec::new(),
                    original_len: 0,
                    rights: Vec::new(),
                    creds: None,
                    sender_addr: UnixAddress::Unnamed,
                    wake: None,
                });
            }
            return Err(UnixSocketError::WouldBlock {
                generation: ch.sides[side].generation,
            });
        }

        if socket_type == UnixSocketType::SeqPacket {
            let msg = if peek {
                let front = ch.sides[side].recv_queue.front().unwrap();
                let copy_len = front.data.len().min(max_len);
                return Ok(UnixReceivedMessage {
                    data: front.data[..copy_len].to_vec(),
                    original_len: front.data.len(),
                    rights: Vec::new(),
                    creds: if ch.sides[side].passcred {
                        front.creds
                    } else {
                        None
                    },
                    sender_addr: UnixAddress::Unnamed,
                    wake: None,
                });
            } else {
                ch.sides[side].recv_queue.pop_front().unwrap()
            };
            let orig_len = msg.data.len();
            ch.sides[side].queued_bytes = ch.sides[side].queued_bytes.saturating_sub(orig_len);
            let copy_len = orig_len.min(max_len);
            let mut data = msg.data;
            data.truncate(copy_len);
            let creds = if ch.sides[side].passcred {
                msg.creds
            } else {
                None
            };
            let wake = ch.sides[peer_side].bump_out();
            return Ok(UnixReceivedMessage {
                data,
                original_len: orig_len,
                rights: msg.rights,
                creds,
                sender_addr: UnixAddress::Unnamed,
                wake: Some(wake),
            });
        }

        // SOCK_STREAM
        let passcred = ch.sides[side].passcred;
        let mut out = Vec::new();
        let mut rights = Vec::new();
        let mut creds = None;
        let mut remaining = max_len;

        if peek {
            for chunk in &ch.sides[side].recv_queue {
                if remaining == 0 {
                    break;
                }
                if !out.is_empty() && (!chunk.rights.is_empty() || (passcred && chunk.creds != creds))
                {
                    break;
                }
                if creds.is_none() && passcred {
                    creds = chunk.creds;
                }
                let avail = chunk.remaining();
                let take = avail.min(remaining);
                out.extend_from_slice(&chunk.data[chunk.offset..chunk.offset + take]);
                remaining -= take;
                if !chunk.rights.is_empty() {
                    break;
                }
            }
            let len = out.len();
            return Ok(UnixReceivedMessage {
                data: out,
                original_len: len,
                rights: Vec::new(),
                creds,
                sender_addr: UnixAddress::Unnamed,
                wake: None,
            });
        }

        while let Some(front) = ch.sides[side].recv_queue.front_mut() {
            if !out.is_empty() && (!front.rights.is_empty() || (passcred && front.creds != creds)) {
                break;
            }
            if creds.is_none() && passcred {
                creds = front.creds;
            }
            if rights.is_empty() && !front.rights.is_empty() {
                rights = core::mem::take(&mut front.rights);
            }
            let avail = front.remaining();
            let take = avail.min(remaining);
            if take > 0 {
                out.extend_from_slice(&front.data[front.offset..front.offset + take]);
                front.offset += take;
                remaining -= take;
            }
            let empty = front.remaining() == 0;
            ch.sides[side].queued_bytes = ch.sides[side].queued_bytes.saturating_sub(take);
            if empty {
                ch.sides[side].recv_queue.pop_front();
            }
            if remaining == 0 || !rights.is_empty() {
                break;
            }
        }
        let len = out.len();
        let wake = ch.sides[peer_side].bump_out();
        Ok(UnixReceivedMessage {
            data: out,
            original_len: len,
            rights,
            creds,
            sender_addr: UnixAddress::Unnamed,
            wake: Some(wake),
        })
    }

    fn try_recv_dgram(
        &self,
        max_len: usize,
        peek: bool,
    ) -> Result<UnixReceivedMessage, UnixSocketError> {
        let mut state = self.endpoint.state.lock();
        let passcred = state.options.passcred;
        let EndpointRole::Dgram {
            recv_queue,
            queued_bytes,
            shut_rd,
            generation,
            out_generation,
            ..
        } = &mut state.role
        else {
            return Err(UnixSocketError::InvalidArgument);
        };
        if recv_queue.is_empty() {
            if *shut_rd {
                return Ok(UnixReceivedMessage {
                    data: Vec::new(),
                    original_len: 0,
                    rights: Vec::new(),
                    creds: None,
                    sender_addr: UnixAddress::Unnamed,
                    wake: None,
                });
            }
            return Err(UnixSocketError::WouldBlock {
                generation: *generation,
            });
        }
        if peek {
            let front = recv_queue.front().unwrap();
            let copy_len = front.data.len().min(max_len);
            return Ok(UnixReceivedMessage {
                data: front.data[..copy_len].to_vec(),
                original_len: front.data.len(),
                rights: Vec::new(),
                creds: if passcred { front.creds } else { None },
                sender_addr: front.sender_addr.clone(),
                wake: None,
            });
        }
        let msg = recv_queue.pop_front().unwrap();
        let orig_len = msg.data.len();
        *queued_bytes = queued_bytes.saturating_sub(orig_len);
        *generation = generation.saturating_add(1);
        *out_generation = out_generation.saturating_add(1);
        let wake = LocalWake {
            token: self.endpoint.description.token(),
            generation: *generation,
        };
        let mut data = msg.data;
        data.truncate(orig_len.min(max_len));
        Ok(UnixReceivedMessage {
            data,
            original_len: orig_len,
            rights: msg.rights,
            creds: if passcred { msg.creds } else { None },
            sender_addr: msg.sender_addr,
            wake: Some(wake),
        })
    }

    pub fn shutdown(&self, how: u64) -> Result<[Option<LocalWake>; 2], UnixSocketError> {
        let shut_rd = how == linux_abi::SHUT_RD || how == linux_abi::SHUT_RDWR;
        let shut_wr = how == linux_abi::SHUT_WR || how == linux_abi::SHUT_RDWR;
        if !shut_rd && !shut_wr {
            return Err(UnixSocketError::InvalidArgument);
        }

        let connected = {
            let mut state = self.endpoint.state.lock();
            match &mut state.role {
                EndpointRole::Connected { channel, side, .. } => Some((channel.clone(), *side)),
                EndpointRole::Unbound => return Err(UnixSocketError::NotConnected),
                EndpointRole::Listening { .. } => return Ok([None, None]),
                EndpointRole::Dgram {
                    shut_rd: rd,
                    shut_wr: wr,
                    generation,
                    in_generation,
                    out_generation,
                    ..
                } => {
                    if shut_rd {
                        *rd = true;
                        *in_generation = in_generation.saturating_add(1);
                    }
                    if shut_wr {
                        *wr = true;
                        *out_generation = out_generation.saturating_add(1);
                    }
                    *generation = generation.saturating_add(1);
                    return Ok([
                        Some(LocalWake {
                            token: self.endpoint.description.token(),
                            generation: *generation,
                        }),
                        None,
                    ]);
                }
            }
        };

        let Some((channel, side)) = connected else {
            return Err(UnixSocketError::NotConnected);
        };
        let mut ch = channel.state.lock();
        if shut_rd {
            ch.sides[side].shut_rd = true;
        }
        if shut_wr {
            ch.sides[side].shut_wr = true;
        }
        let self_wake = ch.sides[side].bump_both();
        let peer_wake = ch.sides[1 - side].bump_both();
        Ok([Some(self_wake), Some(peer_wake)])
    }

    pub fn bytes_available(&self) -> usize {
        let connected = {
            let state = self.endpoint.state.lock();
            match &state.role {
                EndpointRole::Connected { channel, side, .. } => Some((channel.clone(), *side)),
                EndpointRole::Dgram { recv_queue, .. } => {
                    return recv_queue.front().map_or(0, UnixQueuedMessage::remaining);
                }
                EndpointRole::Unbound | EndpointRole::Listening { .. } => return 0,
            }
        };
        connected.map_or(0, |(ch, side)| ch.bytes_available(side))
    }

    pub fn sock_name(&self) -> UnixAddress {
        self.endpoint.state.lock().bound_addr.clone()
    }

    pub fn peer_name(&self) -> Result<UnixAddress, UnixSocketError> {
        let state = self.endpoint.state.lock();
        match &state.role {
            EndpointRole::Connected { peer_addr, .. } => Ok(peer_addr.clone()),
            EndpointRole::Dgram {
                peer: Some(_),
                peer_addr,
                ..
            } => Ok(peer_addr.clone()),
            _ => Err(UnixSocketError::NotConnected),
        }
    }

    pub fn peer_credentials(&self) -> SocketCredentials {
        let connected = {
            let state = self.endpoint.state.lock();
            match &state.role {
                EndpointRole::Connected { channel, side, .. } => Some((channel.clone(), *side)),
                EndpointRole::Dgram {
                    peer_creds: Some(creds),
                    ..
                } => return *creds,
                _ => return SocketCredentials::new(0, u32::MAX, u32::MAX),
            }
        };
        match connected {
            Some((ch, side)) => ch.state.lock().sides[side].peer_creds,
            None => SocketCredentials::new(0, u32::MAX, u32::MAX),
        }
    }

    pub fn is_listening(&self) -> bool {
        matches!(self.endpoint.state.lock().role, EndpointRole::Listening { .. })
    }

    pub fn take_error(&self) -> i32 {
        let mut state = self.endpoint.state.lock();
        core::mem::take(&mut state.options.so_error)
    }

    pub fn passcred(&self) -> bool {
        self.endpoint.state.lock().options.passcred
    }

    pub fn set_passcred(&self, enabled: bool) {
        let connected = {
            let mut state = self.endpoint.state.lock();
            state.options.passcred = enabled;
            match &state.role {
                EndpointRole::Connected { channel, side, .. } => Some((channel.clone(), *side)),
                _ => None,
            }
        };
        if let Some((ch, side)) = connected {
            ch.state.lock().sides[side].passcred = enabled;
        }
    }

    pub fn sndbuf(&self) -> usize {
        self.endpoint.state.lock().options.sndbuf
    }

    pub fn set_sndbuf(&self, requested: usize) {
        let clamped = requested
            .saturating_mul(2)
            .clamp(UNIX_MIN_BUF_SIZE, UNIX_MAX_BUF_SIZE);
        self.endpoint.state.lock().options.sndbuf = clamped;
    }

    pub fn rcvbuf(&self) -> usize {
        self.endpoint.state.lock().options.rcvbuf
    }

    pub fn set_rcvbuf(&self, requested: usize) {
        let clamped = requested
            .saturating_mul(2)
            .clamp(UNIX_MIN_BUF_SIZE, UNIX_MAX_BUF_SIZE);
        let connected = {
            let mut state = self.endpoint.state.lock();
            state.options.rcvbuf = clamped;
            match &state.role {
                EndpointRole::Connected { channel, side, .. } => Some((channel.clone(), *side)),
                _ => None,
            }
        };
        if let Some((ch, side)) = connected {
            ch.state.lock().sides[side].max_bytes = clamped;
        }
    }

    pub fn rcvtimeo_us(&self) -> Option<u64> {
        self.endpoint.state.lock().options.rcvtimeo_us
    }

    pub fn set_rcvtimeo_us(&self, micros: Option<u64>) {
        self.endpoint.state.lock().options.rcvtimeo_us = micros;
    }

    pub fn sndtimeo_us(&self) -> Option<u64> {
        self.endpoint.state.lock().options.sndtimeo_us
    }

    pub fn set_sndtimeo_us(&self, micros: Option<u64>) {
        self.endpoint.state.lock().options.sndtimeo_us = micros;
    }

    pub fn keepalive(&self) -> bool {
        self.endpoint.state.lock().options.keepalive
    }

    pub fn set_keepalive(&self, value: bool) {
        self.endpoint.state.lock().options.keepalive = value;
    }

    pub fn reuseaddr(&self) -> bool {
        self.endpoint.state.lock().options.reuseaddr
    }

    pub fn set_reuseaddr(&self, value: bool) {
        self.endpoint.state.lock().options.reuseaddr = value;
    }

    pub fn reuseport(&self) -> bool {
        self.endpoint.state.lock().options.reuseport
    }

    pub fn set_reuseport(&self, value: bool) {
        self.endpoint.state.lock().options.reuseport = value;
    }
}

/// Deterministic interleaving point for the connect race tests: runs after the
/// accepted-side socket exists and before the client transition.
#[cfg(test)]
mod connect_test_hook {
    use super::UnixSocketHandle;
    use alloc::boxed::Box;

    type Hook = Box<dyn FnOnce(&UnixSocketHandle) + Send>;
    static HOOK: spin::Mutex<Option<(u64, Hook)>> = spin::Mutex::new(None);

    pub(super) fn arm(client_token: u64, hook: Hook) {
        *HOOK.lock() = Some((client_token, hook));
    }

    pub(super) fn run(client: &UnixSocketHandle) {
        let taken = {
            let mut slot = HOOK.lock();
            match slot.as_ref() {
                Some((token, _)) if *token == client.token_id() => slot.take(),
                _ => None,
            }
        };
        if let Some((_, hook)) = taken {
            hook(client);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::user::eventfd::EventFdHandle;
    use kernel_mm::api::charge;

    fn test_creds(pid: i32) -> SocketCredentials {
        SocketCredentials::new(pid, 1000, 1000)
    }

    fn readiness(sock: &UnixSocketHandle) -> LocalReadiness {
        local_wait::readiness_of(sock.token_id()).expect("live socket readiness")
    }

    #[test]
    fn socketpair_stream_dgram_and_seqpacket_exchange_data() {
        let creds = test_creds(42);

        // Stream
        let (a, b) = UnixSocketHandle::new_pair(UnixSocketType::Stream, false, creds, None).unwrap();
        let sent = a.try_send(b"hello ", Vec::new(), creds, None).unwrap();
        assert_eq!(sent.bytes, 6);
        assert_eq!(sent.wake.unwrap().token, b.token_id());
        a.try_send(b"world", Vec::new(), creds, None).unwrap();
        assert_eq!(b.bytes_available(), 11);
        let msg = b.try_recv(64, false).unwrap();
        assert_eq!(msg.data, b"hello world");
        assert_eq!(b.bytes_available(), 0);
        let _ = a.release_descriptor_reference();
        let _ = b.release_descriptor_reference();

        // Dgram
        let (da, db) = UnixSocketHandle::new_pair(UnixSocketType::Dgram, false, creds, None).unwrap();
        da.try_send(b"first", Vec::new(), creds, None).unwrap();
        da.try_send(b"second", Vec::new(), creds, None).unwrap();
        assert_eq!(db.bytes_available(), 5);
        let m1 = db.try_recv(3, false).unwrap();
        assert_eq!(m1.data, b"fir");
        assert_eq!(m1.original_len, 5);
        let m2 = db.try_recv(64, false).unwrap();
        assert_eq!(m2.data, b"second");
        let _ = da.release_descriptor_reference();
        let _ = db.release_descriptor_reference();

        // SeqPacket
        let (sa, sb) = UnixSocketHandle::new_pair(UnixSocketType::SeqPacket, false, creds, None).unwrap();
        sa.try_send(b"pkt1", Vec::new(), creds, None).unwrap();
        sa.try_send(b"pkt22", Vec::new(), creds, None).unwrap();
        let p1 = sb.try_recv(64, false).unwrap();
        assert_eq!(p1.data, b"pkt1");
        let p2 = sb.try_recv(2, false).unwrap();
        assert_eq!(p2.data, b"pk");
        assert_eq!(p2.original_len, 5);
        let _ = sa.release_descriptor_reference();
        let _ = sb.release_descriptor_reference();
    }

    #[test]
    fn abstract_bind_listen_connect_accept_and_peercred() {
        let server_creds = test_creds(101);
        let client_creds = test_creds(202);
        let name = b"rustos-test-abstract-unix".to_vec();
        let addr = UnixAddress::Abstract(name.clone());
        let key = UnixNamespaceKey::Abstract(name);

        let listener = UnixSocketHandle::new(UnixSocketType::Stream, false, server_creds, None).unwrap();
        listener.bind(addr.clone(), key.clone()).unwrap();
        assert_eq!(listener.sock_name(), addr);
        listener.listen(8).unwrap();
        assert!(listener.is_listening());

        // Duplicate bind fails with AddressInUse.
        let dup = UnixSocketHandle::new(UnixSocketType::Stream, false, server_creds, None).unwrap();
        assert_eq!(dup.bind(addr.clone(), key.clone()), Err(UnixSocketError::AddressInUse));
        let _ = dup.release_descriptor_reference();

        let client = UnixSocketHandle::new(UnixSocketType::Stream, false, client_creds, None).unwrap();
        let wake = client.connect(&key, client_creds).unwrap();
        assert_eq!(wake.unwrap().token, listener.token_id());
        assert_eq!(readiness(&listener).events & linux_abi::EPOLLIN, linux_abi::EPOLLIN);

        let (accepted, _, _) = listener.try_accept(false).unwrap();
        assert_eq!(accepted.peer_credentials(), client_creds);
        assert_eq!(client.peer_credentials(), server_creds);
        assert_eq!(client.peer_name().unwrap(), addr);

        client.try_send(b"ping", Vec::new(), client_creds, None).unwrap();
        assert_eq!(accepted.try_recv(16, false).unwrap().data, b"ping");

        let _ = client.release_descriptor_reference();
        let _ = accepted.release_descriptor_reference();
        let _ = listener.release_descriptor_reference();
    }

    #[test]
    fn scm_rights_delivery_and_unread_cleanup_on_close() {
        let creds = test_creds(77);
        let (a, b) = UnixSocketHandle::new_pair(UnixSocketType::Stream, false, creds, None).unwrap();
        let efd = EventFdHandle::new(9, false, false).unwrap();
        assert!(efd.try_acquire_descriptor_reference());
        let passed = PassedHandle::new_with_rights(
            KernelHandle::EventFd(efd.clone()),
            0,
            KernelHandle::EventFd(efd.clone()).default_rights(0),
        );
        a.try_send(b"x", alloc::vec![passed], creds, None).unwrap();
        let received = b.try_recv(16, false).unwrap();
        assert_eq!(received.data, b"x");
        assert_eq!(received.rights.len(), 1);
        assert!(matches!(received.rights[0].handle(), KernelHandle::EventFd(_)));
        let _ = efd.release_descriptor_reference();

        // Now send another handle and close receiver without reading: must be drained.
        assert!(efd.try_acquire_descriptor_reference());
        let unread = PassedHandle::new_with_rights(
            KernelHandle::EventFd(efd.clone()),
            0,
            KernelHandle::EventFd(efd.clone()).default_rights(0),
        );
        a.try_send(b"y", alloc::vec![unread], creds, None).unwrap();
        let (rel_b, drained) = b.release_descriptor_reference();
        assert!(rel_b.last);
        assert_eq!(drained.len(), 1);
        assert!(matches!(drained[0].handle(), KernelHandle::EventFd(_)));
        assert!(!efd.release_descriptor_reference().last);
        assert!(efd.release_descriptor_reference().last);
        let _ = a.release_descriptor_reference();
    }

    #[test]
    fn shutdown_and_close_report_epollhup_epipe_and_distinct_edge_generations() {
        let creds = test_creds(55);
        let (a, b) = UnixSocketHandle::new_pair(UnixSocketType::Stream, false, creds, None).unwrap();
        let before = readiness(&b);
        assert_eq!(before.events, linux_abi::EPOLLOUT);

        a.try_send(b"ab", Vec::new(), creds, None).unwrap();
        let after_send = readiness(&b);
        assert!(after_send.in_generation > before.in_generation);
        assert_eq!(after_send.out_generation, before.out_generation);

        let _ = b.try_recv(1, false).unwrap();
        let after_recv_a = readiness(&a);
        assert!(after_recv_a.out_generation > 1);

        // Shutdown write on `a` -> `b` sees EPOLLRDHUP and EOF after draining.
        let wakes = a.shutdown(linux_abi::SHUT_WR).unwrap();
        assert!(wakes[1].is_some());
        assert_ne!(readiness(&b).events & linux_abi::EPOLLRDHUP, 0);
        assert_eq!(b.try_recv(16, false).unwrap().data, b"b");
        assert_eq!(b.try_recv(16, false).unwrap().data, b"");

        // Writing on `a` after SHUT_WR fails with BrokenPipe.
        let err = a.try_send(b"z", Vec::new(), creds, None).unwrap_err();
        assert_eq!(err.0, UnixSocketError::BrokenPipe);

        // Closing `b` sets EPOLLHUP on `a`.
        let (rel_b, _) = b.release_descriptor_reference();
        assert!(rel_b.wake.is_some());
        assert_ne!(readiness(&a).events & linux_abi::EPOLLHUP, 0);
        let _ = a.release_descriptor_reference();
    }

    // Charge groups are process-wide statics; 36..=49 are reserved for these
    // tests, one fresh group per call.
    static NEXT_GROUP: core::sync::atomic::AtomicUsize = core::sync::atomic::AtomicUsize::new(36);

    fn limited_group(_hint: usize, pages: u64) -> usize {
        let index = NEXT_GROUP.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
        assert!(index <= 49, "socket test charge groups exhausted");
        charge::init_group(index, None);
        charge::set_limit(index, pages);
        assert_eq!(charge::usage(index), 0);
        index
    }

    #[test]
    fn socketpair_charge_is_refused_atomically_and_refunded_once() {
        let creds = test_creds(1);
        let g = limited_group(45, 3);
        // Two endpoints cost 2 pages: fits; a second pair does not.
        let (a, b) =
            UnixSocketHandle::new_pair(UnixSocketType::SeqPacket, false, creds, Some(g)).unwrap();
        assert_eq!(charge::usage(g), 2 * UNIX_SOCKET_OBJECT_PAGES);
        assert_eq!(
            UnixSocketHandle::new_pair(UnixSocketType::Stream, false, creds, Some(g)).err(),
            Some(UnixSocketError::TooManyObjects)
        );
        assert_eq!(charge::usage(g), 2, "a refused pair leaves no partial charge");
        assert_eq!(
            UnixSocketHandle::new_pair(UnixSocketType::Dgram, false, creds, Some(g)).err(),
            Some(UnixSocketError::TooManyObjects)
        );
        assert_eq!(charge::usage(g), 2);
        let c = UnixSocketHandle::new(UnixSocketType::Stream, false, creds, Some(g)).unwrap();
        assert_eq!(charge::usage(g), 3);
        assert_eq!(
            UnixSocketHandle::new(UnixSocketType::Stream, false, creds, Some(g)).err(),
            Some(UnixSocketError::TooManyObjects)
        );
        for h in [&a, &b, &c] {
            let (rel, _) = h.release_descriptor_reference();
            assert!(rel.last);
        }
        drop((a, b, c));
        assert_eq!(charge::usage(g), 0, "every socket page is refunded exactly once");
        assert_eq!(charge::retainers(g), 0);
    }

    #[test]
    fn queued_bytes_are_charged_to_the_sender_and_refunded_on_every_exit() {
        let creds = test_creds(2);
        let g = limited_group(46, 1_000);
        for ty in [UnixSocketType::Stream, UnixSocketType::SeqPacket, UnixSocketType::Dgram] {
            let (a, b) = UnixSocketHandle::new_pair(ty, true, creds, Some(g)).unwrap();
            let base = charge::usage(g);
            a.try_send(&[7_u8; 5000], Vec::new(), creds, None).unwrap();
            assert_eq!(charge::usage(g), base + 2, "{ty:?}: 5000 bytes are two pages");
            // Dequeue refunds.
            let got = b.try_recv(8192, false).unwrap();
            assert_eq!(got.data.len(), 5000);
            drop(got);
            assert_eq!(charge::usage(g), base, "{ty:?}: receive refunds");
            // Close with unread data refunds.
            a.try_send(b"unread", Vec::new(), creds, None).unwrap();
            assert_eq!(charge::usage(g), base + 1);
            let _ = b.release_descriptor_reference();
            let _ = a.release_descriptor_reference();
            drop((a, b));
            assert_eq!(charge::usage(g), 0, "{ty:?}: teardown refunds");
        }
        assert_eq!(charge::retainers(g), 0);
    }

    #[test]
    fn send_over_the_limit_is_enobufs_class_and_loses_no_charge_or_rights() {
        let creds = test_creds(3);
        let g = limited_group(47, 4);
        for ty in [UnixSocketType::SeqPacket, UnixSocketType::Dgram, UnixSocketType::Stream] {
            let (a, b) = UnixSocketHandle::new_pair(ty, true, creds, Some(g)).unwrap();
            let used = charge::usage(g);
            assert_eq!(used, 2);
            let efd = EventFdHandle::new(1, false, false).unwrap();
            assert!(efd.try_acquire_descriptor_reference());
            let passed = PassedHandle::new_with_rights(
                KernelHandle::EventFd(efd.clone()),
                0,
                KernelHandle::EventFd(efd.clone()).default_rights(0),
            );
            // 3 pages needed, only 2 left.
            let err = a
                .try_send(&[1_u8; 3 * 4096], alloc::vec![passed], creds, None)
                .unwrap_err();
            assert_eq!(err.0, UnixSocketError::NoBufferSpace, "{ty:?}");
            assert_eq!(err.1.len(), 1, "the refused send hands the rights back");
            assert_eq!(charge::usage(g), used, "{ty:?}: refusal charges nothing");
            drop(err);
            assert!(!efd.release_descriptor_reference().last);
            assert!(efd.release_descriptor_reference().last);
            let _ = a.release_descriptor_reference();
            let _ = b.release_descriptor_reference();
            drop((a, b));
            assert_eq!(charge::usage(g), 0);
        }
    }

    #[test]
    fn small_stream_writes_coalesce_and_bill_by_bytes() {
        let creds = test_creds(4);
        let g = limited_group(48, 100);
        let (a, b) =
            UnixSocketHandle::new_pair(UnixSocketType::Stream, true, creds, Some(g)).unwrap();
        let base = charge::usage(g);
        for _ in 0..3000 {
            a.try_send(b"x", Vec::new(), creds, None).unwrap();
        }
        assert_eq!(charge::usage(g), base + 1, "3000 one-byte writes share one page");
        a.try_send(&[0_u8; 4000], Vec::new(), creds, None).unwrap();
        assert_eq!(charge::usage(g), base + 2, "the tail chunk grows to 7000 bytes");
        assert_eq!(b.bytes_available(), 7000);
        // A refused growth leaves bytes and charge untouched.
        let tight = limited_group(49, 3);
        let (c, d) =
            UnixSocketHandle::new_pair(UnixSocketType::Stream, true, creds, Some(tight)).unwrap();
        c.try_send(&[0_u8; 4096], Vec::new(), creds, None).unwrap();
        let before = charge::usage(tight);
        assert_eq!(before, 3);
        let err = c.try_send(b"y", Vec::new(), creds, None).unwrap_err();
        assert_eq!(err.0, UnixSocketError::NoBufferSpace);
        assert_eq!(charge::usage(tight), before);
        assert_eq!(d.bytes_available(), 4096);
        assert_eq!(d.try_recv(8192, false).unwrap().data.len(), 4096);
        assert_eq!(charge::usage(tight), 2, "only the object pages remain");
        for h in [&a, &b, &c, &d] {
            let _ = h.release_descriptor_reference();
        }
        drop((a, b, c, d));
        assert_eq!(charge::usage(g), 0);
        assert_eq!(charge::usage(tight), 0);
    }

    #[test]
    fn connect_accept_charges_connector_and_unaccepted_socket_is_refunded() {
        let creds = test_creds(5);
        let g = limited_group(45, 100);
        let name = b"rustos-test-charge-accept".to_vec();
        let key = UnixNamespaceKey::Abstract(name.clone());
        let listener = UnixSocketHandle::new(UnixSocketType::Stream, false, creds, None).unwrap();
        listener.bind(UnixAddress::Abstract(name), key.clone()).unwrap();
        listener.listen(4).unwrap();
        let client = UnixSocketHandle::new(UnixSocketType::Stream, false, creds, Some(g)).unwrap();
        assert_eq!(charge::usage(g), 1);
        client.connect(&key, creds).unwrap();
        assert_eq!(charge::usage(g), 2, "the accepted-side socket bills the connector");
        let _ = client.release_descriptor_reference();
        // Listener closes with the connection still unaccepted: drained handle
        // is released by the caller, which refunds the page.
        let (rel, drained) = listener.release_descriptor_reference();
        assert!(rel.last);
        assert_eq!(drained.len(), 1);
        drop(drained);
        drop((client, listener));
        assert_eq!(charge::usage(g), 0);

        // A refused accepted-side charge fails connect and leaves the client unconnected.
        let g2 = limited_group(46, 1);
        let name2 = b"rustos-test-charge-accept-2".to_vec();
        let key2 = UnixNamespaceKey::Abstract(name2.clone());
        let l2 = UnixSocketHandle::new(UnixSocketType::Stream, false, creds, None).unwrap();
        l2.bind(UnixAddress::Abstract(name2), key2.clone()).unwrap();
        l2.listen(4).unwrap();
        let c2 = UnixSocketHandle::new(UnixSocketType::Stream, false, creds, Some(g2)).unwrap();
        assert_eq!(c2.connect(&key2, creds), Err(UnixSocketError::TooManyObjects));
        assert_eq!(charge::usage(g2), 1);
        assert!(!l2.is_listening() || l2.try_accept(true).is_err());
        let _ = c2.release_descriptor_reference();
        let _ = l2.release_descriptor_reference();
        drop((c2, l2));
        assert_eq!(charge::usage(g2), 0);
    }

    #[test]
    fn full_dgram_receiver_blocks_the_sender_on_the_receivers_token() {
        let creds = test_creds(9);
        let (a, b) = UnixSocketHandle::new_pair(UnixSocketType::Dgram, true, creds, None).unwrap();
        let mut last = None;
        for _ in 0..=UNIX_MAX_QUEUED_MESSAGES {
            match a.try_send(b"", Vec::new(), creds, None) {
                Ok(_) => {}
                Err((err, _)) => {
                    last = Some(err);
                    break;
                }
            }
        }
        let Some(UnixSocketError::PeerWouldBlock { token, generation }) = last else {
            panic!("a full receiver must report PeerWouldBlock, got {last:?}");
        };
        assert_eq!(token, b.token_id(), "wait on the receiver, whose recv publishes the wake");
        assert_eq!(readiness(&b).generation, generation);
        let msg = b.try_recv(16, false).unwrap();
        let wake = msg.wake.expect("a dgram recv publishes a wake");
        assert_eq!(wake.token, token);
        assert!(wake.generation > generation, "the blocked sender's observation is stale");
        let _ = a.release_descriptor_reference();
        let _ = b.release_descriptor_reference();
    }

    #[test]
    fn shutdown_readiness_matches_unix_poll_for_each_direction() {
        let creds = test_creds(8);
        let new_pair = || {
            UnixSocketHandle::new_pair(UnixSocketType::Stream, false, creds, None).unwrap()
        };
        let in_rdhup = linux_abi::EPOLLIN | linux_abi::EPOLLRDHUP;
        // Local SHUT_RD: RDHUP|IN (Linux RCV_SHUTDOWN), writable, not HUP.
        let (a, b) = new_pair();
        a.shutdown(linux_abi::SHUT_RD).unwrap();
        assert_eq!(readiness(&a).events, in_rdhup | linux_abi::EPOLLOUT);
        // The peer's send direction is shut: a write fails at once, so it
        // is reported writable and not hung up.
        assert_eq!(readiness(&b).events, linux_abi::EPOLLOUT);
        assert!(matches!(
            b.try_send(b"x", Vec::new(), creds, None),
            Err((UnixSocketError::BrokenPipe, _))
        ));
        // Local SHUT_WR keeps EPOLLOUT (the write returns EPIPE, never blocks).
        let (c, d) = new_pair();
        c.shutdown(linux_abi::SHUT_WR).unwrap();
        assert_eq!(readiness(&c).events, linux_abi::EPOLLOUT);
        assert_eq!(readiness(&d).events, in_rdhup | linux_abi::EPOLLOUT);
        // Both directions shut by the peer (no close): both ends hang up.
        let (e, f) = new_pair();
        e.shutdown(linux_abi::SHUT_RDWR).unwrap();
        let hup = in_rdhup | linux_abi::EPOLLHUP | linux_abi::EPOLLOUT;
        assert_eq!(readiness(&e).events, hup);
        assert_eq!(readiness(&f).events, hup);
        // The peer shutting only its write side after we shut ours also hangs up.
        let (g, h) = new_pair();
        g.shutdown(linux_abi::SHUT_WR).unwrap();
        h.shutdown(linux_abi::SHUT_WR).unwrap();
        assert_eq!(readiness(&g).events, hup);
        for x in [a, b, c, d, e, f, g, h] {
            let _ = x.release_descriptor_reference();
        }
    }

    #[test]
    fn connect_that_loses_the_client_race_leaves_no_orphan_accept_entry() {
        let creds = test_creds(7);
        let g = limited_group(45, 100);
        let key_a = UnixNamespaceKey::Abstract(b"rustos-test-race-a".to_vec());
        let key_b = UnixNamespaceKey::Abstract(b"rustos-test-race-b".to_vec());
        let mk_listener = |key: &UnixNamespaceKey, name: &[u8]| {
            let l = UnixSocketHandle::new(UnixSocketType::Stream, false, creds, None).unwrap();
            l.bind(UnixAddress::Abstract(name.to_vec()), key.clone()).unwrap();
            l.listen(4).unwrap();
            l
        };
        let la = mk_listener(&key_a, b"rustos-test-race-a");
        let lb = mk_listener(&key_b, b"rustos-test-race-b");
        let client = UnixSocketHandle::new(UnixSocketType::Stream, true, creds, Some(g)).unwrap();
        // A concurrent connect on the same socket (to B) wins between the
        // accepted-side allocation and the client transition of connect(A).
        let kb = key_b.clone();
        connect_test_hook::arm(
            client.token_id(),
            alloc::boxed::Box::new(move |c| {
                c.connect(&kb, creds).unwrap();
            }),
        );
        assert_eq!(client.connect(&key_a, creds), Err(UnixSocketError::AlreadyConnected));
        assert!(
            la.try_accept(true).is_err(),
            "the losing connect must not leave an accept-queue entry on A"
        );
        assert!(lb.try_accept(true).is_ok(), "the winning connect is queued on B");
        // Only the client's own page remains: the loser's accepted socket was
        // refunded and the winner's was dropped by the accept above.
        assert_eq!(charge::usage(g), 1);
        let (rel, drained) = la.release_descriptor_reference();
        assert!(rel.last);
        assert!(drained.is_empty(), "no orphaned unaccepted socket drained at close");
        let _ = lb.release_descriptor_reference();
        let _ = client.release_descriptor_reference();
        drop((la, lb, client));
    }

    fn full_backlog_listener(
        name: &[u8],
        creds: SocketCredentials,
    ) -> (UnixSocketHandle, UnixNamespaceKey, UnixSocketHandle) {
        let key = UnixNamespaceKey::Abstract(name.to_vec());
        let l = UnixSocketHandle::new(UnixSocketType::Stream, false, creds, None).unwrap();
        l.bind(UnixAddress::Abstract(name.to_vec()), key.clone()).unwrap();
        l.listen(1).unwrap();
        let filler = UnixSocketHandle::new(UnixSocketType::Stream, false, creds, None).unwrap();
        filler.connect(&key, creds).unwrap();
        (l, key, filler)
    }

    #[test]
    fn connect_to_full_backlog_waits_on_listener_and_succeeds_after_accept() {
        let creds = test_creds(8);
        let g = limited_group(0, 100);
        let (l, key, filler) = full_backlog_listener(b"rustos-test-backlog-accept", creds);
        let client = UnixSocketHandle::new(UnixSocketType::Stream, false, creds, Some(g)).unwrap();
        assert_eq!(charge::usage(g), 1);
        let Err(UnixSocketError::PeerWouldBlock { token, generation }) =
            client.connect(&key, creds)
        else {
            panic!("a full backlog must report PeerWouldBlock on the listener token");
        };
        assert_eq!(token, l.token_id());
        // Interrupt/timeout/nonblocking all surface this error to the caller;
        // the attempt must leave no charge and the client unconnected.
        assert_eq!(charge::usage(g), 1, "failed attempt refunds the accepted-side charge");
        assert_eq!(client.peer_name(), Err(UnixSocketError::NotConnected));
        let (accepted, _, wake) = l.try_accept(true).unwrap();
        assert_eq!(wake.token, token);
        assert!(wake.generation > generation, "accept must wake blocked connectors");
        client.connect(&key, creds).unwrap();
        assert_eq!(charge::usage(g), 2);
        for h in [accepted, filler, client, l] {
            let _ = h.release_descriptor_reference();
        }
    }

    #[test]
    fn connect_blocked_on_full_backlog_is_refused_when_listener_closes() {
        let creds = test_creds(9);
        let g = limited_group(0, 100);
        let (l, key, filler) = full_backlog_listener(b"rustos-test-backlog-close", creds);
        let client = UnixSocketHandle::new(UnixSocketType::Stream, false, creds, Some(g)).unwrap();
        let Err(UnixSocketError::PeerWouldBlock { token, generation }) =
            client.connect(&key, creds)
        else {
            panic!("expected PeerWouldBlock");
        };
        let (rel, drained) = l.release_descriptor_reference();
        let wake = rel.wake.expect("listener close must wake blocked connectors");
        assert_eq!((wake.token, wake.generation > generation), (token, true));
        drop(drained);
        assert_eq!(client.connect(&key, creds), Err(UnixSocketError::ConnectionRefused));
        assert_eq!(charge::usage(g), 1, "refused retry leaves only the client's own page");
        let _ = filler.release_descriptor_reference();
        let _ = client.release_descriptor_reference();
    }

    #[test]
    fn zero_length_seqpacket_flood_is_bounded_without_a_group() {
        let creds = test_creds(6);
        let (a, b) = UnixSocketHandle::new_pair(UnixSocketType::SeqPacket, true, creds, None).unwrap();
        let mut sent = 0;
        loop {
            match a.try_send(b"", Vec::new(), creds, None) {
                Ok(_) => sent += 1,
                Err((UnixSocketError::WouldBlock { .. }, _)) => break,
                Err(other) => panic!("unexpected {other:?}"),
            }
            assert!(sent <= UNIX_MAX_QUEUED_MESSAGES);
        }
        assert_eq!(sent, UNIX_MAX_QUEUED_MESSAGES);
        assert_eq!(readiness(&a).events & linux_abi::EPOLLOUT, 0);
        let _ = a.release_descriptor_reference();
        let _ = b.release_descriptor_reference();
    }

    #[test]
    fn stream_zero_byte_scm_rights_followed_by_data_and_rights_preserves_both() {
        let creds = test_creds(78);
        let (a, b) = UnixSocketHandle::new_pair(UnixSocketType::Stream, false, creds, None).unwrap();

        let efd1 = EventFdHandle::new(1, false, false).unwrap();
        assert!(efd1.try_acquire_descriptor_reference());
        let passed1 = PassedHandle::new_with_rights(
            KernelHandle::EventFd(efd1.clone()),
            0,
            KernelHandle::EventFd(efd1.clone()).default_rights(0),
        );

        let efd2 = EventFdHandle::new(2, false, false).unwrap();
        assert!(efd2.try_acquire_descriptor_reference());
        let passed2 = PassedHandle::new_with_rights(
            KernelHandle::EventFd(efd2.clone()),
            0,
            KernelHandle::EventFd(efd2.clone()).default_rights(0),
        );

        a.try_send(b"", alloc::vec![passed1], creds, None).unwrap();
        a.try_send(b"abc", alloc::vec![passed2], creds, None).unwrap();

        let msg1 = b.try_recv(64, false).unwrap();
        assert!(msg1.data.is_empty());
        assert_eq!(msg1.rights.len(), 1);
        assert_eq!(msg1.rights[0].handle().token().object_id(), efd1.token_id());

        let msg2 = b.try_recv(64, false).unwrap();
        assert_eq!(msg2.data, b"abc");
        assert_eq!(msg2.rights.len(), 1);
        assert_eq!(msg2.rights[0].handle().token().object_id(), efd2.token_id());

        drop(msg1);
        drop(msg2);
        assert!(!efd1.release_descriptor_reference().last);
        assert!(efd1.release_descriptor_reference().last);
        assert!(!efd2.release_descriptor_reference().last);
        assert!(efd2.release_descriptor_reference().last);
        let _ = a.release_descriptor_reference();
        let _ = b.release_descriptor_reference();
    }

    #[test]
    fn full_dgram_receiver_close_wakes_blocked_sender() {
        let creds = test_creds(79);
        let (a, b) = UnixSocketHandle::new_pair(UnixSocketType::Dgram, true, creds, None).unwrap();
        let mut last = None;
        for _ in 0..=UNIX_MAX_QUEUED_MESSAGES {
            match a.try_send(b"", Vec::new(), creds, None) {
                Ok(_) => {}
                Err((err, _)) => {
                    last = Some(err);
                    break;
                }
            }
        }
        let Some(UnixSocketError::PeerWouldBlock { token, generation }) = last else {
            panic!("expected PeerWouldBlock, got {last:?}");
        };
        assert_eq!(token, b.token_id());

        let (rel, drained) = b.release_descriptor_reference();
        assert!(rel.last);
        assert!(drained.is_empty());
        let wake = rel.wake.expect("closing a dgram receiver must publish a wake");
        assert_eq!(wake.token, token);
        assert!(wake.generation > generation);

        let err = a.try_send(b"after-close", Vec::new(), creds, None).unwrap_err();
        assert_eq!(err.0, UnixSocketError::ConnectionRefused);
        let _ = a.release_descriptor_reference();
    }
}
