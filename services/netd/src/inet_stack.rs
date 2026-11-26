//! Embedded smoltcp stack and the packet-broker device it drives.
//!
//! - **Owner:** `netd` owns the stack; the DVM provider owns the packet ring.
//! - **Boundary:** Ingress frames from the packet broker are untrusted wire bytes.
//! - **Lifecycle:** Created lazily once the provider is active; polled by the readiness worker and request handlers under the state lock.
//! - **Concurrency:** Every access holds the global `NetState` mutex; bounded ingress per turn.
//! - **Failure:** Broker errors surface as idle polls; silent timer transitions are caught by the periodic readiness sweep.
//! - **Forbidden:** No poll outside the state lock, no unbounded ingress per turn, no readiness change without a sweep request.
//! - **Evidence:** `readiness_tests`, `error_queue` tests, `dvm-network-ingress`.

use std::cell::RefCell;

use super::*;
use smoltcp::phy::ChecksumCapabilities;
use smoltcp::wire::{
    ArpOperation, ArpPacket, ArpRepr, EthernetFrame, EthernetProtocol, EthernetRepr,
    Icmpv6Packet, Icmpv6Repr, IpProtocol, Ipv4Packet, Ipv6Packet, Ipv6Repr, NdiscNeighborFlags,
    NdiscRepr, RawHardwareAddress,
};

pub(super) const LINK_LOCAL_IPV6: Ipv6Address =
    Ipv6Address::new(0xfe80, 0, 0, 0, 0x5054, 0x00ff, 0xfe12, 0x3456);

fn is_local_ipv4(addr: Ipv4Address) -> bool {
    addr.octets()[0] == 127 || addr == QEMU_USERNET_ADDR
}

fn is_local_ipv6(addr: Ipv6Address) -> bool {
    addr.is_loopback() || addr == LINK_LOCAL_IPV6
}

thread_local! {
    static TX_SCRATCH: RefCell<[u8; NET_BROKER_PACKET_MTU]> =
        const { RefCell::new([0; NET_BROKER_PACKET_MTU]) };
}

pub(super) struct InetStack {
    pub(super) iface: Interface,
    pub(super) sockets: SocketSet<'static>,
    pub(super) device: BrokerDevice,
    pub(super) next_port: u16,
    /// Sockets whose owner closed them while the connection was still live.
    /// They stay in the set until the FIN handshake finishes (or the deadline
    /// passes) so the peer observes an orderly close instead of silence.
    pub(super) closing: Vec<(SocketHandle, StdInstant)>,
    pub(super) readiness_dirty: bool,
    /// When the last timer-driven full readiness sweep was requested.
    pub(super) last_timer_sweep: StdInstant,
}

impl InetStack {
    pub(super) fn new() -> Result<Self, i32> {
        if packet_provider_state()? != PacketProviderState::Active {
            return Err(libc::ENODEV);
        }
        let mut device = BrokerDevice::default();
        let mut config = SmolConfig::new(HardwareAddress::Ethernet(QEMU_USERNET_MAC));
        // TCP sequence/stack randomness must not share a deterministic boot seed.
        let seed = mint_socket_token()?;
        config.random_seed = seed;
        let mut iface = Interface::new(config, &mut device, smol_now());
        iface.update_ip_addrs(|ip_addrs| {
            let _ = ip_addrs.push(IpCidr::new(IpAddress::Ipv4(QEMU_USERNET_ADDR), 24));
            let _ = ip_addrs.push(IpCidr::new(IpAddress::Ipv4(Ipv4Address::LOCALHOST), 8));
            let _ = ip_addrs.push(IpCidr::new(IpAddress::Ipv6(Ipv6Address::LOCALHOST), 128));
            let _ = ip_addrs.push(IpCidr::new(IpAddress::Ipv6(LINK_LOCAL_IPV6), 64));
        });
        iface
            .routes_mut()
            .add_default_ipv4_route(QEMU_USERNET_GATEWAY)
            .map_err(|_| libc::ENOSPC)?;
        seed_loopback_neighbors(&mut device);
        let mut stack = Self {
            iface,
            sockets: SocketSet::new(Vec::new()),
            device,
            // Unpredictable start so off-path peers cannot guess the next source port.
            next_port: 49152 + (seed % 16384) as u16,
            closing: Vec::new(),
            readiness_dirty: false,
            last_timer_sweep: StdInstant::now(),
        };
        stack.poll_budget(4);
        Ok(stack)
    }

    pub(super) fn add_tcp_socket(&mut self) -> SocketHandle {
        let rx = tcp::SocketBuffer::new(vec![0; INET_TCP_BUFFER_CAPACITY]);
        let tx = tcp::SocketBuffer::new(vec![0; INET_TCP_BUFFER_CAPACITY]);
        let handle = self.sockets.add(tcp::Socket::new(rx, tx));
        inet_tcp::forget_tcp_termination(handle);
        handle
    }

    pub(super) fn remove(&mut self, transport: InetTransport, immediate_abort: bool) {
        let handle = transport.handle();
        if transport.is_datagram() {
            self.sockets.remove(handle);
            return;
        }
        let socket = self.sockets.get_mut::<tcp::Socket>(handle);
        if immediate_abort {
            socket.abort();
            self.discard_aborted(&[handle]);
            return;
        }
        // `close` queues a FIN for a synchronized connection and drops a
        // half-open or listening one straight to Closed.
        socket.close();
        if socket.is_open() && self.closing.len() < INET_CLOSING_CAPACITY {
            self.closing
                .push((handle, StdInstant::now() + INET_CLOSE_LINGER));
        } else {
            socket.abort();
            self.discard_aborted(&[handle]);
        }
    }

    /// Removes aborted sockets after one egress pass: `abort` only queues the
    /// RST, so removing first would leave the peer holding a dead connection.
    fn discard_aborted(&mut self, handles: &[SocketHandle]) {
        let _ = self
            .iface
            .poll_egress(smol_now(), &mut self.device, &mut self.sockets);
        for handle in handles {
            self.sockets.remove(*handle);
            inet_tcp::forget_tcp_termination(*handle);
        }
    }

    pub(super) fn reap_closing(&mut self) {
        if self.closing.is_empty() {
            return;
        }
        let now = StdInstant::now();
        let sockets = &mut self.sockets;
        let mut expired = Vec::new();
        self.closing.retain(|(handle, deadline)| {
            let socket = sockets.get_mut::<tcp::Socket>(*handle);
            if socket.is_open() && now < *deadline {
                return true;
            }
            socket.abort();
            expired.push(*handle);
            false
        });
        if !expired.is_empty() {
            self.discard_aborted(&expired);
        }
    }

    pub(super) fn poll_one(&mut self) -> (bool, bool) {
        let now = smol_now();
        self.iface.poll_maintenance(now);
        let ingress = self
            .iface
            .poll_ingress_single(now, &mut self.device, &mut self.sockets);
        let egress = self
            .iface
            .poll_egress(now, &mut self.device, &mut self.sockets);
        // Captured ICMP errors stay queued until a readiness observation
        // stages them, so any outstanding capture (new or not) is a change.
        let changed =
            poll_turn_changes_readiness(ingress, egress) || self.device.errors.len() != 0;
        // smoltcp reports a socket-state change only when a packet was
        // processed or emitted. A timer-only transition (retransmit or
        // connect timeout reaching `Closed`, TIME-WAIT expiry) is silent, so a
        // full INET sweep is still owed periodically - but not on every poll
        // turn: the idle worker and every request used to force an O(sockets)
        // sweep each time, whether or not anything had happened.
        let timer_sweep_due =
            self.last_timer_sweep.elapsed() >= INET_TIMER_SWEEP_INTERVAL;
        if changed || timer_sweep_due {
            inet_tcp::observe_tcp_states(&self.sockets);
            self.readiness_dirty = true;
        }
        if timer_sweep_due {
            self.last_timer_sweep = StdInstant::now();
        }
        (
            !matches!(ingress, PollIngressSingleResult::None),
            changed,
        )
    }

    pub(super) fn poll_budget(&mut self, budget: usize) -> bool {
        let mut readiness_changed = false;
        for _ in 0..budget {
            let (processed_ingress, changed) = self.poll_one();
            readiness_changed |= changed;
            if !processed_ingress && self.device.loopback_rx.is_empty() {
                break;
            }
        }
        self.reap_closing();
        readiness_changed
    }

    pub(super) fn next_ephemeral_port(&mut self) -> Option<u16> {
        // Include lingering closes: smoltcp does not reject duplicate local endpoints.
        // One pass builds a 16K-bit stack bitset for 49152..=65535 with zero allocation.
        let mut used = [0_u64; 256];
        for (_, socket) in self.sockets.iter() {
            let port = match socket {
                smoltcp::socket::Socket::Tcp(tcp) if tcp.is_open() => {
                    tcp.local_endpoint().map(|ep| ep.port)
                }
                smoltcp::socket::Socket::Udp(udp) if udp.is_open() => Some(udp.endpoint().port),
                _ => None,
            };
            if let Some(offset) = port.and_then(|p| p.checked_sub(49152)) {
                used[(offset >> 6) as usize] |= 1_u64 << (offset & 63);
            }
        }
        select_ephemeral_port(&mut self.next_port, |candidate| {
            let offset = candidate - 49152;
            (used[(offset >> 6) as usize] & (1_u64 << (offset & 63))) != 0
        })
    }
}

pub(super) fn select_ephemeral_port(next: &mut u16, mut in_use: impl FnMut(u16) -> bool) -> Option<u16> {
    for _ in 0..=(u16::MAX - 49152) {
        let candidate = *next;
        *next = if candidate == u16::MAX { 49152 } else { candidate + 1 };
        if !in_use(candidate) {
            return Some(candidate);
        }
    }
    None
}

pub(super) fn poll_turn_changes_readiness(ingress: PollIngressSingleResult, egress: PollResult) -> bool {
    matches!(ingress, PollIngressSingleResult::SocketStateChanged)
        || matches!(egress, PollResult::SocketStateChanged)
}

/// Owns one receive scratch frame so an empty poll costs no allocation.
pub(super) struct BrokerDevice {
    pub(super) rx: [u8; NET_BROKER_PACKET_MTU],
    pub(super) loopback_rx: VecDeque<Vec<u8>>,
    pub(super) errors: inet_datagram::ErrorIngress,
}

impl Default for BrokerDevice {
    fn default() -> Self {
        Self {
            rx: [0; NET_BROKER_PACKET_MTU],
            loopback_rx: VecDeque::new(),
            errors: inet_datagram::ErrorIngress::default(),
        }
    }
}

pub(super) enum BrokerRxToken<'a> {
    Borrowed(&'a [u8]),
    /// Loopback already owns this frame. Hand that ownership to the receive
    /// token instead of copying it into the external-provider scratch area.
    Owned(Vec<u8>),
}

pub(super) struct BrokerTxToken<'a> {
    loopback_rx: &'a mut VecDeque<Vec<u8>>,
}

impl Device for BrokerDevice {
    type RxToken<'a> = BrokerRxToken<'a>;
    type TxToken<'a> = BrokerTxToken<'a>;

    fn receive(
        &mut self,
        _timestamp: SmolInstant,
    ) -> Option<(Self::RxToken<'_>, Self::TxToken<'_>)> {
        if let Some(mut frame) = self.loopback_rx.pop_front() {
            frame.truncate(self.rx.len());
            self.errors.record(&frame);
            return Some((
                BrokerRxToken::Owned(frame),
                BrokerTxToken {
                    loopback_rx: &mut self.loopback_rx,
                },
            ));
        }
        match packet_rx(self.rx.as_mut_slice()) {
            Ok(len) if len > 0 && len <= self.rx.len() => {
                self.errors.record(&self.rx[..len]);
                Some((
                    BrokerRxToken::Borrowed(&self.rx[..len]),
                    BrokerTxToken {
                        loopback_rx: &mut self.loopback_rx,
                    },
                ))
            }
            _ => None,
        }
    }

    fn transmit(&mut self, _timestamp: SmolInstant) -> Option<Self::TxToken<'_>> {
        Some(BrokerTxToken {
            loopback_rx: &mut self.loopback_rx,
        })
    }

    fn capabilities(&self) -> DeviceCapabilities {
        let mut caps = DeviceCapabilities::default();
        caps.medium = Medium::Ethernet;
        caps.max_transmission_unit = NET_BROKER_PACKET_MTU;
        caps
    }
}

impl RxToken for BrokerRxToken<'_> {
    fn consume<R, F>(self, f: F) -> R
    where
        F: FnOnce(&[u8]) -> R,
    {
        match self {
            Self::Borrowed(frame) => f(frame),
            Self::Owned(frame) => f(&frame),
        }
    }
}

pub(super) fn seed_loopback_neighbors(device: &mut BrokerDevice) {
    let push_arp = |queue: &mut VecDeque<Vec<u8>>, src_ip: Ipv4Address| {
        let arp_repr = ArpRepr::EthernetIpv4 {
            operation: ArpOperation::Reply,
            source_hardware_addr: QEMU_USERNET_MAC,
            source_protocol_addr: src_ip,
            target_hardware_addr: QEMU_USERNET_MAC,
            target_protocol_addr: QEMU_USERNET_ADDR,
        };
        let eth_repr = EthernetRepr {
            src_addr: QEMU_USERNET_MAC,
            dst_addr: QEMU_USERNET_MAC,
            ethertype: EthernetProtocol::Arp,
        };
        let mut frame = vec![0_u8; eth_repr.buffer_len() + arp_repr.buffer_len()];
        let mut eth_pkt = EthernetFrame::new_unchecked(&mut frame);
        eth_repr.emit(&mut eth_pkt);
        let mut arp_pkt = ArpPacket::new_unchecked(eth_pkt.payload_mut());
        arp_repr.emit(&mut arp_pkt);
        queue.push_back(frame);
    };
    let push_na = |queue: &mut VecDeque<Vec<u8>>, src_ip: Ipv6Address| {
        let advert = Icmpv6Repr::Ndisc(NdiscRepr::NeighborAdvert {
            flags: NdiscNeighborFlags::OVERRIDE,
            target_addr: src_ip,
            lladdr: Some(RawHardwareAddress::from_bytes(&QEMU_USERNET_MAC.0)),
        });
        let ip_repr = Ipv6Repr {
            src_addr: src_ip,
            dst_addr: Ipv6Address::LOCALHOST,
            next_header: IpProtocol::Icmpv6,
            payload_len: advert.buffer_len(),
            hop_limit: 255,
        };
        let eth_repr = EthernetRepr {
            src_addr: QEMU_USERNET_MAC,
            dst_addr: QEMU_USERNET_MAC,
            ethertype: EthernetProtocol::Ipv6,
        };
        let mut frame =
            vec![0_u8; eth_repr.buffer_len() + ip_repr.buffer_len() + advert.buffer_len()];
        let mut eth_pkt = EthernetFrame::new_unchecked(&mut frame);
        eth_repr.emit(&mut eth_pkt);
        let mut ip_pkt = Ipv6Packet::new_unchecked(eth_pkt.payload_mut());
        ip_repr.emit(&mut ip_pkt);
        let mut icmp_pkt = Icmpv6Packet::new_unchecked(ip_pkt.payload_mut());
        advert.emit(
            &ip_repr.src_addr,
            &ip_repr.dst_addr,
            &mut icmp_pkt,
            &ChecksumCapabilities::default(),
        );
        queue.push_back(frame);
    };
    push_arp(&mut device.loopback_rx, Ipv4Address::LOCALHOST);
    push_arp(&mut device.loopback_rx, QEMU_USERNET_ADDR);
    push_na(&mut device.loopback_rx, Ipv6Address::LOCALHOST);
    push_na(&mut device.loopback_rx, LINK_LOCAL_IPV6);
}

fn maybe_reflect_loopback(frame: &[u8], loopback_rx: &mut VecDeque<Vec<u8>>) -> bool {
    let Ok(eth) = EthernetFrame::new_checked(frame) else {
        return false;
    };
    match eth.ethertype() {
        EthernetProtocol::Arp => {
            let Ok(arp) = ArpPacket::new_checked(eth.payload()) else {
                return false;
            };
            let Ok(ArpRepr::EthernetIpv4 {
                operation: ArpOperation::Request,
                source_hardware_addr,
                source_protocol_addr,
                target_protocol_addr,
                ..
            }) = ArpRepr::parse(&arp) else {
                return false;
            };
            if !is_local_ipv4(target_protocol_addr) {
                return false;
            }
            let reply_repr = ArpRepr::EthernetIpv4 {
                operation: ArpOperation::Reply,
                source_hardware_addr: QEMU_USERNET_MAC,
                source_protocol_addr: target_protocol_addr,
                target_hardware_addr: source_hardware_addr,
                target_protocol_addr: source_protocol_addr,
            };
            let eth_repr = EthernetRepr {
                src_addr: QEMU_USERNET_MAC,
                dst_addr: source_hardware_addr,
                ethertype: EthernetProtocol::Arp,
            };
            let mut reply = vec![0_u8; eth_repr.buffer_len() + reply_repr.buffer_len()];
            let mut eth_pkt = EthernetFrame::new_unchecked(&mut reply);
            eth_repr.emit(&mut eth_pkt);
            let mut arp_pkt = ArpPacket::new_unchecked(eth_pkt.payload_mut());
            reply_repr.emit(&mut arp_pkt);
            loopback_rx.push_back(reply);
            true
        }
        EthernetProtocol::Ipv4 => {
            let Ok(ip) = Ipv4Packet::new_checked(eth.payload()) else {
                return false;
            };
            if is_local_ipv4(ip.dst_addr()) {
                loopback_rx.push_back(frame.to_vec());
                true
            } else {
                false
            }
        }
        EthernetProtocol::Ipv6 => {
            let Ok(ip) = Ipv6Packet::new_checked(eth.payload()) else {
                return false;
            };
            if is_local_ipv6(ip.dst_addr()) {
                loopback_rx.push_back(frame.to_vec());
                return true;
            }
            if ip.next_header() == IpProtocol::Icmpv6 {
                if let Ok(icmp) = Icmpv6Packet::new_checked(ip.payload()) {
                    if let Ok(Icmpv6Repr::Ndisc(NdiscRepr::NeighborSolicit { target_addr, .. })) =
                        Icmpv6Repr::parse(
                            &ip.src_addr(),
                            &ip.dst_addr(),
                            &icmp,
                            &ChecksumCapabilities::ignored(),
                        )
                    {
                        if is_local_ipv6(target_addr) {
                            let advert = Icmpv6Repr::Ndisc(NdiscRepr::NeighborAdvert {
                                flags: NdiscNeighborFlags::SOLICITED
                                    | NdiscNeighborFlags::OVERRIDE,
                                target_addr,
                                lladdr: Some(RawHardwareAddress::from_bytes(&QEMU_USERNET_MAC.0)),
                            });
                            let dst_ip = if ip.src_addr().is_unspecified() {
                                Ipv6Address::LOCALHOST
                            } else {
                                ip.src_addr()
                            };
                            let ip_repr = Ipv6Repr {
                                src_addr: target_addr,
                                dst_addr: dst_ip,
                                next_header: IpProtocol::Icmpv6,
                                payload_len: advert.buffer_len(),
                                hop_limit: 255,
                            };
                            let eth_repr = EthernetRepr {
                                src_addr: QEMU_USERNET_MAC,
                                dst_addr: eth.src_addr(),
                                ethertype: EthernetProtocol::Ipv6,
                            };
                            let total = eth_repr.buffer_len()
                                + ip_repr.buffer_len()
                                + advert.buffer_len();
                            let mut reply = vec![0_u8; total];
                            let mut eth_pkt = EthernetFrame::new_unchecked(&mut reply);
                            eth_repr.emit(&mut eth_pkt);
                            let mut ip_pkt = Ipv6Packet::new_unchecked(eth_pkt.payload_mut());
                            ip_repr.emit(&mut ip_pkt);
                            let mut icmp_pkt = Icmpv6Packet::new_unchecked(ip_pkt.payload_mut());
                            advert.emit(
                                &ip_repr.src_addr,
                                &ip_repr.dst_addr,
                                &mut icmp_pkt,
                                &ChecksumCapabilities::default(),
                            );
                            loopback_rx.push_back(reply);
                            return true;
                        }
                    }
                }
            }
            false
        }
        _ => false,
    }
}

impl TxToken for BrokerTxToken<'_> {
    fn consume<R, F>(self, len: usize, f: F) -> R
    where
        F: FnOnce(&mut [u8]) -> R,
    {
        TX_SCRATCH.with(|cell| {
            let mut buf = cell.borrow_mut();
            let len = len.min(NET_BROKER_PACKET_MTU);
            // smoltcp may leave padding/reserved bytes unwritten; never let a
            // previous frame's bytes reach the wire.
            buf[..len].fill(0);
            let result = f(&mut buf[..len]);
            if !maybe_reflect_loopback(&buf[..len], self.loopback_rx) {
                let _ = packet_tx(&buf[..len]);
            }
            result
        })
    }
}

pub(super) fn smol_now() -> SmolInstant {
    static START: OnceLock<StdInstant> = OnceLock::new();
    let start = START.get_or_init(StdInstant::now);
    SmolInstant::from_millis(start.elapsed().as_millis().min(i64::MAX as u128) as i64)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loopback_receive_keeps_owned_bytes_and_external_scratch_untouched() {
        let mut device = BrokerDevice::default();
        device.rx.fill(0xa5);
        let frame = vec![0x3c; 64];
        let original = frame.as_ptr();
        device.loopback_rx.push_back(frame);
        {
            let (rx, _tx) = device.receive(SmolInstant::from_millis(0)).unwrap();
            rx.consume(|bytes| {
                assert_eq!(bytes.as_ptr(), original);
                assert_eq!(bytes, &[0x3c; 64]);
            });
        }
        assert!(device.rx.iter().all(|&byte| byte == 0xa5));
        assert!(device.loopback_rx.is_empty());
    }
}
