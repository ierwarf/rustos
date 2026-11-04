//! Socket names, options and shutdown.

use super::*;

pub(super) fn handle_getsockname(request: &NetdIpcRequest, response: &mut NetdIpcResponse) -> i32 {
    if inet_socket_exists(request.socket_token) {
        return inet_datagram::socket_name(request, response, false);
    }
    let state = lock_net_state();
    let Some(socket) = state.sockets.get(&request.socket_token) else {
        return libc::EBADF;
    };
    sockaddr_payload(socket.local_path.as_deref().unwrap_or(""), response)
        .unwrap_or_else(|errno| errno)
}

pub(super) fn handle_getpeername(request: &NetdIpcRequest, response: &mut NetdIpcResponse) -> i32 {
    if inet_socket_exists(request.socket_token) {
        return inet_datagram::socket_name(request, response, true);
    }
    let state = lock_net_state();
    let Some(socket) = state.sockets.get(&request.socket_token) else {
        return libc::EBADF;
    };
    let Some(path) = socket.peer_path.as_deref() else {
        return libc::ENOTCONN;
    };
    sockaddr_payload(path, response).unwrap_or_else(|errno| errno)
}

pub(super) fn handle_getsockopt(request: &NetdIpcRequest, response: &mut NetdIpcResponse) -> i32 {
    if request.arg1 != linux_abi::SOL_SOCKET {
        return inet_datagram::get_ip_option(request, response);
    }
    let mut state = lock_net_state();
    if state.inet_sockets.contains_key(&request.socket_token) {
        let pending = if request.arg2 == linux_abi::SO_ERROR && state.inet_sockets[&request.socket_token].transport.is_datagram() {
            inet_datagram::take_socket_error(&mut state, request.socket_token)
        } else if request.arg2 == linux_abi::SO_ERROR
            && state.inet_sockets[&request.socket_token]
                .connect_deadline
                .is_some()
        {
            // Nonblocking connect outcome; reading SO_ERROR consumes it.
            match inet_connect_status(&mut state, request.socket_token) {
                libc::EINPROGRESS => 0,
                errno => errno,
            }
        } else {
            0
        };
        let socket = &state.inet_sockets[&request.socket_token];
        let value = match request.arg2 {
            linux_abi::SO_ERROR => pending,
            linux_abi::SO_TYPE => socket.transport.socket_type(),
            linux_abi::SO_DOMAIN => linux_abi::AF_INET as i32,
            linux_abi::SO_PROTOCOL => socket.transport.protocol(),
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

pub(super) fn handle_setsockopt(request: &NetdIpcRequest) -> i32 {
    if request.arg1 != linux_abi::SOL_SOCKET {
        return inet_datagram::set_ip_option(request);
    }
    let Some(value) = request_i32_payload(request) else {
        return libc::EINVAL;
    };
    let mut state = lock_net_state();
    if let Some(socket) = state.inet_sockets.get_mut(&request.socket_token) {
        match request.arg2 {
            linux_abi::SO_KEEPALIVE => {
                socket.options.keepalive = value != 0;
                let transport = socket.transport;
                if let InetTransport::Tcp(handle) = transport {
                    let Some(stack) = state.inet.as_mut() else { return libc::ENODEV; };
                    stack.sockets.get_mut::<tcp::Socket>(handle).set_keep_alive(
                        (value != 0).then_some(smoltcp::time::Duration::from_secs(7200)));
                }
            }
            // These buffers are fixed for the socket's lifetime. Do not report
            // a successful resize or port-sharing policy that did not happen.
            linux_abi::SO_SNDBUF | linux_abi::SO_RCVBUF => return libc::EOPNOTSUPP,
            linux_abi::SO_REUSEADDR | linux_abi::SO_REUSEPORT if value == 0 => {}
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

pub(super) fn sockaddr_path_from_payload(request: &NetdIpcRequest) -> Result<String, i32> {
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

pub(super) fn sockaddr_payload(path: &str, response: &mut NetdIpcResponse) -> Result<i32, i32> {
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

pub(super) fn peercred_payload_from_socket(socket: &UnixSocket, response: &mut NetdIpcResponse) -> i32 {
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

pub(super) fn request_i32_payload(request: &NetdIpcRequest) -> Option<i32> {
    if (request.payload_len as usize) < size_of::<i32>() {
        return None;
    }
    Some(i32::from_ne_bytes(request.payload[..4].try_into().ok()?))
}

pub(super) fn clamp_socket_buffer(value: i32) -> i32 {
    value.clamp(4096, SOCKET_BUFFER_CAPACITY as i32)
}
