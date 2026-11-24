//! Socket creation, close/retirement, bind/listen/connect/accept and broker publication for INET and netlink sockets.
//!
//! - **Owner:** `netd` owns socket identity and queue custody; the kernel broker owns the target process descriptor table.
//! - **Boundary:** request tokens, flags and accept channels are untrusted; publication binds the exact live reply and operation.
//! - **Lifecycle:** socket and accept are charged, published through a reply-bound record, then referenced until final close retires them.
//! - **Concurrency:** table mutation happens under the network-state mutex; rootd charges run on the lifecycle pool with no lock held.
//! - **Failure:** a rejected binding leaves discard with netd; an accepted-then-abandoned socket is handed to the kernel transfer-drop owner and never requeued.
//! - **Forbidden:** no direct target fd installation, no listener flag inheritance onto an accepted socket, no unbounded socket table.
//! - **Evidence:** `tests/packet_provider_state_tests.rs`, the accept flag cases in kernel `ipc_helpers/tests.rs` and the netd lifecycle mutants.

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
        return prepare_socket_publication(request, response, reply_cap, token, domain, 0, true);
    }
    if let Err(errno) = netlink_socket_admission(domain, socket_type, request.arg2) {
        return errno;
    }
    let token = match mint_socket_token() {
        Ok(token) => token,
        Err(errno) => return errno,
    };
    if let Err(errno) = provider_accounting::charge_before_publish(token, request.pid) {
        return errno;
    }
    let mut state = lock_net_state();
    if !state.netlink_capacity_available() {
        drop(state);
        let _ = provider_accounting::retire_final(token);
        return libc::ENFILE;
    }
    if !state.token_available(token) {
        drop(state);
        let _ = provider_accounting::retire_final(token);
        return libc::EAGAIN;
    }
    state.netlink_sockets.insert(
        token,
        NetlinkSocket {
            refs: 1,
            options: SocketOptions::default(),
            state: netlink::NetlinkState::new(base_type, request.arg2),
        },
    );
    drop(state);
    prepare_socket_publication(request, response, reply_cap, token, domain, 0, true)
}

/// `socketpair` is an `AF_UNIX` operation and `AF_UNIX` is kernel-local, so
/// netd owns no socket family that supports it.
pub(super) fn handle_socketpair(request: &NetdIpcRequest) -> i32 {
    if request.arg0 == linux_abi::AF_INET
        || request.arg0 == linux_abi::AF_INET6
        || request.arg0 == libc::AF_NETLINK as u64
    {
        libc::EOPNOTSUPP
    } else {
        libc::EAFNOSUPPORT
    }
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
    let Some(socket) = state.netlink_sockets.get_mut(&request.socket_token) else {
        return libc::EBADF;
    };
    if socket.refs > 1 {
        socket.refs -= 1;
        response.value = socket.refs as u64;
        return 0;
    }
    retire_netlink_socket(&mut state, request.socket_token);
    drop(state);
    provider_accounting::retire_final(request.socket_token)
        .err()
        .unwrap_or(0)
}

/// Withdraws a netlink identity from the table and its readiness bookkeeping.
pub(super) fn retire_netlink_socket(state: &mut NetState, token: u64) -> Option<NetlinkSocket> {
    let socket = state.netlink_sockets.remove(&token)?;
    state.readiness_generations.remove(&token);
    state.readiness_observations.remove(&token);
    Some(socket)
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
    let mut state = lock_net_state();
    if let Some(inet) = state.inet_sockets.get(&request.socket_token) {
        if inet.transport.is_datagram() {
            return inet_datagram::bind_request_in_state(&mut state, request);
        }
        return inet_tcp::bind_tcp_in_state(&mut state, request);
    }
    if !state.netlink_sockets.contains_key(&request.socket_token) {
        return libc::EBADF;
    }
    drop(state);
    netlink::handle_bind(request)
}

/// Linux `socket(2)` admission for the netlink sockets netd owns in Ring3.
/// Unsupported *types* of a supported family are `ESOCKTNOSUPPORT`, not
/// `EAFNOSUPPORT`; `AF_UNIX` is kernel-local and is not a netd family.
pub(super) fn netlink_socket_admission(
    domain: u64,
    socket_type: u64,
    protocol: u64,
) -> Result<(), i32> {
    if domain != libc::AF_NETLINK as u64 {
        return Err(libc::EAFNOSUPPORT);
    }
    if socket_type & !(linux_abi::SOCK_TYPE_MASK | linux_abi::SOCK_NONBLOCK | linux_abi::SOCK_CLOEXEC)
        != 0
    {
        return Err(libc::EINVAL);
    }
    let base_type = socket_type & linux_abi::SOCK_TYPE_MASK;
    if base_type != libc::SOCK_RAW as u64 && base_type != libc::SOCK_DGRAM as u64 {
        return Err(libc::ESOCKTNOSUPPORT);
    }
    // netd implements NETLINK_ROUTE (0) and NETLINK_KOBJECT_UEVENT (15).
    if !netlink::is_supported_protocol(protocol) {
        return Err(libc::EPROTONOSUPPORT);
    }
    // The published shape is the kernel broker's admission predicate; the two
    // must agree or the publication would be refused after charging.
    if rustos_user_abi::syscall::net_broker_netlink_socket_shape_valid(domain, socket_type, protocol)
        || rustos_user_abi::syscall::net_broker_netlink_socket_shape_valid(domain, socket_type, 0)
    {
        Ok(())
    } else {
        Err(libc::EINVAL)
    }
}

pub(super) fn handle_listen(request: &NetdIpcRequest) -> i32 {
    let mut state = lock_net_state();
    if state.inet_sockets.contains_key(&request.socket_token) {
        return inet_tcp::listen_tcp_in_state(&mut state, request.socket_token, request.arg1);
    }
    if state.netlink_sockets.contains_key(&request.socket_token) {
        libc::EOPNOTSUPP
    } else {
        libc::EBADF
    }
}

pub(super) fn handle_connect(request: &NetdIpcRequest) -> i32 {
    let mut state = lock_net_state();
    if let Some(inet) = state.inet_sockets.get(&request.socket_token) {
        if inet.transport.is_datagram() {
            return inet_datagram::connect_in_state(&mut state, request);
        }
        drop(state);
        return begin_inet_connect(request);
    }
    if !state.netlink_sockets.contains_key(&request.socket_token) {
        return libc::EBADF;
    }
    drop(state);
    // The only netlink peer is the kernel (nl_pid 0): a no-op.
    netlink::handle_connect(request)
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
    // Only INET stream sockets accept; netlink sockets are datagram sockets.
    if lock_net_state().netlink_sockets.contains_key(&request.socket_token) {
        libc::EOPNOTSUPP
    } else {
        libc::EBADF
    }
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
    accepted_domain: u64,
    accepted_type: u64,
    discard_on_failure: bool,
) -> i32 {
    let discard = |token| {
        if discard_on_failure {
            discard_unpublished_socket_token(token)
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
        // INET accept publishes via SOCKET shape with channel_id == 0 so
        // kernel net_broker_socket_publication_shape_valid admits AF_INET/AF_INET6.
        (SYSCALL_OFFLOAD_OP_LINUX_SOCKET as u64, accepted_domain, 0)
    } else {
        (request.op as u64, request.arg0, request.arg2)
    };
    let publication = NetBrokerPrepareSocketPublication {
        version: NET_BROKER_SOCKET_PUBLICATION_VERSION,
        reply_cap,
        caller_pid: request.pid,
        socket_token,
        operation,
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

pub(super) fn discard_unpublished_socket_token(socket_token: u64) {
    if socket_token == 0 {
        return;
    }
    let request = NetdIpcRequest {
        socket_token,
        ..NetdIpcRequest::default()
    };
    let _ = handle_close(&request, &mut NetdIpcResponse::default());
}
