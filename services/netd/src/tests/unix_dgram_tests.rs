use super::*;
use rustos_user_abi::syscall::netd_message;

fn dgram_socket(creds: Credentials) -> UnixSocket {
    UnixSocket {
        kind: UnixSocketKind::Dgram,
        owner: creds,
        refs: 1,
        options: SocketOptions::default(),
        bound_path: None,
        bound_inode: None,
        local_path: None,
        peer_path: None,
        state: UnixSocketState::Dgram(UnixDgramState::default()),
    }
}

fn unix_un_addr(name: &[u8]) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(2 + name.len());
    bytes.extend_from_slice(&(linux_abi::AF_UNIX as u16).to_ne_bytes());
    bytes.extend_from_slice(name);
    bytes
}

fn send_dgram_req(token: u64, pid: u64, data: &[u8], control: &[u8], addr: &[u8]) -> NetdIpcRequest {
    let mut req = NetdIpcRequest {
        op: SYSCALL_OFFLOAD_OP_LINUX_SENDMSG,
        socket_token: token,
        pid,
        euid: 1000,
        egid: 1000,
        ..NetdIpcRequest::default()
    };
    req.payload_len = netd_message::encode(&mut req.payload, data, control, 0, addr).unwrap() as u32;
    req
}

fn recv_dgram_req(op: u16, token: u64, max_len: usize, flags: u64) -> NetdIpcRequest {
    let mut req = NetdIpcRequest {
        op,
        socket_token: token,
        ..NetdIpcRequest::default()
    };
    if op == SYSCALL_OFFLOAD_OP_LINUX_RECVMSG {
        req.payload_len = max_len as u32;
        req.arg2 = flags;
    } else {
        req.arg2 = max_len as u64;
        req.arg3 = flags;
    }
    req
}

#[test]
fn unix_dgram_bind_sendto_recvfrom_peek_trunc_and_empty_datagram() {
    let mut state = NetState::new();
    let server = 0x10_0001;
    let client = 0x10_0002;
    let stream = 0x10_0003;
    let srv_addr = UnixAddress::Abstract(b"\0dgram-srv".to_vec());
    let cli_addr = UnixAddress::Abstract(b"\0dgram-cli".to_vec());

    let mut srv_sock = dgram_socket(Credentials { pid: 10, uid: 100, gid: 100 });
    srv_sock.bound_path = Some(srv_addr.clone());
    srv_sock.local_path = Some(srv_addr.clone());
    srv_sock.options.passcred = true;

    let mut cli_sock = dgram_socket(Credentials { pid: 20, uid: 200, gid: 200 });
    cli_sock.bound_path = Some(cli_addr.clone());
    cli_sock.local_path = Some(cli_addr.clone());

    let stream_addr = UnixAddress::Abstract(b"\0stream-srv".to_vec());
    let mut stream_sock = UnixSocket {
        kind: UnixSocketKind::Stream,
        owner: Credentials::default(),
        refs: 1,
        options: SocketOptions::default(),
        bound_path: Some(stream_addr.clone()),
        bound_inode: None,
        local_path: Some(stream_addr),
        peer_path: None,
        state: UnixSocketState::Idle,
    };
    stream_sock.bound_path = Some(UnixAddress::Abstract(b"\0stream-srv".to_vec()));

    state.sockets.insert(server, srv_sock);
    state.sockets.insert(client, cli_sock);
    state.sockets.insert(stream, stream_sock);
    state.bindings.insert(srv_addr, server);
    state.bindings.insert(cli_addr, client);
    state.bindings.insert(UnixAddress::Abstract(b"\0stream-srv".to_vec()), stream);

    // Send without address on unconnected socket fails ENOTCONN.
    let mut resp = NetdIpcResponse::default();
    let req = send_dgram_req(client, 20, b"hi", &[], &[]);
    assert!(matches!(
        dgram_send_in_state(&mut state, &req, &mut resp, None),
        Err(DgramSendOutcome::Errno(libc::ENOTCONN))
    ));

    // Send to SOCK_STREAM abstract name fails EPROTOTYPE.
    let req = send_dgram_req(client, 20, b"hi", &[], &unix_un_addr(b"\0stream-srv"));
    assert!(matches!(
        dgram_send_in_state(&mut state, &req, &mut resp, None),
        Err(DgramSendOutcome::Errno(libc::EPROTOTYPE))
    ));

    // Send "hello world" and empty 0-byte datagram to DGRAM server.
    let req1 = send_dgram_req(client, 20, b"hello world", &[], &unix_un_addr(b"\0dgram-srv"));
    assert!(dgram_send_in_state(&mut state, &req1, &mut resp, None).is_ok());
    assert_eq!(resp.value, 11);

    let req2 = send_dgram_req(client, 20, b"", &[], &unix_un_addr(b"\0dgram-srv"));
    assert!(dgram_send_in_state(&mut state, &req2, &mut resp, None).is_ok());
    assert_eq!(resp.value, 0);

    // POLLIN & FIONREAD on server report first datagram length (11 bytes).
    assert_ne!(
        unix_socket_revents(&state, server, linux_abi::POLLIN as u32).unwrap()
            & linux_abi::POLLIN as u32,
        0
    );
    let UnixSocketState::Dgram(d) = &state.sockets[&server].state else {
        panic!()
    };
    assert_eq!(d.incoming.front().map(|s| s.bytes.len()), Some(11));

    // MSG_PEEK | MSG_TRUNC keeps the datagram queued and reports full length.
    let mut peek_resp = NetdIpcResponse::default();
    let peek_req = recv_dgram_req(
        SYSCALL_OFFLOAD_OP_LINUX_RECVFROM,
        server,
        5,
        (libc::MSG_PEEK | libc::MSG_TRUNC) as u64,
    );
    assert_eq!(dgram_recv_in_state(&mut state, &peek_req, &mut peek_resp), 0);
    assert_eq!(peek_resp.value, 11);
    let decoded = netd_message::decode(&peek_resp.payload[..peek_resp.payload_len as usize]).unwrap();
    assert_eq!(decoded.data, b"hello");
    assert_eq!(decoded.flags & libc::MSG_TRUNC as u32, libc::MSG_TRUNC as u32);
    assert_eq!(decoded.address, unix_un_addr(b"\0dgram-cli"));

    // RECVMSG with SO_PASSCRED delivers SCM_CREDENTIALS alongside payload & sender addr.
    let mut recv_resp = NetdIpcResponse::default();
    let recv_req = recv_dgram_req(SYSCALL_OFFLOAD_OP_LINUX_RECVMSG, server, 64, 0);
    assert_eq!(dgram_recv_in_state(&mut state, &recv_req, &mut recv_resp), 0);
    assert_eq!(recv_resp.value, 11);
    let decoded = netd_message::decode(&recv_resp.payload[..recv_resp.payload_len as usize]).unwrap();
    assert_eq!(decoded.data, b"hello world");
    assert_eq!(decoded.address, unix_un_addr(b"\0dgram-cli"));
    assert!(!decoded.control.is_empty());
    let hdr = read_cmsghdr(decoded.control).unwrap();
    assert_eq!(hdr.cmsg_level, linux_abi::SOL_SOCKET as u32);
    assert_eq!(hdr.cmsg_type, linux_abi::SCM_CREDENTIALS as u32);

    // Second datagram is empty (0 bytes) and returns 0 with sender addr, not EAGAIN.
    let mut empty_resp = NetdIpcResponse::default();
    assert_eq!(dgram_recv_in_state(&mut state, &recv_req, &mut empty_resp), 0);
    assert_eq!(empty_resp.value, 0);
    let decoded = netd_message::decode(&empty_resp.payload[..empty_resp.payload_len as usize]).unwrap();
    assert!(decoded.data.is_empty());
    assert_eq!(decoded.address, unix_un_addr(b"\0dgram-cli"));

    // Now queue is empty -> EAGAIN.
    assert_eq!(
        dgram_recv_in_state(&mut state, &recv_req, &mut empty_resp),
        libc::EAGAIN
    );
}

#[test]
fn unix_dgram_connect_unconnect_peercred_and_listen_eopnotsupp() {
    let srv = 0x20_0001;
    let cli = 0x20_0002;
    let addr = UnixAddress::Abstract(b"\0dgram-conn-test".to_vec());
    {
        let mut state = lock_net_state();
        let mut s = dgram_socket(Credentials { pid: 55, uid: 56, gid: 57 });
        s.bound_path = Some(addr.clone());
        s.local_path = Some(addr.clone());
        state.sockets.insert(srv, s);
        state.sockets.insert(cli, dgram_socket(Credentials { pid: 66, uid: 67, gid: 68 }));
        state.bindings.insert(addr.clone(), srv);
    }

    // listen() on AF_UNIX SOCK_DGRAM must return EOPNOTSUPP.
    let listen_req = NetdIpcRequest {
        socket_token: srv,
        arg1: 4,
        ..NetdIpcRequest::default()
    };
    assert_eq!(handle_listen(&listen_req), libc::EOPNOTSUPP);

    // connect() client to server sets peer + SO_PEERCRED.
    let mut conn_req = NetdIpcRequest {
        socket_token: cli,
        ..NetdIpcRequest::default()
    };
    let raw_addr = unix_un_addr(b"\0dgram-conn-test");
    conn_req.payload[..raw_addr.len()].copy_from_slice(&raw_addr);
    conn_req.payload_len = raw_addr.len() as u32;
    let mut ns = path_namespace::PrivateNamespace;
    assert_eq!(handle_connect_with(&conn_req, &mut ns), 0);

    let mut resp = NetdIpcResponse::default();
    let peercred_req = NetdIpcRequest {
        socket_token: cli,
        arg1: linux_abi::SOL_SOCKET,
        arg2: linux_abi::SO_PEERCRED,
        ..NetdIpcRequest::default()
    };
    assert_eq!(handle_getsockopt(&peercred_req, &mut resp), 0);
    assert_eq!(i32::from_ne_bytes(resp.payload[0..4].try_into().unwrap()), 55);

    // connect(AF_UNSPEC) dissolves connection; subsequent SO_PEERCRED -> ENOTCONN.
    let mut unconn_req = NetdIpcRequest {
        socket_token: cli,
        ..NetdIpcRequest::default()
    };
    unconn_req.payload[..2].copy_from_slice(&(libc::AF_UNSPEC as u16).to_ne_bytes());
    unconn_req.payload_len = 2;
    assert_eq!(handle_connect_with(&unconn_req, &mut ns), 0);
    assert_eq!(handle_getsockopt(&peercred_req, &mut resp), libc::ENOTCONN);

    let mut state = lock_net_state();
    state.sockets.remove(&srv);
    state.sockets.remove(&cli);
    state.bindings.remove(&addr);
}

#[test]
fn unix_dgram_inode_binding_retires_directly_and_controls_discard_on_recvfrom() {
    let mut state = NetState::new();
    let token = 0x30_0001;
    let sender = 0x30_0002;
    let path = UnixAddress::Filesystem("/tmp/test-dgram.sock".to_string());
    let mut s = dgram_socket(Credentials::default());
    s.bound_path = Some(path.clone());
    s.bound_inode = Some((42, 99));
    s.local_path = Some(path.clone());
    state.sockets.insert(token, s);
    state.sockets.insert(sender, dgram_socket(Credentials::default()));
    state.bindings.insert(path.clone(), token);
    state.inode_bindings.insert((42, 99), token);

    // Queue a message with ancillary bytes and receive via RECVFROM:
    // discarded ancillary sits after payload_len and is reflected in reserved0.
    let mut resp = NetdIpcResponse::default();
    let ctrl = vec![16_u8, 0, 0, 0, 0, 0, 0, 0, 1, 0, 0, 0, 1, 0, 0, 0];
    let send_req = send_dgram_req(sender, 1, b"abc", &ctrl, &[]);
    if let UnixSocketState::Dgram(d) = &mut state.sockets.get_mut(&sender).unwrap().state {
        d.peer = Some(token);
    }
    assert!(dgram_send_in_state(&mut state, &send_req, &mut resp, None).is_ok());

    let mut recv_resp = NetdIpcResponse::default();
    let recv_req = recv_dgram_req(SYSCALL_OFFLOAD_OP_LINUX_RECVFROM, token, 16, 0);
    assert_eq!(dgram_recv_in_state(&mut state, &recv_req, &mut recv_resp), 0);
    assert_eq!(recv_resp.reserved0 as usize, ctrl.len());
    let msg_len = recv_resp.payload_len as usize - recv_resp.reserved0 as usize;
    let decoded = netd_message::decode(&recv_resp.payload[..msg_len]).unwrap();
    assert_eq!(decoded.data, b"abc");

    let _ = retire_unix_socket(&mut state, token);
    assert!(!state.inode_bindings.contains_key(&(42, 99)));
    assert!(!state.bindings.contains_key(&path));
}

#[test]
fn permanent_unix_accept_errors_retire_socket_instead_of_spinning() {
    let mut state = NetState::new();
    let listener = 0x40_0001;
    let broken = 0x40_0002;
    let mut l = UnixSocket {
        kind: UnixSocketKind::Stream,
        owner: Credentials::default(),
        refs: 1,
        options: SocketOptions::default(),
        bound_path: None,
        bound_inode: None,
        local_path: None,
        peer_path: None,
        state: UnixSocketState::Listening {
            backlog: 4,
            pending: VecDeque::new(),
        },
    };
    if let UnixSocketState::Listening { pending, .. } = &mut l.state {
        pending.push_back(broken);
    }
    state.sockets.insert(listener, l);
    state.sockets.insert(
        broken,
        UnixSocket {
            kind: UnixSocketKind::Stream,
            owner: Credentials::default(),
            refs: 1,
            options: SocketOptions::default(),
            bound_path: None,
            bound_inode: None,
            local_path: None,
            peer_path: None,
            state: UnixSocketState::Connected(ConnectedState {
                incoming: VecDeque::new(),
                incoming_bytes: 0,
                incoming_control_bytes: 0,
                pending_listener: Some(listener),
                channel_id: 0,
                peer: 0,
                peer_closed: false,
                peer_read_closed: false,
                peer_write_closed: false,
                peer_credentials: Credentials::default(),
                recv_drain_handoff_armed: false,
                recv_closed: false,
                send_closed: false,
            }),
        },
    );

    // Popping a broken socket with channel_id == 0 retires it rather than re-queueing.
    let accepted = if let UnixSocketState::Listening { pending, .. } =
        &mut state.sockets.get_mut(&listener).unwrap().state
    {
        pending.pop_front().unwrap()
    } else {
        panic!()
    };
    let (retired, _, tokens) = retire_unix_socket(&mut state, accepted);
    assert_eq!(retired.len(), 1);
    assert_eq!(tokens, vec![broken]);
    assert!(!state.sockets.contains_key(&broken));
    let UnixSocketState::Listening { pending, .. } = &state.sockets[&listener].state else {
        panic!()
    };
    assert!(pending.is_empty());
}
