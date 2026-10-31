use super::*;

fn connected_socket(peer: u64, pending_listener: Option<u64>) -> UnixSocket {
    UnixSocket {
        owner: Credentials::default(),
        refs: 1,
        options: SocketOptions::default(),
        bound_path: None,
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

fn enqueue(state: &mut NetState, token: u64, bytes: &[u8], control: &[u8]) {
    let connected = connected_mut(state, token);
    connected.incoming_bytes += bytes.len();
    connected.incoming_control_bytes += cmsg_align(control.len());
    connected.incoming.push_back(UnixStreamSegment {
        bytes: bytes.iter().copied().collect(),
        control: control.to_vec(),
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
            initial_passcred.then_some(connected(&guard, 1).peer_credentials),
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
                recv_connected_bytes(connected_mut(&mut state, client), &mut byte, true)
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
fn failed_inflight_accept_after_listener_close_carries_only_error_release_trailer() {
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
    encode_retired_socket_release(retired, &mut error);
    assert_eq!(error.status, libc::EIO);
    assert_eq!(release_controls(&error), vec![rights(2)]);
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
