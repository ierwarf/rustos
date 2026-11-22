use super::*;

fn connected_socket(peer: u64, pending_listener: Option<u64>) -> UnixSocket {
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
            pending_listener,
            channel_id: 1,
            peer,
            peer_closed: false,
            peer_read_closed: false,
            peer_write_closed: false,
            peer_credentials: Credentials {
                pid: 7,
                uid: 8,
                gid: 9,
            },
            recv_drain_handoff_armed: false,
            recv_closed: false,
            send_closed: false,
        }),
    }
}

fn connected(state: &NetState, token: u64) -> &ConnectedState {
    let UnixSocketState::Connected(connected) = &state.sockets[&token].state else {
        panic!("expected connected socket");
    };
    connected
}

fn connected_mut(state: &mut NetState, token: u64) -> &mut ConnectedState {
    let UnixSocketState::Connected(connected) = &mut state.sockets.get_mut(&token).unwrap().state
    else {
        panic!("expected connected socket");
    };
    connected
}

// Opaque ticket bytes are custody witnesses here, not live kernel authorities.
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

fn request(token: u64) -> NetdIpcRequest {
    NetdIpcRequest {
        socket_token: token,
        ..NetdIpcRequest::default()
    }
}

const QUEUED_SENDER: Credentials = Credentials {
    pid: 41,
    uid: 42,
    gid: 43,
};

fn enqueue(state: &mut NetState, token: u64, bytes: &[u8], control: &[u8]) {
    state.unix_queued_bytes+=bytes.len()+cmsg_align(control.len());
    let connected = connected_mut(state, token);
    connected.incoming_bytes += bytes.len();
    connected.incoming_control_bytes += cmsg_align(control.len());
    connected.incoming.push_back(UnixStreamSegment {
        bytes: bytes.iter().copied().collect(),
        control: control.to_vec(),
        sender: QUEUED_SENDER,
        sender_path: None,
    });
}

fn recv_request(token: u64) -> NetdIpcRequest {
    NetdIpcRequest {
        op: SYSCALL_OFFLOAD_OP_LINUX_RECVMSG,
        payload_len: NetdIpcResponse::default().payload.len() as u32,
        ..request(token)
    }
}

#[test]
fn empty_stream_send_checks_both_endpoints_before_returning_zero() {
    let mut state = NetState::new();
    assert_eq!(
        send_socket_message_in_state(&mut state, &request(99), b"", &[]),
        Err(libc::EBADF)
    );

    state.sockets.insert(1, connected_socket(2, None));
    assert_eq!(
        send_socket_message_in_state(&mut state, &request(1), b"", &[]),
        Err(libc::EPIPE),
        "missing peer cannot silently accept an empty send"
    );

    state.sockets.insert(2, connected_socket(1, None));
    assert_eq!(
        send_socket_message_in_state(&mut state, &request(1), b"", &[]),
        Ok(0)
    );
    assert_eq!(state.unix_queued_bytes, 0);
    assert!(connected(&state, 2).incoming.is_empty());

    connected_mut(&mut state, 2).recv_closed = true;
    assert_eq!(
        send_socket_message_in_state(&mut state, &request(1), b"", &[]),
        Err(libc::EPIPE)
    );
    connected_mut(&mut state, 2).recv_closed = false;
    connected_mut(&mut state, 1).send_closed = true;
    assert_eq!(
        send_socket_message_in_state(&mut state, &request(1), b"", &[]),
        Err(libc::EPIPE)
    );
    assert_eq!(state.unix_queued_bytes, 0);
}

#[test]
fn recvmsg_passcred_toggle_cannot_split_sizing_dequeue_and_encoding() {
    use std::sync::{mpsc, Arc, TryLockError};
    for initial_passcred in [false, true] {
        let mut state = NetState::new();
        state.sockets.insert(1, connected_socket(2, None));
        state.sockets.get_mut(&1).unwrap().options.passcred = initial_passcred;
        let bytes = vec![0x5a; 32768];
        let control = rights(0x71);
        enqueue(&mut state, 1, &bytes, &control);
        let shared = Arc::new(Mutex::new(state));
        let mut guard = shared.lock().unwrap();
        let toggler_state = Arc::clone(&shared);
        let (attempted, attempt) = mpsc::channel();
        let toggler = thread::spawn(move || {
            assert!(matches!(
                toggler_state.try_lock(),
                Err(TryLockError::WouldBlock)
            ));
            attempted.send(()).unwrap();
            toggler_state
                .lock()
                .unwrap()
                .sockets
                .get_mut(&1)
                .unwrap()
                .options
                .passcred = !initial_passcred;
        });
        attempt.recv().unwrap();
        let mut response = NetdIpcResponse::default();
        assert_eq!(
            recvmsg_in_state(&mut guard, &recv_request(1), &mut response),
            0
        );
        let expected_control = append_recvmsg_credentials(
            initial_passcred.then_some(QUEUED_SENDER),
            control,
        );
        let read = response.value as usize;
        assert_eq!(response.payload_len as usize, response.payload.len());
        assert_eq!(
            &response.payload
                [NETD_RECVMSG_PAYLOAD_HEADER_SIZE..NETD_RECVMSG_PAYLOAD_HEADER_SIZE + read],
            &bytes[..read]
        );
        assert_eq!(
            &response.payload[NETD_RECVMSG_PAYLOAD_HEADER_SIZE + read..],
            expected_control.as_slice()
        );
        assert_eq!(connected(&guard, 1).incoming_bytes, bytes.len() - read);
        assert_eq!(connected(&guard, 1).incoming_control_bytes, 0);
        drop(guard);
        toggler.join().unwrap();
        let mut guard = shared.lock().unwrap();
        let mut tail = NetdIpcResponse::default();
        assert_eq!(recvmsg_in_state(&mut guard, &recv_request(1), &mut tail), 0);
        assert_eq!(read + tail.value as usize, bytes.len());
        assert_eq!(connected(&guard, 1).incoming_bytes, 0);
    }
}

fn passcred_credentials(response: &NetdIpcResponse) -> Option<linux_abi::LinuxUCred> {
    let read = response.payload[0..4].try_into().map(u32::from_ne_bytes).unwrap() as usize;
    let control_len = response.payload[4..8].try_into().map(u32::from_ne_bytes).unwrap() as usize;
    let start = NETD_RECVMSG_PAYLOAD_HEADER_SIZE + read;
    let control = &response.payload[start..start + control_len];
    let header = read_cmsghdr(control).ok()?;
    if header.cmsg_type != linux_abi::SCM_CREDENTIALS as u32 {
        return None;
    }
    let data = &control[size_of::<linux_abi::LinuxCmsghdr>()..];
    Some(linux_abi::LinuxUCred {
        pid: i32::from_ne_bytes(data[0..4].try_into().unwrap()),
        uid: u32::from_ne_bytes(data[4..8].try_into().unwrap()),
        gid: u32::from_ne_bytes(data[8..12].try_into().unwrap()),
    })
}

/// A socketpair's ends both carry the creator's identity (`SO_PEERCRED`), but
/// `SCM_CREDENTIALS` must name whichever process performed the send. Chrome's
/// zygote host learns a forked child's pid from the child's reply on a
/// socketpair the browser created; reporting the creator made it see itself.
#[test]
fn scm_credentials_name_the_sender_not_the_connection_creator() {
    let mut state = NetState::new();
    state.sockets.insert(1, connected_socket(2, None));
    state.sockets.insert(2, connected_socket(1, None));
    state.sockets.get_mut(&1).unwrap().options.passcred = true;
    let child = NetdIpcRequest {
        pid: 55,
        euid: 1000,
        egid: 1001,
        ..request(2)
    };
    assert_eq!(send_socket_message_in_state(&mut state, &child, b"x", &[]), Ok(1));
    let mut response = NetdIpcResponse::default();
    assert_eq!(recvmsg_in_state(&mut state, &recv_request(1), &mut response), 0);
    let ucred = passcred_credentials(&response).expect("credentials cmsg");
    assert_eq!((ucred.pid, ucred.uid, ucred.gid), (55, 1000, 1001));
    assert_eq!(connected(&state, 1).peer_credentials.pid, 7, "SO_PEERCRED stays the creator");
}

#[test]
fn stream_receive_never_merges_two_senders_credentials() {
    let mut state = NetState::new();
    state.sockets.insert(1, connected_socket(2, None));
    state.sockets.insert(2, connected_socket(1, None));
    state.sockets.get_mut(&1).unwrap().options.passcred = true;
    for pid in [55, 56] {
        let sender = NetdIpcRequest { pid, ..request(2) };
        assert_eq!(send_socket_message_in_state(&mut state, &sender, b"x", &[]), Ok(1));
    }
    for expected in [55, 56] {
        let mut response = NetdIpcResponse::default();
        assert_eq!(recvmsg_in_state(&mut state, &recv_request(1), &mut response), 0);
        assert_eq!(response.value, 1, "one sender per receive");
        assert_eq!(passcred_credentials(&response).unwrap().pid, expected);
    }
}

#[test]
fn stream_receive_starts_exactly_at_the_segment_carrying_rights() {
    // The kernel binds a rights ticket to the byte offset where its send began
    // and checks it against the receive's start, so plain bytes queued ahead of
    // a rights-bearing segment must come back as their own short read.
    let mut state = NetState::new();
    state.sockets.insert(1, connected_socket(2, None));
    enqueue(&mut state, 1, b"plain", &[]);
    let control = rights(0x72);
    enqueue(&mut state, 1, b"fdmsg", &control);

    let mut first = NetdIpcResponse::default();
    assert_eq!(recvmsg_in_state(&mut state, &recv_request(1), &mut first), 0);
    assert_eq!(first.value, 5, "plain prefix is a short read");
    assert_eq!(connected(&state, 1).incoming.len(), 1);
    assert!(!connected(&state, 1).incoming[0].control.is_empty());

    let mut second = NetdIpcResponse::default();
    assert_eq!(recvmsg_in_state(&mut state, &recv_request(1), &mut second), 0);
    assert_eq!(second.value, 5);
    assert_eq!(connected(&state, 1).incoming_control_bytes, 0);
    let read = second.value as usize;
    let at = NETD_RECVMSG_PAYLOAD_HEADER_SIZE + read;
    assert_eq!(
        &second.payload[at..at + control.len()],
        control.as_slice(),
        "rights arrive with the receive that starts at their segment"
    );
}

#[test]
fn recvmsg_preflight_error_preserves_all_bytes_and_ancillary_custody() {
    let mut state = NetState::new();
    state.sockets.insert(1, connected_socket(2, None));
    let oversized = vec![0x41; NetdIpcResponse::default().payload.len()];
    enqueue(&mut state, 1, b"retained", &oversized);
    assert_eq!(
        recvmsg_in_state(
            &mut state,
            &recv_request(1),
            &mut NetdIpcResponse::default()
        ),
        libc::EMSGSIZE
    );
    let queued = connected(&state, 1);
    assert_eq!(queued.incoming_bytes, 8);
    assert_eq!(
        queued.incoming[0].bytes,
        b"retained".iter().copied().collect::<VecDeque<_>>()
    );
    assert_eq!(queued.incoming[0].control, oversized);
    assert_eq!(queued.incoming_control_bytes, cmsg_align(oversized.len()));
}

#[test]
fn recvmsg_stops_before_second_authority_segment_and_returns_each_once() {
    let mut state = NetState::new();
    state.sockets.insert(1, connected_socket(2, None));
    for marker in [1, 2] {
        enqueue(&mut state, 1, &[marker], &rights(marker));
    }
    for marker in [1, 2] {
        let mut response = NetdIpcResponse::default();
        assert_eq!(
            recvmsg_in_state(&mut state, &recv_request(1), &mut response),
            0
        );
        assert_eq!(response.value, 1);
        let start = NETD_RECVMSG_PAYLOAD_HEADER_SIZE + 1;
        assert_eq!(
            &response.payload[start..response.payload_len as usize],
            rights(marker).as_slice()
        );
    }
    assert_eq!(connected(&state, 1).incoming_control_bytes, 0);
}

fn listener_state() -> NetState {
    let mut state = NetState::new();
    let mut listener = connected_socket(0, None);
    listener.bound_path = Some("listener".into());
    listener.state = UnixSocketState::Listening {
        backlog: 2,
        pending: VecDeque::from([2, 3]),
    };
    state.bindings.insert("listener".into(), 1);
    state.sockets.insert(1, listener);
    for (accepted, client) in [(2, 4), (3, 5)] {
        state
            .sockets
            .insert(accepted, connected_socket(client, Some(1)));
        state
            .sockets
            .insert(client, connected_socket(accepted, None));
    }
    state
}

fn pop_accept(state: &mut NetState) -> u64 {
    let UnixSocketState::Listening { pending, .. } = &mut state.sockets.get_mut(&1).unwrap().state
    else {
        panic!("expected listener");
    };
    pending.pop_front().unwrap()
}

fn release_controls(response: &NetdIpcResponse) -> Vec<Vec<u8>> {
    assert_eq!(response.reserved0, response.payload_len);
    let mut controls = Vec::new();
    let mut offset = 0;
    while offset < response.payload_len as usize {
        let header = read_cmsghdr(&response.payload[offset..]).unwrap();
        let end = offset + header.cmsg_len as usize;
        controls.push(response.payload[offset..end].to_vec());
        offset = cmsg_align(end);
    }
    controls
}

#[test]
fn listener_close_repeatedly_retires_pending_bytes_and_authority_once() {
    for _ in 0..128 {
        let mut state = listener_state();
        for (client, marker) in [(4, 2), (5, 3)] {
            assert_eq!(
                send_socket_message_in_state(
                    &mut state,
                    &request(client),
                    b"queued",
                    &rights(marker)
                ),
                Ok(6)
            );
        }
        let (retired, peers, _funded) = retire_unix_socket(&mut state, 1);
        let mut response = NetdIpcResponse::default();
        encode_retired_socket_release(retired, &mut response);
        let mut released = release_controls(&response);
        released.sort();
        let mut expected = vec![rights(2), rights(3)];
        expected.sort();
        assert_eq!(released, expected);
        assert_eq!(state.sockets.len(), 2);
        assert!(state.bindings.is_empty());
        assert_eq!(peers.len(), 2);
        for client in [4, 5] {
            let peer = connected(&state, client);
            assert!(peer.peer_closed && peer.peer_read_closed && peer.peer_write_closed);
            assert_ne!(
                unix_socket_revents(&state, client, linux_abi::POLLIN as u32).unwrap()
                    & linux_abi::POLLHUP as u32,
                0
            );
            assert_eq!(
                send_socket_message_in_state(&mut state, &request(client), b"late", &[]),
                Err(libc::EPIPE)
            );
            let mut byte = [0];
            assert_eq!(
                recv_socket_bytes_in_state(&mut state, &request(client), &mut byte, true)
                    .unwrap()
                    .read,
                0
            );
        }
        let (retired, peers, _funded) = retire_unix_socket(&mut state, 1);
        assert!(retired.is_empty() && peers.is_empty());
        encode_retired_socket_release(retired, &mut response);
        assert_eq!(response.reserved0, 0);
        for client in [4, 5] {
            let _ = retire_unix_socket(&mut state, client);
        }
        assert!(state.sockets.is_empty());
        assert_eq!(state.unix_queued_bytes,0);
    }
}

#[test]
fn pending_budget_admission_is_aligned_atomic_and_shared_across_accept_pop() {
    let mut state = listener_state();
    // Unaligned cmsgs must charge their trailer padding too.
    let control = append_recvmsg_credentials(Some(Credentials::default()), Vec::new());
    let charge = cmsg_align(control.len());
    assert_ne!(charge, control.len());
    for i in 0..SOCKET_CONTROL_BUFFER_CAPACITY / charge {
        let client = if i % 2 == 0 { 4 } else { 5 };
        assert_eq!(
            send_socket_message_in_state(&mut state, &request(client), b"x", &control),
            Ok(1)
        );
    }
    assert_eq!(
        connected(&state, 2).incoming_control_bytes + connected(&state, 3).incoming_control_bytes,
        SOCKET_CONTROL_BUFFER_CAPACITY
    );
    let accepted = pop_accept(&mut state);
    assert_eq!(accepted, 2);
    let before = connected(&state, 2).incoming_bytes;
    let before_control = connected(&state, 2).incoming_control_bytes;
    assert_eq!(
        send_socket_message_in_state(&mut state, &request(4), b"not consumed", &control),
        Err(libc::EAGAIN)
    );
    assert_eq!(connected(&state, 2).incoming_bytes, before);
    assert_eq!(connected(&state, 2).incoming_control_bytes, before_control);
    let (retired, peers, _funded) = settle_unix_accept(&mut state, 1, accepted, false);
    assert!(retired.is_empty() && peers.is_empty());
    assert_eq!(connected(&state, 2).pending_listener, Some(1));
    assert_eq!(pop_accept(&mut state), 2);
    // Publication transfers budget ownership, without dropping the queued control.
    assert!(settle_unix_accept(&mut state, 1, 2, true).0.is_empty());
    assert_eq!(connected(&state, 2).pending_listener, None);
    assert_eq!(connected(&state, 2).incoming_bytes, before);
    assert_eq!(
        send_socket_message_in_state(&mut state, &request(5), b"x", &control),
        Ok(1)
    );
    let (retired, _, _funded) = retire_unix_socket(&mut state, 1);
    let mut response = NetdIpcResponse::default();
    encode_retired_socket_release(retired, &mut response);
    assert!(state.sockets.contains_key(&2));
    assert!(!connected(&state, 4).peer_closed);
    assert!(response.payload_len as usize <= SOCKET_CONTROL_BUFFER_CAPACITY);
}

#[test]
fn failed_inflight_accept_after_listener_close_releases_control_without_reply() {
    let mut state = listener_state();
    for (client, marker) in [(4, 2), (5, 3)] {
        assert_eq!(
            send_socket_message_in_state(&mut state, &request(client), b"x", &rights(marker)),
            Ok(1)
        );
    }
    assert_eq!(pop_accept(&mut state), 2);
    let (retired, _, _funded) = retire_unix_socket(&mut state, 1);
    let mut close_response = NetdIpcResponse::default();
    encode_retired_socket_release(retired, &mut close_response);
    assert_eq!(release_controls(&close_response), vec![rights(3)]);
    assert_eq!(connected(&state, 2).pending_listener, Some(1));
    let (retired, peers, _funded) = settle_unix_accept(&mut state, 1, 2, false);
    let mut error = NetdIpcResponse {
        status: libc::EIO,
        op: SYSCALL_OFFLOAD_OP_LINUX_ACCEPT,
        payload_len: 7,
        ..NetdIpcResponse::default()
    };
    let mut releases=0;
    release_accept_retired_control_with(retired,&mut error,|control| {
        releases+=1;
        let mut captured=NetdIpcResponse::default();
        captured.payload[..control.len()].copy_from_slice(control);
        captured.payload_len=control.len() as u32;
        captured.reserved0=control.len() as u32;
        assert_eq!(release_controls(&captured),vec![rights(2)]);
        Ok(())
    }).unwrap();
    assert_eq!(releases,1);
    assert_eq!(error.status, libc::EIO);
    assert_eq!(error.payload_len,0);
    assert_eq!(error.reserved0,0);
    assert_eq!(peers, vec![4]);
    assert!(connected(&state, 4).peer_closed);
    assert!(!state.sockets.contains_key(&2));
    assert!(settle_unix_accept(&mut state, 1, 2, false).0.is_empty());
}

#[test]
fn successful_inflight_accept_after_listener_close_keeps_published_socket() {
    let mut state = listener_state();
    assert_eq!(pop_accept(&mut state), 2);
    let _ = retire_unix_socket(&mut state, 1);
    let (retired, peers, _funded) = settle_unix_accept(&mut state, 1, 2, true);
    assert!(retired.is_empty() && peers.is_empty());
    assert_eq!(connected(&state, 2).pending_listener, None);
    assert!(!connected(&state, 4).peer_closed);
}

#[test]
fn listener_close_at_exact_aligned_custody_cap_emits_every_control() {
    let mut state = listener_state();
    let credentials = append_recvmsg_credentials(Some(Credentials::default()), Vec::new());
    let mut expected = Vec::new();
    for marker in 0..4 {
        let control = rights(marker);
        assert_eq!(
            send_socket_message_in_state(&mut state, &request(4), b"x", &control),
            Ok(1)
        );
        expected.push(control);
    }
    let used = connected(&state, 2).incoming_control_bytes;
    for _ in 0..(SOCKET_CONTROL_BUFFER_CAPACITY - used) / cmsg_align(credentials.len()) {
        assert_eq!(
            send_socket_message_in_state(&mut state, &request(5), b"x", &credentials),
            Ok(1)
        );
        expected.push(credentials.clone());
    }
    assert_eq!(
        connected(&state, 2).incoming_control_bytes + connected(&state, 3).incoming_control_bytes,
        SOCKET_CONTROL_BUFFER_CAPACITY
    );
    assert_eq!(
        send_socket_message_in_state(&mut state, &request(4), b"rejected", &rights(9)),
        Err(libc::EAGAIN)
    );
    let (retired, _, _funded) = retire_unix_socket(&mut state, 1);
    let mut response = NetdIpcResponse::default();
    encode_retired_socket_release(retired, &mut response);
    let mut released = release_controls(&response);
    released.sort();
    expected.sort();
    assert_eq!(released, expected);
    assert!(response.payload_len as usize <= SOCKET_CONTROL_BUFFER_CAPACITY);
}

#[test]
fn abstract_unix_addresses_roundtrip_and_stay_distinct_from_filesystem() {
    let make_req = |sun_path: &[u8]| {
        let mut req = NetdIpcRequest::default();
        req.payload[..2].copy_from_slice(&(linux_abi::AF_UNIX as u16).to_ne_bytes());
        req.payload[2..2 + sun_path.len()].copy_from_slice(sun_path);
        req.payload_len = (2 + sun_path.len()) as u32;
        req
    };

    let raw_abs = b"\0bus\0node";
    let abs = sockaddr_path_from_payload(&make_req(raw_abs)).unwrap();
    assert_eq!(abs, UnixAddress::Abstract(raw_abs.to_vec()));
    assert_ne!(abs, UnixAddress::Filesystem("bus\0node".into()));

    let mut resp = NetdIpcResponse::default();
    assert_eq!(sockaddr_payload(Some(&abs), &mut resp), Ok(0));
    assert_eq!(resp.payload_len as usize, 2 + raw_abs.len());
    assert_eq!(&resp.payload[2..2 + raw_abs.len()], raw_abs);

    // Exact UNIX_PATH_MAX abstract name succeeds without needing a trailing NUL.
    let max_abs = vec![0u8; linux_abi::UNIX_PATH_MAX];
    assert!(sockaddr_path_from_payload(&make_req(&max_abs)).is_ok());

    // Overlong abstract name (> UNIX_PATH_MAX) is rejected.
    let too_long = vec![0u8; linux_abi::UNIX_PATH_MAX + 1];
    assert_eq!(sockaddr_path_from_payload(&make_req(&too_long)), Err(libc::EINVAL));

    // Empty sockaddr_un (len == sizeof(sa_family_t)) requests autobind on bind,
    // while connect refuses it with EINVAL.
    let empty = make_req(&[]);
    assert_eq!(sockaddr_bind_address_from_payload(&empty), Ok(None));
    assert_eq!(sockaddr_path_from_payload(&empty), Err(libc::EINVAL));
}

#[test]
fn filesystem_names_accept_a_full_unterminated_sun_path_and_report_it_back() {
    let make_req = |sun_path: &[u8]| {
        let mut req = NetdIpcRequest::default();
        req.payload[..2].copy_from_slice(&(linux_abi::AF_UNIX as u16).to_ne_bytes());
        req.payload[2..2 + sun_path.len()].copy_from_slice(sun_path);
        req.payload_len = (2 + sun_path.len()) as u32;
        req
    };
    let mut full = vec![b'/'];
    full.extend(std::iter::repeat(b'a').take(linux_abi::UNIX_PATH_MAX - 1));
    let address = sockaddr_path_from_payload(&make_req(&full)).unwrap();
    assert_eq!(address, UnixAddress::Filesystem(String::from_utf8(full.clone()).unwrap()));
    let mut response = NetdIpcResponse::default();
    assert_eq!(sockaddr_payload(Some(&address), &mut response), Ok(0));
    assert_eq!(response.payload_len as usize, 2 + linux_abi::UNIX_PATH_MAX);
    assert_eq!(&response.payload[2..2 + full.len()], &full[..]);
    // One byte shorter keeps its NUL; one byte longer is EINVAL.
    let short = &full[..full.len() - 1];
    let short_address = sockaddr_path_from_payload(&make_req(short)).unwrap();
    assert_eq!(sockaddr_payload(Some(&short_address), &mut response), Ok(0));
    assert_eq!(response.payload_len as usize, 2 + short.len() + 1);
    let mut over = full.clone();
    over.push(b'a');
    assert_eq!(sockaddr_path_from_payload(&make_req(&over)), Err(libc::EINVAL));
}

#[test]
fn shutdown_read_makes_drained_receive_eof_and_poll_hup_requires_both_directions() {
    let mut state = NetState::new();
    state.sockets.insert(1, connected_socket(2, None));
    state.sockets.insert(2, connected_socket(1, None));
    let mut dest = [0_u8; 8];
    assert_eq!(
        recv_socket_bytes_in_state(&mut state, &request(1), &mut dest, false).err(),
        Some(libc::EAGAIN)
    );
    connected_mut(&mut state, 1).recv_closed = true;
    let received = recv_socket_bytes_in_state(&mut state, &request(1), &mut dest, false).unwrap();
    assert_eq!(received.read, 0, "SHUT_RD drained receive must report EOF, not block");
    let events = (linux_abi::POLLIN | linux_abi::POLLRDHUP | linux_abi::POLLOUT) as u32;
    let revents = unix_socket_revents(&state, 1, events).unwrap();
    assert_ne!(revents & linux_abi::POLLIN as u32, 0);
    assert_ne!(revents & linux_abi::POLLRDHUP as u32, 0);
    assert_eq!(revents & linux_abi::POLLHUP as u32, 0, "one direction is not a hangup");
    connected_mut(&mut state, 1).send_closed = true;
    let revents = unix_socket_revents(&state, 1, events).unwrap();
    assert_ne!(revents & linux_abi::POLLHUP as u32, 0);
    // The peer shut both directions: this end sees the same hangup.
    let peer = connected_mut(&mut state, 2);
    peer.peer_read_closed = true;
    peer.peer_write_closed = true;
    let revents = unix_socket_revents(&state, 2, events).unwrap();
    assert_ne!(revents & linux_abi::POLLHUP as u32, 0);
}

#[test]
fn sendmsg_credentials_must_name_the_sender() {
    let own = Credentials { pid: 7, uid: 1000, gid: 100 };
    let cred = |pid: i32, uid: u32, gid: u32| {
        let hdr = size_of::<linux_abi::LinuxCmsghdr>();
        let total = hdr + size_of::<linux_abi::LinuxUCred>();
        let mut control = vec![0_u8; total];
        write_plain_old_data(
            &mut control[..hdr],
            &linux_abi::LinuxCmsghdr {
                cmsg_len: total as u64,
                cmsg_level: linux_abi::SOL_SOCKET as u32,
                cmsg_type: linux_abi::SCM_CREDENTIALS as u32,
            },
        );
        write_plain_old_data(
            &mut control[hdr..],
            &linux_abi::LinuxUCred { pid, uid, gid },
        );
        control
    };
    let honest = cred(7, 1000, 100);
    assert_eq!(validate_sendmsg_control(&honest), Ok(()));
    assert_eq!(validate_sendmsg_credentials(&honest, own), Ok(()));
    for forged in [cred(8, 1000, 100), cred(7, 0, 100), cred(7, 1000, 0)] {
        assert_eq!(validate_sendmsg_credentials(&forged, own), Err(libc::EPERM));
    }
    let root = Credentials { pid: 1, uid: 0, gid: 0 };
    assert_eq!(validate_sendmsg_credentials(&cred(8, 5, 5), root), Ok(()));
    // Rights alone never trip the credential check.
    assert_eq!(validate_sendmsg_credentials(&rights(1), own), Ok(()));
}

#[test]
fn unnamed_socket_address_reports_only_the_family() {
    let mut response = NetdIpcResponse::default();
    assert_eq!(sockaddr_payload(None, &mut response), Ok(0));
    assert_eq!(response.payload_len, 2);
}
