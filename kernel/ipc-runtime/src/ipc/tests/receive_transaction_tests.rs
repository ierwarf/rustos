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
            Ok(EndpointReceive::Waiting)
        ));
        assert!(matches!(
            counted_receive(endpoint, 11, 3, true),
            Err(IpcError::BufferTooSmall)
        ));
        assert_eq!(remove_endpoint_waiter_for_task(endpoint, 12), 1);
        assert_eq!(remove_endpoint_waiter_for_task(endpoint, 11), 0);
        assert!(matches!(
            counted_receive(endpoint, 11, 4, false),
            Ok(EndpointReceive::Fast(_))
        ));
    });
}

#[test]
fn unrelated_receiver_can_take_queued_work_while_an_exact_fast_frame_is_pinned() {
    with_isolated_ipc_test(|| {
        let endpoint = create_endpoint().unwrap();
        assert!(matches!(receive_or_wait(endpoint, 11, 16, 0, true), Ok(EndpointReceive::Waiting)));
        let (fast, receiver) = reserve_fast_endpoint_call(endpoint, 70, 7, b"fast", None).unwrap();
        assert_eq!(receiver, 11);
        let (queued, _) = enqueue_endpoint_call(endpoint, 8, b"queued").unwrap();
        let EndpointReceive::Queued((reply, request, _, caller)) = receive_or_wait(endpoint, 12, 16, 0, false).unwrap() else { panic!("other receiver must take queued work"); };
        assert_eq!((reply, caller), (queued, 8));
        assert_eq!(request, b"queued");
        let EndpointReceive::Fast(received) = receive_or_wait(endpoint, 11, 16, 0, false).unwrap() else { panic!("pinned receiver must retain its exact fast frame"); };
        assert_eq!(received.reply, fast);
        assert_eq!(&received.request[..received.request_len], b"fast");
    });
}

#[test]
fn insufficient_handle_capacity_preserves_request_and_allows_a_later_capable_receiver() {
    with_isolated_ipc_test(|| {
        let endpoint = create_endpoint().unwrap();
        let handle = super::transferable_file_handle(11);
        let (reply, _) = enqueue_endpoint_call_with_handles(endpoint, 7, b"open", &[handle]).unwrap();
        assert!(matches!(receive_or_wait(endpoint, 11, 16, 0, false), Err(IpcError::BufferTooSmall)));
        let EndpointReceive::Queued((taken_reply, bytes, handles, caller)) = receive_or_wait(endpoint, 12, 16, 1, false).unwrap() else { panic!("refused request was lost"); };
        assert_eq!((taken_reply, caller), (reply, 7));
        assert_eq!(bytes, b"open");
        assert_eq!(handles, alloc::vec![handle]);
        assert!(matches!(receive_or_wait(endpoint, 12, 16, 1, false), Ok(EndpointReceive::Empty)));
    });
}

#[test]
fn byte_only_admission_rejects_attachments_without_wedging_the_next_call() {
    with_isolated_ipc_test(|| {
        let endpoint = create_endpoint_for_process_with_limits(10, 16, 0, None).unwrap();
        assert!(matches!(receive_or_wait(endpoint, 10, 16, 0, true), Ok(EndpointReceive::Waiting)));
        let handle = super::transferable_file_handle(11);
        assert_eq!(validate_endpoint_request_shape(endpoint, 6, 1), Err(IpcError::InvalidArgument));
        assert_eq!(validate_endpoint_request_shape(endpoint, 17, 0), Err(IpcError::InvalidArgument));
        assert_eq!(enqueue_endpoint_call(endpoint, 41, b"0123456789abcdefg"), Err(IpcError::InvalidArgument));
        assert_eq!(reserve_fast_endpoint_call(endpoint, 41, 41, b"0123456789abcdefg", None), Err(IpcError::InvalidArgument));
        assert_eq!(enqueue_endpoint_call_with_handles_priority_and_custody(endpoint, 41, b"attack", &[handle], EndpointCallPriority::Ordinary, super::test_scheduling_custody(41)), Err(IpcError::InvalidArgument));
        assert!(REPLIES.find_handle(|_| true).is_none());
        assert!(ENDPOINT_MESSAGES.find_handle(|_| true).is_none());
        assert_eq!(ENDPOINTS.with(endpoint.raw(), |object| object.waiting_receivers.len()), Some(1));
        let (reply, receiver) = enqueue_endpoint_call(endpoint, 41, b"legitimate").unwrap();
        assert_eq!(receiver, Some(10));
        let EndpointReceive::Queued((received, bytes, handles, _)) = receive_or_wait(endpoint, 10, 16, 0, false).unwrap() else { panic!("legitimate request was blocked"); };
        assert_eq!(received, reply);
        assert_eq!(bytes, b"legitimate");
        assert!(handles.is_empty());
    });
}

#[test]
fn declared_handle_limit_preserves_generic_receivers_but_rejects_oversized_batches() {
    with_isolated_ipc_test(|| {
        assert_eq!(create_endpoint_for_process_with_limits(10, 16, MAX_ENDPOINT_TRANSFER_HANDLES + 1, None), Err(IpcError::InvalidArgument));
        assert_eq!(create_endpoint_for_process_with_limits(10, 0, 0, None), Err(IpcError::InvalidArgument));
        assert_eq!(create_endpoint_for_process_with_limits(10, MAX_ENDPOINT_INLINE_MESSAGE_BYTES + 1, 0, None), Err(IpcError::InvalidArgument));
        let endpoint = create_endpoint_for_process_with_limits(10, 16, 1, None).unwrap();
        let handle = super::transferable_file_handle(11);
        assert_eq!(enqueue_endpoint_call_with_handles(endpoint, 41, b"too many", &[handle, handle]), Err(IpcError::InvalidArgument));
        let (reply, _) = enqueue_endpoint_call_with_handles(endpoint, 41, b"placement", &[handle]).unwrap();
        assert!(matches!(receive_or_wait(endpoint, 10, 16, 0, false), Err(IpcError::BufferTooSmall)));
        let EndpointReceive::Queued((received, _, handles, _)) = receive_or_wait(endpoint, 11, 16, 1, false).unwrap() else { panic!("capable receiver lost the request"); };
        assert_eq!(received, reply);
        assert_eq!(handles, alloc::vec![handle]);
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
