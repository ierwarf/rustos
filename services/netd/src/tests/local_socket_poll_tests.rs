use super::*;

#[test]
fn pending_slot_reservation_is_global_and_bounded() {
    let pending = AtomicUsize::new(0);
    assert!(reserve_pending_slot(&pending, 2));
    assert!(reserve_pending_slot(&pending, 2));
    assert!(!reserve_pending_slot(&pending, 2));
    release_pending_slot(&pending);
    assert!(reserve_pending_slot(&pending, 2));
    assert_eq!(pending.load(Ordering::Acquire), 2);
}

#[test]
fn poisoned_deferred_queue_is_drained_for_fail_closed_replies() {
    let queue = Mutex::new(VecDeque::from([1_u8, 2_u8]));
    let guard = queue.lock().unwrap();
    let (drained, poisoned) = take_deferred_queue(Err(std::sync::PoisonError::new(guard)));
    assert!(poisoned);
    assert_eq!(drained, VecDeque::from([1_u8, 2_u8]));
    assert!(queue.lock().unwrap().is_empty());
}

fn connected_socket(peer: u64) -> UnixSocket {
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
            channel_id: 1,
            peer,
            peer_closed: false,
            peer_read_closed: false,
            peer_write_closed: false,
            peer_credentials: Credentials::default(),
            recv_drain_handoff_armed: false,
            recv_closed: false,
            send_closed: false,
        }),
    }
}

#[test]
fn unix_readiness_publication_targets_only_the_socket_and_its_peer() {
    let first = 41_u64;
    let second = 42_u64;
    let unrelated = 43_u64;
    let mut state = NetState::new();
    state.sockets.insert(first, connected_socket(second));
    state.sockets.insert(second, connected_socket(first));
    state.sockets.insert(unrelated, connected_socket(u64::MAX));
    let request = NetdIpcRequest {
        op: SYSCALL_OFFLOAD_OP_LINUX_SENDTO,
        socket_token: first,
        ..NetdIpcRequest::default()
    };

    assert_eq!(
        readiness_targets_in_state(&request, &state),
        vec![first, second]
    );
}

#[test]
fn unix_poll_readiness_tracks_data_space_and_peer_close() {
    let mut state = NetState::new();
    state.sockets.insert(1, connected_socket(2));
    state.sockets.insert(2, connected_socket(1));

    assert_eq!(
        unix_socket_revents(&state, 1, linux_abi::POLLIN as u32).unwrap(),
        0
    );
    assert_eq!(
        unix_socket_revents(&state, 1, linux_abi::POLLOUT as u32).unwrap(),
        linux_abi::POLLOUT as u32
    );

    let UnixSocketState::Connected(connected) = &mut state.sockets.get_mut(&1).unwrap().state
    else {
        unreachable!();
    };
    connected.incoming_bytes = 1;
    connected.incoming.push_back(UnixStreamSegment {
        bytes: [1].into_iter().collect(),
        control: Vec::new(),
    });
    assert_eq!(
        unix_socket_revents(&state, 1, linux_abi::POLLIN as u32).unwrap(),
        linux_abi::POLLIN as u32
    );

    let UnixSocketState::Connected(connected) = &mut state.sockets.get_mut(&1).unwrap().state
    else {
        unreachable!();
    };
    connected.peer_closed = true;
    assert_ne!(
        unix_socket_revents(&state, 1, linux_abi::POLLIN as u32).unwrap()
            & linux_abi::POLLHUP as u32,
        0
    );
}

#[test]
fn local_wait_is_deferred_without_consuming_a_worker() {
    let wait = NetdIpcRequest {
        op: SYSCALL_OFFLOAD_OP_LINUX_POLL_SOCKET,
        arg2: NETD_POLL_MODE_WAIT,
        socket_token: u64::MAX,
        ..NetdIpcRequest::default()
    };
    assert!(is_deferred_local_poll_request(&wait));
    assert!(!is_blocking_request(&wait));

    let query = NetdIpcRequest {
        arg2: NETD_POLL_MODE_QUERY,
        ..wait
    };
    assert!(!is_deferred_local_poll_request(&query));
    assert!(!is_blocking_request(&query));
}

#[test]
fn netd_v7_requires_a_nonzero_caller_deadline_and_exact_header_length() {
    let request = NetdIpcRequest {
        version: NETD_IPC_ABI_VERSION,
        op: SYSCALL_OFFLOAD_OP_LINUX_POLL_SOCKET,
        pid: 1,
        tid: 1,
        arg2: NETD_POLL_MODE_QUERY,
        deadline_ns: 2,
        ..NetdIpcRequest::default()
    };
    assert_eq!(
        validate_request(NETD_IPC_REQUEST_HEADER_SIZE, &request),
        Ok(())
    );
    assert_eq!(
        validate_request(size_of::<NetdIpcRequest>(), &request),
        Err(libc::EINVAL)
    );
    assert_eq!(
        validate_request(
            NETD_IPC_REQUEST_HEADER_SIZE,
            &NetdIpcRequest {
                deadline_ns: 0,
                ..request
            },
        ),
        Err(libc::EINVAL)
    );
    assert_eq!(
        validate_request(
            NETD_IPC_REQUEST_HEADER_SIZE,
            &NetdIpcRequest {
                version: NETD_IPC_ABI_VERSION - 1,
                ..request
            },
        ),
        Err(libc::EINVAL)
    );
}

#[test]
fn admission_clamps_each_operation_class_once_without_freshening() {
    const NOW_NS: u64 = 2_000_000_000;
    for (op, cap_ms) in [
        (
            SYSCALL_OFFLOAD_OP_LINUX_POLL_SOCKET,
            rustos_user_abi::performance::IPC_READINESS_QUERY_HARD_LIMIT_MS,
        ),
        (
            NETD_IPC_OP_DVM_SESSION,
            rustos_user_abi::performance::IPC_INTERACTIVE_CONTROL_HARD_LIMIT_MS,
        ),
        (
            SYSCALL_OFFLOAD_OP_LINUX_DUP,
            rustos_user_abi::performance::IPC_INTERACTIVE_CONTROL_HARD_LIMIT_MS,
        ),
        (
            SYSCALL_OFFLOAD_OP_LINUX_CLOSE,
            rustos_user_abi::performance::IPC_INTERACTIVE_CONTROL_HARD_LIMIT_MS,
        ),
        (
            NETD_IPC_OP_REF_ACK,
            rustos_user_abi::performance::IPC_INTERACTIVE_CONTROL_HARD_LIMIT_MS,
        ),
        (
            SYSCALL_OFFLOAD_OP_LINUX_SOCKET,
            rustos_user_abi::performance::IPC_BULK_DATA_HARD_LIMIT_MS,
        ),
    ] {
        let mut request = NetdIpcRequest {
            op,
            deadline_ns: u64::MAX,
            ..NetdIpcRequest::default()
        };
        let expected_end = NOW_NS + cap_ms * 1_000_000;

        assert_eq!(clamp_request_deadline_at(&mut request, NOW_NS), Ok(()));
        assert_eq!(request.deadline_ns, expected_end);
        // Re-checking an admitted request may not give it another full
        // class budget. The original clamp remains its immutable end.
        assert_eq!(clamp_request_deadline_at(&mut request, NOW_NS + 1), Ok(()));
        assert_eq!(request.deadline_ns, expected_end);
    }
}

#[test]
fn far_future_poll_admission_stays_on_the_readiness_rail() {
    const NOW_NS: u64 = 3_000_000_000;
    let mut request = NetdIpcRequest {
        op: SYSCALL_OFFLOAD_OP_LINUX_POLL_SOCKET,
        deadline_ns: u64::MAX,
        ..NetdIpcRequest::default()
    };

    assert_eq!(clamp_request_deadline_at(&mut request, NOW_NS), Ok(()));
    assert_eq!(
        request.deadline_ns,
        NOW_NS + rustos_user_abi::performance::IPC_READINESS_QUERY_HARD_LIMIT_MS * 1_000_000
    );
}

#[test]
fn expired_work_is_rejected_before_any_queue_reservation() {
    PENDING_BLOCKING_REQUESTS.store(0, Ordering::Release);
    PENDING_LOCAL_POLLS.store(0, Ordering::Release);
    let request = NetdIpcRequest {
        deadline_ns: 9,
        ..NetdIpcRequest::default()
    };

    assert_eq!(
        enqueue_blocking_request_at(request, 1, 9),
        Err(libc::ETIMEDOUT)
    );
    assert_eq!(
        defer_local_poll_request_at(request, 1, 9),
        Err(libc::ETIMEDOUT)
    );
    assert_eq!(PENDING_BLOCKING_REQUESTS.load(Ordering::Acquire), 0);
    assert_eq!(PENDING_LOCAL_POLLS.load(Ordering::Acquire), 0);
}

#[test]
fn expired_blocking_work_never_starts_a_provider_after_queueing() {
    BLOCKING_PROVIDER_STARTS.store(0, Ordering::Release);
    let request = NetdIpcRequest {
        op: SYSCALL_OFFLOAD_OP_LINUX_POLL_SOCKET,
        deadline_ns: 21,
        ..NetdIpcRequest::default()
    };
    let mut response = NetdIpcResponse::default();

    assert_eq!(
        run_blocking_request_at(&request, &mut response, 21),
        libc::ETIMEDOUT
    );
    assert_eq!(BLOCKING_PROVIDER_STARTS.load(Ordering::Acquire), 0);
}

#[test]
fn expired_detached_local_poll_replies_and_releases_exactly_once() {
    let waiter = DeferredLocalPoll {
        request: Box::new(NetdIpcRequest {
            op: SYSCALL_OFFLOAD_OP_LINUX_POLL_SOCKET,
            deadline_ns: 31,
            ..NetdIpcRequest::default()
        }),
        reply_cap: 7,
    };
    let reply_count = std::cell::Cell::new(0_u8);
    let release_count = std::cell::Cell::new(0_u8);
    let reply_status = std::cell::Cell::new(0_i32);

    let still_waiting = service_one_local_poll_waiter(
        waiter,
        false,
        31,
        |reply_cap, response| {
            assert_eq!(reply_cap, 7);
            reply_count.set(reply_count.get() + 1);
            reply_status.set(response.status);
        },
        || release_count.set(release_count.get() + 1),
    );

    assert!(still_waiting.is_none());
    assert_eq!(reply_count.get(), 1);
    assert_eq!(release_count.get(), 1);
    assert_eq!(reply_status.get(), libc::ETIMEDOUT);
}

#[test]
fn child_retry_timeout_never_exceeds_the_caller_remaining_budget() {
    let request = NetdIpcRequest {
        deadline_ns: 50,
        ..NetdIpcRequest::default()
    };
    assert_eq!(
        capped_request_wait_at(&request, 49, INET_CONNECT_TIMEOUT),
        Ok(Duration::from_nanos(1))
    );
    assert_eq!(
        capped_request_wait_at(&request, 49, BLOCKING_RETRY_BACKOFF),
        Ok(Duration::from_nanos(1))
    );
    assert_eq!(
        capped_request_wait_at(&request, 50, BLOCKING_RETRY_BACKOFF),
        Err(libc::ETIMEDOUT)
    );
}

#[test]
fn expired_authenticated_provider_request_issues_no_status_query_or_publication_permit() {
    let request = NetdIpcRequest {
        deadline_ns: 100,
        ..NetdIpcRequest::default()
    };
    let status_queries = std::cell::Cell::new(0_u8);
    let sleeps = std::cell::Cell::new(0_u8);

    assert_eq!(
        await_authenticated_packet_provider_with(
            &request,
            || {
                status_queries.set(status_queries.get() + 1);
                Ok(PacketProviderState::Active)
            },
            || 100,
            || Some(AUTHENTICATED_CONTROL_WAIT),
            |_| sleeps.set(sleeps.get() + 1),
        ),
        Err(libc::ETIMEDOUT)
    );
    assert_eq!(status_queries.get(), 0);
    assert_eq!(sleeps.get(), 0);
}

#[test]
fn provider_activation_crossing_the_request_end_is_not_a_publication_permit() {
    let request = NetdIpcRequest {
        deadline_ns: 100,
        ..NetdIpcRequest::default()
    };
    let now_turn = std::cell::Cell::new(0_u8);
    let status_queries = std::cell::Cell::new(0_u8);
    let sleeps = std::cell::Cell::new(0_u8);

    assert_eq!(
        await_authenticated_packet_provider_with(
            &request,
            || {
                status_queries.set(status_queries.get() + 1);
                Ok(PacketProviderState::Active)
            },
            || {
                let turn = now_turn.get();
                now_turn.set(turn + 1);
                if turn == 0 {
                    99
                } else {
                    100
                }
            },
            || Some(AUTHENTICATED_CONTROL_WAIT),
            |_| sleeps.set(sleeps.get() + 1),
        ),
        Err(libc::ETIMEDOUT)
    );
    assert_eq!(status_queries.get(), 1);
    assert_eq!(sleeps.get(), 0);
}

#[test]
fn authenticated_provider_sleep_uses_the_same_request_end() {
    let request = NetdIpcRequest {
        deadline_ns: 1_001_000,
        ..NetdIpcRequest::default()
    };
    let status_turn = std::cell::Cell::new(0_u8);
    let sleep_for = std::cell::Cell::new(Duration::ZERO);

    assert_eq!(
        await_authenticated_packet_provider_with(
            &request,
            || {
                let turn = status_turn.get();
                status_turn.set(turn + 1);
                Ok(if turn == 0 {
                    PacketProviderState::AwaitingAuthenticatedControl
                } else {
                    PacketProviderState::Active
                })
            },
            || 1_000_000,
            || Some(AUTHENTICATED_CONTROL_WAIT),
            |duration| sleep_for.set(duration),
        ),
        Ok(())
    );
    assert_eq!(sleep_for.get(), Duration::from_nanos(1_000));
}

fn install_segmented_test_socket(token: u64, segments: Vec<UnixStreamSegment>) {
    let mut socket = connected_socket(token.wrapping_add(1));
    let UnixSocketState::Connected(connected) = &mut socket.state else {
        unreachable!();
    };
    connected.incoming_bytes = segments.iter().map(|segment| segment.bytes.len()).sum();
    connected.incoming_control_bytes = segments.iter().map(|segment| segment.control.len()).sum();
    connected.incoming = segments.into_iter().collect();
    net_state().lock().unwrap().sockets.insert(token, socket);
}

#[test]
fn recvmsg_ancillary_stays_with_its_stream_segment() {
    let token = u64::MAX - 910;
    install_segmented_test_socket(
        token,
        vec![
            UnixStreamSegment {
                bytes: [1, 2].into_iter().collect(),
                control: Vec::new(),
            },
            UnixStreamSegment {
                bytes: [3, 4].into_iter().collect(),
                control: vec![9, 8, 7, 6],
            },
        ],
    );
    let request = NetdIpcRequest {
        socket_token: token,
        ..NetdIpcRequest::default()
    };
    let mut first = [0_u8; 2];
    let received = recv_socket_bytes(&request, &mut first, true).unwrap();
    assert_eq!(first, [1, 2]);
    assert!(received.control.is_empty());

    let mut second = [0_u8; 2];
    let received = recv_socket_bytes(&request, &mut second, true).unwrap();
    assert_eq!(second, [3, 4]);
    assert_eq!(received.control, vec![9, 8, 7, 6]);
    net_state().lock().unwrap().sockets.remove(&token);
}

#[test]
fn ordinary_read_discards_ancillary_exactly_once() {
    let token = u64::MAX - 911;
    install_segmented_test_socket(
        token,
        vec![UnixStreamSegment {
            bytes: [5, 6].into_iter().collect(),
            control: vec![4, 3, 2, 1],
        }],
    );
    let request = NetdIpcRequest {
        socket_token: token,
        ..NetdIpcRequest::default()
    };
    let mut bytes = [0_u8; 2];
    let received = recv_socket_bytes(&request, &mut bytes, false).unwrap();
    assert_eq!(bytes, [5, 6]);
    assert!(received.control.is_empty());
    assert_eq!(received.discarded, vec![4, 3, 2, 1]);

    let connected = net_state().lock().unwrap().sockets.remove(&token).unwrap();
    let UnixSocketState::Connected(connected) = connected.state else {
        unreachable!();
    };
    assert_eq!(connected.incoming_bytes, 0);
    assert_eq!(connected.incoming_control_bytes, 0);
    assert!(connected.incoming.is_empty());
}
