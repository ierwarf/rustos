//! AF_NETLINK/NETLINK_ROUTE: byte-exact builders and the socket flow.

use super::*;
use netlink::*;

fn nlmsg(len: u32, ty: u16, flags: u16, seq: u32, pid: u32) -> Vec<u8> {
    let mut bytes = Vec::new();
    bytes.extend_from_slice(&len.to_ne_bytes());
    bytes.extend_from_slice(&ty.to_ne_bytes());
    bytes.extend_from_slice(&flags.to_ne_bytes());
    bytes.extend_from_slice(&seq.to_ne_bytes());
    bytes.extend_from_slice(&pid.to_ne_bytes());
    bytes
}

fn attr(ty: u16, data: &[u8]) -> Vec<u8> {
    let mut bytes = Vec::new();
    bytes.extend_from_slice(&((4 + data.len()) as u16).to_ne_bytes());
    bytes.extend_from_slice(&ty.to_ne_bytes());
    bytes.extend_from_slice(data);
    while bytes.len() % 4 != 0 {
        bytes.push(0);
    }
    bytes
}

fn dump_request(ty: u16, family: u8, seq: u32) -> Vec<u8> {
    // nlmsghdr + rtgenmsg (padded to 4): the shape glibc/libnl/Chromium send.
    let mut bytes = nlmsg(20, ty, 0x1 | 0x300, seq, 0);
    bytes.extend_from_slice(&[family, 0, 0, 0]);
    bytes
}

#[test]
fn loopback_link_message_is_byte_exact() {
    let message = build_link_message(&loopback_link(), 7, 4242);
    let mut body = Vec::new();
    // ifinfomsg: AF_UNSPEC, pad, ARPHRD_LOOPBACK, index 1, flags, change 0.
    body.extend_from_slice(&[0, 0]);
    body.extend_from_slice(&772_u16.to_ne_bytes());
    body.extend_from_slice(&1_i32.to_ne_bytes());
    body.extend_from_slice(&0x10049_u32.to_ne_bytes()); // UP|LOOPBACK|RUNNING|LOWER_UP
    body.extend_from_slice(&0_u32.to_ne_bytes());
    body.extend_from_slice(&attr(3, b"lo\0"));
    body.extend_from_slice(&attr(4, &65536_u32.to_ne_bytes()));
    body.extend_from_slice(&attr(16, &[0]));
    body.extend_from_slice(&attr(1, &[0; 6]));
    body.extend_from_slice(&attr(2, &[0; 6]));
    let mut expected = nlmsg((16 + body.len()) as u32, 16, 0x2, 7, 4242);
    expected.extend_from_slice(&body);
    assert_eq!(message, expected);
    assert_eq!(message.len() % 4, 0);
    assert_eq!(message.len(), 16 + 16 + 8 + 8 + 8 + 12 + 12);
}

#[test]
fn loopback_address_message_is_byte_exact() {
    let message = build_addr_message(&loopback_addr(), 9, 1);
    let mut body = Vec::new();
    // ifaddrmsg: AF_INET, /8, IFA_F_PERMANENT, RT_SCOPE_HOST, ifindex 1.
    body.extend_from_slice(&[2, 8, 0x80, 254]);
    body.extend_from_slice(&1_u32.to_ne_bytes());
    body.extend_from_slice(&attr(1, &[127, 0, 0, 1]));
    body.extend_from_slice(&attr(2, &[127, 0, 0, 1]));
    body.extend_from_slice(&attr(3, b"lo\0"));
    let mut expected = nlmsg((16 + body.len()) as u32, 20, 0x2, 9, 1);
    expected.extend_from_slice(&body);
    assert_eq!(message, expected);
}

#[test]
fn provider_interface_is_built_from_the_real_stack_values() {
    let link = provider_link(2, QEMU_USERNET_MAC.0);
    assert_eq!(link.name, "eth0");
    assert_eq!(link.mtu, 1500);
    assert_eq!(link.mac, [0x52, 0x54, 0x00, 0x12, 0x34, 0x56]);
    assert_eq!(
        link.flags,
        IFF_UP | IFF_BROADCAST | IFF_RUNNING | IFF_MULTICAST | IFF_LOWER_UP
    );
    let addr = provider_addr(2, QEMU_USERNET_ADDR.octets(), 24);
    assert_eq!(addr.local, [10, 0, 2, 15]);
    assert_eq!(addr.broadcast, Some([10, 0, 2, 255]));
    assert_eq!(
        (addr.scope, addr.prefix_len, addr.label.as_str()),
        (RT_SCOPE_UNIVERSE, 24, "eth0")
    );
    let message = build_addr_message(&addr, 1, 1);
    // ADDRESS, LOCAL, BROADCAST, LABEL ("eth0\0" pads to 8).
    assert_eq!(message.len(), 16 + 8 + 8 + 8 + 8 + 12);
    assert_eq!(&message[16 + 8 + 4 + 4 + 4..][..4], &[10, 0, 2, 15]);
}

#[test]
fn done_and_error_messages_are_byte_exact() {
    let mut done = nlmsg(20, 3, 0x2, 5, 77);
    done.extend_from_slice(&0_i32.to_ne_bytes());
    assert_eq!(build_done(5, 77), done);
    let request = nlmsg(16, 99, 1, 5, 0);
    let mut error = nlmsg(16 + 4 + 16, 2, 0, 5, 77);
    error.extend_from_slice(&(-libc::EOPNOTSUPP).to_ne_bytes());
    error.extend_from_slice(&request);
    assert_eq!(build_error(libc::EOPNOTSUPP, &request, 5, 77), error);
}

#[test]
fn getlink_dump_answers_every_link_then_done() {
    let links = [loopback_link(), provider_link(2, [1, 2, 3, 4, 5, 6])];
    let reply = process_request(&dump_request(RTM_GETLINK, 0, 11), 4242, &links, &[]);
    let mut expected = build_link_message(&links[0], 11, 4242);
    expected.extend_from_slice(&build_link_message(&links[1], 11, 4242));
    expected.extend_from_slice(&build_done(11, 4242));
    assert_eq!(reply, expected);
}

#[test]
fn getaddr_dump_filters_by_family() {
    let addrs = [loopback_addr(), provider_addr(2, [10, 0, 2, 15], 24)];
    let all = process_request(&dump_request(RTM_GETADDR, 0, 3), 9, &[], &addrs);
    let mut expected = build_addr_message(&addrs[0], 3, 9);
    expected.extend_from_slice(&build_addr_message(&addrs[1], 3, 9));
    expected.extend_from_slice(&build_done(3, 9));
    assert_eq!(all, expected);
    assert_eq!(
        process_request(&dump_request(RTM_GETADDR, 2, 3), 9, &[], &addrs),
        expected
    );
    // IPv6 has no address table here: an empty dump, still terminated.
    assert_eq!(
        process_request(&dump_request(RTM_GETADDR, 10, 3), 9, &[], &addrs),
        build_done(3, 9)
    );
}

#[test]
fn unsupported_types_and_non_dump_requests_get_eopnotsupp() {
    let links = [loopback_link()];
    let route = {
        let mut bytes = nlmsg(20, 26, 0x1 | 0x300, 2, 0); // RTM_GETROUTE
        bytes.extend_from_slice(&[0; 4]);
        bytes
    };
    assert_eq!(
        process_request(&route, 5, &links, &[]),
        build_error(libc::EOPNOTSUPP, &route, 2, 5)
    );
    let single = {
        let mut bytes = nlmsg(20, RTM_GETLINK, 0x1, 8, 0); // no NLM_F_ROOT
        bytes.extend_from_slice(&[0; 4]);
        bytes
    };
    assert_eq!(
        process_request(&single, 5, &links, &[]),
        build_error(libc::EOPNOTSUPP, &single, 8, 5)
    );
}

#[test]
fn several_messages_and_malformed_tails_are_handled_like_the_kernel() {
    let links = [loopback_link()];
    let mut two = dump_request(RTM_GETLINK, 0, 1);
    two.extend_from_slice(&dump_request(RTM_GETADDR, 0, 2));
    let reply = process_request(&two, 5, &links, &[loopback_addr()]);
    let mut expected = build_link_message(&links[0], 1, 5);
    expected.extend_from_slice(&build_done(1, 5));
    expected.extend_from_slice(&build_addr_message(&loopback_addr(), 2, 5));
    expected.extend_from_slice(&build_done(2, 5));
    assert_eq!(reply, expected);
    // Length shorter than a header, longer than the buffer, or a short buffer.
    assert!(process_request(&nlmsg(8, RTM_GETLINK, 0x301, 1, 0), 5, &links, &[]).is_empty());
    assert!(process_request(&nlmsg(64, RTM_GETLINK, 0x301, 1, 0), 5, &links, &[]).is_empty());
    assert!(process_request(&[1, 2, 3], 5, &links, &[]).is_empty());
    // A valid message followed by garbage still answers the valid one.
    let mut garbage = dump_request(RTM_GETLINK, 0, 6);
    garbage.extend_from_slice(&[0xff; 7]);
    assert_eq!(
        process_request(&garbage, 5, &links, &[]).len(),
        build_link_message(&links[0], 6, 5).len() + 20
    );
}

#[test]
fn sockaddr_nl_is_twelve_bytes() {
    let bytes = sockaddr_nl_bytes(0x1234, 0x5);
    assert_eq!(&bytes[..2], &(libc::AF_NETLINK as u16).to_ne_bytes());
    assert_eq!(&bytes[2..4], &[0, 0]);
    assert_eq!(&bytes[4..8], &0x1234_u32.to_ne_bytes());
    assert_eq!(&bytes[8..12], &5_u32.to_ne_bytes());
}

fn netlink_socket(sock_type: u64) -> UnixSocket {
    UnixSocket {
        kind: UnixSocketKind::Netlink,
        owner: Credentials::default(),
        refs: 1,
        options: SocketOptions::default(),
        bound_path: None,
        local_path: None,
        peer_path: None,
        state: UnixSocketState::Netlink(NetlinkState::new(sock_type)),
    }
}

fn request(token: u64, pid: u64) -> NetdIpcRequest {
    NetdIpcRequest {
        socket_token: token,
        pid,
        ..NetdIpcRequest::default()
    }
}

fn recv_request(token: u64, len: u64, flags: u64) -> NetdIpcRequest {
    NetdIpcRequest {
        op: SYSCALL_OFFLOAD_OP_LINUX_RECVFROM,
        arg2: len,
        arg3: flags,
        ..request(token, 0)
    }
}

#[test]
fn socket_flow_dump_poll_recv_and_getsockname() {
    let mut state = NetState::new();
    state
        .sockets
        .insert(1, netlink_socket(libc::SOCK_RAW as u64));
    // Unbound: nl_pid 0, not readable, always writable.
    let idle =
        unix_socket_revents(&state, 1, (linux_abi::POLLIN | linux_abi::POLLOUT) as u32).unwrap();
    assert_eq!(idle, linux_abi::POLLOUT as u32);
    let dump = dump_request(RTM_GETLINK, 0, 21);
    assert_eq!(
        send_socket_message_in_state(&mut state, &request(1, 31337), &dump, &[]),
        Ok(dump.len())
    );
    let UnixSocketState::Netlink(netlink) = &state.sockets[&1].state else {
        panic!()
    };
    // Autobind took the caller's process id.
    assert_eq!(netlink.portid(), 31337);
    assert_eq!(netlink.queued_messages(), 1);
    let ready = unix_socket_revents(&state, 1, linux_abi::POLLIN as u32).unwrap();
    assert_eq!(ready, linux_abi::POLLIN as u32);

    let mut response = NetdIpcResponse::default();
    assert_eq!(
        netlink::handle_recv(&mut state, &recv_request(1, 4096, 0), &mut response),
        0
    );
    let expected: Vec<u8> = {
        let mut bytes = build_link_message(&loopback_link(), 21, 31337);
        bytes.extend_from_slice(&build_done(21, 31337));
        bytes
    };
    assert_eq!(response.value as usize, expected.len());
    assert_eq!(&response.payload[..expected.len()], expected.as_slice());
    // Drained: EAGAIN, and no longer readable.
    assert_eq!(
        netlink::handle_recv(
            &mut state,
            &recv_request(1, 4096, 0),
            &mut NetdIpcResponse::default()
        ),
        libc::EAGAIN
    );
    assert_eq!(
        unix_socket_revents(&state, 1, linux_abi::POLLIN as u32).unwrap(),
        0
    );
    // getsockname: sockaddr_nl with the assigned port id.
    let UnixSocketState::Netlink(netlink) = &state.sockets[&1].state else {
        panic!()
    };
    let mut name = NetdIpcResponse::default();
    assert_eq!(netlink.write_sockaddr(&mut name), 0);
    assert_eq!(name.payload_len, 12);
    assert_eq!(&name.payload[..12], &sockaddr_nl_bytes(31337, 0));
}

#[test]
fn short_recv_buffer_truncates_and_msg_trunc_returns_the_real_length() {
    let mut state = NetState::new();
    state
        .sockets
        .insert(1, netlink_socket(libc::SOCK_DGRAM as u64));
    let dump = dump_request(RTM_GETLINK, 0, 1);
    send_socket_message_in_state(&mut state, &request(1, 5), &dump, &[]).unwrap();
    let full = {
        let mut bytes = build_link_message(&loopback_link(), 1, 5);
        bytes.extend_from_slice(&build_done(1, 5));
        bytes
    };
    // MSG_PEEK|MSG_TRUNC with a tiny buffer: the sizing probe glibc performs.
    let mut response = NetdIpcResponse::default();
    let flags = libc::MSG_PEEK as u64 | libc::MSG_TRUNC as u64;
    assert_eq!(
        netlink::handle_recv(&mut state, &recv_request(1, 4, flags), &mut response),
        0
    );
    assert_eq!(
        (response.value as usize, response.payload_len),
        (full.len(), 4)
    );
    // The peek did not dequeue; a recvmsg with a short buffer sets MSG_TRUNC.
    let mut msg = NetdIpcResponse::default();
    let recvmsg = NetdIpcRequest {
        op: SYSCALL_OFFLOAD_OP_LINUX_RECVMSG,
        payload_len: 8,
        ..request(1, 5)
    };
    assert_eq!(netlink::handle_recvmsg(&mut state, &recvmsg, &mut msg), 0);
    let msg_flags = u32::from_ne_bytes(msg.payload[8..12].try_into().unwrap());
    assert_ne!(msg_flags & libc::MSG_TRUNC as u32, 0);
    assert_eq!(msg.value, 8);
    let UnixSocketState::Netlink(netlink) = &state.sockets[&1].state else {
        panic!()
    };
    assert_eq!(netlink.queued_messages(), 0);
}

#[test]
fn receive_buffer_overflow_reports_enobufs_once() {
    let mut state = NetState::new();
    let mut socket = netlink_socket(libc::SOCK_RAW as u64);
    socket.options.recv_buffer = 4096;
    state.sockets.insert(1, socket);
    let dump = dump_request(RTM_GETLINK, 0, 1);
    // Each reply is ~170 bytes: far more than 4096 requests' worth fit never.
    let mut sent = 0;
    while let UnixSocketState::Netlink(netlink) = &state.sockets[&1].state {
        if netlink.queued_messages() == sent && sent < 64 {
            send_socket_message_in_state(&mut state, &request(1, 5), &dump, &[]).unwrap();
            sent += 1;
        } else {
            break;
        }
    }
    let UnixSocketState::Netlink(netlink) = &state.sockets[&1].state else {
        panic!()
    };
    assert!(netlink.queued_messages() < sent, "a reply was dropped");
    let mut response = NetdIpcResponse::default();
    let mut errors = 0;
    loop {
        match netlink::handle_recv(&mut state, &recv_request(1, 4096, 0), &mut response) {
            0 => {}
            libc::EAGAIN => break,
            libc::ENOBUFS => errors += 1,
            other => panic!("unexpected {other}"),
        }
    }
    assert_eq!(errors, 1);
}

#[test]
fn bind_assigns_and_protects_port_ids() {
    // Global state: the handlers lock it.
    let (a, b) = (0x9b0_0001_u64, 0x9b0_0002_u64);
    {
        let mut state = lock_net_state();
        state
            .sockets
            .insert(a, netlink_socket(libc::SOCK_RAW as u64));
        state
            .sockets
            .insert(b, netlink_socket(libc::SOCK_RAW as u64));
    }
    let bind = |token: u64, pid: u32, groups: u32| {
        let mut req = request(token, 777);
        req.payload[..12].copy_from_slice(&sockaddr_nl_bytes(pid, groups));
        req.payload_len = 12;
        netlink::handle_bind(&req)
    };
    assert_eq!(bind(a, 0xabc0_1111, 0x7), libc::EOPNOTSUPP);
    assert_eq!(bind(a, 0xabc0_1111, 0), 0);
    assert_eq!(bind(b, 0xabc0_1111, 0), libc::EADDRINUSE);
    // pid 0 assigns a free id: the process id first, then a generated one.
    assert_eq!(bind(b, 0, 0), 0);
    let portid_of = |token: u64| match &lock_net_state().sockets[&token].state {
        UnixSocketState::Netlink(netlink) => netlink.portid(),
        _ => unreachable!(),
    };
    assert_eq!(portid_of(a), 0xabc0_1111);
    assert_eq!(portid_of(b), 777);
    let mut short = request(a, 1);
    short.payload_len = 4;
    assert_eq!(netlink::handle_bind(&short), libc::EINVAL);
    let mut wrong_family = request(a, 1);
    wrong_family.payload[..2].copy_from_slice(&(linux_abi::AF_UNIX as u16).to_ne_bytes());
    wrong_family.payload_len = 12;
    assert_eq!(netlink::handle_bind(&wrong_family), libc::EINVAL);
    let mut state = lock_net_state();
    state.sockets.remove(&a);
    state.sockets.remove(&b);
}

#[test]
fn netlink_shape_and_so_type() {
    use rustos_user_abi::syscall::net_broker_netlink_socket_shape_valid as shape;
    assert!(shape(16, 3, 0) && shape(16, 2, 0) && shape(16, 3 | 0o2004000, 0));
    assert!(!shape(16, 1, 0) && !shape(16, 3, 9) && !shape(1, 3, 0) && !shape(16, 3 | 1 << 30, 0));
    let token = 0x9b0_0010_u64;
    lock_net_state()
        .sockets
        .insert(token, netlink_socket(libc::SOCK_DGRAM as u64));
    let query = |option| {
        let req = NetdIpcRequest {
            socket_token: token,
            arg1: linux_abi::SOL_SOCKET,
            arg2: option,
            ..NetdIpcRequest::default()
        };
        let mut response = NetdIpcResponse::default();
        assert_eq!(handle_getsockopt(&req, &mut response), 0);
        i32::from_ne_bytes(response.payload[..4].try_into().unwrap())
    };
    assert_eq!(query(linux_abi::SO_TYPE), libc::SOCK_DGRAM);
    assert_eq!(query(linux_abi::SO_DOMAIN), libc::AF_NETLINK);
    // SO_RCVBUF/SO_SNDBUF are accepted.
    for option in [linux_abi::SO_RCVBUF, linux_abi::SO_SNDBUF] {
        let mut req = NetdIpcRequest {
            socket_token: token,
            arg1: linux_abi::SOL_SOCKET,
            arg2: option,
            payload_len: 4,
            ..NetdIpcRequest::default()
        };
        req.payload[..4].copy_from_slice(&(1_i32 << 20).to_ne_bytes());
        assert_eq!(handle_setsockopt(&req), 0);
    }
    lock_net_state().sockets.remove(&token);
}
