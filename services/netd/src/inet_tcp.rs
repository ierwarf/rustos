//! AF_INET TCP connect/send/receive/readiness and request address decoding.

use super::*;

pub(super) fn handle_inet_connect(request: &NetdIpcRequest) -> i32 {
    if inet_datagram::is_datagram(request.socket_token) {
        return inet_datagram::connect(request);
    }
    begin_inet_connect(request)
}

pub(super) fn begin_inet_connect(request: &NetdIpcRequest) -> i32 {
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
    let mut state = lock_net_state();
    let Some(inet) = state.inet_sockets.get(&request.socket_token) else {
        return libc::EBADF;
    };
    let InetTransport::Tcp(tcp_handle) = inet.transport else { return libc::EOPNOTSUPP; };
    let stack = match state.inet_stack() {
        Ok(stack) => stack,
        Err(errno) => return errno,
    };
    {
        let socket = stack.sockets.get_mut::<tcp::Socket>(tcp_handle);
        if socket.may_send() {
            return 0;
        }
        if socket.is_open() {
            return libc::EINPROGRESS;
        }
    }
    let Some(local_port) = stack.next_ephemeral_port() else {
        return libc::EADDRNOTAVAIL;
    };
    {
        let socket = stack.sockets.get_mut::<tcp::Socket>(tcp_handle);
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
    if let Some(inet) = state.inet_sockets.get_mut(&request.socket_token) {
        inet.connect_deadline = Some(StdInstant::now() + connect_timeout);
    }
    inet_connect_status(&mut state, request.socket_token)
}

/// The 10 s connect budget is smoltcp's socket timeout, which keeps applying to
/// the established connection (abort when the peer stays silent with data
/// queued). Once the handshake completed, raise it to the Linux
/// `tcp_retries2` horizon so a transient stall does not reset the stream.
pub(super) fn settle_established_timeout(socket: &mut tcp::Socket) {
    let established = smoltcp::time::Duration::from_secs(INET_ESTABLISHED_TIMEOUT.as_secs());
    if socket.may_send() && socket.timeout().is_some_and(|t| t < established) {
        socket.set_timeout(Some(established));
    }
}

/// Evaluates an in-flight active open. A terminal outcome disarms the
/// deadline so it is reported exactly once (to `connect` or to `SO_ERROR`).
pub(super) fn inet_connect_status(state: &mut NetState, token: u64) -> i32 {
    // smoltcp stamps its own timeout later than our deadline; allow slack so
    // its closure is not misread as a refusal.
    const TIMEOUT_SLACK: Duration = Duration::from_millis(100);
    let Some(inet) = state.inet_sockets.get(&token) else {
        return libc::EBADF;
    };
    let InetTransport::Tcp(tcp_handle) = inet.transport else { return libc::EOPNOTSUPP; };
    let deadline = inet.connect_deadline;
    let stack = match state.inet_stack() {
        Ok(stack) => stack,
        Err(errno) => return errno,
    };
    let socket = stack.sockets.get_mut::<tcp::Socket>(tcp_handle);
    let status = if socket.may_send() {
        debug_line("netd: inet connect ok");
        settle_established_timeout(socket);
        0
    } else if !socket.is_open() {
        if deadline.is_some_and(|d| StdInstant::now() + TIMEOUT_SLACK >= d) {
            libc::ETIMEDOUT
        } else {
            libc::ECONNREFUSED
        }
    } else {
        libc::EINPROGRESS
    };
    if status != libc::EINPROGRESS {
        if let Some(inet) = state.inet_sockets.get_mut(&token) {
            inet.connect_deadline = None;
        }
    }
    status
}

pub(super) fn poll_inet_connect(request: &NetdIpcRequest) -> i32 {
    if let Err(errno) = request_deadline_remaining_ns(request) {
        return errno;
    }
    let mut state = lock_net_state();
    if !state.inet_sockets.contains_key(&request.socket_token) {
        return libc::EBADF;
    }
    match state.inet_stack() {
        Ok(stack) => {
            stack.poll_budget(8);
        }
        Err(errno) => return errno,
    }
    inet_connect_status(&mut state, request.socket_token)
}

pub(super) fn handle_inet_send(
    request: &NetdIpcRequest,
    bytes: &[u8],
    response: &mut NetdIpcResponse,
) -> i32 {
    if bytes.is_empty() {
        response.value = 0;
        return 0;
    }
    let mut state = lock_net_state();
    let Some(inet) = state.inet_sockets.get(&request.socket_token) else {
        return libc::EBADF;
    };
    let InetTransport::Tcp(tcp_handle) = inet.transport else { return libc::EOPNOTSUPP; };
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

pub(super) fn handle_inet_recv(request: &NetdIpcRequest, response: &mut NetdIpcResponse) -> i32 {
    // RECVMSG carries its length in `payload_len` (arg2 holds the msg flags)
    // and expects the reply to start with the recvmsg envelope header.
    let recvmsg = request.op == SYSCALL_OFFLOAD_OP_LINUX_RECVMSG;
    let (requested, data_start) = if recvmsg {
        (request.payload_len as usize, NETD_RECVMSG_PAYLOAD_HEADER_SIZE)
    } else {
        (usize::try_from(request.arg2).unwrap_or(usize::MAX), 0)
    };
    let limit = requested.min(response.payload.len() - data_start);
    if limit == 0 {
        response.value = 0;
        response.payload_len = data_start as u32;
        response.payload[..data_start].fill(0);
        return 0;
    }
    let mut state = lock_net_state();
    let Some(inet) = state.inet_sockets.get(&request.socket_token) else {
        return libc::EBADF;
    };
    let InetTransport::Tcp(tcp_handle) = inet.transport else { return libc::EOPNOTSUPP; };
    let stack = match state.inet_stack() {
        Ok(stack) => stack,
        Err(errno) => return errno,
    };
    stack.poll_budget(8);
    let socket = stack.sockets.get_mut::<tcp::Socket>(tcp_handle);
    if socket.can_recv() {
        // MSG_PEEK must leave the bytes queued; consuming them would corrupt
        // the stream for liveness probes such as `recv(fd, &b, 1, MSG_PEEK)`.
        let peek = request_msg_flags(request) & libc::MSG_PEEK as u64 != 0;
        let dest = &mut response.payload[data_start..data_start + limit];
        let outcome = if peek { socket.peek_slice(dest) } else { socket.recv_slice(dest) };
        match outcome {
            Ok(read) => {
                response.value = read as u64;
                response.payload_len = (data_start + read) as u32;
                response.payload[..data_start].fill(0);
                if recvmsg {
                    response.payload[0..4].copy_from_slice(&(read as u32).to_ne_bytes());
                }
                stack.poll_budget(8);
                return 0;
            }
            Err(_) => return libc::ECONNRESET,
        }
    }
    if !socket.may_recv() {
        response.value = 0;
        response.payload_len = data_start as u32;
        response.payload[..data_start].fill(0);
        0
    } else {
        libc::EAGAIN
    }
}

/// The peer's FIN has been received: reads return EOF, so the socket is both
/// readable and `POLLRDHUP`.
pub(super) fn tcp_receive_closed(socket: &tcp::Socket) -> bool {
    matches!(
        socket.state(),
        tcp::State::CloseWait | tcp::State::LastAck | tcp::State::Closing | tcp::State::TimeWait
    )
}

/// Readiness of one AF_INET socket. The caller has already advanced the
/// stack, so a batch pays for one `poll_budget` instead of one per socket.
pub(super) fn inet_socket_revents(state: &mut NetState, token: u64, requested: u32) -> Result<u32, i32> {
    let transport = state.inet_sockets.get(&token).ok_or(libc::EBADF)?.transport;
    if transport.is_datagram() {
        return inet_datagram::revents(state, token, requested);
    }
    let InetTransport::Tcp(tcp_handle) = transport else { unreachable!() };
    let stack = state.inet_stack()?;
    let socket = stack.sockets.get_mut::<tcp::Socket>(tcp_handle);
    let rx_closed = tcp_receive_closed(socket);
    let mut revents = 0_u32;
    if requested & linux_abi::POLLIN as u32 != 0 && (socket.can_recv() || rx_closed) {
        revents |= linux_abi::POLLIN as u32;
    }
    if requested & linux_abi::POLLOUT as u32 != 0 && socket.can_send() {
        revents |= linux_abi::POLLOUT as u32;
    }
    if requested & linux_abi::POLLRDHUP as u32 != 0 && rx_closed {
        revents |= linux_abi::POLLRDHUP as u32;
    }
    if !socket.is_open() {
        revents |= linux_abi::POLLHUP as u32;
    }
    Ok(revents)
}

/// Pure membership probe on the dispatch hot path. It reads without taking a
/// `NetStateGuard`: that guard's drop runs the readiness sweep and publication,
/// work a request classifier that mutated nothing must not trigger.
pub(super) fn inet_socket_exists(token: u64) -> bool {
    match net_state().lock() {
        Ok(state) => state.inet_sockets.contains_key(&token),
        Err(_) => {
            eprintln!("netd: state lock poisoned; exiting");
            std::process::exit(101)
        }
    }
}

pub(super) fn sockaddr_family(request: &NetdIpcRequest) -> Option<u64> {
    if request.payload_len as usize >= size_of::<u16>() {
        Some(u16::from_ne_bytes([request.payload[0], request.payload[1]]) as u64)
    } else {
        None
    }
}

pub(super) fn sockaddr_in_from_payload(request: &NetdIpcRequest) -> Result<(Ipv4Address, u16), i32> {
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
