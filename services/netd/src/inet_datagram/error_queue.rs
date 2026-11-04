//! Linux UDP IP_RECVERR from original Ethernet ingress. smoltcp's ICMP socket
//! re-emits quotations (losing fragmentation/MTU fields) and rejects valid short
//! quotations, so this bounded Ring3 seam preserves the validated wire bytes.
use super::*;

const CONTROL_BYTES: usize = 48;
const INGRESS_SLOTS: usize = 32;

#[derive(Default)]
pub(crate) struct ErrorIngress {
    packets: VecDeque<WireError>,
    dropped: u64,
}

struct WireError {
    offender: IpAddress,
    bytes: Vec<u8>,
}

impl ErrorIngress {
    pub(crate) fn len(&self) -> usize {
        self.packets.len()
    }

    pub(crate) fn record(&mut self, frame: &[u8]) {
        let Some(error) = capture(frame) else {
            return;
        };
        if self.packets.len() < INGRESS_SLOTS {
            self.packets.push_back(error);
        } else {
            self.dropped = self.dropped.saturating_add(1);
            if self.dropped.is_power_of_two() {
                debug_line(&format!(
                    "netd: ICMP error ingress overflow dropped={}",
                    self.dropped
                ));
            }
        }
    }
}

pub(crate) struct ErrorState {
    queue: VecDeque<DatagramError>,
    pending: Option<i32>,
}

struct DatagramError {
    errno: i32,
    type_: u8,
    code: u8,
    info: u32,
    offender: IpAddress,
    destination: IpEndpoint,
    payload: Vec<u8>,
}

impl ErrorState {
    fn new() -> Self {
        Self {
            queue: VecDeque::new(),
            pending: None,
        }
    }
    pub(super) fn has_error(&self) -> bool {
        self.pending.is_some() || !self.queue.is_empty()
    }

    fn enqueue(&mut self, error: DatagramError) {
        // Linux sk_err reflects the newest asynchronous error; the queue still
        // retains records in arrival order for MSG_ERRQUEUE.
        self.pending = Some(error.errno);
        let bytes: usize = self.queue.iter().map(|e| e.payload.len()).sum();
        if self.queue.len() < PACKET_SLOTS
            && bytes
                .checked_add(error.payload.len())
                .is_some_and(|n| n <= PACKET_BYTES)
        {
            self.queue.push_back(error);
        }
    }
}

fn checksum_valid(bytes: &[u8]) -> bool {
    let mut sum = 0_u32;
    for pair in bytes.chunks(2) {
        sum += u32::from(pair[0]) << 8 | u32::from(*pair.get(1).unwrap_or(&0));
    }
    while sum >> 16 != 0 {
        sum = (sum & 0xffff) + (sum >> 16);
    }
    sum == 0xffff
}

fn capture(frame: &[u8]) -> Option<WireError> {
    if frame.len() > NET_BROKER_PACKET_MTU {
        return None;
    }
    if frame.get(..6)? != QEMU_USERNET_MAC.as_bytes() || frame.get(12..14)? != [8, 0] {
        return None;
    }
    let ip = frame.get(14..)?;
    if ip.len() < 20 || ip[0] >> 4 != 4 || ip[9] != 1 {
        return None;
    }
    let header = usize::from(ip[0] & 15) * 4;
    if !(20..=60).contains(&header) {
        return None;
    }
    let total = usize::from(u16::from_be_bytes([ip[2], ip[3]]));
    let ip = ip.get(..total)?;
    if total < header + 8
        || !checksum_valid(ip.get(..header)?)
        || u16::from_be_bytes([ip[6], ip[7]]) & 0x3fff != 0
        || ip[16..20] != QEMU_USERNET_ADDR.octets()
    {
        return None;
    }
    let offender = Ipv4Address::new(ip[12], ip[13], ip[14], ip[15]);
    if !unicast_destination(IpAddress::Ipv4(offender)) || offender.is_loopback() {
        return None;
    }
    let bytes = ip.get(header..)?;
    if !matches!(bytes[0], 3 | 11) || !checksum_valid(bytes) {
        return None;
    }
    Some(WireError {
        offender: IpAddress::Ipv4(offender),
        bytes: bytes.to_vec(),
    })
}

pub(super) fn set_option(request: &NetdIpcRequest) -> i32 {
    let mut state = lock_net_state();
    let Some(socket) = state.inet_sockets.get_mut(&request.socket_token) else {
        return libc::EBADF;
    };
    if request.arg1 != libc::SOL_IP as u64
        || request.arg2 != libc::IP_RECVERR as u64
        || !matches!(socket.transport, InetTransport::Udp(_))
    {
        return libc::ENOPROTOOPT;
    }
    let Some(value) = request_i32_payload(request) else {
        return libc::EINVAL;
    };
    if value == 0 {
        socket.errors = None;
    } else {
        socket.errors.get_or_insert_with(ErrorState::new);
    }
    0
}

pub(super) fn get_option(request: &NetdIpcRequest, response: &mut NetdIpcResponse) -> i32 {
    let state = lock_net_state();
    let Some(socket) = state.inet_sockets.get(&request.socket_token) else {
        return libc::EBADF;
    };
    if request.arg1 != libc::SOL_IP as u64
        || request.arg2 != libc::IP_RECVERR as u64
        || !matches!(socket.transport, InetTransport::Udp(_))
    {
        return libc::ENOPROTOOPT;
    }
    response.payload[..4].copy_from_slice(&i32::from(socket.errors.is_some()).to_ne_bytes());
    response.payload_len = 4;
    0
}

fn parse_error(
    bytes: &[u8],
    offender: IpAddress,
    local: IpEndpoint,
    peer: Option<IpEndpoint>,
) -> Option<DatagramError> {
    let header = bytes.get(..8)?;
    let errno = match (header[0], header[1]) {
        (3, 0) => libc::ENETUNREACH,
        (3, 1) => libc::EHOSTUNREACH,
        (3, 2) => libc::ENOPROTOOPT,
        (3, 3) => libc::ECONNREFUSED,
        (3, 4) => libc::EMSGSIZE,
        (3, 5) => libc::EOPNOTSUPP,
        (3, 6 | 9 | 11) => libc::ENETUNREACH,
        (3, 7) => libc::EHOSTDOWN,
        (3, 8) => libc::ENONET,
        (3, 10 | 12..=15) | (11, 0..=1) => libc::EHOSTUNREACH,
        _ => return None,
    };
    let quoted = bytes.get(8..)?;
    if quoted.len() < 20 || quoted[0] >> 4 != 4 || quoted[9] != 17 {
        return None;
    }
    let ip_header = usize::from(quoted[0] & 15) * 4;
    if !(20..=60).contains(&ip_header) || !checksum_valid(quoted.get(..ip_header)?) {
        return None;
    }
    let total = usize::from(u16::from_be_bytes([quoted[2], quoted[3]]));
    // A quotation may legally omit all UDP payload while preserving the
    // original IPv4/UDP lengths. Do not compare total with quoted.len().
    if total < ip_header + 8 || u16::from_be_bytes([quoted[6], quoted[7]]) & 0x3fff != 0 {
        return None;
    }
    let udp = quoted.get(ip_header..ip_header.checked_add(8)?)?;
    let source_port = u16::from_be_bytes([udp[0], udp[1]]);
    let source = IpAddress::Ipv4(Ipv4Address::new(
        quoted[12], quoted[13], quoted[14], quoted[15],
    ));
    if source_port != local.port || source != IpAddress::Ipv4(QEMU_USERNET_ADDR) {
        return None;
    }
    let destination = IpEndpoint::new(
        IpAddress::Ipv4(Ipv4Address::new(
            quoted[16], quoted[17], quoted[18], quoted[19],
        )),
        u16::from_be_bytes([udp[2], udp[3]]),
    );
    if peer.is_some_and(|p| p != destination) {
        return None;
    }
    let udp_len = usize::from(u16::from_be_bytes([udp[4], udp[5]]));
    if udp_len < 8 || udp_len > total - ip_header {
        return None;
    }
    let end = quoted.len().min(ip_header + udp_len);
    Some(DatagramError {
        errno,
        type_: header[0],
        code: header[1],
        info: if errno == libc::EMSGSIZE {
            u32::from(u16::from_be_bytes([header[6], header[7]]))
        } else {
            0
        },
        offender,
        destination,
        payload: quoted.get(ip_header + 8..end)?.to_vec(),
    })
}

pub(super) fn stage_all(stack: &mut InetStack, sockets: &mut SocketTable<InetSocket>) {
    for _ in 0..INGRESS_SLOTS {
        let Some(wire) = stack.device.errors.packets.pop_front() else {
            return;
        };
        for socket in sockets.values_mut() {
            if !matches!(socket.transport, InetTransport::Udp(_)) {
                continue;
            }
            let (Some(local), Some(errors)) = (socket.local, socket.errors.as_mut()) else {
                continue;
            };
            if let Some(error) = parse_error(&wire.bytes, wire.offender, local, socket.peer) {
                errors.enqueue(error);
                break;
            }
        }
    }
}

pub(super) fn take_pending(socket: &mut InetSocket) -> i32 {
    socket
        .errors
        .as_mut()
        .and_then(|e| e.pending.take())
        .unwrap_or(0)
}

fn control(error: &DatagramError) -> [u8; CONTROL_BYTES] {
    let mut control = [0; CONTROL_BYTES];
    control[..8].copy_from_slice(&(CONTROL_BYTES as u64).to_ne_bytes());
    control[8..12].copy_from_slice(&(libc::SOL_IP as u32).to_ne_bytes());
    control[12..16].copy_from_slice(&(libc::IP_RECVERR as u32).to_ne_bytes());
    control[16..20].copy_from_slice(&(error.errno as u32).to_ne_bytes());
    control[20] = 2;
    control[21] = error.type_;
    control[22] = error.code;
    control[24..28].copy_from_slice(&error.info.to_ne_bytes());
    control[32..].copy_from_slice(&encode_address(IpEndpoint::new(error.offender, 0)));
    control
}

pub(super) fn receive(
    socket: &mut InetSocket,
    response: &mut NetdIpcResponse,
    limit: usize,
    flags: u64,
) -> i32 {
    let Some(errors) = socket.errors.as_mut() else {
        return libc::EAGAIN;
    };
    let Some(error) = errors.queue.front() else {
        return libc::EAGAIN;
    };
    let copied = error
        .payload
        .len()
        .min(limit)
        .min(NETD_IPC_PAYLOAD_CAPACITY - netd_message::HEADER_SIZE - 16 - CONTROL_BYTES);
    let output_flags = libc::MSG_ERRQUEUE as u32
        | if copied < error.payload.len() {
            libc::MSG_TRUNC as u32
        } else {
            0
        };
    let address = encode_address(error.destination);
    let Some(len) = netd_message::encode(
        &mut response.payload,
        &error.payload[..copied],
        &control(error),
        output_flags,
        &address,
    ) else {
        return libc::EMSGSIZE;
    };
    response.payload_len = len as u32;
    response.value = if flags & libc::MSG_TRUNC as u64 != 0 {
        error.payload.len()
    } else {
        copied
    } as u64;
    errors.queue.pop_front();
    errors.pending = errors.queue.front().map(|e| e.errno);
    0
}

#[cfg(test)]
mod tests {
    use super::*;

    fn quote(port: u16) -> Vec<u8> {
        let mut bytes = vec![0; 8 + 20 + 8 + 4];
        bytes[0] = 3;
        bytes[1] = 3;
        let ip = &mut bytes[8..];
        ip[0] = 0x45;
        ip[2..4].copy_from_slice(&32_u16.to_be_bytes());
        ip[9] = 17;
        ip[12..16].copy_from_slice(&QEMU_USERNET_ADDR.octets());
        ip[16..20].copy_from_slice(&[8, 8, 8, 8]);
        ip[20..22].copy_from_slice(&port.to_be_bytes());
        ip[22..24].copy_from_slice(&53_u16.to_be_bytes());
        ip[24..26].copy_from_slice(&12_u16.to_be_bytes());
        ip[28..32].copy_from_slice(b"dns?");
        smoltcp::wire::Ipv4Packet::new_unchecked(&mut bytes[8..]).fill_checksum();
        bytes
    }

    fn frame(error: &[u8]) -> Vec<u8> {
        let mut frame = vec![0; 14 + 20 + error.len()];
        frame[..6].copy_from_slice(QEMU_USERNET_MAC.as_bytes());
        frame[6..12].copy_from_slice(&[0x52, 0x55, 10, 0, 2, 2]);
        frame[12..14].copy_from_slice(&0x0800_u16.to_be_bytes());
        let ip = &mut frame[14..];
        ip[0] = 0x45;
        ip[2..4].copy_from_slice(&((20 + error.len()) as u16).to_be_bytes());
        ip[8] = 64;
        ip[9] = 1;
        ip[12..16].copy_from_slice(&QEMU_USERNET_GATEWAY.octets());
        ip[16..20].copy_from_slice(&QEMU_USERNET_ADDR.octets());
        ip[20..].copy_from_slice(error);
        Icmpv4Packet::new_unchecked(&mut ip[20..]).fill_checksum();
        smoltcp::wire::Ipv4Packet::new_unchecked(ip).fill_checksum();
        frame
    }

    #[derive(Default)]
    struct FrameDevice {
        frame: Option<Vec<u8>>,
        errors: ErrorIngress,
    }
    struct FrameRx(Vec<u8>);
    struct FrameTx;
    impl RxToken for FrameRx {
        fn consume<R, F: FnOnce(&[u8]) -> R>(self, f: F) -> R {
            f(&self.0)
        }
    }
    impl TxToken for FrameTx {
        fn consume<R, F: FnOnce(&mut [u8]) -> R>(self, len: usize, f: F) -> R {
            f(&mut vec![0; len])
        }
    }
    impl Device for FrameDevice {
        type RxToken<'a> = FrameRx;
        type TxToken<'a> = FrameTx;
        fn receive(&mut self, _: SmolInstant) -> Option<(FrameRx, FrameTx)> {
            let frame = self.frame.take()?;
            self.errors.record(&frame);
            Some((FrameRx(frame), FrameTx))
        }
        fn transmit(&mut self, _: SmolInstant) -> Option<FrameTx> {
            Some(FrameTx)
        }
        fn capabilities(&self) -> DeviceCapabilities {
            let mut c = DeviceCapabilities::default();
            c.medium = Medium::Ethernet;
            c.max_transmission_unit = NET_BROKER_PACKET_MTU;
            c
        }
    }

    fn ingress_state(error: &[u8]) -> NetState {
        let mut device = BrokerDevice {
            rx: [0; NET_BROKER_PACKET_MTU],
            errors: ErrorIngress::default(),
        };
        let config = SmolConfig::new(HardwareAddress::Ethernet(QEMU_USERNET_MAC));
        let mut iface = Interface::new(config, &mut device, SmolInstant::from_millis(0));
        iface.update_ip_addrs(|a| {
            a.push(IpCidr::new(IpAddress::Ipv4(QEMU_USERNET_ADDR), 24))
                .unwrap();
        });
        let mut stack = InetStack {
            iface,
            device,
            sockets: SocketSet::new(Vec::new()),
            next_port: 49152,
            closing: Vec::new(),
            readiness_dirty: false,
            last_timer_sweep: StdInstant::now(),
        };
        let mut socket = super::super::tests::staged_socket(false);
        socket.transport = add_socket(&mut stack, linux_abi::SOCK_DGRAM, 17);
        let local = IpEndpoint::new(IpAddress::Ipv4(QEMU_USERNET_ADDR), 49152);
        stack
            .sockets
            .get_mut::<udp::Socket>(socket.transport.handle())
            .bind(local)
            .unwrap();
        socket.local = Some(local);
        socket.peer = Some(IpEndpoint::new(
            IpAddress::Ipv4(Ipv4Address::new(8, 8, 8, 8)),
            53,
        ));
        socket.received = None;
        socket.errors = Some(ErrorState::new());
        let mut state = NetState::new();
        state.inet_sockets.insert(1, socket);
        state.inet = Some(stack);
        state.record_readiness_changes();
        state.readiness_publications.clear();
        let stack = state.inet.as_mut().unwrap();
        let mut device = FrameDevice {
            frame: Some(frame(error)),
            ..FrameDevice::default()
        };
        let _ = stack.iface.poll_ingress_single(
            SmolInstant::from_millis(1),
            &mut device,
            &mut stack.sockets,
        );
        stack.device.errors = device.errors;
        state
    }

    fn ingress(error: &[u8]) -> InetSocket {
        let mut state = ingress_state(error);
        stage_all(state.inet.as_mut().unwrap(), &mut state.inet_sockets);
        state.inet_sockets.remove(&1).unwrap()
    }

    #[test]
    fn another_sockets_staging_publishes_cross_socket_error_before_unlock() {
        let mut state = ingress_state(&quote(49152));
        let before = state.readiness_generations[&1];
        let transport = add_socket(state.inet.as_mut().unwrap(), linux_abi::SOCK_DGRAM, 17);
        let mut other = super::super::tests::staged_socket(false);
        other.transport = transport;
        other.received = None;
        state.inet_sockets.insert(2, other);
        let state = Mutex::new(state);
        let generation;
        {
            let mut guard = crate::readiness::NetStateGuard::new(state.lock().unwrap());
            assert_eq!(crate::inet_datagram::revents(&mut guard, 2, libc::POLLIN as u32), Ok(0));
            generation = guard.socket_readiness_generation(1);
            assert!(generation > before);
            assert!(guard.readiness_publications.contains(&(1, generation)));
            assert_ne!(crate::inet_datagram::revents(&mut guard, 1, 0).unwrap() & libc::POLLERR as u32, 0);
        }
        assert_eq!(state.lock().unwrap().readiness_generations[&1], generation);
    }

    #[test]
    fn stack_poll_inside_another_handler_stamps_all_affected_sockets() {
        let mut state = ingress_state(&quote(49152));
        let before = state.readiness_generations[&1];
        // The shared poll, not a request-success classifier, is the producer.
        // In host tests broker RX is ENOSYS; the real captured error is already
        // waiting in the exact device's raw ingress queue.
        state.inet.as_mut().unwrap().poll_budget(1);
        state.record_readiness_changes();
        assert!(state.readiness_generations[&1] > before);
        assert!(state.readiness_publications.iter().any(|(token, _)| *token == 1));
    }

    #[test]
    fn real_ipv4_icmp_ingress_reaches_udp_error_queue() {
        let mut socket = ingress(&quote(49152));
        assert_eq!(take_pending(&mut socket), libc::ECONNREFUSED);
        let mut response = NetdIpcResponse::default();
        assert_eq!(
            receive(&mut socket, &mut response, 32, libc::MSG_ERRQUEUE as u64),
            0
        );
        let message =
            netd_message::decode(&response.payload[..response.payload_len as usize]).unwrap();
        assert_eq!(message.data, b"dns?");
        assert_eq!(message.control[20..24], [2, 3, 3, 0]);
    }

    #[test]
    fn minimum_quote_and_original_mtu_survive_real_ingress_but_fragments_do_not() {
        let mut minimum = quote(49152);
        minimum.truncate(8 + 20 + 8);
        let mut socket = ingress(&minimum);
        assert_eq!(take_pending(&mut socket), libc::ECONNREFUSED);
        assert!(socket.errors.as_ref().unwrap().queue[0].payload.is_empty());
        minimum[1] = 4;
        minimum[6..8].copy_from_slice(&1280_u16.to_be_bytes());
        let mut socket = ingress(&minimum);
        assert_eq!(take_pending(&mut socket), libc::EMSGSIZE);
        let mut response = NetdIpcResponse::default();
        assert_eq!(
            receive(&mut socket, &mut response, 32, libc::MSG_ERRQUEUE as u64),
            0
        );
        let message =
            netd_message::decode(&response.payload[..response.payload_len as usize]).unwrap();
        assert_eq!(
            u32::from_ne_bytes(message.control[24..28].try_into().unwrap()),
            1280
        );
        minimum[15] = 1;
        smoltcp::wire::Ipv4Packet::new_unchecked(&mut minimum[8..]).fill_checksum();
        assert!(!ingress(&minimum).errors.as_ref().unwrap().has_error());
    }

    #[test]
    fn quoted_source_port_and_connected_peer_are_authority_not_just_packet_shape() {
        assert!(!ingress(&quote(49153)).errors.as_ref().unwrap().has_error());
        let mut foreign = quote(49152);
        foreign[24..28].copy_from_slice(&[8, 8, 8, 9]);
        smoltcp::wire::Ipv4Packet::new_unchecked(&mut foreign[8..]).fill_checksum();
        assert!(!ingress(&foreign).errors.as_ref().unwrap().has_error());
        let mut source = quote(49152);
        source[20..24].copy_from_slice(&[10, 0, 2, 16]);
        smoltcp::wire::Ipv4Packet::new_unchecked(&mut source[8..]).fill_checksum();
        assert!(!ingress(&source).errors.as_ref().unwrap().has_error());
        let mut unknown = quote(49152);
        unknown[1] = 255;
        assert!(!ingress(&unknown).errors.as_ref().unwrap().has_error());
    }

    #[test]
    fn malformed_or_foreign_wire_errors_are_not_captured() {
        let mut bytes = frame(&quote(49152));
        assert!(capture(&bytes).is_some());
        bytes[24] ^= 1;
        assert!(capture(&bytes).is_none());
        let mut bytes = frame(&quote(49152));
        bytes[40] ^= 1;
        assert!(capture(&bytes).is_none());
        let mut bytes = frame(&quote(49152));
        bytes[0] ^= 1;
        assert!(capture(&bytes).is_none());
    }

    #[test]
    fn so_error_tracks_the_latest_error_without_reordering_error_queue() {
        let local = IpEndpoint::new(IpAddress::Ipv4(QEMU_USERNET_ADDR), 49152);
        let offender = IpAddress::Ipv4(QEMU_USERNET_GATEWAY);
        let mut state = ErrorState::new();
        state.enqueue(parse_error(&quote(local.port), offender, local, None).unwrap());
        let mut newest = quote(local.port);
        newest[1] = 0;
        state.enqueue(parse_error(&newest, offender, local, None).unwrap());
        let mut socket = super::super::tests::staged_socket(false);
        socket.errors = Some(state);
        assert_eq!(take_pending(&mut socket), libc::ENETUNREACH);
        assert_eq!(
            socket.errors.as_ref().unwrap().queue[0].errno,
            libc::ECONNREFUSED
        );
        let mut response = NetdIpcResponse::default();
        assert_eq!(
            receive(&mut socket, &mut response, 32, libc::MSG_ERRQUEUE as u64),
            0
        );
        assert_eq!(take_pending(&mut socket), libc::ENETUNREACH);
    }

    #[test]
    fn error_queue_and_so_error_are_independently_consumable_and_bounded() {
        let local = IpEndpoint::new(IpAddress::Ipv4(QEMU_USERNET_ADDR), 49152);
        let offender = IpAddress::Ipv4(QEMU_USERNET_GATEWAY);
        let mut socket = ingress(&quote(local.port));
        assert_eq!(take_pending(&mut socket), libc::ECONNREFUSED);
        assert_eq!(take_pending(&mut socket), 0);
        assert!(socket.errors.as_ref().unwrap().has_error());
        let mut response = NetdIpcResponse::default();
        assert_eq!(
            receive(&mut socket, &mut response, 32, libc::MSG_ERRQUEUE as u64),
            0
        );
        assert!(!socket.errors.as_ref().unwrap().has_error());
        assert_eq!(
            receive(&mut socket, &mut response, 32, libc::MSG_ERRQUEUE as u64),
            libc::EAGAIN
        );
        let mut state = ErrorState::new();
        for _ in 0..PACKET_SLOTS + 2 {
            state.enqueue(parse_error(&quote(local.port), offender, local, None).unwrap());
        }
        assert_eq!(state.queue.len(), PACKET_SLOTS);
        let mut state = ErrorState::new();
        for size in [8000, 8000, 400] {
            let mut e = parse_error(&quote(local.port), offender, local, None).unwrap();
            e.payload = vec![0; size];
            state.enqueue(e);
        }
        assert_eq!(state.queue.len(), 2);
    }
}
