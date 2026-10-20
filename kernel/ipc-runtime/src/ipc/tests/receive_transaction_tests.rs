use super::super::*;
use super::with_isolated_ipc_test;

fn counted_receive(
    endpoint: KernelEndpointHandle,
    receiver: u64,
    capacity: usize,
    may_wait: bool,
) -> Result<EndpointReceive, IpcError> {
    let class = LockClass::IpcEndpoint as usize;
    let _ = nucleus_core::util::lockdep::work_budget::take_class_census();
    let result = receive_or_wait(endpoint, receiver, capacity, 0, may_wait);
    let census = nucleus_core::util::lockdep::work_budget::take_class_census();
    assert_eq!(
        census[class], 1,
        "receive-or-wait must enter the endpoint once"
    );
    result
}

#[test]
fn receive_or_wait_is_one_endpoint_transaction_for_empty_queued_and_fast() {
    with_isolated_ipc_test(|| {
        let endpoint = create_endpoint().unwrap();
        assert!(matches!(
            counted_receive(endpoint, 11, 16, false),
            Ok(EndpointReceive::Empty)
        ));
        assert!(matches!(
            counted_receive(endpoint, 11, 16, true),
            Ok(EndpointReceive::Waiting)
        ));
        let (reply, receiver) = enqueue_endpoint_call(endpoint, 7, b"queued").unwrap();
        assert_eq!(receiver, Some(11));
        let EndpointReceive::Queued((received_reply, bytes, handles, sender)) =
            counted_receive(endpoint, 11, 16, true).unwrap()
        else {
            panic!("queued request was not delivered")
        };
        assert_eq!((received_reply, sender), (reply, 7));
        assert_eq!(bytes, b"queued");
        assert!(handles.is_empty());
        // A successful receive never publishes a phantom waiter.
        assert_eq!(remove_endpoint_waiter_for_task(endpoint, 11), 0);
        assert!(matches!(
            counted_receive(endpoint, 11, 16, true),
            Ok(EndpointReceive::Waiting)
        ));
        let (fast_reply, receiver) =
            reserve_fast_endpoint_call(endpoint, 70, 7, b"fast", None).unwrap();
        assert_eq!(receiver, 11);
        let EndpointReceive::Fast(received) = counted_receive(endpoint, 11, 16, true).unwrap()
        else {
            panic!("reserved fast request was not delivered")
        };
        assert_eq!(received.reply, fast_reply);
        assert_eq!(&received.request[..received.request_len], b"fast");
        assert_eq!(
            (received.caller_process_id, received.caller_task_id),
            (70, 7)
        );
    });
}

#[test]
fn receive_rejection_preserves_exact_fast_frame_and_publishes_no_waiter() {
    with_isolated_ipc_test(|| {
        let endpoint = create_endpoint().unwrap();
        assert!(matches!(
            receive_or_wait(endpoint, 11, 16, 0, true),
            Ok(EndpointReceive::Waiting)
        ));
        reserve_fast_endpoint_call(endpoint, 70, 7, b"four", None).unwrap();
        assert!(matches!(
            counted_receive(endpoint, 12, 16, true),
            Err(IpcError::PermissionDenied)
        ));
        assert!(matches!(
            counted_receive(endpoint, 11, 3, true),
            Err(IpcError::BufferTooSmall)
        ));
        assert_eq!(remove_endpoint_waiter_for_task(endpoint, 12), 0);
        assert_eq!(remove_endpoint_waiter_for_task(endpoint, 11), 0);
        assert!(matches!(
            counted_receive(endpoint, 11, 4, false),
            Ok(EndpointReceive::Fast(_))
        ));
    });
}

#[test]
fn receive_expiry_still_delivers_queued_request_without_registering_waiter() {
    with_isolated_ipc_test(|| {
        let endpoint = create_endpoint().unwrap();
        enqueue_endpoint_call(endpoint, 7, b"four").unwrap();
        assert!(matches!(
            counted_receive(endpoint, 11, 3, true),
            Err(IpcError::BufferTooSmall)
        ));
        assert!(matches!(
            counted_receive(endpoint, 11, 4, false),
            Ok(EndpointReceive::Queued(_))
        ));
        assert!(matches!(
            counted_receive(endpoint, 11, 4, false),
            Ok(EndpointReceive::Empty)
        ));
        assert_eq!(remove_endpoint_waiter_for_task(endpoint, 11), 0);
    });
}

#[test]
fn receiver_registration_is_bounded_deduplicated_and_refreshes_capacity() {
    with_isolated_ipc_test(|| {
        let endpoint = create_endpoint().unwrap();
        for receiver in 1..=MAX_ENDPOINT_WAITERS as u64 {
            assert!(matches!(
                receive_or_wait(endpoint, receiver, 1, 0, true),
                Ok(EndpointReceive::Waiting)
            ));
        }
        assert!(matches!(
            counted_receive(endpoint, 1, 4, true),
            Ok(EndpointReceive::Waiting)
        ));
        assert!(matches!(
            counted_receive(endpoint, 10000, 4, true),
            Err(IpcError::NoMemory)
        ));
        let (_, receiver) = reserve_fast_endpoint_call(endpoint, 70, 7000, b"four", None).unwrap();
        assert_eq!(receiver, 1);
    });
}

#[test]
fn received_request_custody_observation_requires_actual_dequeue_and_exact_live_reply() {
    with_isolated_ipc_test(|| {
        let endpoint = create_endpoint_for_process(10).unwrap();
        let custody = super::test_scheduling_custody(41);
        let (reply, _) = enqueue_endpoint_call_with_handles_priority_and_custody(
            endpoint, 41, b"original", &[], EndpointCallPriority::Ordinary, custody).unwrap();
        assert!(!received_request_retains_custody(reply, 41));
        assert_eq!(receive_or_wait(endpoint, 10, 1, 0, false).unwrap_err(), IpcError::BufferTooSmall);
        assert!(!received_request_retains_custody(reply, 41));
        assert!(matches!(receive_or_wait(endpoint, 10, 16, 0, false).unwrap(), EndpointReceive::Queued(_)));
        assert!(received_request_retains_custody(reply, 41));
        assert!(!received_request_retains_custody(reply, 42));
        assert!(!received_request_retains_custody(KernelReplyHandle::from_raw(reply.raw() ^ (1 << 48)), 41));

        let (other, _) = enqueue_endpoint_call_with_handles_priority_and_custody(
            endpoint, 41, b"other", &[], EndpointCallPriority::Ordinary, custody).unwrap();
        assert!(matches!(receive_or_wait(endpoint, 10, 16, 0, false).unwrap(), EndpointReceive::Queued(_)));
        let original_message = REPLIES.with(reply.raw(), |object| object.message_id).unwrap();
        let other_message = REPLIES.with(other.raw(), |object| object.message_id).unwrap();
        // Explicit earlier malformed-candidate input for a NEGATIVE observer
        // test, never a transport/SC grant. Restore before real cancellation.
        REPLIES.with_mut(reply.raw(), |object| object.message_id = other_message).unwrap();
        assert!(!received_request_retains_custody(reply, 41));
        REPLIES.with_mut(reply.raw(), |object| object.message_id = original_message).unwrap();
        cancel_endpoint_call_with_transfers(other, 41).unwrap();
        assert!(!received_request_retains_custody(other, 41));

        // Exercise the SAME production cancel mark before physical removal.
        assert!(mark_endpoint_call_consumed(original_message, reply.raw(), 41));
        assert!(!received_request_retains_custody(reply, 41));
        drop(ENDPOINT_MESSAGES.remove(original_message));
        drop(REPLIES.remove(reply.raw()));
        assert!(!received_request_retains_custody(reply, 41));

        // Enqueue itself refuses a foreign SC caller. Inject that malformed
        // earlier input only into the NEGATIVE read-only observer fixture,
        // after a real, valid enqueue/dequeue, then restore before cancellation.
        let (mismatched, _) = enqueue_endpoint_call_with_handles_priority_and_custody(
            endpoint, 41, b"mismatch", &[], EndpointCallPriority::Ordinary, custody).unwrap();
        assert!(matches!(receive_or_wait(endpoint, 10, 16, 0, false).unwrap(), EndpointReceive::Queued(_)));
        REPLIES.with_mut(mismatched.raw(), |object|
            object.scheduling_context = Some(super::test_scheduling_custody(42))).unwrap();
        assert!(!received_request_retains_custody(mismatched, 41));
        assert!(!received_request_retains_custody(mismatched, 42));
        REPLIES.with_mut(mismatched.raw(), |object| object.scheduling_context = Some(custody)).unwrap();
        cancel_endpoint_call_with_transfers(mismatched, 41).unwrap();
    });
}

#[test]
fn concurrent_enqueue_and_receive_have_no_unowned_empty_window() {
    with_isolated_ipc_test(|| {
        for _ in 0..64 {
            let endpoint = create_endpoint().unwrap();
            let barrier = std::sync::Barrier::new(2);
            let (received, enqueued) = std::thread::scope(|scope| {
                let receiver = scope.spawn(|| {
                    barrier.wait();
                    receive_or_wait(endpoint, 11, 16, 0, true).unwrap()
                });
                barrier.wait();
                let sent = enqueue_endpoint_call(endpoint, 7, b"request").unwrap();
                (receiver.join().unwrap(), sent)
            });
            match received {
                EndpointReceive::Waiting => {
                    assert_eq!(enqueued.1, Some(11), "published wait must be woken");
                    assert!(matches!(
                        receive_or_wait(endpoint, 11, 16, 0, false),
                        Ok(EndpointReceive::Queued(_))
                    ));
                }
                EndpointReceive::Queued((reply, bytes, _, sender)) => {
                    assert_eq!(enqueued.1, None, "delivery must not leave a waiter");
                    assert_eq!(reply, enqueued.0);
                    assert_eq!(sender, 7);
                    assert_eq!(bytes, b"request");
                }
                other => panic!("unexpected receive result: {other:?}"),
            }
            assert_eq!(remove_endpoint_waiter_for_task(endpoint, 11), 0);
        }
    });
}
