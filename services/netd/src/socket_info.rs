//! Socket names, options and shutdown.

use super::*;

pub(super) fn handle_getsockname(request: &NetdIpcRequest, response: &mut NetdIpcResponse) -> i32 {
    let state = lock_net_state();
    if state.inet_sockets.contains_key(&request.socket_token) {
        return inet_datagram::socket_name_in_state(&state, request, response, false);
    }
    let Some(socket) = state.sockets.get(&request.socket_token) else {
        return libc::EBADF;
    };
    if let UnixSocketState::Netlink(netlink) = &socket.state {
        return netlink.write_sockaddr(response);
    }
    sockaddr_payload(socket.local_path.as_ref(), response).unwrap_or_else(|errno| errno)
}

pub(super) fn handle_getpeername(request: &NetdIpcRequest, response: &mut NetdIpcResponse) -> i32 {
    let state = lock_net_state();
    if state.inet_sockets.contains_key(&request.socket_token) {
        return inet_datagram::socket_name_in_state(&state, request, response, true);
    }
    let Some(socket) = state.sockets.get(&request.socket_token) else {
        return libc::EBADF;
    };
    if let UnixSocketState::Netlink(_) = &socket.state {
        // The only netlink peer is the kernel: nl_pid 0, no groups.
        return netlink::write_kernel_sockaddr(response);
    }
    let has_peer = matches!(&socket.state, UnixSocketState::Connected(_))
        || matches!(&socket.state, UnixSocketState::Dgram(d) if d.peer.is_some());
    // A connected endpoint with an unnamed peer (socketpair, unbound client)
    // still has a peer: it reports the unnamed address, not ENOTCONN.
    if socket.peer_path.is_none() && !has_peer {
        return libc::ENOTCONN;
    }
    sockaddr_payload(socket.peer_path.as_ref(), response).unwrap_or_else(|errno| errno)
}

fn write_i32_opt(response: &mut NetdIpcResponse, value: i32) -> i32 {
    response.payload[..size_of::<i32>()].copy_from_slice(&value.to_ne_bytes());
    response.payload_len = size_of::<i32>() as u32;
    response.value = 0;
    0
}

fn write_timeval_opt(response: &mut NetdIpcResponse, tv: (i64, i64)) -> i32 {
    response.payload[0..8].copy_from_slice(&tv.0.to_ne_bytes());
    response.payload[8..16].copy_from_slice(&tv.1.to_ne_bytes());
    response.payload_len = TIMEVAL_SIZE as u32;
    response.value = 0;
    0
}

pub(super) fn handle_getsockopt(request: &NetdIpcRequest, response: &mut NetdIpcResponse) -> i32 {
    if request.arg1 == netlink::SOL_NETLINK {
        return netlink::handle_getsockopt(request, response);
    }
    let mut state = lock_net_state();
    handle_getsockopt_in_state(&mut state, request, response)
}

pub(super) fn handle_getsockopt_in_state(
    state: &mut NetState,
    request: &NetdIpcRequest,
    response: &mut NetdIpcResponse,
) -> i32 {
    if (request.arg1 == libc::SOL_IP as u64 && request.arg2 == libc::IP_RECVERR as u64)
        || (request.arg1 == linux_abi::IPPROTO_IPV6 && request.arg2 == linux_abi::IPV6_RECVERR)
    {
        return inet_datagram::get_ip_option_in_state(state, request, response);
    }
    if state.inet_sockets.contains_key(&request.socket_token) {
        let pending = if request.arg1 == linux_abi::SOL_SOCKET
            && request.arg2 == linux_abi::SO_ERROR
        {
            take_inet_socket_error(state, request.socket_token)
        } else {
            0
        };
        let socket = &state.inet_sockets[&request.socket_token];
        match request.arg1 {
            linux_abi::SOL_SOCKET => {
                let value = match request.arg2 {
                    linux_abi::SO_ERROR => pending,
                    linux_abi::SO_TYPE => socket.transport.socket_type(),
                    linux_abi::SO_DOMAIN => socket.domain as i32,
                    linux_abi::SO_PROTOCOL => socket.transport.protocol(),
                    linux_abi::SO_SNDBUF => socket.options.send_buffer,
                    linux_abi::SO_RCVBUF => socket.options.recv_buffer,
                    linux_abi::SO_KEEPALIVE => socket.options.keepalive as i32,
                    linux_abi::SO_REUSEADDR => socket.options.reuse_addr as i32,
                    linux_abi::SO_REUSEPORT => socket.options.reuse_port as i32,
                    linux_abi::SO_BROADCAST => socket.options.broadcast as i32,
                    SO_OOBINLINE => i32::from(socket.options.oobinline),
                    SO_PRIORITY => socket.options.priority,
                    SO_TIMESTAMP => i32::from(socket.options.timestamp),
                    SO_TIMESTAMPNS => i32::from(socket.options.timestampns),
                    linux_abi::SO_LINGER => {
                        response.payload[0..4]
                            .copy_from_slice(&i32::from(socket.options.linger.0).to_ne_bytes());
                        response.payload[4..8]
                            .copy_from_slice(&socket.options.linger.1.to_ne_bytes());
                        response.payload_len = 8;
                        response.value = 0;
                        return 0;
                    }
                    linux_abi::SO_RCVTIMEO => {
                        return write_timeval_opt(response, socket.options.recv_timeout);
                    }
                    linux_abi::SO_SNDTIMEO => {
                        return write_timeval_opt(response, socket.options.snd_timeout);
                    }
                    linux_abi::SO_ACCEPTCONN => {
                        if socket.tcp_listener.is_some() {
                            1
                        } else {
                            match socket.transport {
                                InetTransport::Tcp(tcp_handle) => i32::from(
                                    state
                                        .inet
                                        .as_ref()
                                        .is_some_and(|s| {
                                            s.sockets.get::<tcp::Socket>(tcp_handle).is_listening()
                                        }),
                                ),
                                _ => 0,
                            }
                        }
                    }
                    _ => return libc::EOPNOTSUPP,
                };
                return write_i32_opt(response, value);
            }
            val if val == linux_abi::SOL_IP as u64 => {
                let value = match request.arg2 {
                    linux_abi::IP_TOS => i32::from(socket.options.ip_tos),
                    linux_abi::IP_TTL => socket.options.ip_ttl,
                    linux_abi::IP_PKTINFO => i32::from(socket.options.ip_pktinfo),
                    linux_abi::IP_RECVTOS => i32::from(socket.options.ip_recvtos),
                    IP_MULTICAST_IF => {
                        response.payload[..4].copy_from_slice(&socket.options.ip_multicast_if);
                        response.payload_len = 4;
                        response.value = 0;
                        return 0;
                    }
                    IP_MULTICAST_TTL => socket.options.ip_multicast_ttl,
                    IP_MULTICAST_LOOP => i32::from(socket.options.ip_multicast_loop),
                    _ => return libc::ENOPROTOOPT,
                };
                return write_i32_opt(response, value);
            }
            linux_abi::IPPROTO_IPV6 if socket.domain == linux_abi::AF_INET6 => {
                let value = match request.arg2 {
                    linux_abi::IPV6_V6ONLY => i32::from(socket.options.ipv6_only),
                    linux_abi::IPV6_TCLASS => socket.options.ipv6_tclass,
                    linux_abi::IPV6_RECVPKTINFO => i32::from(socket.options.ipv6_recvpktinfo),
                    IPV6_MULTICAST_IF => socket.options.ipv6_multicast_if,
                    IPV6_MULTICAST_HOPS => socket.options.ipv6_multicast_hops,
                    IPV6_MULTICAST_LOOP => i32::from(socket.options.ipv6_multicast_loop),
                    _ => return libc::ENOPROTOOPT,
                };
                return write_i32_opt(response, value);
            }
            linux_abi::SOL_TCP => {
                let InetTransport::Tcp(tcp_handle) = socket.transport else {
                    return libc::ENOPROTOOPT;
                };
                let value = match request.arg2 {
                    val if val == libc::TCP_NODELAY as u64 => {
                        let nagle_disabled = state
                            .inet
                            .as_ref()
                            .map(|s| !s.sockets.get::<tcp::Socket>(tcp_handle).nagle_enabled())
                            .unwrap_or(socket.options.nodelay);
                        i32::from(nagle_disabled)
                    }
                    TCP_CORK => i32::from(socket.options.tcp_cork),
                    val if val == linux_abi::TCP_KEEPIDLE => socket.options.tcp_keepidle,
                    val if val == linux_abi::TCP_KEEPINTVL => socket.options.tcp_keepintvl,
                    val if val == linux_abi::TCP_KEEPCNT => socket.options.tcp_keepcnt,
                    TCP_DEFER_ACCEPT => socket.options.tcp_defer_accept,
                    TCP_INFO => {
                        return write_tcp_info_opt(state.inet.as_ref(), socket, tcp_handle, response);
                    }
                    TCP_QUICKACK => i32::from(socket.options.tcp_quickack),
                    val if val == linux_abi::TCP_USER_TIMEOUT => socket.options.tcp_user_timeout as i32,
                    _ => return libc::ENOPROTOOPT,
                };
                return write_i32_opt(response, value);
            }
            _ => return libc::ENOPROTOOPT,
        }
    }
    let Some(socket) = state.sockets.get(&request.socket_token) else {
        return libc::EBADF;
    };
    if request.arg1 != linux_abi::SOL_SOCKET {
        return libc::ENOPROTOOPT;
    }
    let value = match request.arg2 {
        linux_abi::SO_ERROR => 0_i32,
        linux_abi::SO_TYPE => match &socket.state {
            UnixSocketState::Netlink(netlink) => netlink.sock_type() as i32,
            _ => socket.kind.unix_socket_type() as i32,
        },
        linux_abi::SO_DOMAIN => match socket.kind {
            UnixSocketKind::Netlink => libc::AF_NETLINK,
            _ => linux_abi::AF_UNIX as i32,
        },
        linux_abi::SO_PROTOCOL => match &socket.state {
            UnixSocketState::Netlink(netlink) => netlink.protocol() as i32,
            _ => 0_i32,
        },
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
        linux_abi::SO_BROADCAST => socket.options.broadcast as i32,
        SO_OOBINLINE => i32::from(socket.options.oobinline),
        SO_PRIORITY => socket.options.priority,
        SO_TIMESTAMP => i32::from(socket.options.timestamp),
        SO_TIMESTAMPNS => i32::from(socket.options.timestampns),
        linux_abi::SO_LINGER => {
            response.payload[0..4]
                .copy_from_slice(&i32::from(socket.options.linger.0).to_ne_bytes());
            response.payload[4..8].copy_from_slice(&socket.options.linger.1.to_ne_bytes());
            response.payload_len = 8;
            response.value = 0;
            return 0;
        }
        linux_abi::SO_SNDBUF => socket.options.send_buffer,
        linux_abi::SO_RCVBUF => socket.options.recv_buffer,
        linux_abi::SO_PASSCRED => socket.options.passcred as i32,
        linux_abi::SO_PEERCRED => return peercred_payload_from_socket(socket, response),
        SO_RCVTIMEO => return write_timeval_opt(response, socket.options.recv_timeout),
        linux_abi::SO_SNDTIMEO => return write_timeval_opt(response, socket.options.snd_timeout),
        _ => return libc::EOPNOTSUPP,
    };
    write_i32_opt(response, value)
}

pub(super) fn take_inet_socket_error(state: &mut NetState, token: u64) -> i32 {
    let socket = &state.inet_sockets[&token];
    if socket.transport.is_datagram() {
        inet_datagram::take_socket_error(state, token)
    } else if socket.connect_deadline.is_some() {
        // A pending active open takes precedence over established resets.
        match inet_connect_status(state, token) {
            libc::EINPROGRESS => 0,
            errno => errno,
        }
    } else {
        inet_tcp::take_tcp_socket_error(state, token)
    }
}

fn parse_timeval(request: &NetdIpcRequest) -> Result<(i64, i64), i32> {
    if (request.payload_len as usize) < TIMEVAL_SIZE {
        return Err(libc::EINVAL);
    }
    let seconds = i64::from_ne_bytes(request.payload[0..8].try_into().unwrap_or_default());
    let micros = i64::from_ne_bytes(request.payload[8..16].try_into().unwrap_or_default());
    if !(0..1_000_000).contains(&micros) {
        return Err(libc::EDOM);
    }
    Ok(if seconds < 0 { (0, 0) } else { (seconds, micros) })
}

pub(super) fn handle_setsockopt(request: &NetdIpcRequest) -> i32 {
    if request.arg1 == netlink::SOL_NETLINK {
        return netlink::handle_setsockopt(request);
    }
    let mut state = lock_net_state();
    handle_setsockopt_in_state(&mut state, request)
}

pub(super) fn handle_setsockopt_in_state(state: &mut NetState, request: &NetdIpcRequest) -> i32 {
    if (request.arg1 == libc::SOL_IP as u64 && request.arg2 == libc::IP_RECVERR as u64)
        || (request.arg1 == linux_abi::IPPROTO_IPV6 && request.arg2 == linux_abi::IPV6_RECVERR)
    {
        return inet_datagram::set_ip_option_in_state(state, request);
    }
    if request.arg1 == linux_abi::SOL_SOCKET
        && matches!(request.arg2, SO_RCVTIMEO | linux_abi::SO_SNDTIMEO)
    {
        let tv = match parse_timeval(request) {
            Ok(tv) => tv,
            Err(errno) => return errno,
        };
        if let Some(socket) = state.inet_sockets.get_mut(&request.socket_token) {
            if request.arg2 == SO_RCVTIMEO {
                socket.options.recv_timeout = tv;
            } else {
                socket.options.snd_timeout = tv;
            }
            return 0;
        }
        let Some(socket) = state.sockets.get_mut(&request.socket_token) else {
            return libc::EBADF;
        };
        if request.arg2 == SO_RCVTIMEO {
            socket.options.recv_timeout = tv;
        } else {
            socket.options.snd_timeout = tv;
        }
        return 0;
    }
    if request.arg1 == linux_abi::SOL_SOCKET && request.arg2 == linux_abi::SO_LINGER {
        if (request.payload_len as usize) < 8 {
            return libc::EINVAL;
        }
        let onoff = i32::from_ne_bytes(request.payload[0..4].try_into().unwrap_or_default()) != 0;
        let linger = i32::from_ne_bytes(request.payload[4..8].try_into().unwrap_or_default());
        if let Some(socket) = state.inet_sockets.get_mut(&request.socket_token) {
            socket.options.linger = (onoff, linger);
            return 0;
        }
        let Some(socket) = state.sockets.get_mut(&request.socket_token) else {
            return libc::EBADF;
        };
        socket.options.linger = (onoff, linger);
        return 0;
    }
    if request.arg1 == linux_abi::SOL_IP as u64
        && matches!(
            request.arg2,
            IP_MULTICAST_IF
                | IP_MULTICAST_TTL
                | IP_MULTICAST_LOOP
                | IP_ADD_MEMBERSHIP
                | IP_DROP_MEMBERSHIP
        )
    {
        return set_ipv4_multicast_option(state, request);
    }
    if request.arg1 == linux_abi::IPPROTO_IPV6
        && matches!(request.arg2, IPV6_JOIN_GROUP | IPV6_LEAVE_GROUP)
    {
        return set_ipv6_multicast_membership(state, request);
    }
    let Some(value) = request_i32_payload(request) else {
        return libc::EINVAL;
    };
    if state.inet_sockets.contains_key(&request.socket_token) {
        match request.arg1 {
            linux_abi::SOL_SOCKET => {
                let Some(socket) = state.inet_sockets.get_mut(&request.socket_token) else {
                    return libc::EBADF;
                };
                match request.arg2 {
                    linux_abi::SO_KEEPALIVE => {
                        socket.options.keepalive = value != 0;
                        let keepidle = socket.options.tcp_keepidle.max(1) as u64;
                        let transport = socket.transport;
                        if let InetTransport::Tcp(handle) = transport {
                            let Some(stack) = state.inet.as_mut() else {
                                return libc::ENODEV;
                            };
                            stack
                                .sockets
                                .get_mut::<tcp::Socket>(handle)
                                .set_keep_alive(
                                    (value != 0).then_some(smoltcp::time::Duration::from_secs(keepidle)),
                                );
                        }
                    }
                    linux_abi::SO_REUSEADDR => socket.options.reuse_addr = value != 0,
                    linux_abi::SO_REUSEPORT => socket.options.reuse_port = value != 0,
                    linux_abi::SO_BROADCAST => socket.options.broadcast = value != 0,
                    SO_OOBINLINE => socket.options.oobinline = value != 0,
                    SO_PRIORITY => socket.options.priority = value,
                    SO_TIMESTAMP => socket.options.timestamp = value != 0,
                    SO_TIMESTAMPNS => socket.options.timestampns = value != 0,
                    linux_abi::SO_SNDBUF | linux_abi::SO_RCVBUF => {
                        let clamped = value.clamp(4096, MAX_INET_TCP_BUFFER_CAPACITY as i32);
                        if request.arg2 == linux_abi::SO_SNDBUF {
                            socket.options.send_buffer = clamped;
                        } else {
                            socket.options.recv_buffer = clamped;
                        }
                        let transport = socket.transport;
                        let new_rx = socket.options.recv_buffer as usize;
                        let new_tx = socket.options.send_buffer as usize;
                        let options = socket.options;
                        if let InetTransport::Tcp(handle) = transport {
                            if let Some(stack) = state.inet.as_mut() {
                                let tcp_sock = stack.sockets.get_mut::<tcp::Socket>(handle);
                                if tcp_sock.state() == tcp::State::Closed {
                                    *tcp_sock = tcp::Socket::new(
                                        tcp::SocketBuffer::new(vec![0; new_rx]),
                                        tcp::SocketBuffer::new(vec![0; new_tx]),
                                    );
                                    // The replacement socket starts from smoltcp
                                    // defaults: restore NODELAY/KEEPALIVE/TTL.
                                    inet_tcp::configure_accepted_or_listening_tcp(
                                        tcp_sock, options,
                                    );
                                }
                            }
                        }
                    }
                    _ => return libc::EOPNOTSUPP,
                }
                return 0;
            }
            val if val == linux_abi::SOL_IP as u64 => {
                let Some(socket) = state.inet_sockets.get_mut(&request.socket_token) else {
                    return libc::EBADF;
                };
                match request.arg2 {
                    linux_abi::IP_TOS => socket.options.ip_tos = (value & 0xff) as u8,
                    linux_abi::IP_TTL => {
                        let ttl = if value == -1 {
                            64
                        } else if (1..=255).contains(&value) {
                            value
                        } else {
                            return libc::EINVAL;
                        };
                        socket.options.ip_ttl = ttl;
                        if let InetTransport::Tcp(handle) = socket.transport {
                            if let Some(stack) = state.inet.as_mut() {
                                stack
                                    .sockets
                                    .get_mut::<tcp::Socket>(handle)
                                    .set_hop_limit(Some(ttl as u8));
                            }
                        }
                    }
                    linux_abi::IP_PKTINFO => socket.options.ip_pktinfo = value != 0,
                    linux_abi::IP_RECVTOS => socket.options.ip_recvtos = value != 0,
                    _ => return libc::ENOPROTOOPT,
                }
                return 0;
            }
            linux_abi::IPPROTO_IPV6 => {
                let Some(socket) = state.inet_sockets.get_mut(&request.socket_token) else {
                    return libc::EBADF;
                };
                if socket.domain != linux_abi::AF_INET6 {
                    return libc::ENOPROTOOPT;
                }
                match request.arg2 {
                    linux_abi::IPV6_V6ONLY => {
                        if socket.local.is_some() {
                            return libc::EINVAL;
                        }
                        socket.options.ipv6_only = value != 0;
                    }
                    linux_abi::IPV6_TCLASS => {
                        if value == -1 {
                            socket.options.ipv6_tclass = 0;
                        } else if (0..=255).contains(&value) {
                            socket.options.ipv6_tclass = value;
                        } else {
                            return libc::EINVAL;
                        }
                    }
                    linux_abi::IPV6_RECVPKTINFO => socket.options.ipv6_recvpktinfo = value != 0,
                    IPV6_MULTICAST_IF => {
                        if value < 0 {
                            return libc::EINVAL;
                        }
                        socket.options.ipv6_multicast_if = value;
                    }
                    IPV6_MULTICAST_HOPS => {
                        if value == -1 {
                            socket.options.ipv6_multicast_hops = 1;
                        } else if (0..=255).contains(&value) {
                            socket.options.ipv6_multicast_hops = value;
                        } else {
                            return libc::EINVAL;
                        }
                    }
                    IPV6_MULTICAST_LOOP => socket.options.ipv6_multicast_loop = value != 0,
                    _ => return libc::ENOPROTOOPT,
                }
                return 0;
            }
            linux_abi::SOL_TCP => {
                let Some(socket) = state.inet_sockets.get_mut(&request.socket_token) else {
                    return libc::EBADF;
                };
                let InetTransport::Tcp(tcp_handle) = socket.transport else {
                    return libc::ENOPROTOOPT;
                };
                match request.arg2 {
                    val if val == libc::TCP_NODELAY as u64 => {
                        socket.options.nodelay = value != 0;
                        let Some(stack) = state.inet.as_mut() else {
                            return libc::ENODEV;
                        };
                        stack
                            .sockets
                            .get_mut::<tcp::Socket>(tcp_handle)
                            .set_nagle_enabled(value == 0);
                    }
                    TCP_CORK => socket.options.tcp_cork = value != 0,
                    val if val == linux_abi::TCP_KEEPIDLE => {
                        if !(1..=32767).contains(&value) {
                            return libc::EINVAL;
                        }
                        socket.options.tcp_keepidle = value;
                        if socket.options.keepalive {
                            if let Some(stack) = state.inet.as_mut() {
                                stack
                                    .sockets
                                    .get_mut::<tcp::Socket>(tcp_handle)
                                    .set_keep_alive(Some(smoltcp::time::Duration::from_secs(
                                        value as u64,
                                    )));
                            }
                        }
                    }
                    val if val == linux_abi::TCP_KEEPINTVL => {
                        if !(1..=32767).contains(&value) {
                            return libc::EINVAL;
                        }
                        socket.options.tcp_keepintvl = value;
                    }
                    val if val == linux_abi::TCP_KEEPCNT => {
                        if !(1..=127).contains(&value) {
                            return libc::EINVAL;
                        }
                        socket.options.tcp_keepcnt = value;
                    }
                    TCP_DEFER_ACCEPT => socket.options.tcp_defer_accept = value.max(0),
                    TCP_QUICKACK => socket.options.tcp_quickack = value != 0,
                    val if val == linux_abi::TCP_USER_TIMEOUT => {
                        if value < 0 {
                            return libc::EINVAL;
                        }
                        socket.options.tcp_user_timeout = value as u32;
                    }
                    _ => return libc::ENOPROTOOPT,
                }
                return 0;
            }
            _ => return libc::ENOPROTOOPT,
        }
    }
    let Some(socket) = state.sockets.get_mut(&request.socket_token) else {
        return libc::EBADF;
    };
    if request.arg1 != linux_abi::SOL_SOCKET {
        return libc::ENOPROTOOPT;
    }
    match request.arg2 {
        linux_abi::SO_REUSEADDR => socket.options.reuse_addr = value != 0,
        linux_abi::SO_REUSEPORT => socket.options.reuse_port = value != 0,
        linux_abi::SO_KEEPALIVE => socket.options.keepalive = value != 0,
        linux_abi::SO_BROADCAST => socket.options.broadcast = value != 0,
        SO_OOBINLINE => socket.options.oobinline = value != 0,
        SO_PRIORITY => socket.options.priority = value,
        SO_TIMESTAMP => socket.options.timestamp = value != 0,
        SO_TIMESTAMPNS => socket.options.timestampns = value != 0,
        linux_abi::SO_PASSCRED => socket.options.passcred = value != 0,
        linux_abi::SO_SNDBUF => socket.options.send_buffer = clamp_socket_buffer(value),
        linux_abi::SO_RCVBUF => socket.options.recv_buffer = clamp_socket_buffer(value),
        SO_SNDBUFFORCE => {
            if request.euid != 0 {
                return libc::EPERM;
            }
            socket.options.send_buffer = clamp_socket_buffer(value);
        }
        SO_RCVBUFFORCE => {
            if request.euid != 0 {
                return libc::EPERM;
            }
            socket.options.recv_buffer = clamp_socket_buffer(value);
        }
        _ => return libc::EOPNOTSUPP,
    }
    0
}

const SO_OOBINLINE: u64 = 10;
const SO_PRIORITY: u64 = 12;
const SO_RCVTIMEO: u64 = 20;
const SO_TIMESTAMP: u64 = 29;
const SO_SNDBUFFORCE: u64 = 32;
const SO_RCVBUFFORCE: u64 = 33;
const SO_TIMESTAMPNS: u64 = 35;
const TIMEVAL_SIZE: usize = 16;

const IP_MULTICAST_IF: u64 = 32;
const IP_MULTICAST_TTL: u64 = 33;
const IP_MULTICAST_LOOP: u64 = 34;
const IP_ADD_MEMBERSHIP: u64 = 35;
const IP_DROP_MEMBERSHIP: u64 = 36;

const IPV6_MULTICAST_IF: u64 = 17;
const IPV6_MULTICAST_HOPS: u64 = 18;
const IPV6_MULTICAST_LOOP: u64 = 19;
const IPV6_JOIN_GROUP: u64 = 20;
const IPV6_LEAVE_GROUP: u64 = 21;

const TCP_CORK: u64 = 3;
const TCP_DEFER_ACCEPT: u64 = 9;
const TCP_INFO: u64 = 11;
const TCP_QUICKACK: u64 = 12;

const TCP_INFO_LEN: usize = 104;

fn set_ipv4_multicast_option(state: &mut NetState, request: &NetdIpcRequest) -> i32 {
    let Some(socket) = state.inet_sockets.get_mut(&request.socket_token) else {
        return if state.sockets.contains_key(&request.socket_token) {
            libc::ENOPROTOOPT
        } else {
            libc::EBADF
        };
    };
    let len = request.payload_len as usize;
    match request.arg2 {
        IP_MULTICAST_IF => {
            // Linux accepts struct ip_mreqn (12), struct ip_mreq (8), or struct in_addr (4).
            let addr: [u8; 4] = if len >= 8 {
                request.payload[4..8].try_into().unwrap_or_default()
            } else if len >= 4 {
                request.payload[0..4].try_into().unwrap_or_default()
            } else {
                return libc::EINVAL;
            };
            socket.options.ip_multicast_if = addr;
            0
        }
        IP_MULTICAST_TTL => {
            let value = if len >= 4 {
                i32::from_ne_bytes(request.payload[0..4].try_into().unwrap_or_default())
            } else if len == 1 {
                i32::from(request.payload[0])
            } else {
                return libc::EINVAL;
            };
            let ttl = if value == -1 {
                1
            } else if (0..=255).contains(&value) {
                value
            } else {
                return libc::EINVAL;
            };
            socket.options.ip_multicast_ttl = ttl;
            0
        }
        IP_MULTICAST_LOOP => {
            let value = if len >= 4 {
                i32::from_ne_bytes(request.payload[0..4].try_into().unwrap_or_default())
            } else if len == 1 {
                i32::from(request.payload[0])
            } else {
                return libc::EINVAL;
            };
            socket.options.ip_multicast_loop = value != 0;
            0
        }
        IP_ADD_MEMBERSHIP | IP_DROP_MEMBERSHIP => {
            // struct ip_mreq (8 bytes) or struct ip_mreqn (12 bytes).
            if len < 8 {
                return libc::EINVAL;
            }
            if (request.payload[0] & 0xf0) != 0xe0 {
                return libc::EINVAL;
            }
            0
        }
        _ => libc::ENOPROTOOPT,
    }
}

fn set_ipv6_multicast_membership(state: &mut NetState, request: &NetdIpcRequest) -> i32 {
    let Some(socket) = state.inet_sockets.get(&request.socket_token) else {
        return if state.sockets.contains_key(&request.socket_token) {
            libc::ENOPROTOOPT
        } else {
            libc::EBADF
        };
    };
    if socket.domain != linux_abi::AF_INET6 {
        return libc::ENOPROTOOPT;
    }
    // struct ipv6_mreq: 16-byte in6_addr + 4-byte interface index = 20 bytes.
    if (request.payload_len as usize) < 20 {
        return libc::EINVAL;
    }
    // IPv6 multicast addresses are ff00::/8.
    if request.payload[0] != 0xff {
        return libc::EINVAL;
    }
    0
}

fn linux_tcp_state(state: tcp::State) -> u8 {
    match state {
        tcp::State::Established => 1,
        tcp::State::SynSent => 2,
        tcp::State::SynReceived => 3,
        tcp::State::FinWait1 => 4,
        tcp::State::FinWait2 => 5,
        tcp::State::TimeWait => 6,
        tcp::State::Closed => 7,
        tcp::State::CloseWait => 8,
        tcp::State::LastAck => 9,
        tcp::State::Listen => 10,
        tcp::State::Closing => 11,
    }
}

fn write_tcp_info_opt(
    stack: Option<&InetStack>,
    socket: &InetSocket,
    tcp_handle: SocketHandle,
    response: &mut NetdIpcResponse,
) -> i32 {
    let mut info = [0_u8; TCP_INFO_LEN];
    let (tcpi_state, rcv_space) = if socket.tcp_listener.is_some() {
        (10_u8, socket.options.recv_buffer.max(0) as u32)
    } else if let Some(stack) = stack {
        let tcp_sock = stack.sockets.get::<tcp::Socket>(tcp_handle);
        (
            linux_tcp_state(tcp_sock.state()),
            tcp_sock.recv_capacity().min(u32::MAX as usize) as u32,
        )
    } else {
        (7_u8, socket.options.recv_buffer.max(0) as u32)
    };
    let write_u32 = |buf: &mut [u8], offset: usize, value: u32| {
        buf[offset..offset + 4].copy_from_slice(&value.to_ne_bytes());
    };
    info[0] = tcpi_state;
    write_u32(&mut info, 8, 200_000); // tcpi_rto (200ms)
    write_u32(&mut info, 12, 40_000); // tcpi_ato (40ms)
    write_u32(&mut info, 16, 1448); // tcpi_snd_mss
    write_u32(&mut info, 20, 1448); // tcpi_rcv_mss
    write_u32(&mut info, 60, 1500); // tcpi_pmtu
    write_u32(&mut info, 64, 65535); // tcpi_rcv_ssthresh
    write_u32(&mut info, 68, 1_000); // tcpi_rtt (1ms)
    write_u32(&mut info, 72, 500); // tcpi_rttvar
    write_u32(&mut info, 76, 0x7fff_ffff); // tcpi_snd_ssthresh
    write_u32(&mut info, 80, 10); // tcpi_snd_cwnd
    write_u32(&mut info, 84, 1448); // tcpi_advmss
    write_u32(&mut info, 88, 3); // tcpi_reordering
    write_u32(&mut info, 96, rcv_space); // tcpi_rcv_space

    response.payload[..TCP_INFO_LEN].copy_from_slice(&info);
    response.payload_len = TCP_INFO_LEN as u32;
    response.value = 0;
    0
}

pub(super) fn handle_ioctl(request: &NetdIpcRequest, response: &mut NetdIpcResponse) -> i32 {
    let mut state = lock_net_state();
    handle_ioctl_in_state(&mut state, request, response)
}

pub(super) fn handle_ioctl_in_state(
    state: &mut NetState,
    request: &NetdIpcRequest,
    response: &mut NetdIpcResponse,
) -> i32 {
    if request.arg1 != linux_abi::FIONREAD {
        return libc::ENOTTY;
    }
    if state.inet_sockets.contains_key(&request.socket_token) {
        if let Some(stack) = state.inet.as_mut() {
            stack.poll_budget(4);
        }
        if state.inet_sockets[&request.socket_token].transport.is_datagram() {
            if let Err(errno) = inet_datagram::stage_one(state, request.socket_token) {
                return errno;
            }
        }
        let socket = &state.inet_sockets[&request.socket_token];
        if socket.tcp_listener.is_some() {
            return libc::EINVAL;
        }
        let available = match socket.transport {
            InetTransport::Tcp(h) => state
                .inet
                .as_ref()
                .map(|s| s.sockets.get::<tcp::Socket>(h).recv_queue())
                .unwrap_or(0),
            // Staging moves the next eligible packet out of smoltcp, which
            // is also what a following recv would consume first.
            InetTransport::Udp(_) | InetTransport::Icmp(_) => {
                socket.received.as_ref().map_or(0, |pkt| pkt.bytes.len())
            }
        };
        return write_i32_opt(response, available.min(i32::MAX as usize) as i32);
    }
    let Some(socket) = state.sockets.get(&request.socket_token) else {
        return libc::EBADF;
    };
    let available = match &socket.state {
        UnixSocketState::Connected(connected) => {
            if socket.kind.preserves_message_boundaries() {
                connected.incoming.front().map_or(0, |seg| seg.bytes.len())
            } else {
                connected.incoming_bytes
            }
        }
        UnixSocketState::Dgram(dgram) => dgram.incoming.front().map_or(0, |seg| seg.bytes.len()),
        UnixSocketState::Netlink(netlink) => netlink.front_len(),
        UnixSocketState::Idle | UnixSocketState::Listening { .. } => 0,
    };
    write_i32_opt(response, available.min(i32::MAX as usize) as i32)
}

pub(super) fn handle_shutdown(request: &NetdIpcRequest) -> i32 {
    let mut state = lock_net_state();
    if let Some(inet) = state.inet_sockets.get(&request.socket_token) {
        let InetTransport::Tcp(tcp_handle) = inet.transport else { return libc::EOPNOTSUPP; };
        let how = request.arg1;
        if !matches!(how, linux_abi::SHUT_RD | linux_abi::SHUT_WR | linux_abi::SHUT_RDWR) {
            return libc::EINVAL;
        }
        let stack = match state.inet_stack() {
            Ok(stack) => stack,
            Err(errno) => return errno,
        };
        // smoltcp has no receive half-close; only the write side (FIN) maps.
        // SHUT_RD alone must not send a FIN.
        if how != linux_abi::SHUT_RD {
            stack.sockets.get_mut::<tcp::Socket>(tcp_handle).close();
            stack.poll_budget(8);
        }
        return 0;
    }
    enum UnixShutdownTarget {
        StreamPeer(u64),
        Dgram,
    }
    let target = {
        let Some(socket) = state.sockets.get_mut(&request.socket_token) else {
            return libc::EBADF;
        };
        match &mut socket.state {
            UnixSocketState::Connected(connected) => {
                match request.arg1 {
                    linux_abi::SHUT_RD => connected.recv_closed = true,
                    linux_abi::SHUT_WR => connected.send_closed = true,
                    linux_abi::SHUT_RDWR => {
                        connected.recv_closed = true;
                        connected.send_closed = true;
                    }
                    _ => return libc::EINVAL,
                }
                UnixShutdownTarget::StreamPeer(connected.peer)
            }
            UnixSocketState::Dgram(dgram) => {
                match request.arg1 {
                    linux_abi::SHUT_RD => dgram.recv_closed = true,
                    linux_abi::SHUT_WR => dgram.send_closed = true,
                    linux_abi::SHUT_RDWR => {
                        dgram.recv_closed = true;
                        dgram.send_closed = true;
                    }
                    _ => return libc::EINVAL,
                }
                UnixShutdownTarget::Dgram
            }
            _ => return libc::ENOTCONN,
        }
    };
    if let UnixShutdownTarget::StreamPeer(peer) = target {
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
    }
    drop(state);
    0
}

pub(super) fn sockaddr_path_from_payload(request: &NetdIpcRequest) -> Result<UnixAddress, i32> {
    sockaddr_bind_address_from_payload(request)?.ok_or(libc::EINVAL)
}

/// Parses an `AF_UNIX` `sockaddr_un` payload for `bind`: `Some(addr)` for
/// filesystem or abstract names, or `None` when `addrlen == sizeof(sa_family_t)`
/// (autobind request).
pub(super) fn sockaddr_bind_address_from_payload(
    request: &NetdIpcRequest,
) -> Result<Option<UnixAddress>, i32> {
    let len = request.payload_len as usize;
    if len < size_of::<u16>() {
        return Err(libc::EINVAL);
    }
    let family = u16::from_ne_bytes([request.payload[0], request.payload[1]]) as u64;
    if family != linux_abi::AF_UNIX {
        return Err(libc::EAFNOSUPPORT);
    }
    let path_bytes = &request.payload[size_of::<u16>()..len];
    if path_bytes.is_empty() {
        return Ok(None);
    }
    if path_bytes.first().copied() == Some(0) {
        if path_bytes.len() > linux_abi::UNIX_PATH_MAX {
            return Err(libc::EINVAL);
        }
        return Ok(Some(UnixAddress::Abstract(path_bytes.to_vec())));
    }
    // Linux accepts a full `sun_path` (108 bytes) without a terminating NUL;
    // only a longer `addrlen` is invalid.
    if path_bytes.len() > linux_abi::UNIX_PATH_MAX {
        return Err(libc::EINVAL);
    }
    let end = path_bytes
        .iter()
        .position(|byte| *byte == 0)
        .unwrap_or(path_bytes.len());
    if end == 0 {
        return Err(libc::EINVAL);
    }
    String::from_utf8(path_bytes[..end].to_vec())
        .map(|path| Some(UnixAddress::Filesystem(path)))
        .map_err(|_| libc::EINVAL)
}

pub(super) fn sockaddr_payload(
    address: Option<&UnixAddress>,
    response: &mut NetdIpcResponse,
) -> Result<i32, i32> {
    response.payload[..2].copy_from_slice(&(linux_abi::AF_UNIX as u16).to_ne_bytes());
    let needed = match address {
        // An unnamed socket reports only its family (addrlen 2), as Linux does;
        // a trailing NUL byte would read as an empty abstract name.
        None => size_of::<u16>(),
        Some(UnixAddress::Filesystem(path)) => {
            // A shorter path is reported with its NUL; a full 108-byte
            // `sun_path` has no room for one and is reported without it.
            if path.len() > linux_abi::UNIX_PATH_MAX {
                return Err(libc::EINVAL);
            }
            let needed = size_of::<u16>() + path.len() + usize::from(path.len() < linux_abi::UNIX_PATH_MAX);
            if needed > response.payload.len() {
                return Err(libc::EINVAL);
            }
            response.payload[2..2 + path.len()].copy_from_slice(path.as_bytes());
            if path.len() < linux_abi::UNIX_PATH_MAX {
                response.payload[2 + path.len()] = 0;
            }
            needed
        }
        Some(UnixAddress::Abstract(bytes)) => {
            if bytes.is_empty() || bytes.len() > linux_abi::UNIX_PATH_MAX {
                return Err(libc::EINVAL);
            }
            let needed = size_of::<u16>()
                .checked_add(bytes.len())
                .ok_or(libc::EINVAL)?;
            if needed > response.payload.len() {
                return Err(libc::EINVAL);
            }
            response.payload[2..2 + bytes.len()].copy_from_slice(bytes);
            needed
        }
    };
    response.payload_len = needed as u32;
    response.value = 0;
    Ok(0)
}

pub(super) fn peercred_payload_from_socket(socket: &UnixSocket, response: &mut NetdIpcResponse) -> i32 {
    let creds = match &socket.state {
        UnixSocketState::Connected(connected) => connected.peer_credentials,
        UnixSocketState::Dgram(dgram) => match dgram.peer_credentials {
            Some(c) => c,
            None => return libc::ENOTCONN,
        },
        _ => return libc::ENOTCONN,
    };
    let value = linux_abi::LinuxUCred {
        pid: creds.pid,
        uid: creds.uid,
        gid: creds.gid,
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

pub(super) fn request_i32_payload(request: &NetdIpcRequest) -> Option<i32> {
    if (request.payload_len as usize) < size_of::<i32>() {
        return None;
    }
    Some(i32::from_ne_bytes(request.payload[..4].try_into().ok()?))
}

pub(super) fn clamp_socket_buffer(value: i32) -> i32 {
    value.clamp(4096, SOCKET_BUFFER_CAPACITY as i32)
}
