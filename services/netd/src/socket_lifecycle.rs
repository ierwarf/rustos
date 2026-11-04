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
    if domain == linux_abi::AF_INET {
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
                refs: 1,
                options: SocketOptions {
                    send_buffer: INET_TCP_BUFFER_CAPACITY as i32,
                    recv_buffer: INET_TCP_BUFFER_CAPACITY as i32,
                    ..SocketOptions::default()
                },
                transport,
                local: None,
                peer: None,
                received: None,
                errors: None,
                connect_deadline: None,
            },
        );
        drop(state);
        return prepare_socket_publication(request, response, reply_cap, token, 0, 0, true);
    }
    if !rustos_user_abi::syscall::net_broker_unix_socket_shape_valid(
        domain,
        socket_type,
        request.arg2,
    ) {
        return libc::EAFNOSUPPORT;
    }

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
    prepare_socket_publication(request, response, reply_cap, token, 0, 0, true)
}

pub(super) fn handle_socketpair(
    request: &NetdIpcRequest,
    response: &mut NetdIpcResponse,
    reply_cap: u64,
) -> i32 {
    if !rustos_user_abi::syscall::net_broker_unix_socket_shape_valid(
        request.arg0,
        request.arg1,
        request.arg2,
    ) {
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
                pending_listener: None,
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
                pending_listener: None,
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
    prepare_socket_publication(request, response, reply_cap, left, right, 0, true)
}

pub(super) fn handle_close(request: &NetdIpcRequest, response: &mut NetdIpcResponse) -> i32 {
    let mut state = lock_net_state();
    if let Some(socket) = state.inet_sockets.get_mut(&request.socket_token) {
        if socket.refs > 1 {
            socket.refs -= 1;
            response.value = socket.refs as u64;
            return 0;
        }
        let Some(socket) = state.inet_sockets.remove(&request.socket_token) else {
            return libc::EBADF;
        };
        state.readiness_generations.remove(&request.socket_token);
        if let Some(stack) = state.inet.as_mut() {
            stack.remove(socket.transport);
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
            UnixSocketState::Idle => {}
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
    if inet_socket_exists(request.socket_token) {
        return inet_datagram::bind(request);
    }
    let path = match sockaddr_path_from_payload(request) {
        Ok(path) => path,
        Err(errno) => return errno,
    };
    if is_wayland_path(path.as_str()) {
        debug_line("netd: wayland bind");
    }
    let mut state = lock_net_state();
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

pub(super) fn handle_listen(request: &NetdIpcRequest) -> i32 {
    let mut state = lock_net_state();
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

pub(super) fn handle_connect(request: &NetdIpcRequest) -> i32 {
    if inet_datagram::is_datagram(request.socket_token) {
        return inet_datagram::connect(request);
    }
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
    let mut state = lock_net_state();
    let Some(listener_token) = state.bindings.get(&path).copied() else {
        return libc::ENOENT;
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
        if is_wayland {
            debug_line("netd: wayland connect queued");
        }
        listener.local_path.clone()
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
    // accept4 flags control the NEW descriptor, not the listener's wait mode.
    let nonblocking = request.status_flags & linux_abi::O_NONBLOCK != 0;
    let accepted = {
        let mut state = lock_net_state();
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
    let mut charged = false;
    let result = (|| {
        let channel_id = {
            let state = lock_net_state();
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
            let state = lock_net_state();
            if let Some(socket) = state.sockets.get(&accepted) {
                if let Err(errno) =
                    sockaddr_payload(socket.peer_path.as_deref().unwrap_or(""), response)
                {
                    return errno;
                }
            }
        }
        // An accepted endpoint is not funded merely because it is queued.
        // Charge the client only at the public accept handoff boundary.
        if let Err(errno) = provider_accounting::charge_before_publish(accepted, request.pid) {
            return errno;
        }
        charged = true;
        prepare_socket_publication(request, response, reply_cap, accepted, 0, channel_id, false)
    })();
    if result != 0 && charged {
        // Mark refund BEFORE requeue. Another accept cannot charge this token
        // until the exact old nonce has settled, even after a lost ACK.
        let _ = provider_accounting::retire_final(accepted);
    }
    let (retired, _peers, tokens) = {
        let mut state = lock_net_state();
        settle_unix_accept(&mut state, request.socket_token, accepted, result == 0)
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

pub(super) fn is_wayland_path(path: &str) -> bool {
    path.ends_with("/wayland-0") || path == "wayland-0"
}

/// Linux accept does not inherit the listener's open-description flags.
pub(super) fn socket_publication_type(request: &NetdIpcRequest) -> u64 {
    if request.op == SYSCALL_OFFLOAD_OP_LINUX_ACCEPT {
        linux_abi::SOCK_STREAM
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
    let publication = NetBrokerPrepareSocketPublication {
        version: NET_BROKER_SOCKET_PUBLICATION_VERSION,
        reply_cap,
        caller_pid: request.pid,
        socket_token,
        operation: request.op as u64,
        second_socket_token,
        channel_id,
        domain: if request.op == SYSCALL_OFFLOAD_OP_LINUX_ACCEPT {
            linux_abi::AF_UNIX
        } else {
            request.arg0
        },
        socket_type: socket_publication_type(request),
        protocol: if request.op == SYSCALL_OFFLOAD_OP_LINUX_ACCEPT {
            0
        } else {
            request.arg2
        },
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
