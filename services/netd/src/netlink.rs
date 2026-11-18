//! Minimal `AF_NETLINK`/`NETLINK_ROUTE`: link and address dumps.
//!
//! - **Owner:** `netd` owns the interface/address view; it reports the real
//!   `smoltcp` interface of the active packet provider plus a synthetic `lo`.
//!   There is no kernel routing table or netns behind this socket.
//! - **Boundary:** request bytes are untrusted; every header length is checked
//!   against the buffer and malformed trailing data is ignored (as Linux does).
//! - **Lifecycle:** one request datagram yields one reply datagram holding the
//!   whole answer (`NLM_F_MULTI` records then `NLMSG_DONE`), queued on the
//!   sending socket. A socket that overflows its receive buffer reports
//!   `ENOBUFS` once, then resumes.
//! - **Failure:** unsupported message types yield `NLMSG_ERROR` carrying
//!   `-EOPNOTSUPP`; nothing is silently answered with an empty success.
//! - **Forbidden:** inventing hardware, delivering multicast notifications that
//!   are never generated, or per-socket state outside `NetlinkState`.
//!
//! Everything that builds bytes is a pure function so it can be tested
//! byte-exact.

use super::*;

const AF_INET_FAMILY: u8 = 2;

pub(super) const NLMSG_HDRLEN: usize = 16;
pub(super) const NLMSG_ERROR: u16 = 2;
pub(super) const NLMSG_DONE: u16 = 3;
pub(super) const RTM_NEWLINK: u16 = 16;
pub(super) const RTM_GETLINK: u16 = 18;
pub(super) const RTM_NEWADDR: u16 = 20;
pub(super) const RTM_GETADDR: u16 = 22;
pub(super) const NLM_F_MULTI: u16 = 0x2;
pub(super) const NLM_F_ROOT: u16 = 0x100;

pub(super) const IFLA_ADDRESS: u16 = 1;
pub(super) const IFLA_BROADCAST: u16 = 2;
pub(super) const IFLA_IFNAME: u16 = 3;
pub(super) const IFLA_MTU: u16 = 4;
pub(super) const IFLA_OPERSTATE: u16 = 16;
pub(super) const IFA_ADDRESS: u16 = 1;
pub(super) const IFA_LOCAL: u16 = 2;
pub(super) const IFA_LABEL: u16 = 3;
pub(super) const IFA_BROADCAST: u16 = 4;

pub(super) const IFF_UP: u32 = 0x1;
pub(super) const IFF_BROADCAST: u32 = 0x2;
pub(super) const IFF_LOOPBACK: u32 = 0x8;
pub(super) const IFF_RUNNING: u32 = 0x40;
pub(super) const IFF_MULTICAST: u32 = 0x1000;
pub(super) const IFF_LOWER_UP: u32 = 0x10000;
pub(super) const ARPHRD_ETHER: u16 = 1;
pub(super) const ARPHRD_LOOPBACK: u16 = 772;
pub(super) const IF_OPER_UNKNOWN: u8 = 0;
pub(super) const IF_OPER_UP: u8 = 6;
pub(super) const IFA_F_PERMANENT: u8 = 0x80;
pub(super) const RT_SCOPE_UNIVERSE: u8 = 0;
pub(super) const RT_SCOPE_HOST: u8 = 254;

pub(super) const SOCKADDR_NL_LEN: usize = 12;
/// `SOL_NETLINK`: options at this level are accepted and ignored (no
/// multicast groups are ever delivered).
pub(super) const SOL_NETLINK: u64 = 270;

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct LinkInfo {
    pub(super) index: i32,
    pub(super) name: String,
    pub(super) arphrd: u16,
    pub(super) flags: u32,
    pub(super) mtu: u32,
    pub(super) mac: [u8; 6],
    pub(super) broadcast: [u8; 6],
    pub(super) oper_state: u8,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct AddrInfo {
    pub(super) index: i32,
    pub(super) prefix_len: u8,
    pub(super) scope: u8,
    pub(super) flags: u8,
    pub(super) local: [u8; 4],
    pub(super) broadcast: Option<[u8; 4]>,
    pub(super) label: String,
}

pub(super) fn loopback_link() -> LinkInfo {
    LinkInfo {
        index: 1,
        name: "lo".to_string(),
        arphrd: ARPHRD_LOOPBACK,
        flags: IFF_UP | IFF_LOOPBACK | IFF_RUNNING | IFF_LOWER_UP,
        mtu: 65536,
        mac: [0; 6],
        broadcast: [0; 6],
        oper_state: IF_OPER_UNKNOWN,
    }
}

pub(super) fn loopback_addr() -> AddrInfo {
    AddrInfo {
        index: 1,
        prefix_len: 8,
        scope: RT_SCOPE_HOST,
        flags: IFA_F_PERMANENT,
        local: [127, 0, 0, 1],
        broadcast: None,
        label: "lo".to_string(),
    }
}

/// Name of the packet-provider NIC. netd has no device naming; this is the
/// conventional Linux name for the first Ethernet interface.
pub(super) const PROVIDER_INTERFACE_NAME: &str = "eth0";
/// IP MTU of the packet provider link (frame MTU minus the Ethernet header).
pub(super) const PROVIDER_INTERFACE_MTU: u32 = (NET_BROKER_PACKET_MTU - 14) as u32;

pub(super) fn provider_link(index: i32, mac: [u8; 6]) -> LinkInfo {
    LinkInfo {
        index,
        name: PROVIDER_INTERFACE_NAME.to_string(),
        arphrd: ARPHRD_ETHER,
        flags: IFF_UP | IFF_BROADCAST | IFF_RUNNING | IFF_MULTICAST | IFF_LOWER_UP,
        mtu: PROVIDER_INTERFACE_MTU,
        mac,
        broadcast: [0xff; 6],
        oper_state: IF_OPER_UP,
    }
}

pub(super) fn provider_addr(index: i32, address: [u8; 4], prefix_len: u8) -> AddrInfo {
    let mask = if prefix_len == 0 {
        0
    } else {
        u32::MAX << (32 - u32::from(prefix_len.min(32)))
    };
    let broadcast = (u32::from_be_bytes(address) | !mask).to_be_bytes();
    AddrInfo {
        index,
        prefix_len,
        scope: RT_SCOPE_UNIVERSE,
        flags: IFA_F_PERMANENT,
        local: address,
        broadcast: Some(broadcast),
        label: PROVIDER_INTERFACE_NAME.to_string(),
    }
}

fn pad4(buf: &mut Vec<u8>) {
    while buf.len() % 4 != 0 {
        buf.push(0);
    }
}

fn push_attr(buf: &mut Vec<u8>, attr_type: u16, data: &[u8]) {
    buf.extend_from_slice(&((4 + data.len()) as u16).to_ne_bytes());
    buf.extend_from_slice(&attr_type.to_ne_bytes());
    buf.extend_from_slice(data);
    pad4(buf);
}

fn finish_message(mut body: Vec<u8>, msg_type: u16, flags: u16, seq: u32, pid: u32) -> Vec<u8> {
    let len = (NLMSG_HDRLEN + body.len()) as u32;
    let mut message = Vec::with_capacity(len as usize);
    message.extend_from_slice(&len.to_ne_bytes());
    message.extend_from_slice(&msg_type.to_ne_bytes());
    message.extend_from_slice(&flags.to_ne_bytes());
    message.extend_from_slice(&seq.to_ne_bytes());
    message.extend_from_slice(&pid.to_ne_bytes());
    message.append(&mut body);
    message
}

/// `RTM_NEWLINK` record: `ifinfomsg` + IFNAME, MTU, OPERSTATE, ADDRESS, BROADCAST.
pub(super) fn build_link_message(link: &LinkInfo, seq: u32, pid: u32) -> Vec<u8> {
    let mut body = Vec::with_capacity(96);
    body.push(0); // ifi_family = AF_UNSPEC
    body.push(0);
    body.extend_from_slice(&link.arphrd.to_ne_bytes());
    body.extend_from_slice(&link.index.to_ne_bytes());
    body.extend_from_slice(&link.flags.to_ne_bytes());
    body.extend_from_slice(&0_u32.to_ne_bytes()); // ifi_change
    let mut name = link.name.clone().into_bytes();
    name.push(0);
    push_attr(&mut body, IFLA_IFNAME, &name);
    push_attr(&mut body, IFLA_MTU, &link.mtu.to_ne_bytes());
    push_attr(&mut body, IFLA_OPERSTATE, &[link.oper_state]);
    push_attr(&mut body, IFLA_ADDRESS, &link.mac);
    push_attr(&mut body, IFLA_BROADCAST, &link.broadcast);
    finish_message(body, RTM_NEWLINK, NLM_F_MULTI, seq, pid)
}

/// `RTM_NEWADDR` record: `ifaddrmsg` + ADDRESS, LOCAL, [BROADCAST], LABEL.
pub(super) fn build_addr_message(addr: &AddrInfo, seq: u32, pid: u32) -> Vec<u8> {
    let mut body = Vec::with_capacity(48);
    body.push(AF_INET_FAMILY);
    body.push(addr.prefix_len);
    body.push(addr.flags);
    body.push(addr.scope);
    body.extend_from_slice(&(addr.index as u32).to_ne_bytes());
    push_attr(&mut body, IFA_ADDRESS, &addr.local);
    push_attr(&mut body, IFA_LOCAL, &addr.local);
    if let Some(broadcast) = addr.broadcast {
        push_attr(&mut body, IFA_BROADCAST, &broadcast);
    }
    let mut label = addr.label.clone().into_bytes();
    label.push(0);
    push_attr(&mut body, IFA_LABEL, &label);
    finish_message(body, RTM_NEWADDR, NLM_F_MULTI, seq, pid)
}

pub(super) fn build_done(seq: u32, pid: u32) -> Vec<u8> {
    finish_message(
        0_i32.to_ne_bytes().to_vec(),
        NLMSG_DONE,
        NLM_F_MULTI,
        seq,
        pid,
    )
}

/// `NLMSG_ERROR`: `nlmsgerr { error, msg }` echoing the failed request.
pub(super) fn build_error(errno: i32, request: &[u8], seq: u32, pid: u32) -> Vec<u8> {
    let mut body = Vec::with_capacity(4 + request.len());
    body.extend_from_slice(&(-errno).to_ne_bytes());
    body.extend_from_slice(request);
    finish_message(body, NLMSG_ERROR, 0, seq, pid)
}

fn rd_u16(bytes: &[u8], at: usize) -> u16 {
    u16::from_ne_bytes([bytes[at], bytes[at + 1]])
}

fn rd_u32(bytes: &[u8], at: usize) -> u32 {
    u32::from_ne_bytes([bytes[at], bytes[at + 1], bytes[at + 2], bytes[at + 3]])
}

/// Answers every message in a request datagram. Returns the whole reply as one
/// byte string (possibly empty when the request held no well-formed message).
pub(super) fn process_request(
    request: &[u8],
    portid: u32,
    links: &[LinkInfo],
    addrs: &[AddrInfo],
) -> Vec<u8> {
    let mut reply = Vec::new();
    let mut offset = 0usize;
    while request.len() - offset >= NLMSG_HDRLEN {
        let len = rd_u32(request, offset) as usize;
        // A malformed header ends processing silently, like the kernel.
        if len < NLMSG_HDRLEN || len > request.len() - offset {
            break;
        }
        let message = &request[offset..offset + len];
        let msg_type = rd_u16(message, 4);
        let flags = rd_u16(message, 6);
        let seq = rd_u32(message, 8);
        // glibc requests `NLM_F_ROOT` (+`NLM_F_MATCH`), libnl and Chromium the
        // full `NLM_F_DUMP`; both are dumps.
        let dump = flags & NLM_F_ROOT != 0;
        match msg_type {
            RTM_GETLINK if dump => {
                for link in links {
                    reply.extend_from_slice(&build_link_message(link, seq, portid));
                }
                reply.extend_from_slice(&build_done(seq, portid));
            }
            RTM_GETADDR if dump => {
                // `rtgenmsg.rtgen_family` selects the family; 0 means all.
                let family = message.get(NLMSG_HDRLEN).copied().unwrap_or(0);
                if family == 0 || family == AF_INET_FAMILY {
                    for addr in addrs {
                        reply.extend_from_slice(&build_addr_message(addr, seq, portid));
                    }
                }
                // Other families (AF_INET6, ...) have no address table here:
                // an empty dump, still terminated by NLMSG_DONE.
                reply.extend_from_slice(&build_done(seq, portid));
            }
            _ => reply.extend_from_slice(&build_error(libc::EOPNOTSUPP, message, seq, portid)),
        }
        offset += (len + 3) & !3;
        if offset >= request.len() {
            break;
        }
    }
    reply
}

/// Per-socket state for one `AF_NETLINK` socket.
#[derive(Debug)]
pub(super) struct NetlinkState {
    sock_type: u64,
    portid: u32,
    groups: u32,
    rx: VecDeque<Vec<u8>>,
    rx_bytes: usize,
    /// A reply was dropped because the receive buffer was full.
    overrun: bool,
}

impl NetlinkState {
    pub(super) fn new(sock_type: u64) -> Self {
        Self {
            sock_type,
            portid: 0,
            groups: 0,
            rx: VecDeque::new(),
            rx_bytes: 0,
            overrun: false,
        }
    }

    pub(super) fn sock_type(&self) -> u64 {
        self.sock_type
    }

    pub(super) fn readable(&self) -> bool {
        !self.rx.is_empty() || self.overrun
    }

    #[cfg(test)]
    pub(super) fn queued_messages(&self) -> usize {
        self.rx.len()
    }

    #[cfg(test)]
    pub(super) fn portid(&self) -> u32 {
        self.portid
    }

    pub(super) fn write_sockaddr(&self, response: &mut NetdIpcResponse) -> i32 {
        write_sockaddr_nl(self.portid, self.groups, response)
    }

    fn enqueue(&mut self, reply: Vec<u8>, capacity: usize) {
        if reply.is_empty() {
            return;
        }
        if self.rx_bytes.saturating_add(reply.len()) > capacity {
            self.overrun = true;
            return;
        }
        self.rx_bytes += reply.len();
        self.rx.push_back(reply);
    }
}

pub(super) fn write_sockaddr_nl(pid: u32, groups: u32, response: &mut NetdIpcResponse) -> i32 {
    response.payload[..SOCKADDR_NL_LEN].copy_from_slice(&sockaddr_nl_bytes(pid, groups));
    response.payload_len = SOCKADDR_NL_LEN as u32;
    response.value = 0;
    0
}

pub(super) fn write_kernel_sockaddr(response: &mut NetdIpcResponse) -> i32 {
    write_sockaddr_nl(0, 0, response)
}

/// `struct sockaddr_nl { u16 family; u16 pad; u32 pid; u32 groups; }`.
pub(super) fn sockaddr_nl_bytes(pid: u32, groups: u32) -> [u8; SOCKADDR_NL_LEN] {
    let mut bytes = [0u8; SOCKADDR_NL_LEN];
    bytes[0..2].copy_from_slice(&(libc::AF_NETLINK as u16).to_ne_bytes());
    bytes[4..8].copy_from_slice(&pid.to_ne_bytes());
    bytes[8..12].copy_from_slice(&groups.to_ne_bytes());
    bytes
}

fn portid_in_use(state: &NetState, portid: u32) -> bool {
    state.sockets.values().any(|socket| {
        matches!(&socket.state, UnixSocketState::Netlink(netlink) if netlink.portid == portid)
    })
}

/// Linux uses the process id when free, otherwise a unique generated id
/// (negative as `i32`, i.e. high bit set).
fn allocate_portid(state: &NetState, preferred: u32) -> u32 {
    if preferred != 0 && !portid_in_use(state, preferred) {
        return preferred;
    }
    let mut candidate = 0x8000_0000_u32;
    loop {
        candidate = candidate.wrapping_add(1) | 0x8000_0000;
        if !portid_in_use(state, candidate) {
            return candidate;
        }
    }
}

pub(super) fn handle_connect(_request: &NetdIpcRequest) -> i32 {
    0
}

pub(super) fn handle_bind(request: &NetdIpcRequest) -> i32 {
    let len = request.payload_len as usize;
    if len < SOCKADDR_NL_LEN || len > request.payload.len() {
        return libc::EINVAL;
    }
    if rd_u16(&request.payload, 0) != libc::AF_NETLINK as u16 {
        return libc::EINVAL;
    }
    let requested_pid = rd_u32(&request.payload, 4);
    let groups = rd_u32(&request.payload, 8);
    if groups != 0 {
        return libc::EOPNOTSUPP;
    }
    let mut state = lock_net_state();
    let Some(socket) = state.sockets.get(&request.socket_token) else {
        return libc::EBADF;
    };
    let UnixSocketState::Netlink(netlink) = &socket.state else {
        return libc::EINVAL;
    };
    let current = netlink.portid;
    if requested_pid != 0 && requested_pid != current && portid_in_use(&state, requested_pid) {
        return libc::EADDRINUSE;
    }
    let portid = if current != 0 {
        // Rebinding keeps the id (groups may change), as Linux does.
        current
    } else {
        allocate_portid(
            &state,
            if requested_pid != 0 {
                requested_pid
            } else {
                request.pid as u32
            },
        )
    };
    let Some(socket) = state.sockets.get_mut(&request.socket_token) else {
        return libc::EBADF;
    };
    if let UnixSocketState::Netlink(netlink) = &mut socket.state {
        netlink.portid = portid;
        netlink.groups = groups;
    }
    0
}

/// Current interfaces and IPv4 addresses: the synthetic loopback plus the real
/// provider interface when the packet provider is active.
pub(super) fn current_interfaces(state: &mut NetState) -> (Vec<LinkInfo>, Vec<AddrInfo>) {
    #[cfg(not(test))]
    {
        // The stack exists only once the provider is active; creating it here
        // is what the first INET socket would do anyway.
        let _ = state.inet_stack();
    }
    let mut links = vec![loopback_link()];
    let mut addrs = vec![loopback_addr()];
    #[cfg(not(test))]
    let provider_active = packet_provider_state() == Ok(PacketProviderState::Active);
    #[cfg(test)]
    let provider_active = true;
    if let Some(stack) = state.inet.as_ref().filter(|_| provider_active) {
        const INDEX: i32 = 2;
        // Both enums are single-variant under this crate's smoltcp features
        // (ethernet medium, IPv4 only).
        let HardwareAddress::Ethernet(mac) = stack.iface.hardware_addr();
        links.push(provider_link(INDEX, mac.0));
        for cidr in stack.iface.ip_addrs() {
            let IpCidr::Ipv4(cidr) = cidr;
            addrs.push(provider_addr(
                INDEX,
                cidr.address().octets(),
                cidr.prefix_len(),
            ));
        }
    }
    (links, addrs)
}

/// `send`/`sendto`/`sendmsg` on a netlink socket: answer into its own queue.
pub(super) fn send_message(
    state: &mut NetState,
    request: &NetdIpcRequest,
    bytes: &[u8],
    control: &[u8],
) -> Result<usize, i32> {
    if !control.is_empty() {
        return Err(libc::EOPNOTSUPP);
    }
    if bytes.len() > SOCKET_BUFFER_CAPACITY {
        return Err(libc::EMSGSIZE);
    }
    let (links, addrs) = current_interfaces(state);
    let preferred = request.pid as u32;
    let needs_portid = match state.sockets.get(&request.socket_token) {
        Some(UnixSocket {
            state: UnixSocketState::Netlink(netlink),
            ..
        }) => netlink.portid == 0,
        Some(_) => return Err(libc::EOPNOTSUPP),
        None => return Err(libc::EBADF),
    };
    let autobound = needs_portid.then(|| allocate_portid(state, preferred));
    let socket = state
        .sockets
        .get_mut(&request.socket_token)
        .ok_or(libc::EBADF)?;
    let capacity = socket.options.recv_buffer.max(4096) as usize;
    let UnixSocketState::Netlink(netlink) = &mut socket.state else {
        return Err(libc::EOPNOTSUPP);
    };
    if let Some(portid) = autobound {
        netlink.portid = portid;
    }
    let reply = process_request(bytes, netlink.portid, &links, &addrs);
    netlink.enqueue(reply, capacity);
    Ok(bytes.len())
}

/// Copies the front datagram into `dest`. Returns `(copied, total_len,
/// msg_flags)` and dequeues unless `peek`.
fn take_datagram(
    state: &mut NetState,
    token: u64,
    dest: &mut [u8],
    peek: bool,
) -> Result<(usize, usize), i32> {
    let socket = state.sockets.get_mut(&token).ok_or(libc::EBADF)?;
    let UnixSocketState::Netlink(netlink) = &mut socket.state else {
        return Err(libc::EOPNOTSUPP);
    };
    if netlink.rx.is_empty() {
        if core::mem::take(&mut netlink.overrun) {
            return Err(libc::ENOBUFS);
        }
        return Err(libc::EAGAIN);
    }
    let total;
    let copied;
    {
        let front = netlink.rx.front().expect("checked non-empty");
        total = front.len();
        copied = total.min(dest.len());
        dest[..copied].copy_from_slice(&front[..copied]);
    }
    if !peek {
        netlink.rx.pop_front();
        netlink.rx_bytes = netlink.rx_bytes.saturating_sub(total);
    }
    Ok((copied, total))
}

pub(super) fn handle_recv(
    state: &mut NetState,
    request: &NetdIpcRequest,
    response: &mut NetdIpcResponse,
) -> i32 {
    let flags = request_msg_flags(request);
    let requested = usize::try_from(request.arg2).unwrap_or(usize::MAX);
    let limit = requested.min(response.payload.len());
    let peek = flags & libc::MSG_PEEK as u64 != 0;
    match take_datagram(
        state,
        request.socket_token,
        &mut response.payload[..limit],
        peek,
    ) {
        Ok((copied, total)) => {
            response.payload_len = copied as u32;
            response.reserved0 = 0;
            response.reserved1 = 0;
            response.value = if flags & libc::MSG_TRUNC as u64 != 0 {
                total as u64
            } else {
                copied as u64
            };
            0
        }
        Err(errno) => errno,
    }
}

pub(super) fn handle_recvmsg(
    state: &mut NetState,
    request: &NetdIpcRequest,
    response: &mut NetdIpcResponse,
) -> i32 {
    let flags = request_msg_flags(request);
    let room = response.payload.len() - NETD_RECVMSG_PAYLOAD_HEADER_SIZE;
    let limit = (request.payload_len as usize).min(room);
    let peek = flags & libc::MSG_PEEK as u64 != 0;
    let start = NETD_RECVMSG_PAYLOAD_HEADER_SIZE;
    match take_datagram(
        state,
        request.socket_token,
        &mut response.payload[start..start + limit],
        peek,
    ) {
        Ok((copied, total)) => {
            let msg_flags = if total > copied {
                libc::MSG_TRUNC as u32
            } else {
                0
            };
            response.payload[0..4].copy_from_slice(&(copied as u32).to_ne_bytes());
            response.payload[4..8].copy_from_slice(&0_u32.to_ne_bytes());
            response.payload[8..12].copy_from_slice(&msg_flags.to_ne_bytes());
            response.payload[12..16].copy_from_slice(&0_u32.to_ne_bytes());
            response.payload_len = (start + copied) as u32;
            response.reserved0 = 0;
            response.reserved1 = 0;
            response.value = if flags & libc::MSG_TRUNC as u64 != 0 {
                total as u64
            } else {
                copied as u64
            };
            0
        }
        Err(errno) => errno,
    }
}
