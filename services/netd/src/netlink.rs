//! Minimal `AF_NETLINK` (`NETLINK_ROUTE` and `NETLINK_KOBJECT_UEVENT`):
//! link, address and route dumps plus change notifications for `NETLINK_ROUTE`,
//! and uevent monitor socket lifecycle/group/option state for `NETLINK_KOBJECT_UEVENT`.
//!
//! - **Owner:** `netd` owns the interface/address view; it reports the real
//!   `smoltcp` interface of the active packet provider plus a synthetic `lo`.
//!   There is no kernel routing table or netns behind this socket.
//! - **Boundary:** request bytes are untrusted; every header length is checked
//!   against the buffer and malformed trailing data is ignored (as Linux does).
//! - **Lifecycle:** one request datagram on `NETLINK_ROUTE` yields one reply
//!   datagram holding the whole answer (`NLM_F_MULTI` records then `NLMSG_DONE`),
//!   queued on the sending socket. A socket that overflows its receive buffer
//!   reports `ENOBUFS` once (unless `NETLINK_NO_ENOBUFS` is enabled), then resumes.
//!   `NETLINK_KOBJECT_UEVENT` has no kernel uevent producer in netd, so reads
//!   stay empty (`EAGAIN`) and sends fail with `ECONNREFUSED`.
//! - **Failure:** unsupported message types yield `NLMSG_ERROR` carrying
//!   `-EOPNOTSUPP`; nothing is silently answered with an empty success.
//! - **Receive:** an empty queue is `EAGAIN` here; a blocking caller waits on
//!   the syscall side (kernel `wait_and_retry_local_receive`), honouring
//!   `O_NONBLOCK`, `MSG_DONTWAIT` and `SO_RCVTIMEO`.
//! - **Groups:** `bind(nl_groups)` and `NETLINK_ADD/DROP_MEMBERSHIP` record the
//!   memberships. A member receives `RTM_NEW/DELLINK` (`RTMGRP_LINK`),
//!   `RTM_NEW/DELADDR` (`RTMGRP_IPV4_IFADDR`) and `RTM_NEW/DELROUTE`
//!   (`RTMGRP_IPV4_ROUTE`) only when the interface view of the packet provider
//!   differs from the last published one ([`publish_interface_changes`]); there
//!   is no notification source for other groups, so those memberships stay
//!   silent. Detection is lazy: it runs when a member polls or receives, and
//!   at every (un)subscription so a new member never sees past history.
//! - **Neighbours:** `RTM_GETNEIGH` answers an empty dump: smoltcp does not
//!   expose its neighbour cache and none is invented.
//! - **Forbidden:** inventing hardware, delivering notifications that are never
//!   generated, or per-socket state outside `NetlinkState`.
//!
//! Everything that builds bytes is a pure function so it can be tested
//! byte-exact.

use super::*;

const AF_INET_FAMILY: u8 = 2;
const AF_INET6_FAMILY: u8 = 10;

pub(super) const NLMSG_HDRLEN: usize = 16;
pub(super) const NLMSG_ERROR: u16 = 2;
pub(super) const NLMSG_DONE: u16 = 3;
pub(super) const RTM_NEWLINK: u16 = 16;
pub(super) const RTM_DELLINK: u16 = 17;
pub(super) const RTM_GETLINK: u16 = 18;
pub(super) const RTM_NEWADDR: u16 = 20;
pub(super) const RTM_DELADDR: u16 = 21;
pub(super) const RTM_GETADDR: u16 = 22;
pub(super) const RTM_NEWROUTE: u16 = 24;
pub(super) const RTM_DELROUTE: u16 = 25;
pub(super) const RTM_GETROUTE: u16 = 26;
pub(super) const RTM_GETNEIGH: u16 = 30;

/// `rtnetlink` multicast groups netd can generate events for (bit masks as
/// used by `sockaddr_nl.nl_groups`; the group number is the bit index + 1).
pub(super) const RTMGRP_LINK: u32 = 1 << 0;
pub(super) const RTMGRP_IPV4_IFADDR: u32 = 1 << 4;
pub(super) const RTMGRP_IPV4_ROUTE: u32 = 1 << 6;
pub(super) const RTMGRP_IPV6_IFADDR: u32 = 1 << 8;
pub(super) const RTMGRP_IPV6_ROUTE: u32 = 1 << 10;
/// `RTNLGRP_MAX` is 31 on current kernels: group numbers 1..=31 are valid.
const NETLINK_GROUP_NUMBER_MAX: u32 = 31;
pub(super) const NETLINK_ROUTE: u64 = 0;
pub(super) const NETLINK_KOBJECT_UEVENT: u64 = 15;
pub(super) const NETLINK_ADD_MEMBERSHIP: u64 = 1;
pub(super) const NETLINK_DROP_MEMBERSHIP: u64 = 2;
pub(super) const NETLINK_NO_ENOBUFS: u64 = 5;
pub(super) const NETLINK_LIST_MEMBERSHIPS: u64 = 9;

/// Netlink protocol families netd implements. Every other protocol number is
/// refused at socket creation with `EPROTONOSUPPORT`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum NetlinkFamily {
    /// `NETLINK_ROUTE`: rtnetlink dumps and interface change notifications.
    Route,
    /// `NETLINK_KOBJECT_UEVENT`: monitor-only; netd has no hotplug source, so
    /// it never receives events and accepts no requests.
    KobjectUevent,
}

impl NetlinkFamily {
    pub(super) const fn from_protocol(protocol: u64) -> Option<Self> {
        match protocol {
            NETLINK_ROUTE => Some(Self::Route),
            NETLINK_KOBJECT_UEVENT => Some(Self::KobjectUevent),
            _ => None,
        }
    }

    pub(super) const fn protocol(self) -> u64 {
        match self {
            Self::Route => NETLINK_ROUTE,
            Self::KobjectUevent => NETLINK_KOBJECT_UEVENT,
        }
    }

    /// Whether requests sent on the socket are answered by netd.
    const fn answers_requests(self) -> bool {
        matches!(self, Self::Route)
    }

    /// Whether the family can ever queue a message for a group member.
    const fn has_event_source(self) -> bool {
        matches!(self, Self::Route)
    }
}

pub(super) const fn is_supported_protocol(protocol: u64) -> bool {
    NetlinkFamily::from_protocol(protocol).is_some()
}

pub(super) const RTA_DST: u16 = 1;
pub(super) const RTA_OIF: u16 = 4;
pub(super) const RTA_GATEWAY: u16 = 5;
pub(super) const RTA_PREFSRC: u16 = 7;
pub(super) const RTA_TABLE: u16 = 15;
pub(super) const RT_TABLE_MAIN: u8 = 254;
pub(super) const RTPROT_KERNEL: u8 = 2;
pub(super) const RTPROT_BOOT: u8 = 3;
pub(super) const RTN_UNICAST: u8 = 1;
pub(super) const RT_SCOPE_LINK: u8 = 253;
pub(super) const NLM_F_REQUEST: u16 = 0x1;
pub(super) const NLM_F_MULTI: u16 = 0x2;
pub(super) const NLM_F_ACK: u16 = 0x4;
/// First message type that is a real request (below it: control records).
pub(super) const NLMSG_MIN_TYPE: u16 = 0x10;
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
/// `SOL_NETLINK`: membership options are implemented; every other option is
/// `ENOPROTOOPT` rather than accepted without effect.
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

/// One IPv4 route of the main table.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct RouteInfo {
    pub(super) oif: i32,
    pub(super) dst: [u8; 4],
    pub(super) dst_len: u8,
    pub(super) gateway: Option<[u8; 4]>,
    pub(super) prefsrc: Option<[u8; 4]>,
    pub(super) scope: u8,
    pub(super) protocol: u8,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct Addr6Info {
    pub(super) index: i32,
    pub(super) prefix_len: u8,
    pub(super) scope: u8,
    pub(super) flags: u8,
    pub(super) local: [u8; 16],
    pub(super) label: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct Route6Info {
    pub(super) oif: i32,
    pub(super) dst: [u8; 16],
    pub(super) dst_len: u8,
    pub(super) prefsrc: Option<[u8; 16]>,
    pub(super) scope: u8,
    pub(super) protocol: u8,
}

/// Everything a dump or an event comparison needs, taken under one lock.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(super) struct InterfaceSnapshot {
    pub(super) links: Vec<LinkInfo>,
    pub(super) addrs: Vec<AddrInfo>,
    pub(super) routes: Vec<RouteInfo>,
    pub(super) addrs_v6: Vec<Addr6Info>,
    pub(super) routes_v6: Vec<Route6Info>,
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
    link_record(link, RTM_NEWLINK, NLM_F_MULTI, seq, pid)
}

fn link_record(link: &LinkInfo, msg_type: u16, flags: u16, seq: u32, pid: u32) -> Vec<u8> {
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
    finish_message(body, msg_type, flags, seq, pid)
}

/// `RTM_NEWADDR` record: `ifaddrmsg` + ADDRESS, LOCAL, [BROADCAST], LABEL.
pub(super) fn build_addr_message(addr: &AddrInfo, seq: u32, pid: u32) -> Vec<u8> {
    addr_record(addr, RTM_NEWADDR, NLM_F_MULTI, seq, pid)
}

fn addr_record(addr: &AddrInfo, msg_type: u16, flags: u16, seq: u32, pid: u32) -> Vec<u8> {
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
    finish_message(body, msg_type, flags, seq, pid)
}

pub(super) fn build_addr6_message(addr: &Addr6Info, seq: u32, pid: u32) -> Vec<u8> {
    addr6_record(addr, RTM_NEWADDR, NLM_F_MULTI, seq, pid)
}

fn addr6_record(addr: &Addr6Info, msg_type: u16, flags: u16, seq: u32, pid: u32) -> Vec<u8> {
    let mut body = Vec::with_capacity(64);
    body.push(AF_INET6_FAMILY);
    body.push(addr.prefix_len);
    body.push(addr.flags);
    body.push(addr.scope);
    body.extend_from_slice(&(addr.index as u32).to_ne_bytes());
    push_attr(&mut body, IFA_ADDRESS, &addr.local);
    let mut label = addr.label.clone().into_bytes();
    label.push(0);
    push_attr(&mut body, IFA_LABEL, &label);
    finish_message(body, msg_type, flags, seq, pid)
}

pub(super) fn build_route6_message(route: &Route6Info, seq: u32, pid: u32) -> Vec<u8> {
    route6_record(route, RTM_NEWROUTE, NLM_F_MULTI, seq, pid)
}

fn route6_record(route: &Route6Info, msg_type: u16, flags: u16, seq: u32, pid: u32) -> Vec<u8> {
    let mut body = Vec::with_capacity(80);
    body.push(AF_INET6_FAMILY);
    body.push(route.dst_len);
    body.push(0);
    body.push(0);
    body.push(RT_TABLE_MAIN);
    body.push(route.protocol);
    body.push(route.scope);
    body.push(RTN_UNICAST);
    body.extend_from_slice(&0_u32.to_ne_bytes());
    push_attr(&mut body, RTA_TABLE, &u32::from(RT_TABLE_MAIN).to_ne_bytes());
    if route.dst_len != 0 {
        push_attr(&mut body, RTA_DST, &route.dst);
    }
    push_attr(&mut body, RTA_OIF, &(route.oif as u32).to_ne_bytes());
    if let Some(prefsrc) = route.prefsrc {
        push_attr(&mut body, RTA_PREFSRC, &prefsrc);
    }
    finish_message(body, msg_type, flags, seq, pid)
}

/// `RTM_NEWROUTE` record: `rtmsg` + TABLE, [DST], OIF, [GATEWAY], [PREFSRC].
pub(super) fn build_route_message(route: &RouteInfo, seq: u32, pid: u32) -> Vec<u8> {
    route_record(route, RTM_NEWROUTE, NLM_F_MULTI, seq, pid)
}

fn route_record(route: &RouteInfo, msg_type: u16, flags: u16, seq: u32, pid: u32) -> Vec<u8> {
    let mut body = Vec::with_capacity(56);
    body.push(AF_INET_FAMILY);
    body.push(route.dst_len);
    body.push(0); // rtm_src_len
    body.push(0); // rtm_tos
    body.push(RT_TABLE_MAIN);
    body.push(route.protocol);
    body.push(route.scope);
    body.push(RTN_UNICAST);
    body.extend_from_slice(&0_u32.to_ne_bytes()); // rtm_flags
    push_attr(&mut body, RTA_TABLE, &u32::from(RT_TABLE_MAIN).to_ne_bytes());
    if route.dst_len != 0 {
        push_attr(&mut body, RTA_DST, &route.dst);
    }
    push_attr(&mut body, RTA_OIF, &(route.oif as u32).to_ne_bytes());
    if let Some(gateway) = route.gateway {
        push_attr(&mut body, RTA_GATEWAY, &gateway);
    }
    if let Some(prefsrc) = route.prefsrc {
        push_attr(&mut body, RTA_PREFSRC, &prefsrc);
    }
    finish_message(body, msg_type, flags, seq, pid)
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
#[cfg(test)]
pub(super) fn process_request(
    request: &[u8],
    portid: u32,
    links: &[LinkInfo],
    addrs: &[AddrInfo],
) -> Vec<u8> {
    let snapshot = InterfaceSnapshot {
        links: links.to_vec(),
        addrs: addrs.to_vec(),
        ..InterfaceSnapshot::default()
    };
    process_request_snapshot(request, portid, &snapshot)
}

pub(super) fn process_request_snapshot(
    request: &[u8],
    portid: u32,
    snapshot: &InterfaceSnapshot,
) -> Vec<u8> {
    let (links, addrs) = (&snapshot.links, &snapshot.addrs);
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
        // Like `netlink_rcv_skb`: only requests of a real message type reach a
        // handler. Replies/control records (NLMSG_NOOP/ERROR/DONE) sent to the
        // kernel are skipped; only an explicit NLM_F_ACK is answered, with a
        // success acknowledgement.
        if flags & NLM_F_REQUEST == 0 || msg_type < NLMSG_MIN_TYPE {
            if flags & NLM_F_ACK != 0 {
                reply.extend_from_slice(&build_error(0, &message[..NLMSG_HDRLEN], seq, portid));
            }
            offset += (len + 3) & !3;
            if offset >= request.len() {
                break;
            }
            continue;
        }
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
                if family == 0 || family == AF_INET6_FAMILY {
                    for addr in &snapshot.addrs_v6 {
                        reply.extend_from_slice(&build_addr6_message(addr, seq, portid));
                    }
                }
                reply.extend_from_slice(&build_done(seq, portid));
            }
            RTM_GETROUTE if dump => {
                let family = message.get(NLMSG_HDRLEN).copied().unwrap_or(0);
                if family == 0 || family == AF_INET_FAMILY {
                    for route in &snapshot.routes {
                        reply.extend_from_slice(&build_route_message(route, seq, portid));
                    }
                }
                if family == 0 || family == AF_INET6_FAMILY {
                    for route in &snapshot.routes_v6 {
                        reply.extend_from_slice(&build_route6_message(route, seq, portid));
                    }
                }
                reply.extend_from_slice(&build_done(seq, portid));
            }
            // No neighbour cache is exposed by the stack: an honest empty dump.
            RTM_GETNEIGH if dump => reply.extend_from_slice(&build_done(seq, portid)),
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
    family: NetlinkFamily,
    portid: u32,
    /// A classic-BPF receive filter was attached (uevent monitors only).
    filter_attached: bool,
    groups: u32,
    no_enobufs: bool,
    rx: VecDeque<Vec<u8>>,
    rx_bytes: usize,
    /// A reply was dropped because the receive buffer was full.
    overrun: bool,
}

impl NetlinkState {
    pub(super) fn new(sock_type: u64, protocol: u64) -> Self {
        Self {
            sock_type,
            family: NetlinkFamily::from_protocol(protocol)
                .expect("netlink protocol is validated at socket admission"),
            portid: 0,
            filter_attached: false,
            groups: 0,
            no_enobufs: false,
            rx: VecDeque::new(),
            rx_bytes: 0,
            overrun: false,
        }
    }

    pub(super) fn sock_type(&self) -> u64 {
        self.sock_type
    }

    pub(super) fn protocol(&self) -> u64 {
        self.family.protocol()
    }

    pub(super) fn family(&self) -> NetlinkFamily {
        self.family
    }

    pub(super) fn readable(&self) -> bool {
        !self.rx.is_empty() || self.overrun
    }

    #[cfg(test)]
    pub(super) fn groups(&self) -> u32 {
        self.groups
    }

    pub(super) fn front_len(&self) -> usize {
        self.rx.front().map_or(0, Vec::len)
    }

    #[cfg(test)]
    pub(super) fn queued_messages(&self) -> usize {
        self.rx.len()
    }

    #[cfg(test)]
    pub(super) fn portid(&self) -> u32 {
        self.portid
    }

    #[cfg(test)]
    pub(super) fn no_enobufs_for_test(&mut self, value: bool) {
        self.no_enobufs = value;
    }

    pub(super) fn write_sockaddr(&self, response: &mut NetdIpcResponse) -> i32 {
        write_sockaddr_nl(self.portid, self.groups, response)
    }

    fn enqueue(&mut self, reply: Vec<u8>, capacity: usize) {
        if reply.is_empty() {
            return;
        }
        if self.rx_bytes.saturating_add(reply.len()) > capacity {
            if !self.no_enobufs {
                self.overrun = true;
            }
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

fn portid_in_use(state: &NetState, protocol: NetlinkFamily, portid: u32) -> bool {
    state
        .netlink_sockets
        .values()
        .any(|socket| socket.state.family == protocol && socket.state.portid == portid)
}

/// Linux uses the process id when free, otherwise a unique generated id
/// (negative as `i32`, i.e. high bit set).
fn allocate_portid(state: &NetState, protocol: NetlinkFamily, preferred: u32) -> u32 {
    if preferred != 0 && !portid_in_use(state, protocol, preferred) {
        return preferred;
    }
    let mut candidate = 0x8000_0000_u32;
    loop {
        candidate = candidate.wrapping_add(1) | 0x8000_0000;
        if !portid_in_use(state, protocol, candidate) {
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
    let mut state = lock_net_state();
    // Publish what changed before this socket joins: a new member sees only
    // later changes, never history.
    publish_interface_changes(&mut state);
    let Some(socket) = state.netlink_sockets.get(&request.socket_token) else {
        return libc::EBADF;
    };
    let current = socket.state.portid;
    let protocol = socket.state.family;
    if current != 0 && requested_pid != 0 && requested_pid != current {
        // An already bound netlink socket cannot move to another port id.
        return libc::EINVAL;
    }
    if requested_pid != 0 && requested_pid != current && portid_in_use(&state, protocol, requested_pid) {
        return libc::EADDRINUSE;
    }
    let portid = if current != 0 {
        // Rebinding keeps the id (groups may change), as Linux does.
        current
    } else {
        allocate_portid(
            &state,
            protocol,
            if requested_pid != 0 {
                requested_pid
            } else {
                request.pid as u32
            },
        )
    };
    let Some(socket) = state.netlink_sockets.get_mut(&request.socket_token) else {
        return libc::EBADF;
    };
    socket.state.portid = portid;
    socket.state.groups = groups;
    // Re-baseline for the new member set (a first member records its baseline).
    publish_interface_changes(&mut state);
    0
}

/// Current interfaces, IPv4 addresses and routes: the synthetic loopback plus
/// the real provider interface when the packet provider is active.
pub(super) fn current_interfaces(state: &mut NetState) -> InterfaceSnapshot {
    #[cfg(not(test))]
    {
        // The stack exists only once the provider is active; creating it here
        // is what the first INET socket would do anyway.
        let _ = state.inet_stack();
    }
    let mut snapshot = InterfaceSnapshot {
        links: vec![loopback_link()],
        addrs: vec![loopback_addr()],
        routes: Vec::new(),
        addrs_v6: vec![Addr6Info {
            index: 1,
            prefix_len: 128,
            scope: RT_SCOPE_HOST,
            flags: IFA_F_PERMANENT,
            local: Ipv6Address::LOCALHOST.octets(),
            label: "lo".to_string(),
        }],
        routes_v6: Vec::new(),
    };
    #[cfg(not(test))]
    let provider_active = packet_provider_state() == Ok(PacketProviderState::Active);
    #[cfg(test)]
    let provider_active = true;
    if let Some(stack) = state.inet.as_ref().filter(|_| provider_active) {
        const INDEX: i32 = 2;
        let HardwareAddress::Ethernet(mac) = stack.iface.hardware_addr();
        snapshot.links.push(provider_link(INDEX, mac.0));
        for cidr in stack.iface.ip_addrs() {
            match cidr {
                IpCidr::Ipv4(cidr) => {
                    let (address, prefix_len) = (cidr.address().octets(), cidr.prefix_len());
                    if address[0] == 127 {
                        continue;
                    }
                    snapshot.addrs.push(provider_addr(INDEX, address, prefix_len));
                    snapshot.routes.push(connected_route(INDEX, address, prefix_len));
                }
                IpCidr::Ipv6(cidr) => {
                    let address = cidr.address();
                    let prefix_len = cidr.prefix_len();
                    if address.is_loopback() {
                        continue;
                    }
                    let octets = address.octets();
                    let scope = if address.is_unicast_link_local() {
                        RT_SCOPE_LINK
                    } else {
                        RT_SCOPE_UNIVERSE
                    };
                    snapshot.addrs_v6.push(Addr6Info {
                        index: INDEX,
                        prefix_len,
                        scope,
                        flags: IFA_F_PERMANENT,
                        local: octets,
                        label: PROVIDER_INTERFACE_NAME.to_string(),
                    });
                    snapshot.routes_v6.push(Route6Info {
                        oif: INDEX,
                        dst: mask_ipv6(octets, prefix_len),
                        dst_len: prefix_len,
                        prefsrc: Some(octets),
                        scope: RT_SCOPE_LINK,
                        protocol: RTPROT_KERNEL,
                    });
                }
            }
        }
        if let Some(route) = stack.iface.routes().get_default_ipv4_route() {
            if let IpAddress::Ipv4(gateway) = route.via_router {
                snapshot.routes.push(RouteInfo {
                    oif: INDEX,
                    dst: [0; 4],
                    dst_len: 0,
                    gateway: Some(gateway.octets()),
                    prefsrc: None,
                    scope: RT_SCOPE_UNIVERSE,
                    protocol: RTPROT_BOOT,
                });
            }
        }
    }
    snapshot
}

fn mask_ipv6(octets: [u8; 16], prefix_len: u8) -> [u8; 16] {
    let mut masked = [0_u8; 16];
    let bits = prefix_len.min(128) as usize;
    let full = bits / 8;
    let rem = bits % 8;
    masked[..full].copy_from_slice(&octets[..full]);
    if rem != 0 && full < 16 {
        masked[full] = octets[full] & (!0_u8 << (8 - rem));
    }
    masked
}

/// The on-link route the kernel installs for an interface address.
pub(super) fn connected_route(index: i32, address: [u8; 4], prefix_len: u8) -> RouteInfo {
    let mask = if prefix_len == 0 {
        0
    } else {
        u32::MAX << (32 - u32::from(prefix_len.min(32)))
    };
    RouteInfo {
        oif: index,
        dst: (u32::from_be_bytes(address) & mask).to_be_bytes(),
        dst_len: prefix_len,
        gateway: None,
        prefsrc: Some(address),
        scope: RT_SCOPE_LINK,
        protocol: RTPROT_KERNEL,
    }
}

/// Events that turn `old` into `new`, each tagged with its multicast group.
/// Pure so it can be tested byte-exact; seq and portid are zero (kernel origin).
pub(super) fn diff_events(old: &InterfaceSnapshot, new: &InterfaceSnapshot) -> Vec<(u32, Vec<u8>)> {
    let mut events = Vec::new();
    for link in &new.links {
        if old.links.iter().find(|candidate| candidate.index == link.index) != Some(link) {
            events.push((RTMGRP_LINK, link_record(link, RTM_NEWLINK, 0, 0, 0)));
        }
    }
    for link in &old.links {
        if !new.links.iter().any(|candidate| candidate.index == link.index) {
            events.push((RTMGRP_LINK, link_record(link, RTM_DELLINK, 0, 0, 0)));
        }
    }
    for addr in &new.addrs {
        if !old.addrs.contains(addr) {
            events.push((RTMGRP_IPV4_IFADDR, addr_record(addr, RTM_NEWADDR, 0, 0, 0)));
        }
    }
    for addr in &old.addrs {
        if !new.addrs.contains(addr) {
            events.push((RTMGRP_IPV4_IFADDR, addr_record(addr, RTM_DELADDR, 0, 0, 0)));
        }
    }
    for route in &new.routes {
        if !old.routes.contains(route) {
            events.push((RTMGRP_IPV4_ROUTE, route_record(route, RTM_NEWROUTE, 0, 0, 0)));
        }
    }
    for route in &old.routes {
        if !new.routes.contains(route) {
            events.push((RTMGRP_IPV4_ROUTE, route_record(route, RTM_DELROUTE, 0, 0, 0)));
        }
    }
    for addr in &new.addrs_v6 {
        if !old.addrs_v6.contains(addr) {
            events.push((RTMGRP_IPV6_IFADDR, addr6_record(addr, RTM_NEWADDR, 0, 0, 0)));
        }
    }
    for addr in &old.addrs_v6 {
        if !new.addrs_v6.contains(addr) {
            events.push((RTMGRP_IPV6_IFADDR, addr6_record(addr, RTM_DELADDR, 0, 0, 0)));
        }
    }
    for route in &new.routes_v6 {
        if !old.routes_v6.contains(route) {
            events.push((RTMGRP_IPV6_ROUTE, route6_record(route, RTM_NEWROUTE, 0, 0, 0)));
        }
    }
    for route in &old.routes_v6 {
        if !new.routes_v6.contains(route) {
            events.push((RTMGRP_IPV6_ROUTE, route6_record(route, RTM_DELROUTE, 0, 0, 0)));
        }
    }
    events
}

/// Whether any `NETLINK_ROUTE` socket is subscribed to a group netd generates.
fn has_observed_members(state: &NetState) -> bool {
    state.netlink_sockets.values().any(|socket| {
        socket.state.family == NetlinkFamily::Route
            && socket.state.groups
                & (RTMGRP_LINK
                    | RTMGRP_IPV4_IFADDR
                    | RTMGRP_IPV4_ROUTE
                    | RTMGRP_IPV6_IFADDR
                    | RTMGRP_IPV6_ROUTE)
                != 0
    })
}

/// Compares the live interface view with the last published one and queues the
/// difference on every subscribed socket. The first call only records the
/// baseline. Without a subscriber the baseline is left alone, so the stack is
/// not created or inspected for nobody.
pub(super) fn publish_interface_changes(state: &mut NetState) {
    if !has_observed_members(state) {
        state.netlink_view = None;
        return;
    }
    let current = current_interfaces(state);
    apply_interface_snapshot(state, current);
}

pub(super) fn apply_interface_snapshot(state: &mut NetState, current: InterfaceSnapshot) {
    let Some(previous) = state.netlink_view.replace(current.clone()) else {
        return;
    };
    for (group, message) in diff_events(&previous, &current) {
        let members: Vec<u64> = state
            .netlink_sockets
            .iter()
            .filter_map(|(token, socket)| {
                (socket.state.family == NetlinkFamily::Route && socket.state.groups & group != 0)
                    .then_some(*token)
            })
            .collect();
        for token in members {
            if let Some(socket) = state.netlink_sockets.get_mut(&token) {
                let capacity = socket.options.recv_buffer.max(4096) as usize;
                socket.state.enqueue(message.clone(), capacity);
            }
        }
    }
}

/// Poll/receive hook: only a `NETLINK_ROUTE` socket that is itself a group
/// member can be waiting for an interface event, so only it pays for the comparison.
pub(super) fn refresh_for_member(state: &mut NetState, token: u64) {
    let member = state
        .netlink_sockets
        .get(&token)
        .is_some_and(|socket| socket.state.family == NetlinkFamily::Route && socket.state.groups != 0);
    if member {
        publish_interface_changes(state);
    }
}

/// `setsockopt(SOL_NETLINK, ...)`: membership options and `NETLINK_NO_ENOBUFS`.
pub(super) fn handle_setsockopt(request: &NetdIpcRequest) -> i32 {
    let mut state = lock_net_state();
    let Some(protocol) = state
        .netlink_sockets
        .get(&request.socket_token)
        .map(|socket| socket.state.family)
    else {
        return if state.socket_exists(request.socket_token) {
            libc::ENOPROTOOPT
        } else {
            libc::EBADF
        };
    };
    let option = request.arg2;
    if !matches!(
        option,
        NETLINK_ADD_MEMBERSHIP | NETLINK_DROP_MEMBERSHIP | NETLINK_NO_ENOBUFS
    ) {
        return libc::ENOPROTOOPT;
    }
    let Some(value) = request_i32_payload(request) else {
        return libc::EINVAL;
    };
    if option == NETLINK_NO_ENOBUFS {
        if let Some(socket) = state.netlink_sockets.get_mut(&request.socket_token) {
            socket.state.no_enobufs = value != 0;
            if socket.state.no_enobufs {
                socket.state.overrun = false;
            }
        }
        return 0;
    }
    let group = value as u32;
    if group == 0 || group > NETLINK_GROUP_NUMBER_MAX {
        return libc::EINVAL;
    }
    if protocol.has_event_source() {
        publish_interface_changes(&mut state);
    }
    let bit = 1_u32 << (group - 1);
    if let Some(socket) = state.netlink_sockets.get_mut(&request.socket_token) {
        if option == NETLINK_ADD_MEMBERSHIP {
            socket.state.groups |= bit;
        } else {
            socket.state.groups &= !bit;
        }
    }
    if protocol.has_event_source() {
        // Re-baseline for the new member set (a first member records its baseline).
        publish_interface_changes(&mut state);
    }
    0
}

/// `BPF_MAXINSNS`: the longest classic-BPF program `SO_ATTACH_FILTER` accepts.
const BPF_MAXINSNS: u16 = 4096;

/// `setsockopt(SOL_SOCKET, SO_ATTACH_FILTER / SO_DETACH_FILTER)`.
///
/// libudev attaches a subsystem/devtype filter to its monitor socket before
/// `bind`. A `NETLINK_KOBJECT_UEVENT` socket has no event source, so no filter
/// can change what it receives (nothing); the program shape (`struct
/// sock_fprog`) is validated and its presence recorded for `SO_DETACH_FILTER`.
/// The program is not interpreted. Families that do deliver messages would
/// need real filtering, so they refuse rather than ignore the filter.
pub(super) fn handle_socket_filter(state: &mut NetState, request: &NetdIpcRequest) -> i32 {
    use super::socket_info::{SO_ATTACH_FILTER, SO_DETACH_FILTER};
    let Some(socket) = state.netlink_sockets.get_mut(&request.socket_token) else {
        return libc::EBADF;
    };
    if socket.state.family.has_event_source() {
        return libc::EOPNOTSUPP;
    }
    match request.arg2 {
        SO_ATTACH_FILTER => {
            // struct sock_fprog { u16 len; <pad>; struct sock_filter *filter; }
            if (request.payload_len as usize) < 16 {
                return libc::EINVAL;
            }
            let len = rd_u16(&request.payload, 0);
            if len == 0 || len > BPF_MAXINSNS {
                return libc::EINVAL;
            }
            socket.state.filter_attached = true;
            0
        }
        SO_DETACH_FILTER => {
            if core::mem::take(&mut socket.state.filter_attached) {
                0
            } else {
                libc::ENOENT
            }
        }
        _ => libc::ENOPROTOOPT,
    }
}

/// `getsockopt(SOL_NETLINK, ...)`: group bit mask and `NETLINK_NO_ENOBUFS`.
pub(super) fn handle_getsockopt(request: &NetdIpcRequest, response: &mut NetdIpcResponse) -> i32 {
    let state = lock_net_state();
    let Some(NetlinkSocket { state: netlink, .. }) = state.netlink_sockets.get(&request.socket_token)
    else {
        return if state.socket_exists(request.socket_token) {
            libc::ENOPROTOOPT
        } else {
            libc::EBADF
        };
    };
    match request.arg2 {
        NETLINK_LIST_MEMBERSHIPS => {
            response.payload[..4].copy_from_slice(&netlink.groups.to_ne_bytes());
        }
        NETLINK_NO_ENOBUFS => {
            response.payload[..4].copy_from_slice(&i32::from(netlink.no_enobufs).to_ne_bytes());
        }
        _ => return libc::ENOPROTOOPT,
    }
    response.payload_len = 4;
    response.value = 0;
    0
}

/// `send`/`sendto`/`sendmsg` on a netlink socket: answer `NETLINK_ROUTE` into its own queue.
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
    let (protocol, needs_portid) = match state.netlink_sockets.get(&request.socket_token) {
        Some(socket) => (socket.state.family, socket.state.portid == 0),
        None => return Err(libc::EBADF),
    };
    if !protocol.answers_requests() {
        return Err(libc::ECONNREFUSED);
    }
    let snapshot = current_interfaces(state);
    let preferred = request.pid as u32;
    let autobound = needs_portid.then(|| allocate_portid(state, protocol, preferred));
    let socket = state
        .netlink_sockets
        .get_mut(&request.socket_token)
        .ok_or(libc::EBADF)?;
    let capacity = socket.options.recv_buffer.max(4096) as usize;
    let netlink = &mut socket.state;
    if let Some(portid) = autobound {
        netlink.portid = portid;
    }
    let reply = process_request_snapshot(bytes, netlink.portid, &snapshot);
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
    refresh_for_member(state, token);
    let socket = state.netlink_sockets.get_mut(&token).ok_or(libc::EBADF)?;
    let netlink = &mut socket.state;
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
