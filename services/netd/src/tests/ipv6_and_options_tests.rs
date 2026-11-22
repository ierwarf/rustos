use super::*;
use rustos_user_abi::syscall::netd_message;

fn loopback_test_state() -> NetState {
    let mut device = BrokerDevice::default();
    let config = SmolConfig::new(HardwareAddress::Ethernet(QEMU_USERNET_MAC));
    let mut iface = Interface::new(config, &mut device, smol_now());
    iface.update_ip_addrs(|addrs| {
        let _ = addrs.push(IpCidr::new(IpAddress::Ipv4(QEMU_USERNET_ADDR), 24));
        let _ = addrs.push(IpCidr::new(IpAddress::Ipv4(Ipv4Address::LOCALHOST), 8));
        let _ = addrs.push(IpCidr::new(IpAddress::Ipv6(Ipv6Address::LOCALHOST), 128));
        let _ = addrs.push(IpCidr::new(IpAddress::Ipv6(LINK_LOCAL_IPV6), 64));
    });
    seed_loopback_neighbors(&mut device);
    let mut state = NetState::new();
    let mut stack = InetStack {
        iface,
        sockets: SocketSet::new(Vec::new()),
        device,
        next_port: 49152,
        closing: Vec::new(),
        readiness_dirty: false,
        last_timer_sweep: StdInstant::now(),
    };
    stack.poll_budget(4);
    state.inet = Some(stack);
    state
}

fn insert_inet_sock(state: &mut NetState, token: u64, domain: u64, type_: u64) {
    let transport = inet_datagram::add_socket(state.inet.as_mut().unwrap(), type_, 0);
    state.inet_sockets.insert(
        token,
        InetSocket {
            domain,
            refs: 1,
            options: SocketOptions::inet_default(),
            transport,
            tcp_listener: None,
            local: None,
            peer: None,
            received: None,
            errors: None,
            connect_deadline: None,
            connect_started: None,
            connect_budget: None,
        },
    );
}

fn sockaddr_in4_req(token: u64, ip: [u8; 4], port: u16) -> NetdIpcRequest {
    let mut req = NetdIpcRequest {
        socket_token: token,
        ..NetdIpcRequest::default()
    };
    req.payload[..2].copy_from_slice(&(libc::AF_INET as u16).to_ne_bytes());
    req.payload[2..4].copy_from_slice(&port.to_be_bytes());
    req.payload[4..8].copy_from_slice(&ip);
    req.payload_len = 16;
    req
}

fn sockaddr_in6_bytes(ip: [u8; 16], port: u16) -> [u8; 28] {
    let mut buf = [0_u8; 28];
    buf[..2].copy_from_slice(&(linux_abi::AF_INET6 as u16).to_ne_bytes());
    buf[2..4].copy_from_slice(&port.to_be_bytes());
    buf[8..24].copy_from_slice(&ip);
    buf
}

fn sockaddr_in6_req(token: u64, ip: [u8; 16], port: u16) -> NetdIpcRequest {
    let mut req = NetdIpcRequest {
        socket_token: token,
        ..NetdIpcRequest::default()
    };
    let bytes = sockaddr_in6_bytes(ip, port);
    req.payload[..28].copy_from_slice(&bytes);
    req.payload_len = 28;
    req
}

#[test]
fn ipv6_and_dual_stack_sockaddr_round_trip() {
    let v6_loop = IpEndpoint::new(IpAddress::Ipv6(Ipv6Address::LOCALHOST), 8443);
    let encoded = inet_datagram::encode_inet_endpoint(v6_loop, linux_abi::AF_INET6);
    assert_eq!(encoded.len, netd_message::IPV6_ADDRESS_SIZE);
    assert_eq!(
        inet_datagram::decode_inet_endpoint(encoded.as_slice(), linux_abi::AF_INET6, true),
        Ok(v6_loop)
    );

    // Dual-stack IPv4-mapped ::ffff:127.0.0.1 decodes to IPv4 when !ipv6_only.
    let mut mapped = [0_u8; 16];
    mapped[10] = 0xff;
    mapped[11] = 0xff;
    mapped[12..16].copy_from_slice(&[127, 0, 0, 1]);
    let raw = sockaddr_in6_bytes(mapped, 5353);
    assert_eq!(
        inet_datagram::decode_inet_endpoint(&raw, linux_abi::AF_INET6, false),
        Ok(IpEndpoint::new(IpAddress::Ipv4(Ipv4Address::LOCALHOST), 5353))
    );
    // With IPV6_V6ONLY, it remains IPv6 ::ffff:127.0.0.1 (unaddressable for IPv4).
    assert!(matches!(
        inet_datagram::decode_inet_endpoint(&raw, linux_abi::AF_INET6, true),
        Ok(ep) if matches!(ep.addr, IpAddress::Ipv6(_))
    ));
}

#[test]
fn udp_loopback_works_on_both_ipv4_and_ipv6_through_broker_device() {
    let mut state = loopback_test_state();
    let (v4_srv, v4_cli) = (0x60_0001, 0x60_0002);
    let (v6_srv, v6_cli) = (0x60_0003, 0x60_0004);
    insert_inet_sock(&mut state, v4_srv, linux_abi::AF_INET, linux_abi::SOCK_DGRAM);
    insert_inet_sock(&mut state, v4_cli, linux_abi::AF_INET, linux_abi::SOCK_DGRAM);
    insert_inet_sock(&mut state, v6_srv, linux_abi::AF_INET6, linux_abi::SOCK_DGRAM);
    insert_inet_sock(&mut state, v6_cli, linux_abi::AF_INET6, linux_abi::SOCK_DGRAM);

    assert_eq!(
        inet_datagram::bind_request_in_state(&mut state, &sockaddr_in4_req(v4_srv, [127, 0, 0, 1], 19001)),
        0
    );
    assert_eq!(
        inet_datagram::bind_request_in_state(
            &mut state,
            &sockaddr_in6_req(v6_srv, Ipv6Address::LOCALHOST.octets(), 19002)
        ),
        0
    );

    // Send IPv4 loopback packet -> BrokerDevice reflects ARP & UDP locally.
    let mut resp = NetdIpcResponse::default();
    let mut send4 = NetdIpcRequest {
        op: SYSCALL_OFFLOAD_OP_LINUX_SENDTO,
        socket_token: v4_cli,
        ..NetdIpcRequest::default()
    };
    let dst4 = sockaddr_in4_req(0, [127, 0, 0, 1], 19001);
    send4.payload_len =
        netd_message::encode(&mut send4.payload, b"ping4", &[], 0, &dst4.payload[..16]).unwrap()
            as u32;
    assert_eq!(inet_datagram::send_in_state(&mut state, &send4, &mut resp), 0);

    let recv4 = NetdIpcRequest {
        op: SYSCALL_OFFLOAD_OP_LINUX_RECVFROM,
        socket_token: v4_srv,
        arg2: 64,
        ..NetdIpcRequest::default()
    };
    assert_eq!(inet_datagram::recv_in_state(&mut state, &recv4, &mut resp), 0);
    let msg4 = netd_message::decode(&resp.payload[..resp.payload_len as usize]).unwrap();
    assert_eq!(msg4.data, b"ping4");

    // Send IPv6 loopback packet -> BrokerDevice reflects NDP NA & UDPv6 locally.
    let mut send6 = NetdIpcRequest {
        op: SYSCALL_OFFLOAD_OP_LINUX_SENDTO,
        socket_token: v6_cli,
        ..NetdIpcRequest::default()
    };
    let dst6 = sockaddr_in6_bytes(Ipv6Address::LOCALHOST.octets(), 19002);
    send6.payload_len =
        netd_message::encode(&mut send6.payload, b"ping6", &[], 0, &dst6).unwrap() as u32;
    assert_eq!(inet_datagram::send_in_state(&mut state, &send6, &mut resp), 0);

    let recv6 = NetdIpcRequest {
        op: SYSCALL_OFFLOAD_OP_LINUX_RECVFROM,
        socket_token: v6_srv,
        arg2: 64,
        ..NetdIpcRequest::default()
    };
    assert_eq!(inet_datagram::recv_in_state(&mut state, &recv6, &mut resp), 0);
    let msg6 = netd_message::decode(&resp.payload[..resp.payload_len as usize]).unwrap();
    assert_eq!(msg6.data, b"ping6");
    assert_eq!(msg6.address.len(), 28);
}

#[test]
fn tcp_bind_listen_accept_and_data_transfer_over_ipv6_loopback() {
    let mut state = loopback_test_state();
    let (listener, client, accepted) = (0x70_0001, 0x70_0002, 0x70_0003);
    insert_inet_sock(&mut state, listener, linux_abi::AF_INET6, linux_abi::SOCK_STREAM);
    insert_inet_sock(&mut state, client, linux_abi::AF_INET6, linux_abi::SOCK_STREAM);

    assert_eq!(
        inet_tcp::bind_tcp_in_state(
            &mut state,
            &sockaddr_in6_req(listener, Ipv6Address::LOCALHOST.octets(), 18443)
        ),
        0
    );
    assert_eq!(inet_tcp::listen_tcp_in_state(&mut state, listener, 4), 0);

    // Connect TCP client to [::1]:18443 over BrokerDevice loopback.
    let status = inet_tcp::begin_inet_connect_in_state(
        &mut state,
        client,
        IpAddress::Ipv6(Ipv6Address::LOCALHOST),
        18443,
        Duration::from_secs(5),
        5000,
    );
    assert!(matches!(status, 0 | libc::EINPROGRESS));
    state.inet.as_mut().unwrap().poll_budget(16);
    assert_eq!(inet_tcp::inet_connect_status(&mut state, client), 0);

    // Listener POLLIN should be set and pop_ready_tcp_accept creates accepted socket.
    let rev = inet_tcp::inet_socket_revents(&mut state, listener, linux_abi::POLLIN as u32).unwrap();
    assert_ne!(rev & linux_abi::POLLIN as u32, 0);

    let mut acc_resp = NetdIpcResponse::default();
    assert_eq!(
        inet_tcp::pop_ready_tcp_accept_in_state(&mut state, listener, accepted, &mut acc_resp, true),
        Ok(())
    );
    assert_eq!(acc_resp.payload_len, 28);

    // Bidirectional TCP payload send/recv.
    let mut send_resp = NetdIpcResponse::default();
    let send_req = NetdIpcRequest {
        socket_token: client,
        ..NetdIpcRequest::default()
    };
    assert_eq!(
        inet_tcp::handle_inet_send_in_state(&mut state, &send_req, b"GET / HTTP/1.1\r\n", &mut send_resp),
        0
    );
    assert_eq!(send_resp.value, 16);

    let mut recv_resp = NetdIpcResponse::default();
    let recv_req = NetdIpcRequest {
        socket_token: accepted,
        arg2: 64,
        ..NetdIpcRequest::default()
    };
    assert_eq!(
        inet_tcp::handle_inet_recv_in_state(&mut state, &recv_req, &mut recv_resp),
        0
    );
    assert_eq!(&recv_resp.payload[..recv_resp.payload_len as usize], b"GET / HTTP/1.1\r\n");
}

#[test]
fn connect_short_budget_fast_rst_reports_econnrefused_not_etimedout() {
    let mut state = loopback_test_state();
    let cli = 0x71_0001;
    let ext = 0x71_0002;
    insert_inet_sock(&mut state, cli, linux_abi::AF_INET, linux_abi::SOCK_STREAM);
    insert_inet_sock(&mut state, ext, linux_abi::AF_INET, linux_abi::SOCK_STREAM);

    // Loopback RST on closed port 127.0.0.1:19999 completes within poll_budget(8)
    // and with a 40ms budget must still report ECONNREFUSED, not ETIMEDOUT!
    let lo_status = inet_tcp::begin_inet_connect_in_state(
        &mut state,
        cli,
        IpAddress::Ipv4(Ipv4Address::LOCALHOST),
        19999,
        Duration::from_millis(40),
        40,
    );
    assert_eq!(lo_status, libc::ECONNREFUSED);

    // Unacknowledged external SYN stays EINPROGRESS; immediate RST (abort) still
    // reports ECONNREFUSED despite the short 40ms connect budget.
    let status = inet_tcp::begin_inet_connect_in_state(
        &mut state,
        ext,
        IpAddress::Ipv4(Ipv4Address::new(10, 0, 2, 99)),
        19999,
        Duration::from_millis(40),
        40,
    );
    assert_eq!(status, libc::EINPROGRESS);

    let InetTransport::Tcp(h) = state.inet_sockets[&ext].transport else {
        panic!()
    };
    state.inet.as_mut().unwrap().sockets.get_mut::<tcp::Socket>(h).abort();
    assert_eq!(inet_tcp::inet_connect_status(&mut state, ext), libc::ECONNREFUSED);
}

#[test]
fn chromium_sockopts_and_ioctl_fionread_and_netlink_ipv6() {
    let sock_v6_tcp = 0x80_0001;
    let sock_v6_udp = 0x80_0002;
    let mut state = loopback_test_state();
    insert_inet_sock(&mut state, sock_v6_tcp, linux_abi::AF_INET6, linux_abi::SOCK_STREAM);
    insert_inet_sock(&mut state, sock_v6_udp, linux_abi::AF_INET6, linux_abi::SOCK_DGRAM);
    let snap = netlink::current_interfaces(&mut state);
    // Netlink IPv6 snapshot contains ::1 and fe80::...
    assert!(snap.addrs_v6.iter().any(|a| a.local == Ipv6Address::LOCALHOST.octets()));
    let v6_dump = netlink::process_request_snapshot(
        &[
            20_u32.to_ne_bytes().as_slice(),
            &netlink::RTM_GETADDR.to_ne_bytes(),
            &(netlink::NLM_F_REQUEST | netlink::NLM_F_ROOT | 0x200).to_ne_bytes(),
            &5_u32.to_ne_bytes(),
            &0_u32.to_ne_bytes(),
            &[10_u8, 0, 0, 0],
        ]
        .concat(),
        9,
        &snap,
    );
    assert!(v6_dump.len() > netlink::NLMSG_HDRLEN);

    let set_i32 = |st: &mut NetState, tok, level, opt, val: i32| {
        let mut req = NetdIpcRequest {
            socket_token: tok,
            arg1: level,
            arg2: opt,
            payload_len: 4,
            ..NetdIpcRequest::default()
        };
        req.payload[..4].copy_from_slice(&val.to_ne_bytes());
        socket_info::handle_setsockopt_in_state(st, &req)
    };
    let get_i32 = |st: &mut NetState, tok, level, opt| {
        let req = NetdIpcRequest {
            socket_token: tok,
            arg1: level,
            arg2: opt,
            ..NetdIpcRequest::default()
        };
        let mut resp = NetdIpcResponse::default();
        assert_eq!(socket_info::handle_getsockopt_in_state(st, &req, &mut resp), 0);
        i32::from_ne_bytes(resp.payload[..4].try_into().unwrap())
    };

    assert_eq!(get_i32(&mut state, sock_v6_tcp, linux_abi::SOL_SOCKET, linux_abi::SO_DOMAIN), linux_abi::AF_INET6 as i32);
    assert_eq!(set_i32(&mut state, sock_v6_tcp, linux_abi::SOL_SOCKET, linux_abi::SO_REUSEPORT, 1), 0);
    assert_eq!(get_i32(&mut state, sock_v6_tcp, linux_abi::SOL_SOCKET, linux_abi::SO_REUSEPORT), 1);
    assert_eq!(set_i32(&mut state, sock_v6_tcp, linux_abi::SOL_SOCKET, linux_abi::SO_SNDBUF, 262144), 0);
    assert_eq!(get_i32(&mut state, sock_v6_tcp, linux_abi::SOL_SOCKET, linux_abi::SO_SNDBUF), 262144);
    assert_eq!(set_i32(&mut state, sock_v6_tcp, linux_abi::SOL_TCP, libc::TCP_NODELAY as u64, 1), 0);
    assert_eq!(get_i32(&mut state, sock_v6_tcp, linux_abi::SOL_TCP, libc::TCP_NODELAY as u64), 1);
    assert_eq!(set_i32(&mut state, sock_v6_tcp, linux_abi::SOL_TCP, linux_abi::TCP_KEEPIDLE, 60), 0);
    assert_eq!(get_i32(&mut state, sock_v6_tcp, linux_abi::SOL_TCP, linux_abi::TCP_KEEPIDLE), 60);
    assert_eq!(set_i32(&mut state, sock_v6_tcp, linux_abi::SOL_TCP, linux_abi::TCP_KEEPINTVL, 10), 0);
    assert_eq!(set_i32(&mut state, sock_v6_tcp, linux_abi::SOL_TCP, linux_abi::TCP_KEEPCNT, 5), 0);
    assert_eq!(set_i32(&mut state, sock_v6_tcp, linux_abi::SOL_TCP, linux_abi::TCP_USER_TIMEOUT, 5000), 0);
    assert_eq!(get_i32(&mut state, sock_v6_tcp, linux_abi::SOL_TCP, linux_abi::TCP_USER_TIMEOUT), 5000);
    assert_eq!(set_i32(&mut state, sock_v6_udp, linux_abi::IPPROTO_IPV6, linux_abi::IPV6_V6ONLY, 1), 0);
    assert_eq!(get_i32(&mut state, sock_v6_udp, linux_abi::IPPROTO_IPV6, linux_abi::IPV6_V6ONLY), 1);
    assert_eq!(set_i32(&mut state, sock_v6_udp, linux_abi::IPPROTO_IPV6, linux_abi::IPV6_TCLASS, 0x28), 0);
    assert_eq!(get_i32(&mut state, sock_v6_udp, linux_abi::IPPROTO_IPV6, linux_abi::IPV6_TCLASS), 0x28);
    assert_eq!(set_i32(&mut state, sock_v6_udp, linux_abi::IPPROTO_IPV6, linux_abi::IPV6_RECVERR, 1), 0);
    assert_eq!(get_i32(&mut state, sock_v6_udp, linux_abi::IPPROTO_IPV6, linux_abi::IPV6_RECVERR), 1);
    assert_eq!(set_i32(&mut state, sock_v6_udp, linux_abi::SOL_IP, linux_abi::IP_TOS, 0x10), 0);
    assert_eq!(set_i32(&mut state, sock_v6_udp, linux_abi::SOL_IP, linux_abi::IP_TTL, 32), 0);
    assert_eq!(get_i32(&mut state, sock_v6_udp, linux_abi::SOL_IP, linux_abi::IP_TTL), 32);

    // Test FIONREAD returns 0 on empty UDP socket.
    let mut ioctl_resp = NetdIpcResponse::default();
    let ioctl_req = NetdIpcRequest {
        op: SYSCALL_OFFLOAD_OP_LINUX_IOCTL,
        socket_token: sock_v6_udp,
        arg1: linux_abi::FIONREAD,
        ..NetdIpcRequest::default()
    };
    assert_eq!(socket_info::handle_ioctl_in_state(&mut state, &ioctl_req, &mut ioctl_resp), 0);
    assert_eq!(i32::from_ne_bytes(ioctl_resp.payload[..4].try_into().unwrap()), 0);
}

#[test]
fn buffer_resize_keeps_tcp_nodelay_keepalive_and_ttl() {
    let tok = 0x80_0010;
    let mut state = loopback_test_state();
    insert_inet_sock(&mut state, tok, linux_abi::AF_INET, linux_abi::SOCK_STREAM);
    let set_i32 = |st: &mut NetState, level, opt, val: i32| {
        let mut req = NetdIpcRequest {
            socket_token: tok,
            arg1: level,
            arg2: opt,
            payload_len: 4,
            ..NetdIpcRequest::default()
        };
        req.payload[..4].copy_from_slice(&val.to_ne_bytes());
        socket_info::handle_setsockopt_in_state(st, &req)
    };
    assert_eq!(set_i32(&mut state, linux_abi::SOL_TCP, libc::TCP_NODELAY as u64, 1), 0);
    assert_eq!(set_i32(&mut state, linux_abi::SOL_SOCKET, linux_abi::SO_KEEPALIVE, 1), 0);
    assert_eq!(set_i32(&mut state, linux_abi::SOL_IP, linux_abi::IP_TTL, 17), 0);
    assert_eq!(set_i32(&mut state, linux_abi::SOL_SOCKET, linux_abi::SO_RCVBUF, 200_000), 0);
    let InetTransport::Tcp(h) = state.inet_sockets[&tok].transport else { panic!() };
    let sock = state.inet.as_ref().unwrap().sockets.get::<tcp::Socket>(h);
    assert!(!sock.nagle_enabled(), "SO_RCVBUF resize must not reset TCP_NODELAY");
    assert_eq!(sock.hop_limit(), Some(17));
    assert!(sock.keep_alive().is_some());
}

#[test]
fn so_reuseaddr_allows_rebind_unless_existing_socket_listens() {
    let ep = IpEndpoint::new(IpAddress::Ipv4(Ipv4Address::UNSPECIFIED), 8080);
    let v4 = linux_abi::AF_INET;
    // Both sides set SO_REUSEADDR and the holder only lingers (accepted
    // connection / bound-only): the rebind must succeed.
    assert!(!inet_datagram::inet_bind_endpoints_conflict(
        v4, false, true, false, ep, v4, false, true, false, false, ep
    ));
    // A listener always conflicts.
    assert!(inet_datagram::inet_bind_endpoints_conflict(
        v4, false, true, false, ep, v4, false, true, false, true, ep
    ));
    // Without SO_REUSEADDR on the new socket the port stays busy.
    assert!(inet_datagram::inet_bind_endpoints_conflict(
        v4, false, false, false, ep, v4, false, true, false, false, ep
    ));
}

#[test]
fn fionread_on_udp_reports_next_datagram_size() {
    let (a, b) = (0x80_0020, 0x80_0021);
    let mut state = loopback_test_state();
    insert_inet_sock(&mut state, a, linux_abi::AF_INET, linux_abi::SOCK_DGRAM);
    insert_inet_sock(&mut state, b, linux_abi::AF_INET, linux_abi::SOCK_DGRAM);
    assert_eq!(
        inet_datagram::bind_request_in_state(&mut state, &sockaddr_in4_req(b, [127, 0, 0, 1], 9100)),
        0
    );
    let mut send = NetdIpcRequest {
        op: SYSCALL_OFFLOAD_OP_LINUX_SENDTO,
        socket_token: a,
        ..NetdIpcRequest::default()
    };
    let mut addr = [0_u8; 16];
    addr[..2].copy_from_slice(&(libc::AF_INET as u16).to_ne_bytes());
    addr[2..4].copy_from_slice(&9100_u16.to_be_bytes());
    addr[4..8].copy_from_slice(&[127, 0, 0, 1]);
    send.payload_len = netd_message::encode(&mut send.payload, b"hello", &[], 0, &addr).unwrap() as u32;
    let mut resp = NetdIpcResponse::default();
    assert_eq!(inet_datagram::send_in_state(&mut state, &send, &mut resp), 0);
    state.inet.as_mut().unwrap().poll_budget(8);
    let ioctl = NetdIpcRequest {
        op: SYSCALL_OFFLOAD_OP_LINUX_IOCTL,
        socket_token: b,
        arg1: linux_abi::FIONREAD,
        ..NetdIpcRequest::default()
    };
    let mut resp = NetdIpcResponse::default();
    assert_eq!(socket_info::handle_ioctl_in_state(&mut state, &ioctl, &mut resp), 0);
    assert_eq!(i32::from_ne_bytes(resp.payload[..4].try_into().unwrap()), 5);
}
