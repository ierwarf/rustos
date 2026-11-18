//! AF_UNIX `SOCK_SEQPACKET`: boundaries, truncation, ancillary custody, EOF,
//! queue bounds, type admission and listener/connector type agreement.

use super::*;

fn seq_socket(peer: u64) -> UnixSocket {
    let mut socket = super::local_socket_poll_tests::connected_socket(peer);
    socket.kind = UnixSocketKind::SeqPacket;
    socket
}

fn seq_pair(state: &mut NetState) {
    state.sockets.insert(1, seq_socket(2));
    state.sockets.insert(2, seq_socket(1));
}

fn connected_of(state: &NetState, token: u64) -> &ConnectedState {
    let UnixSocketState::Connected(connected) = &state.sockets[&token].state else {
        panic!("connected");
    };
    connected
}

fn send_request(token: u64) -> NetdIpcRequest {
    NetdIpcRequest {
        socket_token: token,
        ..NetdIpcRequest::default()
    }
}

fn recvmsg_request(token: u64, buffer: usize, flags: u64) -> NetdIpcRequest {
    NetdIpcRequest {
        op: SYSCALL_OFFLOAD_OP_LINUX_RECVMSG,
        payload_len: buffer as u32,
        arg2: flags,
        socket_token: token,
        ..NetdIpcRequest::default()
    }
}

/// One `SCM_RIGHTS` cmsg whose ticket bytes are all `marker` (opaque custody
/// witness, as in the stream lifecycle tests).
fn rights(marker: u8) -> Vec<u8> {
    let len = size_of::<linux_abi::LinuxCmsghdr>()
        + rustos_user_abi::syscall::IPC_TRANSFER_TICKET_WIRE_BYTES;
    let mut control = vec![marker; len];
    write_plain_old_data(
        &mut control[..size_of::<linux_abi::LinuxCmsghdr>()],
        &linux_abi::LinuxCmsghdr {
            cmsg_len: len as u64,
            cmsg_level: linux_abi::SOL_SOCKET as u32,
            cmsg_type: linux_abi::SCM_RIGHTS as u32,
        },
    );
    control
}

struct Received {
    value: u64,
    discarded_tail: u32,
    data_len: usize,
    control_len: usize,
    msg_flags: u32,
    record_present: bool,
    data: Vec<u8>,
    control: Vec<u8>,
}

fn recvmsg(state: &mut NetState, token: u64, buffer: usize, flags: u64) -> Result<Received, i32> {
    let mut response = NetdIpcResponse::default();
    match recvmsg_in_state(state, &recvmsg_request(token, buffer, flags), &mut response) {
        0 => {}
        errno => return Err(errno),
    }
    let data_len = u32::from_ne_bytes(response.payload[0..4].try_into().unwrap()) as usize;
    let control_len = u32::from_ne_bytes(response.payload[4..8].try_into().unwrap()) as usize;
    let msg_flags = u32::from_ne_bytes(response.payload[8..12].try_into().unwrap());
    let data_start = NETD_RECVMSG_PAYLOAD_HEADER_SIZE;
    Ok(Received {
        value: response.value,
        discarded_tail: response.discarded_tail,
        data_len,
        control_len,
        msg_flags,
        record_present: response.reserved1 & NETD_IPC_RESPONSE_FLAG_RECORD_PRESENT != 0,
        data: response.payload[data_start..data_start + data_len].to_vec(),
        control: response.payload[data_start + data_len..data_start + data_len + control_len]
            .to_vec(),
    })
}

#[test]
fn messages_keep_their_boundaries_and_empty_message_is_not_eof() {
    let mut state = NetState::new();
    seq_pair(&mut state);
    let tx = send_request(1);
    assert_eq!(
        send_socket_message_in_state(&mut state, &tx, b"ab", &[]),
        Ok(2)
    );
    assert_eq!(
        send_socket_message_in_state(&mut state, &tx, b"cde", &[]),
        Ok(3)
    );
    assert_eq!(
        send_socket_message_in_state(&mut state, &tx, b"", &[]),
        Ok(0)
    );
    assert_eq!(connected_of(&state, 2).incoming.len(), 3, "never coalesced");
    // A queued zero-length message makes the socket readable.
    assert_ne!(
        unix_socket_revents(&state, 2, linux_abi::POLLIN as u32).unwrap()
            & linux_abi::POLLIN as u32,
        0
    );
    // A large buffer still returns exactly one message.
    let first = recvmsg(&mut state, 2, 4096, 0).unwrap();
    assert_eq!((first.value, first.data.as_slice()), (2, &b"ab"[..]));
    let second = recvmsg(&mut state, 2, 4096, 0).unwrap();
    assert_eq!((second.value, second.data.as_slice()), (3, &b"cde"[..]));
    let empty = recvmsg(&mut state, 2, 4096, 0).unwrap();
    assert_eq!((empty.value, empty.data_len, empty.msg_flags), (0, 0, 0));
    // Drained and the peer still open: would block, not EOF.
    assert_eq!(recvmsg(&mut state, 2, 4096, 0).err(), Some(libc::EAGAIN));
    // EOF only after the peer shut down and the queue is drained.
    if let UnixSocketState::Connected(connected) = &mut state.sockets.get_mut(&2).unwrap().state {
        connected.peer_write_closed = true;
    }
    let eof = recvmsg(&mut state, 2, 4096, 0).unwrap();
    assert_eq!((eof.value, eof.data_len), (0, 0));
    assert_eq!(state.unix_queued_bytes, 0);
}

#[test]
fn queued_data_is_delivered_before_eof_after_peer_close() {
    let mut state = NetState::new();
    seq_pair(&mut state);
    let tx = send_request(1);
    assert_eq!(
        send_socket_message_in_state(&mut state, &tx, b"last", &[]),
        Ok(4)
    );
    if let UnixSocketState::Connected(connected) = &mut state.sockets.get_mut(&2).unwrap().state {
        connected.peer_closed = true;
    }
    assert_eq!(recvmsg(&mut state, 2, 64, 0).unwrap().data, b"last");
    assert_eq!(recvmsg(&mut state, 2, 64, 0).unwrap().value, 0);
}

#[test]
fn short_buffer_discards_the_tail_and_reports_msg_trunc() {
    let mut state = NetState::new();
    seq_pair(&mut state);
    let tx = send_request(1);
    send_socket_message_in_state(&mut state, &tx, b"abcde", &[]).unwrap();
    send_socket_message_in_state(&mut state, &tx, b"next", &[]).unwrap();
    let truncated = recvmsg(&mut state, 2, 2, 0).unwrap();
    assert_eq!(truncated.data, b"ab");
    assert_eq!(truncated.value, 2, "copied length without MSG_TRUNC");
    assert_eq!(
        truncated.discarded_tail, 3,
        "kernel advances its position by the tail"
    );
    assert_ne!(truncated.msg_flags & libc::MSG_TRUNC as u32, 0);
    // The tail is gone: the next receive is the next message.
    assert_eq!(recvmsg(&mut state, 2, 64, 0).unwrap().data, b"next");
    assert_eq!(state.unix_queued_bytes, 0);
    assert_eq!(connected_of(&state, 2).incoming_bytes, 0);

    // With MSG_TRUNC in the receive flags the return is the real length and the
    // kernel position advances by that alone.
    send_socket_message_in_state(&mut state, &tx, b"abcde", &[]).unwrap();
    let real = recvmsg(&mut state, 2, 2, libc::MSG_TRUNC as u64).unwrap();
    assert_eq!(
        (real.value, real.discarded_tail, real.data.as_slice()),
        (5, 0, &b"ab"[..])
    );
    assert_ne!(real.msg_flags & libc::MSG_TRUNC as u32, 0);
}

#[test]
fn zero_length_buffer_still_consumes_exactly_one_message() {
    let mut state = NetState::new();
    seq_pair(&mut state);
    let tx = send_request(1);
    send_socket_message_in_state(&mut state, &tx, b"xyz", &[]).unwrap();
    send_socket_message_in_state(&mut state, &tx, b"w", &[]).unwrap();
    let dropped = recvmsg(&mut state, 2, 0, 0).unwrap();
    assert_eq!((dropped.data_len, dropped.discarded_tail), (0, 3));
    assert_eq!(recvmsg(&mut state, 2, 8, 0).unwrap().data, b"w");
}

#[test]
fn recv_accounting_matches_linux_msg_trunc_rules() {
    let truncated = SocketReceive {
        read: 2,
        extra: 3,
        truncated: true,
        ..SocketReceive::default()
    };
    assert_eq!(unix_seqpacket::receive_accounting(0, &truncated), (2, 3));
    assert_eq!(
        unix_seqpacket::receive_accounting(libc::MSG_TRUNC as u64, &truncated),
        (5, 0)
    );
    let whole = SocketReceive {
        read: 4,
        ..SocketReceive::default()
    };
    assert_eq!(
        unix_seqpacket::receive_accounting(libc::MSG_TRUNC as u64, &whole),
        (4, 0)
    );
}

#[test]
fn scm_rights_attach_to_their_own_message_only() {
    let mut state = NetState::new();
    seq_pair(&mut state);
    let tx = send_request(1);
    let (a, b) = (rights(0xa1), rights(0xb2));
    send_socket_message_in_state(&mut state, &tx, b"m1", &a).unwrap();
    send_socket_message_in_state(&mut state, &tx, b"m2", &[]).unwrap();
    send_socket_message_in_state(&mut state, &tx, b"m3", &b).unwrap();
    let first = recvmsg(&mut state, 2, 64, 0).unwrap();
    assert_eq!((first.data.as_slice(), first.control), (&b"m1"[..], a));
    let second = recvmsg(&mut state, 2, 64, 0).unwrap();
    assert_eq!(
        (second.data.as_slice(), second.control_len),
        (&b"m2"[..], 0)
    );
    let third = recvmsg(&mut state, 2, 64, 0).unwrap();
    assert_eq!((third.data.as_slice(), third.control), (&b"m3"[..], b));
    assert_eq!(connected_of(&state, 2).incoming_control_bytes, 0);
    assert_eq!(state.unix_queued_bytes, 0);
}

#[test]
fn truncated_message_still_delivers_its_descriptors() {
    let mut state = NetState::new();
    seq_pair(&mut state);
    let tx = send_request(1);
    let control = rights(0xc3);
    send_socket_message_in_state(&mut state, &tx, b"payload", &control).unwrap();
    let received = recvmsg(&mut state, 2, 3, 0).unwrap();
    assert_eq!(received.data, b"pay");
    assert_eq!(received.control, control);
    assert_ne!(received.msg_flags & libc::MSG_TRUNC as u32, 0);
}

#[test]
fn plain_recv_returns_one_message_and_releases_discarded_control() {
    let mut state = NetState::new();
    seq_pair(&mut state);
    let tx = send_request(1);
    let control = rights(0x5e);
    send_socket_message_in_state(&mut state, &tx, b"one", &control).unwrap();
    send_socket_message_in_state(&mut state, &tx, b"two", &[]).unwrap();
    let request = NetdIpcRequest {
        op: SYSCALL_OFFLOAD_OP_LINUX_RECVFROM,
        arg2: 64,
        socket_token: 2,
        ..NetdIpcRequest::default()
    };
    let mut buffer = [0_u8; 64];
    let received = recv_socket_bytes_in_state(&mut state, &request, &mut buffer, false).unwrap();
    assert_eq!(&buffer[..received.read], b"one");
    // recv() has no cmsg buffer: custody moves to the kernel release trailer.
    assert_eq!(received.discarded, control);
}

#[test]
fn oversized_message_is_emsgsize_and_full_queue_is_atomic_eagain() {
    let mut state = NetState::new();
    seq_pair(&mut state);
    let tx = send_request(1);
    let huge = vec![0_u8; SOCKET_BUFFER_CAPACITY + 1];
    assert_eq!(
        send_socket_message_in_state(&mut state, &tx, &huge, &[]),
        Err(libc::EMSGSIZE)
    );
    assert_eq!(
        unix_seqpacket::validate_send(&[], &rights(1)),
        Ok(())
    );
    // 10 bytes of room: a 16-byte message is refused whole, not split.
    if let UnixSocketState::Connected(connected) = &mut state.sockets.get_mut(&2).unwrap().state {
        connected.incoming_bytes = SOCKET_BUFFER_CAPACITY - 10;
    }
    assert_eq!(
        send_socket_message_in_state(&mut state, &tx, &[1; 16], &[]),
        Err(libc::EAGAIN)
    );
    assert_eq!(connected_of(&state, 2).incoming.len(), 0);
    assert_eq!(
        send_socket_message_in_state(&mut state, &tx, &[1; 10], &[]),
        Ok(10)
    );
}

#[test]
fn empty_messages_are_bounded_per_socket() {
    let mut state = NetState::new();
    seq_pair(&mut state);
    let tx = send_request(1);
    for _ in 0..SEQPACKET_MAX_QUEUED_MESSAGES {
        assert_eq!(
            send_socket_message_in_state(&mut state, &tx, b"", &[]),
            Ok(0)
        );
    }
    assert_eq!(
        send_socket_message_in_state(&mut state, &tx, b"", &[]),
        Err(libc::EAGAIN)
    );
    assert_eq!(
        unix_socket_revents(&state, 1, linux_abi::POLLOUT as u32).unwrap()
            & linux_abi::POLLOUT as u32,
        0,
        "poll must agree with send"
    );
    recvmsg(&mut state, 2, 8, 0).unwrap();
    assert_eq!(
        send_socket_message_in_state(&mut state, &tx, b"", &[]),
        Ok(0)
    );
}

#[test]
fn peek_reports_the_front_message_without_dequeuing() {
    let mut state = NetState::new();
    seq_pair(&mut state);
    let tx = send_request(1);
    send_socket_message_in_state(&mut state, &tx, b"hello", &[]).unwrap();
    send_socket_message_in_state(&mut state, &tx, b"world", &[]).unwrap();
    let peeked = recvmsg(
        &mut state,
        2,
        3,
        libc::MSG_PEEK as u64 | libc::MSG_TRUNC as u64,
    )
    .unwrap();
    assert_eq!((peeked.value, peeked.data.as_slice()), (5, &b"hel"[..]));
    assert_ne!(peeked.msg_flags & libc::MSG_TRUNC as u32, 0);
    assert_eq!(connected_of(&state, 2).incoming.len(), 2);
    assert_eq!(recvmsg(&mut state, 2, 64, 0).unwrap().data, b"hello");
}

#[test]
fn stream_sockets_keep_byte_stream_semantics() {
    let mut state = NetState::new();
    state
        .sockets
        .insert(1, super::local_socket_poll_tests::connected_socket(2));
    state
        .sockets
        .insert(2, super::local_socket_poll_tests::connected_socket(1));
    let tx = send_request(1);
    send_socket_message_in_state(&mut state, &tx, b"ab", &[]).unwrap();
    send_socket_message_in_state(&mut state, &tx, b"cd", &[]).unwrap();
    // Coalesced, and a zero-byte send is a no-op rather than a message.
    assert_eq!(
        send_socket_message_in_state(&mut state, &tx, b"", &[]),
        Ok(0)
    );
    assert_eq!(connected_of(&state, 2).incoming.len(), 1);
    let received = recvmsg(&mut state, 2, 64, 0).unwrap();
    assert_eq!(received.data, b"abcd");
    assert_eq!(received.msg_flags, 0);
}

#[test]
fn socket_type_admission_distinguishes_unsupported_type_from_family() {
    let unix = linux_abi::AF_UNIX;
    let flags = linux_abi::SOCK_NONBLOCK | linux_abi::SOCK_CLOEXEC;
    let kind = |domain, ty, proto| local_socket_kind(domain, ty, proto);
    assert_eq!(
        kind(unix, libc::SOCK_STREAM as u64, 0),
        Ok(UnixSocketKind::Stream)
    );
    assert_eq!(
        kind(unix, libc::SOCK_SEQPACKET as u64 | flags, 0),
        Ok(UnixSocketKind::SeqPacket)
    );
    assert_eq!(
        kind(unix, libc::SOCK_DGRAM as u64, 0),
        Err(libc::ESOCKTNOSUPPORT)
    );
    assert_eq!(
        kind(unix, libc::SOCK_RAW as u64, 0),
        Err(libc::ESOCKTNOSUPPORT)
    );
    assert_eq!(
        kind(unix, libc::SOCK_SEQPACKET as u64, 1),
        Err(libc::EPROTONOSUPPORT)
    );
    assert_eq!(
        kind(unix, libc::SOCK_SEQPACKET as u64 | 1 << 30, 0),
        Err(libc::EINVAL)
    );
    assert_eq!(
        kind(libc::AF_INET6 as u64, libc::SOCK_STREAM as u64, 0),
        Err(libc::EAFNOSUPPORT)
    );
    let netlink = libc::AF_NETLINK as u64;
    assert_eq!(
        kind(netlink, libc::SOCK_RAW as u64 | flags, 0),
        Ok(UnixSocketKind::Netlink)
    );
    assert_eq!(
        kind(netlink, libc::SOCK_DGRAM as u64, 0),
        Ok(UnixSocketKind::Netlink)
    );
    assert_eq!(
        kind(netlink, libc::SOCK_STREAM as u64, 0),
        Err(libc::ESOCKTNOSUPPORT)
    );
    // The netd admission and the kernel broker shape predicate agree.
    for ty in [
        libc::SOCK_STREAM,
        libc::SOCK_SEQPACKET,
        libc::SOCK_DGRAM,
        libc::SOCK_RAW,
    ] {
        for domain in [unix, netlink] {
            let admitted = kind(domain, ty as u64, 0).is_ok();
            let shape =
                rustos_user_abi::syscall::net_broker_unix_socket_shape_valid(domain, ty as u64, 0)
                    || rustos_user_abi::syscall::net_broker_netlink_socket_shape_valid(
                        domain, ty as u64, 0,
                    );
            assert_eq!(admitted, shape, "domain {domain} type {ty}");
        }
    }
}

fn insert_global(token: u64, socket: UnixSocket) {
    lock_net_state().sockets.insert(token, socket);
}

fn remove_global(tokens: &[u64]) {
    let mut state = lock_net_state();
    for token in tokens {
        state.sockets.remove(token);
        state.bindings.retain(|_, bound| bound != token);
    }
}

fn idle_socket(kind: UnixSocketKind) -> UnixSocket {
    UnixSocket {
        kind,
        owner: Credentials::default(),
        refs: 1,
        options: SocketOptions::default(),
        bound_path: None,
        local_path: None,
        peer_path: None,
        state: UnixSocketState::Idle,
    }
}

fn sockaddr_un_request(token: u64, name: &[u8]) -> NetdIpcRequest {
    let mut request = NetdIpcRequest {
        socket_token: token,
        ..NetdIpcRequest::default()
    };
    request.payload[..2].copy_from_slice(&(linux_abi::AF_UNIX as u16).to_ne_bytes());
    request.payload[2..2 + name.len()].copy_from_slice(name);
    request.payload_len = (2 + name.len()) as u32;
    request
}

#[test]
fn connect_with_mismatched_socket_type_is_eprototype() {
    let (listener, stream_client, seq_client) = (0x5e0_0001, 0x5e0_0002, 0x5e0_0003);
    let address = UnixAddress::Abstract(b"\0seqpacket-type-check".to_vec());
    let mut listening = idle_socket(UnixSocketKind::SeqPacket);
    listening.state = UnixSocketState::Listening {
        backlog: 4,
        pending: VecDeque::new(),
    };
    listening.bound_path = Some(address.clone());
    listening.local_path = Some(address.clone());
    insert_global(listener, listening);
    insert_global(stream_client, idle_socket(UnixSocketKind::Stream));
    insert_global(seq_client, idle_socket(UnixSocketKind::SeqPacket));
    lock_net_state().bindings.insert(address, listener);

    let mut namespace = path_namespace::PrivateNamespace;
    let request = sockaddr_un_request(stream_client, b"\0seqpacket-type-check");
    assert_eq!(
        handle_connect_with(&request, &mut namespace),
        libc::EPROTOTYPE
    );
    // A matching client gets past the type check (the broker syscall after it
    // is not available on the host, so stop before it).
    let accepted_type_ok = {
        let state = lock_net_state();
        state.sockets[&seq_client].kind == state.sockets[&listener].kind
    };
    assert!(accepted_type_ok);
    remove_global(&[listener, stream_client, seq_client]);
}

#[test]
fn so_type_reports_the_socket_type() {
    let (seq, stream) = (0x5e0_0011, 0x5e0_0012);
    insert_global(seq, idle_socket(UnixSocketKind::SeqPacket));
    insert_global(stream, idle_socket(UnixSocketKind::Stream));
    let query = |token| {
        let request = NetdIpcRequest {
            socket_token: token,
            arg1: linux_abi::SOL_SOCKET,
            arg2: linux_abi::SO_TYPE,
            ..NetdIpcRequest::default()
        };
        let mut response = NetdIpcResponse::default();
        assert_eq!(handle_getsockopt(&request, &mut response), 0);
        i32::from_ne_bytes(response.payload[..4].try_into().unwrap())
    };
    assert_eq!(query(seq), libc::SOCK_SEQPACKET);
    assert_eq!(query(stream), libc::SOCK_STREAM);
    remove_global(&[seq, stream]);
}

#[test]
fn receive_buffer_bounds_seqpacket_admission_and_writability() {
    let mut state = NetState::new();
    seq_pair(&mut state);
    state.sockets.get_mut(&2).unwrap().options.recv_buffer = 4096;
    let tx = send_request(1);
    assert_eq!(send_socket_message_in_state(&mut state, &tx, &[1; 4096], &[]), Ok(4096));
    assert_eq!(send_socket_message_in_state(&mut state, &tx, &[2], &[]), Err(libc::EAGAIN));
    assert_eq!(unix_socket_revents(&state, 1, linux_abi::POLLOUT as u32).unwrap() & linux_abi::POLLOUT as u32, 0);
    recvmsg(&mut state, 2, 1, 0).unwrap();
    assert_ne!(unix_socket_revents(&state, 1, linux_abi::POLLOUT as u32).unwrap() & linux_abi::POLLOUT as u32, 0);
}

#[test]
fn ancillary_only_record_is_distinct_from_eof_and_keeps_its_control() {
    let mut state = NetState::new();
    seq_pair(&mut state);
    let control = rights(0x55);
    assert_eq!(send_socket_message_in_state(&mut state, &send_request(1), b"", &control), Ok(0));
    let mut reply = NetdIpcResponse::default();
    assert_eq!(recvmsg_in_state(&mut state, &recvmsg_request(2, 0, 0), &mut reply), 0);
    assert_ne!(reply.reserved1 & NETD_IPC_RESPONSE_FLAG_RECORD_PRESENT, 0);
    assert_eq!(u32::from_ne_bytes(reply.payload[4..8].try_into().unwrap()) as usize, control.len());
    assert_eq!(&reply.payload[NETD_RECVMSG_PAYLOAD_HEADER_SIZE..reply.payload_len as usize], &control);
    if let UnixSocketState::Connected(conn) = &mut state.sockets.get_mut(&2).unwrap().state { conn.peer_closed = true; }
    let mut eof = NetdIpcResponse::default();
    assert_eq!(recvmsg_in_state(&mut state, &recvmsg_request(2, 0, 0), &mut eof), 0);
    assert_eq!(eof.reserved1 & NETD_IPC_RESPONSE_FLAG_RECORD_PRESENT, 0);
}

#[test]
fn truncated_then_empty_records_preserve_rights_and_eof_positions() {
    let mut state = NetState::new();
    seq_pair(&mut state);
    let tx = send_request(1);
    let (first_rights, empty_rights, last_rights) = (rights(11), rights(12), rights(13));
    send_socket_message_in_state(&mut state, &tx, b"abcde", &first_rights).unwrap();
    send_socket_message_in_state(&mut state, &tx, b"", &empty_rights).unwrap();
    send_socket_message_in_state(&mut state, &tx, b"z", &last_rights).unwrap();
    if let UnixSocketState::Connected(conn) = &mut state.sockets.get_mut(&2).unwrap().state { conn.peer_closed = true; }
    let first = recvmsg(&mut state, 2, 2, 0).unwrap();
    assert!(first.record_present);
    assert_eq!((first.value, first.discarded_tail, first.control), (2, 3, first_rights));
    let empty = recvmsg(&mut state, 2, 2, 0).unwrap();
    assert!(empty.record_present);
    assert_eq!((empty.value, empty.control), (0, empty_rights));
    let last = recvmsg(&mut state, 2, 2, 0).unwrap();
    assert_eq!((last.value, last.control), (1, last_rights));
    let eof = recvmsg(&mut state, 2, 2, 0).unwrap();
    assert!(!eof.record_present);
    assert_eq!((eof.value, eof.control_len), (0, 0));
    assert_eq!(state.unix_queued_bytes, 0);
}
