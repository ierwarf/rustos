use super::*;
use crate::netlink_tests::{make_readable, netlink_socket};

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

#[test]
fn idle_deferred_pass_retains_queue_storage_for_the_next_batch() {
    let queue = Mutex::new(VecDeque::<u8>::with_capacity(8));
    let capacity = queue.lock().unwrap().capacity();
    let (drained, poisoned) = take_deferred_queue(queue.lock());
    assert!(!poisoned);
    assert!(drained.is_empty());
    assert_eq!(queue.lock().unwrap().capacity(), capacity);
    queue.lock().unwrap().extend([1, 2, 3]);
    let (drained, poisoned) = take_deferred_queue(queue.lock());
    assert!(!poisoned);
    assert_eq!(drained, VecDeque::from([1, 2, 3]));
    assert_eq!(drained.capacity(), capacity);
    assert!(queue.lock().unwrap().is_empty());
}

#[test]
fn netlink_readiness_publication_targets_only_the_socket_that_changed() {
    let first = 41_u64;
    let second = 42_u64;
    let unrelated = 43_u64;
    let mut state = NetState::new();
    for token in [first, second, unrelated] {
        state.netlink_sockets.insert(token, netlink_socket(libc::SOCK_RAW as u64));
    }
    state.record_readiness_changes();
    state.readiness_publications.clear();
    // Queueing a reply on one socket changes only its own readability.
    make_readable(&mut state, first);
    state.record_readiness_changes();
    assert_eq!(state.readiness_publications.iter().map(|(token, _)| *token).collect::<Vec<_>>(), vec![first]);
}

#[test]
fn netlink_poll_readiness_tracks_queued_replies() {
    let mut state = NetState::new();
    state.netlink_sockets.insert(1, netlink_socket(libc::SOCK_RAW as u64));

    assert_eq!(
        netlink_socket_revents(&state, 1, linux_abi::POLLIN as u32).unwrap(),
        0
    );
    assert_eq!(
        netlink_socket_revents(&state, 1, linux_abi::POLLOUT as u32).unwrap(),
        linux_abi::POLLOUT as u32
    );
    assert_eq!(netlink_socket_revents(&state, 2, linux_abi::POLLIN as u32), Err(libc::EBADF));

    make_readable(&mut state, 1);
    assert_eq!(
        netlink_socket_revents(&state, 1, linux_abi::POLLIN as u32).unwrap(),
        linux_abi::POLLIN as u32
    );
}

#[test]
fn single_socket_poll_returns_readiness_and_generation_together() {
    let first = 0xba7c_1001_u64;
    let generation = {
        let mut state = lock_net_state();
        state.netlink_sockets.insert(first, netlink_socket(libc::SOCK_RAW as u64));
        state.socket_readiness_generation(first)
    };
    let request = NetdIpcRequest {
        socket_token: first,
        arg1: linux_abi::POLLOUT as u64,
        ..NetdIpcRequest::default()
    };
    let mut response = NetdIpcResponse::default();
    assert_eq!(handle_poll_socket(&request, &mut response), 0);
    assert_eq!(response.value, linux_abi::POLLOUT as u64);
    assert_eq!(response.payload_len, 8);
    assert_eq!(u64::from_le_bytes(response.payload[..8].try_into().unwrap()), generation);
    let mut state = lock_net_state();
    state.netlink_sockets.remove(&first);
    state.readiness_generations.remove(&first);
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
            NETD_IPC_OP_POLL_BATCH,
            rustos_user_abi::performance::IPC_READINESS_QUERY_HARD_LIMIT_MS,
        ),
        (
            NETD_IPC_OP_DVM_SESSION,
            rustos_user_abi::performance::IPC_INTERACTIVE_CONTROL_HARD_LIMIT_MS,
        ),
        (
            SYSCALL_OFFLOAD_OP_LINUX_DUP,
            rustos_user_abi::performance::IPC_STATE_MUTATION_HARD_LIMIT_MS,
        ),
        (
            SYSCALL_OFFLOAD_OP_LINUX_CLOSE,
            rustos_user_abi::performance::IPC_STATE_MUTATION_HARD_LIMIT_MS,
        ),
        (
            NETD_IPC_OP_REF_ACK,
            rustos_user_abi::performance::IPC_STATE_MUTATION_HARD_LIMIT_MS,
        ),
        (
            SYSCALL_OFFLOAD_OP_LINUX_SOCKET,
            rustos_user_abi::performance::IPC_STATE_MUTATION_HARD_LIMIT_MS,
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
        run_blocking_request_at(&request, &mut response, 0, 21),
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
        |_pid| release_count.set(release_count.get() + 1),
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

fn poll_batch_request(entries: &[(u64, u32)]) -> NetdIpcRequest {
    let mut request = NetdIpcRequest {
        op: NETD_IPC_OP_POLL_BATCH,
        ..NetdIpcRequest::default()
    };
    for (index, (token, events)) in entries.iter().enumerate() {
        let offset = index * size_of::<NetdPollQueryWire>();
        request.payload[offset..offset + 8].copy_from_slice(&token.to_le_bytes());
        request.payload[offset + 8..offset + 12].copy_from_slice(&events.to_le_bytes());
    }
    request.payload_len = (entries.len() * size_of::<NetdPollQueryWire>()) as u32;
    request
}

fn poll_batch_result(response: &NetdIpcResponse, index: usize) -> (u64, u32, i32) {
    let offset = index * size_of::<NetdPollResultWire>();
    (
        u64::from_le_bytes(response.payload[offset..offset + 8].try_into().unwrap()),
        u32::from_le_bytes(response.payload[offset + 8..offset + 12].try_into().unwrap()),
        i32::from_le_bytes(response.payload[offset + 12..offset + 16].try_into().unwrap()),
    )
}

#[test]
fn poll_batch_answers_every_socket_in_order_and_isolates_failures() {
    // Unique tokens: the batch handler reads the process-wide state.
    let (first, second, missing) = (0xba7c_0001_u64, 0xba7c_0002_u64, 0xba7c_0003_u64);
    {
        let mut state = lock_net_state();
        state.netlink_sockets.insert(first, netlink_socket(libc::SOCK_RAW as u64));
        state.netlink_sockets.insert(second, netlink_socket(libc::SOCK_RAW as u64));
    }
    let pollout = linux_abi::POLLOUT as u32;
    let request = poll_batch_request(&[(first, pollout), (missing, pollout), (second, pollout)]);
    let mut response = NetdIpcResponse::default();
    assert_eq!(handle_poll_batch(&request, &mut response), 0);
    assert_eq!(response.value, 3);
    assert_eq!(
        response.payload_len as usize,
        3 * size_of::<NetdPollResultWire>()
    );
    let (generation, revents, status) = poll_batch_result(&response, 0);
    assert_eq!((revents, status), (pollout, 0));
    assert_ne!(generation, 0);
    // The unknown socket fails alone; its neighbours are unaffected.
    assert_eq!(poll_batch_result(&response, 1), (0, 0, libc::EBADF));
    assert_eq!(poll_batch_result(&response, 2).1, pollout);

    // Per-socket generation moves only for the sockets that changed.
    let before = poll_batch_result(&response, 2).0;
    {
        let mut state = lock_net_state();
        make_readable(&mut state, first);
    }
    let mut after = NetdIpcResponse::default();
    assert_eq!(handle_poll_batch(&request, &mut after), 0);
    assert!(poll_batch_result(&after, 0).0 > poll_batch_result(&response, 0).0);
    assert_eq!(poll_batch_result(&after, 2).0, before);

    let mut state = lock_net_state();
    state.netlink_sockets.remove(&first);
    state.netlink_sockets.remove(&second);
    state.readiness_generations.remove(&first);
    state.readiness_generations.remove(&second);
}

#[test]
fn poll_batch_rejects_malformed_payloads() {
    let mut response = NetdIpcResponse::default();
    let mut request = poll_batch_request(&[(5, 1)]);
    request.payload_len -= 1;
    assert_eq!(handle_poll_batch(&request, &mut response), libc::EINVAL);
    request.payload_len = 0;
    assert_eq!(handle_poll_batch(&request, &mut response), libc::EINVAL);
    assert_eq!(
        handle_poll_batch(&poll_batch_request(&[(0, 1)]), &mut response),
        libc::EINVAL
    );
    let mut reserved = poll_batch_request(&[(5, 1)]);
    reserved.payload[12] = 1;
    assert_eq!(handle_poll_batch(&reserved, &mut response), libc::EINVAL);
}
