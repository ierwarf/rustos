//! Socket creation, close/retirement, bind/listen/connect/accept and broker publication.
//!
//! - **Owner:** `netd` owns socket identity and queue custody; the kernel broker owns the target process descriptor table.
//! - **Boundary:** request tokens, flags and accept channels are untrusted; publication binds the exact live reply and operation.
//! - **Lifecycle:** socket, socketpair and accept are charged, published through a reply-bound record, then referenced until final close retires them.
//! - **Concurrency:** table mutation happens under the network-state mutex; rootd charges run on the lifecycle pool with no lock held.
//! - **Failure:** a rejected binding leaves discard with netd; an accepted-then-abandoned socket is handed to the kernel transfer-drop owner and never requeued.
//! - **Forbidden:** no direct target fd installation, no listener flag inheritance onto an accepted socket, no unbounded socket table.
//! - **Evidence:** `tests/unix_socket_lifecycle_tests.rs`, the accept flag cases in kernel `ipc_helpers/tests.rs` and the netd lifecycle mutants.

use super::*;

pub(super) fn mint_socket_token() -> Result<u64, i32> {
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
        // SAFETY: the kernel writes exactly `size_of::<u64>()` bytes into the
        // live local `token`.
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

pub(super) fn handle_socket(
    request: &NetdIpcRequest,
    response: &mut NetdIpcResponse,
    reply_cap: u64,
) -> i32 {
    let domain = request.arg0;
    let socket_type = request.arg1;
    let base_type = socket_type & linux_abi::SOCK_TYPE_MASK;
    if domain == linux_abi::AF_INET || domain == linux_abi::AF_INET6 {
        if !rustos_user_abi::syscall::net_broker_inet_socket_shape_valid(
            domain,
            socket_type,
            request.arg2,
        ) {
            return libc::EPROTONOSUPPORT;
        }
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
        // Charge the *calling process Job* before publishing a socket
        // identity. No NetState lock is held over the rootd IPC.
        if let Err(errno) = provider_accounting::charge_before_publish(token, request.pid) {
            return errno;
        }
        let mut state = lock_net_state();
        if !state.inet_capacity_available() {
            drop(state);
            let _ = provider_accounting::retire_final(token);
            return libc::ENFILE;
        }
        let token_available = state.token_available(token);
        let transport = match start_inet_socket_provider_at(
            request,
            rustos_svc_runtime::syscall::monotonic_nanos(),
            token_available,
            || {
                state
                    .inet_stack()
                    .map(|stack| inet_datagram::add_socket(stack, base_type, request.arg2))
            },
        ) {
            Ok(transport) => transport,
            Err(errno) => {
                drop(state);
                let _ = provider_accounting::retire_final(token);
                return errno;
            }
        };
        state.inet_sockets.insert(
            token,
            InetSocket {
                domain,
                refs: 1,
                options: SocketOptions::inet_default(),
                transport,
                tcp_listener: None,
                local: None,
                peer: None,
                received: None,
                errors: None,
                connect_deadline: None,
                connect_started: None,
                connect_budget: None,
            },
        );
        drop(state);
        notify_inet_worker();
        return prepare_socket_publication(
            request,
            response,
            reply_cap,
            token,
            0,
            0,
            domain,
            0,
            true,
        );
    }
    let kind = match local_socket_kind(domain, socket_type, request.arg2) {
        Ok(kind) => kind,
        Err(errno) => return errno,
    };

    let token = match mint_socket_token() {
        Ok(token) => token,
        Err(errno) => return errno,
    };
    if let Err(errno) = provider_accounting::charge_before_publish(token, request.pid) {
        return errno;
    }
    let mut state = lock_net_state();
    if !state.unix_capacity_available(1) {
        drop(state);
        let _ = provider_accounting::retire_final(token);
        return libc::ENFILE;
    }
    if !state.token_available(token) {
        drop(state);
        let _ = provider_accounting::retire_final(token);
        return libc::EAGAIN;
    }
    let initial_state = match kind {
        UnixSocketKind::Netlink => {
            UnixSocketState::Netlink(netlink::NetlinkState::new(base_type, request.arg2))
        }
        UnixSocketKind::Dgram => UnixSocketState::Dgram(UnixDgramState::default()),
        UnixSocketKind::Stream | UnixSocketKind::SeqPacket => UnixSocketState::Idle,
    };
    state.sockets.insert(
        token,
        UnixSocket {
            kind,
            owner: request_credentials(request),
            refs: 1,
            options: SocketOptions::default(),
            bound_path: None,
            bound_inode: None,
            local_path: None,
            peer_path: None,
            state: initial_state,
        },
    );
    drop(state);
    prepare_socket_publication(request, response, reply_cap, token, 0, 0, domain, 0, true)
}

pub(super) fn handle_socketpair(
    request: &NetdIpcRequest,
    response: &mut NetdIpcResponse,
    reply_cap: u64,
) -> i32 {
    let kind = match local_socket_kind(request.arg0, request.arg1, request.arg2) {
        Ok(UnixSocketKind::Netlink) => return libc::EOPNOTSUPP,
        Ok(kind) => kind,
        Err(errno) => return errno,
    };
    let credentials = request_credentials(request);
    let left = match mint_socket_token() {
        Ok(token) => token,
        Err(errno) => return errno,
    };
    let right = match mint_socket_token() {
        Ok(token) => token,
        Err(errno) => return errno,
    };
    // Socketpair owns two independently closeable descriptions. Fund each
    // once and unwind the first if the second is denied before publication.
    if let Err(errno) = provider_accounting::charge_before_publish(left, request.pid) {
        return errno;
    }
    if let Err(errno) = provider_accounting::charge_before_publish(right, request.pid) {
        let _ = provider_accounting::retire_final(left);
        return errno;
    }
    let mut state = lock_net_state();
    if !state.unix_capacity_available(2) {
        drop(state);
        let _ = provider_accounting::retire_final(left);
        let _ = provider_accounting::retire_final(right);
        return libc::ENFILE;
    }
    if left == right || !state.token_available(left) || !state.token_available(right) {
        drop(state);
        let _ = provider_accounting::retire_final(left);
        let _ = provider_accounting::retire_final(right);
        return libc::EAGAIN;
    }
    let make_state = |peer| match kind {
        UnixSocketKind::Dgram => UnixSocketState::Dgram(UnixDgramState {
            peer: Some(peer),
            peer_credentials: Some(credentials),
            ..UnixDgramState::default()
        }),
        _ => UnixSocketState::Connected(ConnectedState {
            incoming: VecDeque::new(),
            incoming_bytes: 0,
            incoming_control_bytes: 0,
            pending_listener: None,
            channel_id: 0,
            peer,
            peer_closed: false,
            peer_read_closed: false,
            peer_write_closed: false,
            peer_credentials: credentials,
            recv_drain_handoff_armed: false,
            recv_closed: false,
            send_closed: false,
        }),
    };
    state.sockets.insert(
        left,
        UnixSocket {
            kind,
            owner: credentials,
            refs: 1,
            options: SocketOptions::default(),
            bound_path: None,
            bound_inode: None,
            local_path: None,
            peer_path: None,
            state: make_state(right),
        },
    );
    state.sockets.insert(
        right,
        UnixSocket {
            kind,
            owner: credentials,
            refs: 1,
            options: SocketOptions::default(),
            bound_path: None,
            bound_inode: None,
            local_path: None,
            peer_path: None,
            state: make_state(left),
        },
    );
    drop(state);
    prepare_socket_publication(request, response, reply_cap, left, right, 0, request.arg0, 0, true)
}

pub(super) fn handle_close(request: &NetdIpcRequest, response: &mut NetdIpcResponse) -> i32 {
    let mut state = lock_net_state();
    if let Some(socket) = state.inet_sockets.get_mut(&request.socket_token) {
        if socket.refs > 1 {
            socket.refs -= 1;
            response.value = socket.refs as u64;
            return 0;
        }
        let Some(mut socket) = state.inet_sockets.remove(&request.socket_token) else {
            return libc::EBADF;
        };
        state.readiness_generations.remove(&request.socket_token);
        state.readiness_observations.remove(&request.socket_token);
        if let Some(stack) = state.inet.as_mut() {
            let immediate_abort = socket.options.linger.0 && socket.options.linger.1 == 0;
            if let Some(mut listener) = socket.tcp_listener.take() {
                for handle in listener.handles.drain(..) {
                    stack.remove(InetTransport::Tcp(handle), true);
                }
            } else {
                stack.remove(socket.transport, immediate_abort);
            }
            stack.poll_budget(8);
        }
        drop(state);
        // No local socket lock crosses the rootd billing control IPC.
        return provider_accounting::retire_final(request.socket_token)
            .err()
            .unwrap_or(0);
    }
    let Some(socket) = state.sockets.get_mut(&request.socket_token) else {
        return libc::EBADF;
    };
    if socket.refs > 1 {
        socket.refs -= 1;
        response.value = socket.refs as u64;
        return 0;
    }
    let (retired, _peers, retired_tokens) = retire_unix_socket(&mut state, request.socket_token);
    drop(state);
    encode_retired_socket_release(retired, response);
    let mut refund_error = 0;
    for token in retired_tokens {
        if let Err(errno) = provider_accounting::retire_final(token) {
            // A missing ACK must leave the exact token in the client book.
            if refund_error == 0 {
                refund_error = errno
            }
        }
    }
    refund_error
}

/// Withdraw identities and notify peers atomically; backing and authority are
/// carried out of the state lock for destruction and the kernel release trailer.
pub(super) fn retire_unix_socket(
    state: &mut NetState,
    token: u64,
) -> (Vec<UnixSocket>, Vec<u64>, Vec<u64>) {
    let mut tokens = vec![token];
    let mut retired = Vec::new();
    let mut peers = Vec::new();
    let mut retired_tokens = Vec::new();
    while let Some(token) = tokens.pop() {
        let Some(mut socket) = state.sockets.remove(&token) else {
            continue;
        };
        state.readiness_generations.remove(&token);
        state.readiness_observations.remove(&token);
        if let Some(inode) = socket.bound_inode.take() {
            if state.inode_bindings.get(&inode).copied() == Some(token) {
                state.inode_bindings.remove(&inode);
            }
        }
        if let Some(path) = &socket.bound_path {
            if state.bindings.get(path).copied() == Some(token) {
                state.bindings.remove(path);
            }
        }
        match &mut socket.state {
            UnixSocketState::Listening { pending, .. } => {
                tokens.extend(pending.drain(..));
            }
            UnixSocketState::Connected(connected) => {
                state.release_unix_queued_bytes(
                    connected.incoming_bytes + connected.incoming_control_bytes,
                );
                peers.push(connected.peer);
                if let Some(peer) = state.sockets.get_mut(&connected.peer) {
                    if let UnixSocketState::Connected(peer_connected) = &mut peer.state {
                        peer_connected.peer_closed = true;
                        peer_connected.peer_read_closed = true;
                        peer_connected.peer_write_closed = true;
                    }
                }
            }
            UnixSocketState::Dgram(dgram) => {
                state.release_unix_queued_bytes(
                    dgram.incoming_bytes + dgram.incoming_control_bytes,
                );
                if let Some(peer_token) = dgram.peer {
                    peers.push(peer_token);
                    if let Some(peer) = state.sockets.get_mut(&peer_token) {
                        if let UnixSocketState::Dgram(peer_dgram) = &mut peer.state {
                            if peer_dgram.peer == Some(token) {
                                peer_dgram.peer_closed = true;
                            }
                        }
                    }
                }
            }
            UnixSocketState::Idle | UnixSocketState::Netlink(_) => {}
        }
        retired.push(socket);
        retired_tokens.push(token);
    }
    (retired, peers, retired_tokens)
}

pub(super) fn encode_retired_socket_release(
    retired: Vec<UnixSocket>,
    response: &mut NetdIpcResponse,
) {
    let mut released = Vec::new();
    for socket in retired {
        if let UnixSocketState::Connected(connected) = socket.state {
            for segment in connected.incoming {
                if !segment.control.is_empty() {
                    released.resize(cmsg_align(released.len()), 0);
                    released.extend_from_slice(&segment.control);
                }
            }
        }
    }
    // Admission charges aligned controls against one bounded custody budget,
    // including all of a listener's queued and in-flight accepted sockets.
    assert!(released.len() <= response.payload.len());
    response.payload[..released.len()].copy_from_slice(&released);
    response.payload_len = released.len() as u32;
    response.reserved0 = released.len() as u32;
}

/// Performs the final under-lock admission immediately before smoltcp gains a
/// socket. The earlier provider wait and pre-mint checks avoid wasted work;
/// this check is the provider-start linearization point and uses the same
/// immutable request end rather than a refreshed relative timeout.
pub(super) fn start_inet_socket_provider_at<T>(
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

pub(super) fn handle_bind(request: &NetdIpcRequest) -> i32 {
    path_namespace::with_production_namespace(|namespace| handle_bind_with(request, namespace))
}

/// `bind` for AF_UNIX/AF_NETLINK/AF_INET/AF_INET6. No `NetState` lock is held across a
/// namespace call: validate, ask the namespace, then revalidate and publish.
pub(super) fn handle_bind_with(request: &NetdIpcRequest, namespace: &mut dyn PathNamespace) -> i32 {
    let existing_binding = {
        let mut state = lock_net_state();
        if let Some(inet) = state.inet_sockets.get(&request.socket_token) {
            if inet.transport.is_datagram() {
                return inet_datagram::bind_request_in_state(&mut state, request);
            }
            return inet_tcp::bind_tcp_in_state(&mut state, request);
        }
        let Some(socket) = state.sockets.get(&request.socket_token) else {
            return libc::EBADF;
        };
        if socket.kind == UnixSocketKind::Netlink {
            drop(state);
            return netlink::handle_bind(request);
        }
        if socket.bound_path.is_some()
            || !matches!(socket.state, UnixSocketState::Idle | UnixSocketState::Dgram(_))
        {
            return libc::EINVAL;
        }
        drop(state);
        let requested_address = match sockaddr_bind_address_from_payload(request) {
            Ok(address) => address,
            Err(errno) => return errno,
        };
        let state = lock_net_state();
        let exists = requested_address
            .as_ref()
            .is_some_and(|address| state.bindings.contains_key(address));
        (requested_address, exists)
    };
    let (requested_address, existing_binding) = existing_binding;
    // Filesystem names additionally need an inode in the namespace.
    let managed_path = match &requested_address {
        Some(UnixAddress::Filesystem(path)) => {
            Some(path.clone())
        }
        _ => None,
    };
    if managed_path.is_some() && request.arg3 & !0o777 != 0 { return libc::EINVAL; }
    let mut created_node = None;
    let mut replaces_stale_binding = false;
    if let Some(path) = &managed_path {
        match namespace.stat_socket_node(request.pid, path, request.euid, request.egid, false) {
            Err(errno) => return errno,
            Ok(NodeState::Socket(_) | NodeState::NotSocket) => return libc::EADDRINUSE,
            Ok(NodeState::Unmanaged) => {
                if existing_binding {
                    return libc::EADDRINUSE;
                }
            }
            Ok(NodeState::Absent) => {
                match namespace.create_socket_node(
                    request.pid,
                    path,
                    request.euid,
                    request.egid,
                    0o777 & !(request.arg3 as u32),
                ) {
                    Ok(identity) => {
                        created_node = identity;
                        // Absent node + live binding: the name was unlinked.
                        replaces_stale_binding = existing_binding;
                    }
                    Err(libc::EEXIST) => return libc::EADDRINUSE,
                    Err(errno) => return errno,
                }
            }
        }
    }
    let published = {
        let mut state = lock_net_state();
        publish_unix_binding(&mut state, request, requested_address, replaces_stale_binding, created_node)
    };
    match published {
        Ok(()) => 0,
        Err(errno) => {
            if let Some(identity) = created_node {
                if let Some(path) = &managed_path {
                    // The socket changed while the node was being created.
                    let _ = namespace.remove_socket_node(request.pid, path, request.euid, request.egid, identity);
                }
            }
            errno
        }
    }
}

fn publish_unix_binding(
    state: &mut NetState,
    request: &NetdIpcRequest,
    requested_address: Option<UnixAddress>,
    replaces_stale_binding: bool,
    identity: Option<path_namespace::NodeIdentity>,
) -> Result<(), i32> {
    let Some(socket) = state.sockets.get(&request.socket_token) else {
        return Err(libc::EBADF);
    };
    if socket.bound_path.is_some()
        || !matches!(socket.state, UnixSocketState::Idle | UnixSocketState::Dgram(_))
    {
        return Err(libc::EINVAL);
    }
    let address = match requested_address {
        Some(address) => {
            if state.bindings.contains_key(&address) {
                if !replaces_stale_binding {
                    return Err(libc::EADDRINUSE);
                }
                state.bindings.remove(&address);
            }
            address
        }
        None => loop {
            let candidate_id = state.next_autobind_id;
            state.next_autobind_id = candidate_id.wrapping_add(1).max(1);
            let candidate =
                UnixAddress::Abstract(format!("\0{:05x}", candidate_id & 0xfffff).into_bytes());
            if !state.bindings.contains_key(&candidate) {
                break candidate;
            }
        },
    };
    let socket = state
        .sockets
        .get_mut(&request.socket_token)
        .expect("validated unix socket disappeared under net state lock");
    socket.bound_path = Some(address.clone());
    socket.bound_inode = identity.map(|id| (id.mount, id.node));
    socket.local_path = Some(address.clone());
    state.bindings.insert(address, request.socket_token);
    if let Some(id) = identity {
        state
            .inode_bindings
            .insert((id.mount, id.node), request.socket_token);
    }
    Ok(())
}

/// Linux `socket(2)`/`socketpair(2)` type admission for the sockets netd owns
/// in Ring3. Unsupported *types* of a supported family are
/// `ESOCKTNOSUPPORT`, not `EAFNOSUPPORT`.
pub(super) fn local_socket_kind(
    domain: u64,
    socket_type: u64,
    protocol: u64,
) -> Result<UnixSocketKind, i32> {
    let unix = domain == linux_abi::AF_UNIX;
    let netlink = domain == libc::AF_NETLINK as u64;
    if !unix && !netlink {
        return Err(libc::EAFNOSUPPORT);
    }
    if socket_type & !(linux_abi::SOCK_TYPE_MASK | linux_abi::SOCK_NONBLOCK | linux_abi::SOCK_CLOEXEC)
        != 0
    {
        return Err(libc::EINVAL);
    }
    let base_type = socket_type & linux_abi::SOCK_TYPE_MASK;
    let kind = if unix {
        UnixSocketKind::from_unix_base_type(base_type).ok_or(libc::ESOCKTNOSUPPORT)?
    } else if base_type == libc::SOCK_RAW as u64 || base_type == libc::SOCK_DGRAM as u64 {
        UnixSocketKind::Netlink
    } else {
        return Err(libc::ESOCKTNOSUPPORT);
    };
    // AF_UNIX takes only protocol 0; netd implements NETLINK_ROUTE (0) and
    // NETLINK_KOBJECT_UEVENT (15).
    if unix && protocol != 0 {
        return Err(libc::EPROTONOSUPPORT);
    }
    if netlink && !netlink::is_supported_protocol(protocol) {
        return Err(libc::EPROTONOSUPPORT);
    }
    // The published shape is the kernel broker's admission predicate; the two
    // must agree or the publication would be refused after charging.
    let shape_valid = if unix {
        rustos_user_abi::syscall::net_broker_unix_socket_shape_valid(
            domain, socket_type, protocol,
        )
    } else {
        rustos_user_abi::syscall::net_broker_netlink_socket_shape_valid(
            domain, socket_type, protocol,
        ) || (netlink::is_supported_protocol(protocol)
            && rustos_user_abi::syscall::net_broker_netlink_socket_shape_valid(
                domain, socket_type, 0,
            ))
    };
    if !shape_valid {
        return Err(libc::EINVAL);
    }
    Ok(kind)
}

pub(super) fn handle_listen(request: &NetdIpcRequest) -> i32 {
    let mut state = lock_net_state();
    if state.inet_sockets.contains_key(&request.socket_token) {
        return inet_tcp::listen_tcp_in_state(&mut state, request.socket_token, request.arg1);
    }
    let Some(socket) = state.sockets.get_mut(&request.socket_token) else {
        return libc::EBADF;
    };
    if matches!(
        socket.kind,
        UnixSocketKind::Netlink | UnixSocketKind::Dgram
    ) {
        return libc::EOPNOTSUPP;
    }
    let backlog = usize::try_from(request.arg1)
        .unwrap_or(usize::MAX)
        .clamp(1, MAX_LISTEN_BACKLOG);
    // listen(2) on a listening socket only updates the backlog.
    if let UnixSocketState::Listening { backlog: current, .. } = &mut socket.state {
        *current = backlog;
        return 0;
    }
    if socket.bound_path.is_none() || !matches!(socket.state, UnixSocketState::Idle) {
        return libc::EINVAL;
    }
    socket.state = UnixSocketState::Listening {
        backlog,
        pending: VecDeque::new(),
    };
    0
}

pub(super) fn handle_connect(request: &NetdIpcRequest) -> i32 {
    path_namespace::with_production_namespace(|namespace| handle_connect_with(request, namespace))
}

pub(super) fn handle_connect_with(
    request: &NetdIpcRequest,
    namespace: &mut dyn PathNamespace,
) -> i32 {
    let client_kind = {
        let mut state = lock_net_state();
        if let Some(inet) = state.inet_sockets.get(&request.socket_token) {
            if inet.transport.is_datagram() {
                return inet_datagram::connect_in_state(&mut state, request);
            }
            drop(state);
            return begin_inet_connect(request);
        }
        let Some(socket) = state.sockets.get(&request.socket_token) else {
            return libc::EBADF;
        };
        socket.kind
    };
    if client_kind == UnixSocketKind::Netlink {
        // The only netlink peer is the kernel (nl_pid 0): a no-op.
        return netlink::handle_connect(request);
    }
    if client_kind == UnixSocketKind::Dgram
        && sockaddr_family(request) == Some(libc::AF_UNSPEC as u64)
    {
        let mut state = lock_net_state();
        let Some(client) = state.sockets.get_mut(&request.socket_token) else {
            return libc::EBADF;
        };
        if let UnixSocketState::Dgram(dgram) = &mut client.state {
            dgram.peer = None;
            dgram.peer_credentials = None;
            dgram.peer_closed = false;
            client.peer_path = None;
            return 0;
        }
        return libc::EINVAL;
    }
    let path = match sockaddr_path_from_payload(request) {
        Ok(path) => path,
        Err(errno) => return errno,
    };
    let node = match &path {
        UnixAddress::Filesystem(name) => {
            match namespace.stat_socket_node(request.pid, name, request.euid, request.egid, true) {
                Ok(node) => node,
                Err(errno) => return errno,
            }
        }
        _ => NodeState::Unmanaged,
    };
    if client_kind == UnixSocketKind::Dgram {
        let mut state = lock_net_state();
        let target_token = match node {
            NodeState::Absent => return libc::ENOENT,
            NodeState::NotSocket => return libc::ECONNREFUSED,
            NodeState::Socket(id) => match state.inode_bindings.get(&(id.mount, id.node)).copied() {
                Some(token) => token,
                None => return libc::ECONNREFUSED,
            },
            NodeState::Unmanaged => match state.bindings.get(&path).copied() {
                Some(token) => token,
                None => {
                    return if matches!(path, UnixAddress::Abstract(_)) {
                        libc::ECONNREFUSED
                    } else {
                        libc::ENOENT
                    };
                }
            },
        };
        let Some(target) = state.sockets.get(&target_token) else {
            return libc::ECONNREFUSED;
        };
        if target.kind != UnixSocketKind::Dgram {
            return libc::EPROTOTYPE;
        }
        let target_creds = target.owner;
        let Some(client) = state.sockets.get_mut(&request.socket_token) else {
            return libc::EBADF;
        };
        let UnixSocketState::Dgram(dgram) = &mut client.state else {
            return libc::EINVAL;
        };
        dgram.peer = Some(target_token);
        dgram.peer_closed = false;
        dgram.peer_credentials = Some(target_creds);
        client.peer_path = Some(path);
        return 0;
    }
    let credentials = request_credentials(request);
    let accepted = match mint_socket_token() {
        Ok(token) => token,
        Err(errno) => return errno,
    };
    let mut state = lock_net_state();
    let listener_token = match node {
        NodeState::Absent => return libc::ENOENT,
        NodeState::NotSocket => return libc::ECONNREFUSED,
        NodeState::Socket(id) => match state.inode_bindings.get(&(id.mount, id.node)).copied() {
            Some(token) => token,
            None => return libc::ECONNREFUSED,
        },
        NodeState::Unmanaged => match state.bindings.get(&path).copied() {
            Some(token) => token,
            None => return if matches!(path, UnixAddress::Abstract(_)) { libc::ECONNREFUSED } else { libc::ENOENT },
        },
    };
    if !state.token_available(accepted) {
        return libc::EAGAIN;
    }
    if !state.unix_capacity_available(1) {
        return libc::ENFILE;
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
    let client_kind = client.kind;
    if listener.kind != client_kind {
        return libc::EPROTOTYPE;
    }
    let UnixSocketState::Listening { backlog, pending } = &listener.state else {
        return libc::ECONNREFUSED;
    };
    if pending.len() >= *backlog {
        return libc::EAGAIN;
    }
    let listener_owner = listener.owner;
    let channel_id = match bind_unix_connection(request, accepted) {
        Ok(channel_id) => channel_id,
        Err(errno) => return errno,
    };
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
        pending_listener: None,
        channel_id,
        peer: accepted,
        peer_closed: false,
        peer_read_closed: false,
        peer_write_closed: false,
        peer_credentials: listener_owner,
        recv_drain_handoff_armed: false,
        recv_closed: false,
        send_closed: false,
    });
    let listener_path = {
        let Some(listener) = state.sockets.get_mut(&listener_token) else {
            return libc::ECONNREFUSED;
        };
        let UnixSocketState::Listening { pending, .. } = &mut listener.state else {
            return libc::ECONNREFUSED;
        };
        pending.push_back(accepted);
        listener.local_path.clone()
    };
    state.sockets.insert(
        accepted,
        UnixSocket {
            kind: client_kind,
            owner: listener_owner,
            refs: 1,
            options: SocketOptions::default(),
            bound_path: None,
            bound_inode: None,
            local_path: listener_path,
            peer_path: client_local_path,
            state: UnixSocketState::Connected(ConnectedState {
                incoming: VecDeque::new(),
                incoming_bytes: 0,
                incoming_control_bytes: 0,
                pending_listener: Some(listener_token),
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

pub(super) fn bind_unix_connection(request: &NetdIpcRequest, accepted: u64) -> Result<u64, i32> {
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

pub(super) fn handle_accept(
    request: &NetdIpcRequest,
    response: &mut NetdIpcResponse,
    reply_cap: u64,
) -> i32 {
    if request.arg3 & !(linux_abi::SOCK_NONBLOCK | linux_abi::SOCK_CLOEXEC) != 0 {
        return libc::EINVAL;
    }
    if let Some(accepted_domain) = {
        let state = lock_net_state();
        state.inet_sockets.get(&request.socket_token).map(|s| s.domain)
    } {
        let accepted_token = match mint_socket_token() {
            Ok(token) => token,
            Err(errno) => return errno,
        };
        let want_peer_addr = request.arg1 != 0 && request.arg2 != 0;
        {
            let mut state = lock_net_state();
            if let Err(errno) = inet_tcp::pop_ready_tcp_accept_in_state(
                &mut state,
                request.socket_token,
                accepted_token,
                response,
                want_peer_addr,
            ) {
                return errno;
            }
        }
        if let Err(errno) = provider_accounting::charge_before_publish(accepted_token, request.pid) {
            let mut state = lock_net_state();
            if let Some(removed) = state.inet_sockets.remove(&accepted_token) {
                if let Some(stack) = state.inet.as_mut() {
                    stack.remove(removed.transport, true);
                }
            }
            return errno;
        }
        let pub_res = prepare_socket_publication(
            request,
            response,
            reply_cap,
            accepted_token,
            0,
            0,
            accepted_domain,
            linux_abi::SOCK_STREAM,
            false,
        );
        if pub_res != 0 {
            let _ = provider_accounting::retire_final(accepted_token);
            let mut state = lock_net_state();
            if let Some(removed) = state.inet_sockets.remove(&accepted_token) {
                if let Some(stack) = state.inet.as_mut() {
                    stack.remove(removed.transport, true);
                }
            }
        }
        return pub_res;
    }
    // accept4 flags control the NEW descriptor, not the listener's wait mode.
    let accepted = {
        let mut state = lock_net_state();
        let Some(listener) = state.sockets.get_mut(&request.socket_token) else {
            return libc::EBADF;
        };
        match &mut listener.state {
            UnixSocketState::Listening { pending, .. } => match pending.pop_front() {
                Some(token) => token,
                None => return libc::EAGAIN,
            },
            _ => return libc::EINVAL,
        }
    };
    let mut charged = false;
    let result = (|| {
        let (channel_id, accepted_kind) = {
            let state = lock_net_state();
            let Some(socket) = state.sockets.get(&accepted) else {
                return libc::ECONNABORTED;
            };
            let UnixSocketState::Connected(connected) = &socket.state else {
                return libc::ECONNABORTED;
            };
            if request.arg1 != 0 && request.arg2 != 0 {
                if let Err(errno) = sockaddr_payload(socket.peer_path.as_ref(), response) {
                    return errno;
                }
            }
            (connected.channel_id, socket.kind)
        };
        if channel_id == 0 {
            return libc::EPROTO;
        }
        // An accepted endpoint is not funded merely because it is queued.
        // Charge the client only at the public accept handoff boundary.
        if let Err(errno) = provider_accounting::charge_before_publish(accepted, request.pid) {
            return errno;
        }
        charged = true;
        prepare_socket_publication(
            request,
            response,
            reply_cap,
            accepted,
            0,
            channel_id,
            linux_abi::AF_UNIX,
            accepted_kind.unix_socket_type(),
            false,
        )
    })();
    if result != 0 && charged {
        // Mark refund BEFORE requeue. Another accept cannot charge this token
        // until the exact old nonce has settled, even after a lost ACK.
        let _ = provider_accounting::retire_final(accepted);
    }
    let (retired, _peers, tokens) = {
        let mut state = lock_net_state();
        if result != 0 && matches!(result, libc::EPROTO | libc::ECONNABORTED | libc::EINVAL) {
            retire_unix_socket(&mut state, accepted)
        } else {
            settle_unix_accept(&mut state, request.socket_token, accepted, result == 0)
        }
    };
    if let Err(errno) = release_accept_retired_control_with(retired, response, |control| {
        let args = RustosNetBrokerArgs {
            process_id: request.pid,
            op: rustos_user_abi::syscall::NET_BROKER_OP_RELEASE_SOCKET_CONTROL,
            arg0: control.as_ptr() as u64,
            arg1: control.len() as u64,
            ..RustosNetBrokerArgs::default()
        };
        if syscall1(
            SYS_RUSTOS_NET_BROKER,
            (&args as *const RustosNetBrokerArgs) as u64,
        ) < 0
        {
            Err(last_errno())
        } else {
            Ok(())
        }
    }) {
        // No successful/failed reply can carry this obligation after caller
        // abandonment. An impossible local broker failure revokes this service
        // incarnation, whose transfer epoch and Job charges are drained.
        eprintln!("netd: accept ancillary retirement failed errno={errno}");
        std::process::exit(134);
    }
    // Internal not-yet-public accepted sockets may be unfunded; the refund
    // predicate treats those as no-op rather than charging a ghost session.
    for token in tokens {
        let _ = provider_accounting::retire_final(token);
    }
    result
}

/// Retired accept control must reach kernel custody even if its reply is
/// already abandoned. The response is only temporary bounded encoding storage.
pub(super) fn release_accept_retired_control_with(
    retired: Vec<UnixSocket>,
    response: &mut NetdIpcResponse,
    release: impl FnOnce(&[u8]) -> Result<(), i32>,
) -> Result<(), i32> {
    if retired.is_empty() {
        return Ok(());
    }
    encode_retired_socket_release(retired, response);
    let len = response.reserved0 as usize;
    if len != 0 {
        release(&response.payload[..len])?
    }
    response.payload_len = 0;
    response.reserved0 = 0;
    Ok(())
}

pub(super) fn settle_unix_accept(
    state: &mut NetState,
    listener_token: u64,
    accepted: u64,
    published: bool,
) -> (Vec<UnixSocket>, Vec<u64>, Vec<u64>) {
    if published {
        if let Some(socket) = state.sockets.get_mut(&accepted) {
            if let UnixSocketState::Connected(connected) = &mut socket.state {
                connected.pending_listener = None;
            }
        }
        return (Vec::new(), Vec::new(), Vec::new());
    }
    if !state.sockets.contains_key(&accepted) {
        return (Vec::new(), Vec::new(), Vec::new());
    }
    if let Some(listener) = state.sockets.get_mut(&listener_token) {
        if let UnixSocketState::Listening { pending, .. } = &mut listener.state {
            if !pending.contains(&accepted) {
                pending.push_front(accepted);
            }
            return (Vec::new(), Vec::new(), Vec::new());
        }
    }
    retire_unix_socket(state, accepted)
}

/// Linux accept does not inherit the listener's open-description flags, but the
/// accepted socket has the listener's type (`accepted_type`).
pub(super) fn socket_publication_type(request: &NetdIpcRequest, accepted_type: u64) -> u64 {
    if request.op == SYSCALL_OFFLOAD_OP_LINUX_ACCEPT {
        accepted_type
            | (request.arg3 & (linux_abi::SOCK_NONBLOCK | linux_abi::SOCK_CLOEXEC))
    } else {
        request.arg1
    }
}

/// Publishes newly-created/accepted sockets only through the reply that
/// requested it.  Netd keeps the initial `refs = 1` until this succeeds; on a
/// rejection the broker has removed its invisible descriptor and this service
/// performs the single provider-side discard.
pub(super) fn prepare_socket_publication(
    request: &NetdIpcRequest,
    response: &mut NetdIpcResponse,
    reply_cap: u64,
    socket_token: u64,
    second_socket_token: u64,
    channel_id: u64,
    accepted_domain: u64,
    accepted_type: u64,
    discard_on_failure: bool,
) -> i32 {
    let discard = |token| {
        if discard_on_failure {
            discard_unpublished_socket_tokens(token, second_socket_token, 0)
        }
    };
    if let Err(errno) = request_deadline_remaining_ns(request) {
        discard(socket_token);
        return errno;
    }
    if reply_cap == 0 || socket_token == 0 {
        discard(socket_token);
        return libc::EINVAL;
    }
    let (operation, domain, protocol) = if request.op == SYSCALL_OFFLOAD_OP_LINUX_ACCEPT {
        if accepted_domain == linux_abi::AF_UNIX {
            (
                SYSCALL_OFFLOAD_OP_LINUX_ACCEPT as u64,
                linux_abi::AF_UNIX,
                0,
            )
        } else {
            // INET accept publishes via SOCKET shape with channel_id == 0 so
            // kernel net_broker_socket_publication_shape_valid admits AF_INET/AF_INET6.
            (
                SYSCALL_OFFLOAD_OP_LINUX_SOCKET as u64,
                accepted_domain,
                0,
            )
        }
    } else {
        (request.op as u64, request.arg0, request.arg2)
    };
    let publication = NetBrokerPrepareSocketPublication {
        version: NET_BROKER_SOCKET_PUBLICATION_VERSION,
        reply_cap,
        caller_pid: request.pid,
        socket_token,
        operation,
        second_socket_token,
        channel_id,
        domain,
        socket_type: socket_publication_type(request, accepted_type),
        protocol,
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
    let prepared = settle_socket_publication_at(
        request,
        rustos_svc_runtime::syscall::monotonic_nanos(),
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
        discard,
    );
    if let Err(errno) = prepared {
        return errno;
    }
    // The compat receiver verifies this token against the one transferred by
    // the kernel, then overwrites it with the fd it installed after a timely
    // reply take.  No numeric fd crosses this broker boundary.
    response.value = socket_token;
    if second_socket_token != 0 {
        response.payload[..8].copy_from_slice(&second_socket_token.to_ne_bytes());
        response.payload_len = 8;
    }
    0
}

/// Settles ownership after a prepared-reply bind attempt.  The prepare closure
/// owns no provider cleanup; its failure is the one point at which netd still
/// owns and must discard the unpublished initial reference.
pub(super) fn settle_socket_publication_at(
    request: &NetdIpcRequest,
    now_ns: u64,
    socket_token: u64,
    prepare: impl FnOnce() -> Result<(), i32>,
    discard: impl FnOnce(u64),
) -> Result<(), i32> {
    match request_deadline_remaining_ns_at(request, now_ns).and_then(|_| prepare()) {
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

pub(super) fn discard_unpublished_socket_tokens(socket_token: u64, token_a: u64, token_b: u64) {
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
