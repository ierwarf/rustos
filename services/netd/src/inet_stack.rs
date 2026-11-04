//! Embedded smoltcp stack and the packet-broker device it drives.
//!
//! - **Owner:** `netd` owns the stack; the DVM provider owns the packet ring.
//! - **Boundary:** Ingress frames from the packet broker are untrusted wire bytes.
//! - **Lifecycle:** Created lazily once the provider is active; polled by the readiness worker and request handlers under the state lock.
//! - **Concurrency:** Every access holds the global `NetState` mutex; bounded ingress per turn.
//! - **Failure:** Broker errors surface as idle polls; silent timer transitions are caught by the periodic readiness sweep.
//! - **Forbidden:** No poll outside the state lock, no unbounded ingress per turn, no readiness change without a sweep request.
//! - **Evidence:** `readiness_tests`, `error_queue` tests, `dvm-network-ingress`.

use super::*;

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
        let mut device = BrokerDevice {
            rx: [0; NET_BROKER_PACKET_MTU],
            errors: inet_datagram::ErrorIngress::default(),
        };
        let mut config = SmolConfig::new(HardwareAddress::Ethernet(QEMU_USERNET_MAC));
        // TCP sequence/stack randomness must not share a deterministic boot seed.
        config.random_seed = mint_socket_token()?;
        let mut iface = Interface::new(config, &mut device, smol_now());
        iface.update_ip_addrs(|ip_addrs| {
            let _ = ip_addrs.push(IpCidr::new(IpAddress::Ipv4(QEMU_USERNET_ADDR), 24));
        });
        iface
            .routes_mut()
            .add_default_ipv4_route(QEMU_USERNET_GATEWAY)
            .map_err(|_| libc::ENOSPC)?;
        Ok(Self {
            iface,
            sockets: SocketSet::new(Vec::new()),
            device,
            next_port: 49152,
            closing: Vec::new(),
            readiness_dirty: false,
            last_timer_sweep: StdInstant::now(),
        })
    }

    pub(super) fn add_tcp_socket(&mut self) -> SocketHandle {
        let rx = tcp::SocketBuffer::new(vec![0; INET_TCP_BUFFER_CAPACITY]);
        let tx = tcp::SocketBuffer::new(vec![0; INET_TCP_BUFFER_CAPACITY]);
        self.sockets.add(tcp::Socket::new(rx, tx))
    }

    pub(super) fn remove(&mut self, transport: InetTransport) {
        let handle = transport.handle();
        if transport.is_datagram() {
            self.sockets.remove(handle);
            return;
        }
        let socket = self.sockets.get_mut::<tcp::Socket>(handle);
        // `close` queues a FIN for a synchronized connection and drops a
        // half-open or listening one straight to Closed.
        socket.close();
        if socket.is_open() && self.closing.len() < INET_CLOSING_CAPACITY {
            self.closing
                .push((handle, StdInstant::now() + INET_CLOSE_LINGER));
        } else {
            socket.abort();
            self.sockets.remove(handle);
        }
    }

    pub(super) fn reap_closing(&mut self) {
        if self.closing.is_empty() {
            return;
        }
        let now = StdInstant::now();
        let sockets = &mut self.sockets;
        self.closing.retain(|(handle, deadline)| {
            let socket = sockets.get_mut::<tcp::Socket>(*handle);
            if socket.is_open() && now < *deadline {
                return true;
            }
            socket.abort();
            sockets.remove(*handle);
            false
        });
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
            if !processed_ingress {
                break;
            }
        }
        self.reap_closing();
        readiness_changed
    }

    pub(super) fn next_ephemeral_port(&mut self) -> Option<u16> {
        // Include lingering closes: smoltcp does not reject duplicate local endpoints.
        let sockets = &self.sockets;
        select_ephemeral_port(&mut self.next_port, |candidate| {
            sockets.iter().any(|(_, socket)| {
                match socket {
                    smoltcp::socket::Socket::Tcp(tcp) => tcp.is_open()
                        && tcp.local_endpoint().is_some_and(|ep| ep.port == candidate),
                    smoltcp::socket::Socket::Udp(udp) => udp.is_open() && udp.endpoint().port == candidate,
                    _ => false,
                }
            })
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
    pub(super) errors: inet_datagram::ErrorIngress,
}

pub(super) struct BrokerRxToken<'a> {
    frame: &'a [u8],
}

pub(super) struct BrokerTxToken;

impl Device for BrokerDevice {
    type RxToken<'a> = BrokerRxToken<'a>;
    type TxToken<'a> = BrokerTxToken;

    fn receive(
        &mut self,
        _timestamp: SmolInstant,
    ) -> Option<(Self::RxToken<'_>, Self::TxToken<'_>)> {
        match packet_rx(self.rx.as_mut_slice()) {
            Ok(len) if len > 0 && len <= self.rx.len() => {
                self.errors.record(&self.rx[..len]);
                Some((BrokerRxToken { frame: &self.rx[..len] }, BrokerTxToken))
            },
            _ => None,
        }
    }

    fn transmit(&mut self, _timestamp: SmolInstant) -> Option<Self::TxToken<'_>> {
        Some(BrokerTxToken)
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
        f(self.frame)
    }
}

impl TxToken for BrokerTxToken {
    fn consume<R, F>(self, len: usize, f: F) -> R
    where
        F: FnOnce(&mut [u8]) -> R,
    {
        let mut frame = [0_u8; NET_BROKER_PACKET_MTU];
        let len = len.min(NET_BROKER_PACKET_MTU);
        let result = f(&mut frame[..len]);
        let _ = packet_tx(&frame[..len]);
        result
    }
}

pub(super) fn smol_now() -> SmolInstant {
    static START: OnceLock<StdInstant> = OnceLock::new();
    let start = START.get_or_init(StdInstant::now);
    SmolInstant::from_millis(start.elapsed().as_millis().min(i64::MAX as u128) as i64)
}
