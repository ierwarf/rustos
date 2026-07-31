use super::with_isolated_ipc_test;

#[test]
fn endpoint_receiver_waiter_is_woken_by_next_call() {
    with_isolated_ipc_test(|| {
        let endpoint = super::super::create_endpoint().expect("create endpoint");
        super::super::add_endpoint_receiver_waiter(endpoint, 99).expect("add waiter");
        let (_reply, receiver) = super::super::enqueue_endpoint_call(endpoint, 1, b"request")
            .expect("enqueue call");
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
