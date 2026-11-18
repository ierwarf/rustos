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
    begin_inet_connect_in_state(&mut state, request.socket_token, remote, port, connect_timeout, connect_timeout_ms)
}

fn begin_inet_connect_in_state(
    state: &mut NetState, token: u64, remote: Ipv4Address, port: u16,
    connect_timeout: Duration, connect_timeout_ms: u128,
) -> i32 {
    let Some(inet) = state.inet_sockets.get(&token) else {
        return libc::EBADF;
    };
    let InetTransport::Tcp(tcp_handle) = inet.transport else { return libc::EOPNOTSUPP; };
    if inet.connect_deadline.is_some() {
        return match inet_connect_status(state, token) {
            libc::EINPROGRESS => libc::EALREADY,
            status => status,
        };
    }
    let stack = match state.inet_stack() {
        Ok(stack) => stack,
        Err(errno) => return errno,
    };
    if let Some(errno) = tcp_connect_state_errno(stack.sockets.get::<tcp::Socket>(tcp_handle).state()) {
        return errno;
    }
    let Some(local_port) = stack.next_ephemeral_port() else {
        return libc::EADDRNOTAVAIL;
    };
    {
        let socket = stack.sockets.get_mut::<tcp::Socket>(tcp_handle);
        socket.set_timeout(Some(smoltcp::time::Duration::from_millis(
            connect_timeout_ms.min(u128::from(u64::MAX)) as u64,
        )));
        if let Err(error) = socket.connect(
            stack.iface.context(),
            (IpAddress::Ipv4(remote), port),
            local_port,
        ) {
            return tcp_connect_error_errno(error);
        }
        // A fresh active open starts a new connection; a reset recorded for
        // the previous one must not be reported against it.
        forget_tcp_termination(tcp_handle);
    }
    stack.poll_budget(8);
    if let Some(inet) = state.inet_sockets.get_mut(&token) {
        inet.connect_deadline = Some(StdInstant::now() + connect_timeout);
    }
    inet_connect_status(state, token)
}

/// Linux `connect` on a TCP socket that is not `TCP_CLOSE`: a connection still
/// handshaking reports `EALREADY`; an established, closing or listening
/// socket reports `EISCONN` (`__inet_stream_connect` returns it for any
/// non-closed `SS_UNCONNECTED` state, listeners included).
pub(super) fn tcp_connect_state_errno(state: tcp::State) -> Option<i32> {
    match state {
        tcp::State::Closed => None,
        tcp::State::SynSent | tcp::State::SynReceived => Some(libc::EALREADY),
        _ => Some(libc::EISCONN),
    }
}

/// smoltcp rejects an active open only when no source address/route exists
/// for the destination (the endpoint itself was validated by the decoder), or
/// when the socket is open; the latter is screened by
/// `tcp_connect_state_errno` first.
pub(super) fn tcp_connect_error_errno(error: tcp::ConnectError) -> i32 {
    match error {
        tcp::ConnectError::Unaddressable => libc::ENETUNREACH,
        tcp::ConnectError::InvalidState => libc::EISCONN,
    }
}

/// Per-connection record of an abortive close. smoltcp reports an RST (and
/// its own retransmission abort) only as `State::Closed`, which is
/// indistinguishable from an orderly close afterwards, so the transition is
/// recorded while the socket is polled. The table is a leaf lock under the
/// `NetState` lock and is keyed by the stack's singleton socket handles.
#[derive(Clone, Copy)]
struct TcpTermination {
    last: tcp::State,
    reset: bool,
}

fn tcp_terminations() -> &'static Mutex<HashMap<SocketHandle, TcpTermination>> {
    static TABLE: OnceLock<Mutex<HashMap<SocketHandle, TcpTermination>>> = OnceLock::new();
    TABLE.get_or_init(|| Mutex::new(HashMap::new()))
}

fn lock_terminations() -> std::sync::MutexGuard<'static, HashMap<SocketHandle, TcpTermination>> {
    tcp_terminations()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// A synchronized connection that lands in `Closed` was aborted (RST or
/// timeout); the graceful paths end in `LastAck`/`TimeWait`/`Closing` first,
/// and `SynSent` failures are classified by `inet_connect_status`.
fn abortive_close(previous: tcp::State, now: tcp::State) -> bool {
    now == tcp::State::Closed
        && matches!(
            previous,
            tcp::State::SynReceived
                | tcp::State::Established
                | tcp::State::FinWait1
                | tcp::State::FinWait2
                | tcp::State::CloseWait
        )
}

fn note_tcp_state(table: &mut HashMap<SocketHandle, TcpTermination>, handle: SocketHandle, now: tcp::State) {
    let entry = table.entry(handle).or_insert(TcpTermination {
        last: tcp::State::Closed,
        reset: false,
    });
    if abortive_close(entry.last, now) {
        entry.reset = true;
    }
    entry.last = now;
    if now == tcp::State::Closed && !entry.reset {
        table.remove(&handle);
    }
}

/// Records state transitions of every TCP socket; called after the stack made
/// progress so a reset is seen before its socket is next touched.
pub(super) fn observe_tcp_states(sockets: &SocketSet<'_>) {
    let mut table = lock_terminations();
    for (handle, socket) in sockets.iter() {
        if let smoltcp::socket::Socket::Tcp(tcp) = socket {
            note_tcp_state(&mut table, handle, tcp.state());
        }
    }
}

pub(super) fn forget_tcp_termination(handle: SocketHandle) {
    lock_terminations().remove(&handle);
}

/// Reports (without consuming) that a reset is pending for this socket.
fn tcp_reset_pending(socket: &tcp::Socket, handle: SocketHandle) -> bool {
    let mut table = lock_terminations();
    note_tcp_state(&mut table, handle, socket.state());
    table.get(&handle).is_some_and(|entry| entry.reset)
}

/// Consumes the pending reset, like Linux clearing `sk_err` once reported.
fn take_tcp_reset(socket: &tcp::Socket, handle: SocketHandle) -> bool {
    let mut table = lock_terminations();
    note_tcp_state(&mut table, handle, socket.state());
    match table.get_mut(&handle) {
        Some(entry) if entry.reset => {
            entry.reset = false;
            table.remove(&handle);
            true
        }
        _ => false,
    }
}

/// `SO_ERROR` for an established-then-reset TCP socket: `ECONNRESET` once.
pub(super) fn take_tcp_socket_error(state: &mut NetState, token: u64) -> i32 {
    let Some(InetTransport::Tcp(handle)) = state.inet_sockets.get(&token).map(|s| s.transport)
    else {
        return 0;
    };
    let Some(stack) = state.inet.as_mut() else {
        return 0;
    };
    let socket = stack.sockets.get::<tcp::Socket>(handle);
    if take_tcp_reset(socket, handle) {
        libc::ECONNRESET
    } else {
        0
    }
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

pub(super) fn handle_inet_send_in_state(
    state: &mut NetState,
    request: &NetdIpcRequest,
    bytes: &[u8],
    response: &mut NetdIpcResponse,
) -> i32 {
    if bytes.is_empty() {
        response.value = 0;
        return 0;
    }
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
        // A reset not yet reported surfaces as the socket error first.
        if take_tcp_reset(socket, tcp_handle) {
            return libc::ECONNRESET;
        }
        libc::EPIPE
    } else {
        libc::EAGAIN
    }
}

pub(super) fn handle_inet_recv_in_state(
    state: &mut NetState,
    request: &NetdIpcRequest,
    response: &mut NetdIpcResponse,
) -> i32 {
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
        if take_tcp_reset(socket, tcp_handle) {
            return libc::ECONNRESET;
        }
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
    if tcp_reset_pending(socket, tcp_handle) {
        revents |= linux_abi::POLLERR as u32;
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

#[cfg(test)]
mod tests {
    use super::*;
    use smoltcp::iface::{Config, Interface};
    use smoltcp::phy::{Loopback, Medium};
    use smoltcp::wire::{HardwareAddress, IpCidr, IpListenEndpoint};

    #[test]
    fn connect_on_a_live_socket_reports_linux_errnos() {
        assert_eq!(tcp_connect_state_errno(tcp::State::Closed), None);
        assert_eq!(tcp_connect_state_errno(tcp::State::SynSent), Some(libc::EALREADY));
        assert_eq!(tcp_connect_state_errno(tcp::State::SynReceived), Some(libc::EALREADY));
        for state in [
            tcp::State::Established,
            tcp::State::Listen,
            tcp::State::CloseWait,
            tcp::State::FinWait1,
            tcp::State::TimeWait,
        ] {
            assert_eq!(tcp_connect_state_errno(state), Some(libc::EISCONN), "{state}");
        }
    }

    #[test]
    fn smoltcp_connect_errors_map_to_route_errnos_not_einval() {
        assert_eq!(
            tcp_connect_error_errno(tcp::ConnectError::Unaddressable),
            libc::ENETUNREACH
        );
        assert_eq!(
            tcp_connect_error_errno(tcp::ConnectError::InvalidState),
            libc::EISCONN
        );
    }

    #[test]
    fn abortive_close_excludes_graceful_and_connecting_states() {
        assert!(abortive_close(tcp::State::Established, tcp::State::Closed));
        assert!(abortive_close(tcp::State::CloseWait, tcp::State::Closed));
        assert!(!abortive_close(tcp::State::LastAck, tcp::State::Closed));
        assert!(!abortive_close(tcp::State::TimeWait, tcp::State::Closed));
        assert!(!abortive_close(tcp::State::Closing, tcp::State::Closed));
        assert!(!abortive_close(tcp::State::SynSent, tcp::State::Closed));
        assert!(!abortive_close(tcp::State::Established, tcp::State::CloseWait));
    }

    fn poll_loopback(
        iface: &mut Interface,
        device: &mut Loopback,
        sockets: &mut SocketSet<'static>,
        clock: &mut i64,
    ) {
        for _ in 0..8 {
            *clock += 10;
            iface.poll(SmolInstant::from_millis(*clock), device, sockets);
            observe_tcp_states(sockets);
        }
    }

    fn new_tcp(sockets: &mut SocketSet<'static>) -> SocketHandle {
        let rx = tcp::SocketBuffer::new(vec![0; 4096]);
        let tx = tcp::SocketBuffer::new(vec![0; 4096]);
        let handle = sockets.add(tcp::Socket::new(rx, tx));
        forget_tcp_termination(handle);
        handle
    }

    /// Establishes client<->server over smoltcp loopback and returns the
    /// pieces so each test can end the connection its own way.
    fn established() -> (Interface, Loopback, SocketSet<'static>, i64, SocketHandle, SocketHandle) {
        let mut device = Loopback::new(Medium::Ethernet);
        let mut iface = Interface::new(
            Config::new(HardwareAddress::Ethernet(smoltcp::wire::EthernetAddress([2, 0, 0, 0, 0, 1]))),
            &mut device,
            SmolInstant::from_millis(0),
        );
        iface.update_ip_addrs(|addrs| {
            addrs
                .push(IpCidr::new(IpAddress::v4(127, 0, 0, 1), 8))
                .unwrap();
        });
        let mut sockets = SocketSet::new(Vec::new());
        let server = new_tcp(&mut sockets);
        let client = new_tcp(&mut sockets);
        sockets
            .get_mut::<tcp::Socket>(server)
            .listen(IpListenEndpoint { addr: None, port: 7000 })
            .unwrap();
        sockets
            .get_mut::<tcp::Socket>(client)
            .connect(
                iface.context(),
                (IpAddress::v4(127, 0, 0, 1), 7000),
                50000,
            )
            .unwrap();
        let mut clock = 0;
        poll_loopback(&mut iface, &mut device, &mut sockets, &mut clock);
        assert_eq!(sockets.get::<tcp::Socket>(client).state(), tcp::State::Established);
        assert_eq!(sockets.get::<tcp::Socket>(server).state(), tcp::State::Established);
        (iface, device, sockets, clock, client, server)
    }

    fn state_with_tcp(iface: Interface, sockets: SocketSet<'static>, handle: SocketHandle) -> NetState {
        let mut state = NetState::new();
        state.inet = Some(InetStack {
            iface,
            sockets,
            device: BrokerDevice {
                rx: [0; NET_BROKER_PACKET_MTU],
                errors: inet_datagram::ErrorIngress::default(),
            },
            next_port: 49152,
            closing: Vec::new(),
            readiness_dirty: false,
            last_timer_sweep: StdInstant::now(),
        });
        state.inet_sockets.insert(7, InetSocket {
            refs: 1,
            options: SocketOptions::default(),
            transport: InetTransport::Tcp(handle),
            local: None,
            peer: None,
            received: None,
            errors: None,
            connect_deadline: None,
        });
        state
    }

    #[test]
    fn so_error_consumes_established_reset_and_clears_pollerr() {
        let (mut iface, mut device, mut sockets, mut clock, client, server) = established();
        sockets.get_mut::<tcp::Socket>(server).abort();
        poll_loopback(&mut iface, &mut device, &mut sockets, &mut clock);
        let mut state = state_with_tcp(iface, sockets, client);
        assert_ne!(inet_socket_revents(&mut state, 7, 0).unwrap() & linux_abi::POLLERR as u32, 0);
        assert_eq!(socket_info::take_inet_socket_error(&mut state, 7), libc::ECONNRESET);
        assert_eq!(socket_info::take_inet_socket_error(&mut state, 7), 0);
        assert_eq!(inet_socket_revents(&mut state, 7, 0).unwrap() & linux_abi::POLLERR as u32, 0);
    }

    #[test]
    fn connect_retry_collects_completed_result_before_eisconn() {
        let (iface, _device, sockets, _clock, client, _) = established();
        let mut state = state_with_tcp(iface, sockets, client);
        state.inet_sockets.get_mut(&7).unwrap().connect_deadline = Some(StdInstant::now() + Duration::from_secs(10));
        let retry = |state: &mut NetState| begin_inet_connect_in_state(
            state, 7, Ipv4Address::new(127, 0, 0, 1), 7000, Duration::from_secs(10), 10000,
        );
        assert_eq!(retry(&mut state), 0);
        assert_eq!(retry(&mut state), libc::EISCONN);
    }

    #[test]
    fn connect_retry_and_so_error_preserve_pending_and_failed_result() {
        let (mut iface, _device, mut sockets, _clock, client, _) = established();
        sockets.get_mut::<tcp::Socket>(client).abort();
        sockets.get_mut::<tcp::Socket>(client).connect(
            iface.context(), (IpAddress::v4(127, 0, 0, 1), 7001), 50001,
        ).unwrap();
        let mut state = state_with_tcp(iface, sockets, client);
        state.inet_sockets.get_mut(&7).unwrap().connect_deadline = Some(StdInstant::now() + Duration::from_secs(10));
        assert_eq!(socket_info::take_inet_socket_error(&mut state, 7), 0);
        assert!(state.inet_sockets[&7].connect_deadline.is_some());
        assert_eq!(begin_inet_connect_in_state(&mut state, 7, Ipv4Address::new(127, 0, 0, 1), 7001, Duration::from_secs(10), 10000), libc::EALREADY);
        state.inet.as_mut().unwrap().sockets.get_mut::<tcp::Socket>(client).abort();
        assert_eq!(begin_inet_connect_in_state(&mut state, 7, Ipv4Address::new(127, 0, 0, 1), 7001, Duration::from_secs(10), 10000), libc::ECONNREFUSED);
        assert!(state.inet_sockets[&7].connect_deadline.is_none());
    }

    #[test]
    fn peer_reset_is_reported_once_as_econnreset_not_eof() {
        let (mut iface, mut device, mut sockets, mut clock, client, server) = established();
        sockets.get_mut::<tcp::Socket>(server).abort();
        poll_loopback(&mut iface, &mut device, &mut sockets, &mut clock);
        let socket = sockets.get::<tcp::Socket>(client);
        assert_eq!(socket.state(), tcp::State::Closed);
        assert!(tcp_reset_pending(socket, client));
        assert!(take_tcp_reset(socket, client));
        assert!(!take_tcp_reset(socket, client), "reported exactly once");
        assert!(!tcp_reset_pending(socket, client));
        // A fresh connection on the recycled handle starts clean.
        forget_tcp_termination(client);
        assert!(!tcp_reset_pending(sockets.get::<tcp::Socket>(client), client));
    }

    #[test]
    fn orderly_close_is_never_reported_as_a_reset() {
        let (mut iface, mut device, mut sockets, mut clock, client, server) = established();
        sockets.get_mut::<tcp::Socket>(server).close();
        poll_loopback(&mut iface, &mut device, &mut sockets, &mut clock);
        sockets.get_mut::<tcp::Socket>(client).close();
        poll_loopback(&mut iface, &mut device, &mut sockets, &mut clock);
        // Fast-forward past TIME-WAIT so both ends reach Closed gracefully.
        for _ in 0..40 {
            clock += 1000;
            iface.poll(SmolInstant::from_millis(clock), &mut device, &mut sockets);
            observe_tcp_states(&sockets);
        }
        for handle in [client, server] {
            let socket = sockets.get::<tcp::Socket>(handle);
            assert_eq!(socket.state(), tcp::State::Closed);
            assert!(!take_tcp_reset(socket, handle));
        }
    }
}
