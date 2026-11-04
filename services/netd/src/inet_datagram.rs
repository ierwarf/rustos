//! IPv4 UDP and unprivileged ICMP echo policy. The kernel only copies the
//! message envelope and publishes capabilities; all protocol work stays here.
use super::*;
use rustos_user_abi::syscall::{netd_message, NETD_IPC_PAYLOAD_CAPACITY};
use smoltcp::wire::Icmpv4Packet;

mod error_queue;
pub(super) use error_queue::{ErrorIngress, ErrorState};

pub(super) fn set_ip_option(request: &NetdIpcRequest) -> i32 {
    error_queue::set_option(request)
}
pub(super) fn get_ip_option(request: &NetdIpcRequest, response: &mut NetdIpcResponse) -> i32 {
    error_queue::get_option(request, response)
}
pub(super) fn take_socket_error(state: &mut NetState, token: u64) -> i32 {
    if let Err(e) = stage_one(state, token) {
        return e;
    }
    state
        .inet_sockets
        .get_mut(&token)
        .map(error_queue::take_pending)
        .unwrap_or(libc::EBADF)
}

const PACKET_SLOTS: usize = 16;
const PACKET_BYTES: usize = 16 * 1024;
// No IPv4 fragmentation is advertised by this stack.
const MAX_UDP_PAYLOAD: usize = NET_BROKER_PACKET_MTU - 14 - 20 - 8;
const MAX_ICMP_PAYLOAD: usize = NET_BROKER_PACKET_MTU - 14 - 20;

pub(super) fn add_socket(stack: &mut InetStack, type_: u64, protocol: u64) -> InetTransport {
    if type_ == linux_abi::SOCK_STREAM {
        return InetTransport::Tcp(stack.add_tcp_socket());
    }
    if protocol == libc::IPPROTO_ICMP as u64 {
        let rx = icmp::PacketBuffer::new(
            vec![icmp::PacketMetadata::EMPTY; PACKET_SLOTS],
            vec![0; PACKET_BYTES],
        );
        let tx = icmp::PacketBuffer::new(
            vec![icmp::PacketMetadata::EMPTY; PACKET_SLOTS],
            vec![0; PACKET_BYTES],
        );
        InetTransport::Icmp(stack.sockets.add(icmp::Socket::new(rx, tx)))
    } else {
        let rx = udp::PacketBuffer::new(
            vec![udp::PacketMetadata::EMPTY; PACKET_SLOTS],
            vec![0; PACKET_BYTES],
        );
        let tx = udp::PacketBuffer::new(
            vec![udp::PacketMetadata::EMPTY; PACKET_SLOTS],
            vec![0; PACKET_BYTES],
        );
        InetTransport::Udp(stack.sockets.add(udp::Socket::new(rx, tx)))
    }
}

pub(super) fn is_datagram(token: u64) -> bool {
    lock_net_state()
        .inet_sockets
        .get(&token)
        .is_some_and(|s| s.transport.is_datagram())
}

pub(super) struct ReceivedPacket {
    source: IpEndpoint,
    bytes: Vec<u8>,
}

#[cfg(test)]
impl ReceivedPacket {
    pub(super) fn test_packet() -> Self {
        Self {
            source: IpEndpoint::new(IpAddress::Ipv4(QEMU_USERNET_GATEWAY), 53),
            bytes: vec![0; 8],
        }
    }
}

fn eligible(socket: &InetSocket, source: IpEndpoint, bytes: &[u8]) -> bool {
    match socket.transport {
        InetTransport::Udp(_) => socket.peer.is_none_or(|peer| peer == source),
        InetTransport::Icmp(_) => {
            socket.peer.is_none_or(|peer| peer.addr == source.addr)
                && bytes.len() >= 8
                && bytes[0] == 0
                && bytes[1] == 0
        }
        InetTransport::Tcp(_) => false,
    }
}

pub(super) fn stage_socket(stack: &mut InetStack, socket: &mut InetSocket) {
    if let Some(packet) = socket.received.as_ref() {
        if eligible(socket, packet.source, &packet.bytes) {
            return;
        }
        socket.received = None;
    }
    if !socket.transport.is_datagram() {
        return;
    }
    // One staged packet per socket gives both UDP and ICMP non-consuming peek.
    // Drain only the bounded smoltcp receive queue, filtering before readiness.
    for _ in 0..PACKET_SLOTS {
        let (bytes, source) = match socket.transport {
            InetTransport::Udp(h) => match stack.sockets.get_mut::<udp::Socket>(h).recv() {
                Ok((bytes, meta)) => (bytes, meta.endpoint),
                Err(_) => return,
            },
            InetTransport::Icmp(h) => match stack.sockets.get_mut::<icmp::Socket>(h).recv() {
                Ok((bytes, addr)) => (bytes, IpEndpoint::new(addr, 0)),
                Err(_) => return,
            },
            InetTransport::Tcp(_) => return,
        };
        if eligible(socket, source, bytes) {
            socket.received = Some(ReceivedPacket {
                source,
                bytes: bytes.to_vec(),
            });
            return;
        }
    }
}

/// Routes raw ICMP errors captured by the device to their owning sockets.
/// Must run before any readiness comparison; dirties the whole INET table when
/// it consumes a packet, because the owner is not known to the caller.
pub(super) fn stage_errors(stack: &mut InetStack, sockets: &mut SocketTable<InetSocket>) {
    error_queue::stage_all(stack, sockets);
}

fn stage_one(state: &mut NetState, token: u64) -> Result<(), i32> {
    let stack = state.inet.as_mut().ok_or(libc::ENODEV)?;
    error_queue::stage_all(stack, &mut state.inet_sockets);
    let socket = state.inet_sockets.get_mut(&token).ok_or(libc::EBADF)?;
    stage_socket(stack, socket);
    Ok(())
}

pub(super) fn readiness(stack: &InetStack, socket: &InetSocket) -> InetReadiness {
    let (can_recv, can_send, open, rx_closed) = match socket.transport {
        InetTransport::Tcp(h) => {
            let s = stack.sockets.get::<tcp::Socket>(h);
            (
                s.can_recv(),
                s.can_send(),
                s.is_open(),
                tcp_receive_closed(s),
            )
        }
        InetTransport::Udp(h) => {
            let s = stack.sockets.get::<udp::Socket>(h);
            // An unbound datagram socket can autobind on its first send.
            (socket.received.is_some(), s.can_send(), true, false)
        }
        InetTransport::Icmp(h) => {
            let s = stack.sockets.get::<icmp::Socket>(h);
            (socket.received.is_some(), s.can_send(), true, false)
        }
    };
    InetReadiness {
        can_recv,
        can_send,
        open,
        rx_closed,
        error: socket.errors.as_ref().is_some_and(ErrorState::has_error),
    }
}

pub(super) fn revents(state: &mut NetState, token: u64, requested: u32) -> Result<u32, i32> {
    stage_one(state, token)?;
    let socket = state.inet_sockets.get(&token).ok_or(libc::EBADF)?;
    let stack = state.inet.as_ref().ok_or(libc::ENODEV)?;
    let ready = readiness(stack, socket);
    let mut events = 0;
    if ready.can_recv {
        events |= requested & libc::POLLIN as u32;
    }
    if ready.can_send {
        events |= requested & libc::POLLOUT as u32;
    }
    if ready.error {
        events |= libc::POLLERR as u32;
    }
    Ok(events)
}

fn decode_address(bytes: &[u8]) -> Result<IpEndpoint, i32> {
    if bytes.len() != netd_message::IPV4_ADDRESS_SIZE {
        return Err(libc::EINVAL);
    }
    if u16::from_ne_bytes([bytes[0], bytes[1]]) != libc::AF_INET as u16 {
        return Err(libc::EAFNOSUPPORT);
    }
    Ok(IpEndpoint::new(
        IpAddress::Ipv4(Ipv4Address::new(bytes[4], bytes[5], bytes[6], bytes[7])),
        u16::from_be_bytes([bytes[2], bytes[3]]),
    ))
}

fn encode_address(endpoint: IpEndpoint) -> [u8; 16] {
    let mut bytes = [0; 16];
    bytes[..2].copy_from_slice(&(libc::AF_INET as u16).to_ne_bytes());
    bytes[2..4].copy_from_slice(&endpoint.port.to_be_bytes());
    let IpAddress::Ipv4(address) = endpoint.addr;
    bytes[4..8].copy_from_slice(&address.octets());
    bytes
}

fn bind_in_state(state: &mut NetState, token: u64, mut local: IpEndpoint) -> Result<(), i32> {
    let socket = state.inet_sockets.get(&token).ok_or(libc::EBADF)?;
    if !socket.transport.is_datagram() {
        return Err(libc::EOPNOTSUPP);
    }
    if socket.local.is_some() {
        return Err(libc::EINVAL);
    }
    if !local.addr.is_unspecified() && local.addr != IpAddress::Ipv4(QEMU_USERNET_ADDR) {
        return Err(libc::EADDRNOTAVAIL);
    }
    let transport = socket.transport;
    let stack = state.inet.as_mut().ok_or(libc::ENODEV)?;
    let occupied = |candidate: u16, state: &NetState| {
        state.inet_sockets.iter().any(|(other, s)| {
            *other != token
                && s.transport.protocol() == transport.protocol()
                && s.local.is_some_and(|ep| ep.port == candidate)
        })
    };
    if local.port == 0 {
        // TCP/UDP ports and ICMP identifiers have separate namespaces. Avoid
        // duplicate ping identifiers as well as UDP endpoints before binding.
        let mut next = stack.next_port;
        local.port = select_ephemeral_port(&mut next, |port| occupied(port, state))
            .ok_or(libc::EADDRINUSE)?;
        state.inet.as_mut().ok_or(libc::ENODEV)?.next_port = next;
    } else if occupied(local.port, state) {
        return Err(libc::EADDRINUSE);
    }
    let stack = state.inet.as_mut().ok_or(libc::ENODEV)?;
    match transport {
        InetTransport::Udp(h) => {
            let address = if local.addr.is_unspecified() {
                None
            } else {
                Some(local.addr)
            };
            stack
                .sockets
                .get_mut::<udp::Socket>(h)
                .bind(smoltcp::wire::IpListenEndpoint {
                    addr: address,
                    port: local.port,
                })
                .map_err(|_| libc::EINVAL)?;
        }
        InetTransport::Icmp(h) => {
            stack
                .sockets
                .get_mut::<icmp::Socket>(h)
                .bind(icmp::Endpoint::Ident(local.port))
                .map_err(|_| libc::EINVAL)?;
        }
        InetTransport::Tcp(_) => return Err(libc::EOPNOTSUPP),
    }
    let socket = state.inet_sockets.get_mut(&token).ok_or(libc::EBADF)?;
    socket.local = Some(local);
    Ok(())
}

fn autobind(state: &mut NetState, token: u64) -> Result<(), i32> {
    if state
        .inet_sockets
        .get(&token)
        .ok_or(libc::EBADF)?
        .local
        .is_none()
    {
        bind_in_state(
            state,
            token,
            IpEndpoint::new(IpAddress::Ipv4(Ipv4Address::UNSPECIFIED), 0),
        )?;
    }
    Ok(())
}

fn admit_unprivileged_udp_bind(port: u16) -> Result<(), i32> {
    if port == 0 || port >= 1024 {
        Ok(())
    } else {
        Err(libc::EACCES)
    }
}

pub(super) fn bind(request: &NetdIpcRequest) -> i32 {
    let local = match decode_address(&request.payload[..request.payload_len as usize]) {
        Ok(ep) => ep,
        Err(e) => return e,
    };
    let mut state = lock_net_state();
    let Some(socket) = state.inet_sockets.get(&request.socket_token) else {
        return libc::EBADF;
    };
    if matches!(socket.transport, InetTransport::Udp(_)) {
        // Linux-visible credentials belong to syscalld, not these bootstrap
        // kernel UID stamps. Until a binding capability is supplied, never
        // infer privileged-port authority from request.euid == 0.
        if let Err(errno) = admit_unprivileged_udp_bind(local.port) {
            return errno;
        }
    }
    bind_in_state(&mut state, request.socket_token, local)
        .err()
        .unwrap_or(0)
}

fn unicast_destination(addr: IpAddress) -> bool {
    let IpAddress::Ipv4(addr) = addr;
    let bytes = addr.octets();
    let local = QEMU_USERNET_ADDR.octets();
    !addr.is_unspecified()
        && !addr.is_multicast()
        && !addr.is_broadcast()
        && !(bytes[..3] == local[..3] && bytes[3] == 255)
}

pub(super) fn connect(request: &NetdIpcRequest) -> i32 {
    let mut state = lock_net_state();
    if sockaddr_family(request) == Some(libc::AF_UNSPEC as u64) {
        let Some(socket) = state.inet_sockets.get_mut(&request.socket_token) else {
            return libc::EBADF;
        };
        socket.peer = None;
        return 0;
    }
    let mut peer = match decode_address(&request.payload[..request.payload_len as usize]) {
        Ok(ep) => ep,
        Err(e) => return e,
    };
    let Some(socket) = state.inet_sockets.get(&request.socket_token) else {
        return libc::EBADF;
    };
    // glibc's address ordering uses UDP connect(port=0) to discover its source
    // address. Linux admits that route probe without a usable peer port.
    if matches!(socket.transport, InetTransport::Icmp(_)) {
        peer.port = 0;
    }
    if !unicast_destination(peer.addr) {
        return libc::EOPNOTSUPP;
    }
    if let Err(e) = autobind(&mut state, request.socket_token) {
        return e;
    }
    state
        .inet_sockets
        .get_mut(&request.socket_token)
        .unwrap()
        .peer = Some(peer);
    0
}

pub(super) fn socket_name(
    request: &NetdIpcRequest,
    response: &mut NetdIpcResponse,
    peer: bool,
) -> i32 {
    let state = lock_net_state();
    let Some(socket) = state.inet_sockets.get(&request.socket_token) else {
        return libc::EBADF;
    };
    let endpoint = match socket.transport {
        InetTransport::Tcp(h) => {
            let Some(stack) = state.inet.as_ref() else {
                return libc::ENODEV;
            };
            let tcp = stack.sockets.get::<tcp::Socket>(h);
            if peer {
                tcp.remote_endpoint()
            } else {
                tcp.local_endpoint()
            }
        }
        _ => {
            if peer {
                socket
                    .peer
                    .filter(|ep| !matches!(socket.transport, InetTransport::Udp(_)) || ep.port != 0)
            } else {
                socket.local.map(|mut ep| {
                    if ep.addr.is_unspecified() && socket.peer.is_some() {
                        ep.addr = IpAddress::Ipv4(QEMU_USERNET_ADDR);
                    }
                    ep
                })
            }
        }
    };
    let endpoint = match endpoint {
        Some(ep) => ep,
        None if !peer => IpEndpoint::new(IpAddress::Ipv4(Ipv4Address::UNSPECIFIED), 0),
        None => return libc::ENOTCONN,
    };
    let bytes = encode_address(endpoint);
    response.payload[..bytes.len()].copy_from_slice(&bytes);
    response.payload_len = bytes.len() as u32;
    0
}

fn echo_request(bytes: &[u8], ident: u16) -> Result<Vec<u8>, i32> {
    if bytes.len() < 8 || bytes[0] != 8 || bytes[1] != 0 {
        return Err(libc::EINVAL);
    }
    if bytes.len() > MAX_ICMP_PAYLOAD {
        return Err(libc::EMSGSIZE);
    }
    let mut packet = bytes.to_vec();
    packet[4..6].copy_from_slice(&ident.to_be_bytes());
    Icmpv4Packet::new_unchecked(&mut packet).fill_checksum();
    Ok(packet)
}

pub(super) fn send(request: &NetdIpcRequest, response: &mut NetdIpcResponse) -> i32 {
    let flags = request_msg_flags(request);
    if flags & !((libc::MSG_DONTWAIT | libc::MSG_NOSIGNAL) as u64) != 0 {
        return libc::EOPNOTSUPP;
    }
    let Some(message) = netd_message::decode(&request.payload[..request.payload_len as usize])
    else {
        return libc::EINVAL;
    };
    if !message.control.is_empty() || message.flags != 0 {
        return libc::EOPNOTSUPP;
    }
    let mut state = lock_net_state();
    let Some(socket) = state.inet_sockets.get(&request.socket_token) else {
        return libc::EBADF;
    };
    let transport = socket.transport;
    let destination = if message.address.is_empty() {
        match socket.peer {
            Some(ep) => ep,
            None => return libc::EDESTADDRREQ,
        }
    } else {
        match decode_address(message.address) {
            Ok(ep) => ep,
            Err(e) => return e,
        }
    };
    if !unicast_destination(destination.addr) {
        return libc::EOPNOTSUPP;
    }
    if matches!(transport, InetTransport::Udp(_))
        && (destination.port == 0 || message.data.len() > MAX_UDP_PAYLOAD)
    {
        return if destination.port == 0 {
            libc::EINVAL
        } else {
            libc::EMSGSIZE
        };
    }
    // Reject malformed/non-echo ICMP before allocating a local identifier.
    if matches!(transport, InetTransport::Icmp(_)) {
        if let Err(e) = echo_request(message.data, 0) {
            return e;
        }
    }
    let pending_error =
        error_queue::take_pending(state.inet_sockets.get_mut(&request.socket_token).unwrap());
    if pending_error != 0 {
        return pending_error;
    }
    if let Err(e) = autobind(&mut state, request.socket_token) {
        return e;
    }
    let local = state.inet_sockets[&request.socket_token].local.unwrap();
    let Some(stack) = state.inet.as_mut() else {
        return libc::ENODEV;
    };
    stack.poll_budget(4);
    let result = match transport {
        InetTransport::Udp(h) => stack
            .sockets
            .get_mut::<udp::Socket>(h)
            .send_slice(message.data, destination)
            .map_err(|e| match e {
                udp::SendError::BufferFull => libc::EAGAIN,
                udp::SendError::Unaddressable => libc::ENETUNREACH,
            }),
        InetTransport::Icmp(h) => {
            let packet = match echo_request(message.data, local.port) {
                Ok(p) => p,
                Err(e) => return e,
            };
            stack
                .sockets
                .get_mut::<icmp::Socket>(h)
                .send_slice(&packet, destination.addr)
                .map_err(|e| match e {
                    icmp::SendError::BufferFull => libc::EAGAIN,
                    icmp::SendError::Unaddressable => libc::ENETUNREACH,
                })
        }
        InetTransport::Tcp(_) => return libc::EOPNOTSUPP,
    };
    if let Err(e) = result {
        return e;
    }
    response.value = message.data.len() as u64;
    stack.poll_budget(16);
    0
}

fn encode_received(
    response: &mut NetdIpcResponse,
    data: &[u8],
    source: IpEndpoint,
    limit: usize,
    flags: u64,
) -> i32 {
    let copied = data.len().min(limit);
    let truncated = copied != data.len();
    let address = encode_address(source);
    let output_flags = if truncated { libc::MSG_TRUNC as u32 } else { 0 };
    let Some(len) = netd_message::encode(
        &mut response.payload,
        &data[..copied],
        &[],
        output_flags,
        &address,
    ) else {
        return libc::EMSGSIZE;
    };
    response.payload_len = len as u32;
    response.value = if flags & libc::MSG_TRUNC as u64 != 0 {
        data.len()
    } else {
        copied
    } as u64;
    0
}

pub(super) fn recv(request: &NetdIpcRequest, response: &mut NetdIpcResponse) -> i32 {
    let flags = request_msg_flags(request);
    if flags
        & !((libc::MSG_DONTWAIT
            | libc::MSG_PEEK
            | libc::MSG_TRUNC
            | libc::MSG_CMSG_CLOEXEC
            | libc::MSG_ERRQUEUE) as u64)
        != 0
    {
        return libc::EOPNOTSUPP;
    }
    let requested = if request.op == SYSCALL_OFFLOAD_OP_LINUX_RECVMSG {
        request.payload_len as usize
    } else {
        request.arg2.min(usize::MAX as u64) as usize
    };
    let limit = requested.min(
        NETD_IPC_PAYLOAD_CAPACITY - netd_message::HEADER_SIZE - netd_message::IPV4_ADDRESS_SIZE,
    );
    let mut state = lock_net_state();
    let Some(stack) = state.inet.as_mut() else {
        return libc::ENODEV;
    };
    stack.poll_budget(8);
    if let Err(e) = stage_one(&mut state, request.socket_token) {
        return e;
    }
    let socket = state.inet_sockets.get_mut(&request.socket_token).unwrap();
    if flags & libc::MSG_ERRQUEUE as u64 != 0 {
        return error_queue::receive(socket, response, limit, flags);
    }
    let pending_error = error_queue::take_pending(socket);
    if pending_error != 0 {
        return pending_error;
    }
    receive_staged(socket, response, limit, flags)
}

fn receive_staged(
    socket: &mut InetSocket,
    response: &mut NetdIpcResponse,
    limit: usize,
    flags: u64,
) -> i32 {
    let Some(packet) = socket.received.as_ref() else {
        return libc::EAGAIN;
    };
    let result = encode_received(response, &packet.bytes, packet.source, limit, flags);
    if result == 0 && flags & libc::MSG_PEEK as u64 == 0 {
        socket.received = None;
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    pub(super) fn staged_socket(icmp_socket: bool) -> InetSocket {
        let mut sockets = SocketSet::new(Vec::new());
        let transport = if icmp_socket {
            let rx = icmp::PacketBuffer::new(vec![icmp::PacketMetadata::EMPTY; 1], vec![0; 128]);
            let tx = icmp::PacketBuffer::new(vec![icmp::PacketMetadata::EMPTY; 1], vec![0; 128]);
            InetTransport::Icmp(sockets.add(icmp::Socket::new(rx, tx)))
        } else {
            let rx = udp::PacketBuffer::new(vec![udp::PacketMetadata::EMPTY; 1], vec![0; 128]);
            let tx = udp::PacketBuffer::new(vec![udp::PacketMetadata::EMPTY; 1], vec![0; 128]);
            InetTransport::Udp(sockets.add(udp::Socket::new(rx, tx)))
        };
        let source = IpEndpoint::new(IpAddress::Ipv4(QEMU_USERNET_GATEWAY), 53);
        InetSocket {
            refs: 1,
            options: SocketOptions::default(),
            transport,
            local: None,
            peer: Some(source),
            received: Some(ReceivedPacket {
                source,
                bytes: vec![0; 40],
            }),
            errors: None,
            connect_deadline: None,
        }
    }

    #[test]
    fn privileged_udp_binding_requires_authority_not_a_bootstrap_uid_stamp() {
        assert_eq!(admit_unprivileged_udp_bind(0), Ok(()));
        assert_eq!(admit_unprivileged_udp_bind(53), Err(libc::EACCES));
        assert_eq!(admit_unprivileged_udp_bind(1023), Err(libc::EACCES));
        assert_eq!(admit_unprivileged_udp_bind(1024), Ok(()));
    }

    #[test]
    fn connected_readiness_filter_rejects_foreign_peers_and_non_echo_icmp() {
        for is_icmp in [false, true] {
            let socket = staged_socket(is_icmp);
            let packet = socket.received.as_ref().unwrap();
            assert!(eligible(&socket, packet.source, &packet.bytes));
            let foreign = IpEndpoint::new(IpAddress::Ipv4(Ipv4Address::new(1, 2, 3, 4)), 53);
            assert!(!eligible(&socket, foreign, &packet.bytes));
            if is_icmp {
                assert!(!eligible(&socket, packet.source, &[3; 40]));
            }
        }
    }

    #[test]
    fn udp_and_icmp_peek_preserve_packets_but_zero_capacity_recv_consumes() {
        for is_icmp in [false, true] {
            let mut socket = staged_socket(is_icmp);
            let mut response = NetdIpcResponse::default();
            assert_eq!(
                receive_staged(&mut socket, &mut response, 40, libc::MSG_PEEK as u64),
                0
            );
            assert_eq!(response.value, 40);
            assert!(socket.received.is_some());
            assert_eq!(receive_staged(&mut socket, &mut response, 0, 0), 0);
            assert_eq!(response.value, 0);
            assert!(socket.received.is_none());
            assert_eq!(
                receive_staged(&mut socket, &mut response, 40, 0),
                libc::EAGAIN
            );
        }
    }

    #[test]
    fn sockaddr_round_trip_preserves_network_order_and_rejects_wrong_family() {
        let endpoint = IpEndpoint::new(IpAddress::Ipv4(Ipv4Address::new(10, 0, 2, 3)), 53);
        let mut bytes = encode_address(endpoint);
        assert_eq!(bytes[2..4], [0, 53]);
        assert_eq!(decode_address(&bytes), Ok(endpoint));
        bytes[0] = libc::AF_INET6 as u8;
        assert_eq!(decode_address(&bytes), Err(libc::EAFNOSUPPORT));
        assert_eq!(decode_address(&bytes[..15]), Err(libc::EINVAL));
    }

    #[test]
    fn ping_socket_rewrites_identifier_and_checksum_and_rejects_raw_icmp() {
        let mut bytes = [0x5a; 24];
        bytes[0] = 8;
        bytes[1] = 0;
        let packet = echo_request(&bytes, 0x1234).unwrap();
        assert_eq!(packet[4..6], [0x12, 0x34]);
        assert!(Icmpv4Packet::new_checked(&packet)
            .unwrap()
            .verify_checksum());
        bytes[0] = 3;
        assert_eq!(echo_request(&bytes, 1), Err(libc::EINVAL));
        assert_eq!(echo_request(&[8, 0], 1), Err(libc::EINVAL));
    }

    #[test]
    fn datagram_truncation_reports_original_length_only_when_requested() {
        let source = IpEndpoint::new(IpAddress::Ipv4(QEMU_USERNET_GATEWAY), 53);
        for flags in [0, libc::MSG_TRUNC as u64] {
            let mut response = NetdIpcResponse::default();
            assert_eq!(
                encode_received(&mut response, b"hello", source, 2, flags),
                0
            );
            let message =
                netd_message::decode(&response.payload[..response.payload_len as usize]).unwrap();
            assert_eq!(message.data, b"he");
            assert_eq!(message.flags, libc::MSG_TRUNC as u32);
            assert_eq!(response.value, if flags == 0 { 2 } else { 5 });
        }
        let mut response = NetdIpcResponse::default();
        assert_eq!(encode_received(&mut response, b"", source, 0, 0), 0);
        assert_eq!(response.value, 0);
        assert_eq!(
            response.payload_len as usize,
            netd_message::HEADER_SIZE + 16
        );
    }
}
