use super::with_isolated_ipc_test;

#[test]
fn endpoint_receiver_waiter_is_woken_by_next_call() {
    with_isolated_ipc_test(|| {
        let endpoint = super::super::create_endpoint().expect("create endpoint");
        super::super::add_endpoint_receiver_waiter(endpoint, 99).expect("add waiter");
        let (_reply, receiver) =
            super::super::enqueue_endpoint_call(endpoint, 1, b"request").expect("enqueue call");
        assert_eq!(receiver, Some(99));
    });
}

#[test]
fn endpoint_pending_message_does_not_publish_stale_receiver_waiter() {
    with_isolated_ipc_test(|| {
        let endpoint = super::super::create_endpoint().expect("create endpoint");
        super::super::enqueue_endpoint_call(endpoint, 1, b"first").expect("enqueue first call");

        assert_eq!(
            super::super::add_endpoint_receiver_waiter(endpoint, 99),
            Ok(true)
        );
        let (_reply, receiver) = super::super::enqueue_endpoint_call(endpoint, 2, b"second")
            .expect("enqueue second call");
        assert_eq!(receiver, None);
    });
}

#[test]
fn endpoint_system_calls_bypass_backlog_without_starving_ordinary_lane() {
    with_isolated_ipc_test(|| {
        let endpoint = super::super::create_endpoint().expect("create endpoint");
        let enqueue = |task_id, request: &'static [u8], priority| {
            super::super::enqueue_endpoint_call_with_handles_and_priority(
                endpoint,
                task_id,
                request,
                &[],
                priority,
            )
            .expect("enqueue prioritized endpoint call")
        };

        enqueue(
            1,
            b"ordinary-a",
            super::super::EndpointCallPriority::Ordinary,
        );
        enqueue(
            2,
            b"system-a",
            super::super::EndpointCallPriority::System,
        );
        enqueue(
            3,
            b"system-b",
            super::super::EndpointCallPriority::System,
        );
        enqueue(
            4,
            b"system-c",
            super::super::EndpointCallPriority::System,
        );
        enqueue(
            5,
            b"ordinary-b",
            super::super::EndpointCallPriority::Ordinary,
        );

        let mut received = alloc::vec::Vec::new();
        for _ in 0..5 {
            let (_reply, request) = super::super::recv_endpoint(endpoint)
                .expect("receive result")
                .expect("queued request");
            received.push(request);
        }
        assert_eq!(
            received,
            [
                b"system-a".to_vec(),
                b"system-b".to_vec(),
                b"ordinary-a".to_vec(),
                b"system-c".to_vec(),
                b"ordinary-b".to_vec(),
            ]
        );
    });
}
