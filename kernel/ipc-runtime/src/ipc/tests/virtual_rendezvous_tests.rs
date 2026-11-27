use super::super::virtual_rendezvous::{
    VIRTUAL_MAX_BYTES, VIRTUAL_SLOT_COUNT, is_virtual_reply_token,
};
use super::super::*;
use super::with_isolated_ipc_test;

#[test]
fn register_calls_use_disjoint_virtual_tokens_without_touching_replies_slab() {
    with_isolated_ipc_test(|| {
        let endpoint = create_endpoint_for_process(70).unwrap();
        let mut previous = None;
        for seq in 0..32_u64 {
            add_endpoint_receiver_waiter_with_capacity(endpoint, 11, VIRTUAL_MAX_BYTES).unwrap();
            let payload = seq.to_le_bytes();
            let custody = super::test_scheduling_custody(7);
            let (reply, receiver) = reserve_fast_endpoint_call_with_response_capacity(
                endpoint,
                70,
                7,
                &payload,
                VIRTUAL_MAX_BYTES,
                Some(custody),
            )
            .unwrap();
            assert_eq!(receiver, 11);
            assert!(is_virtual_reply_token(reply.raw()));
            assert_eq!(reply.raw() & 0xffff, 0);
            assert_eq!(slab::slot_index::<MAX_REPLY_OBJECTS>(reply.raw()), None);
            assert_eq!(REPLIES.occupied_count(), 0);
            if let Some(stale) = previous {
                assert_ne!(reply, stale);
                assert_eq!(
                    take_fast_endpoint_response_inline::<16>(stale, 7),
                    Err(IpcError::InvalidHandle)
                );
            }
            let received = take_fast_endpoint_request_inline::<16>(endpoint, 11, VIRTUAL_MAX_BYTES)
                .unwrap()
                .unwrap();
            assert_eq!((received.reply, received.caller_task_id), (reply, 7));
            assert_eq!(received.bytes(), &payload);
            assert!(received_request_retains_custody(reply, 7));
            assert!(fast_endpoint_request_owned_by_receiver(reply, 7));
            assert_eq!(endpoint_receiver_process_for_reply(reply), Some(70));
            let completion = complete_fast_endpoint_reply_for_task(reply, 11, &payload).unwrap();
            assert_eq!(completion.completion.scheduling_context, Some(custody));
            assert_eq!(completion.terminal_error, None);
            assert!(!received_request_retains_custody(reply, 7));
            let FastEndpointResponseTake::Response {
                response_len,
                response,
            } = take_fast_endpoint_response_inline::<16>(reply, 7).unwrap()
            else {
                panic!("missing virtual response");
            };
            assert_eq!(response_len, 8);
            assert_eq!(&response[..8], &payload);
            assert_eq!(REPLIES.occupied_count(), 0);
            previous = Some(reply);
        }
    });
}

#[test]
fn virtual_call_rollback_before_and_after_take_matches_slab_semantics() {
    with_isolated_ipc_test(|| {
        let endpoint = create_endpoint().unwrap();
        add_endpoint_receiver_waiter_with_capacity(endpoint, 41, 16).unwrap();
        let custody = super::test_scheduling_custody(40);
        let (reply, receiver) = reserve_fast_endpoint_call_with_response_capacity(
            endpoint,
            400,
            40,
            b"hello",
            16,
            Some(custody),
        )
        .unwrap();
        assert_eq!(receiver, 41);
        assert!(is_virtual_reply_token(reply.raw()));
        assert!(!fast_endpoint_request_owned_by_receiver(reply, 40));
        assert_eq!(
            rollback_fast_endpoint_call(endpoint, reply, 40, 42),
            Err(IpcError::PermissionDenied)
        );
        let rollback = rollback_fast_endpoint_call(endpoint, reply, 40, 41).unwrap();
        assert_eq!(rollback.receiver_task_id, 41);
        assert_eq!(rollback.scheduling_context, Some(custody));
        assert_eq!(
            rollback_fast_endpoint_call(endpoint, reply, 40, 41),
            Err(IpcError::InvalidHandle)
        );

        let custody2 = super::test_scheduling_custody(40);
        let (reply2, _) = reserve_fast_endpoint_call_with_response_capacity(
            endpoint,
            400,
            40,
            b"world",
            16,
            Some(custody2),
        )
        .unwrap();
        assert!(is_virtual_reply_token(reply2.raw()));
        let taken = take_fast_endpoint_request_inline::<16>(endpoint, 41, 16)
            .unwrap()
            .unwrap();
        assert_eq!(taken.reply, reply2);
        assert!(fast_endpoint_request_owned_by_receiver(reply2, 40));
        assert!(received_request_retains_custody(reply2, 40));
        assert_eq!(
            rollback_fast_endpoint_call(endpoint, reply2, 40, 41),
            Err(IpcError::PermissionDenied)
        );
        let completion = complete_fast_endpoint_reply_for_task(reply2, 41, b"ok").unwrap();
        assert_eq!(completion.completion.scheduling_context, Some(custody2));
        assert!(matches!(
            take_fast_endpoint_response_inline::<16>(reply2, 40),
            Ok(FastEndpointResponseTake::Response { response_len: 2, .. })
        ));
    });
}

#[test]
fn virtual_call_cancellation_task_exit_and_owner_exit_settle_once() {
    with_isolated_ipc_test(|| {
        let endpoint = create_endpoint_for_task(Some(90)).unwrap();
        add_endpoint_receiver_waiter_with_capacity(endpoint, 90, 16).unwrap();
        let custody = super::test_scheduling_custody(31);
        let (cancelled_reply, _) = reserve_fast_endpoint_call_with_response_capacity(
            endpoint,
            310,
            31,
            b"cancel",
            16,
            Some(custody),
        )
        .unwrap();
        assert!(is_virtual_reply_token(cancelled_reply.raw()));
        let cancelled = cancel_endpoint_call_with_transfers(cancelled_reply, 31).unwrap();
        assert_eq!(cancelled.disposition, CancelledCallDisposition::Queued);
        assert_eq!(cancelled.scheduling_context, Some(custody));
        assert_eq!(
            take_fast_endpoint_response_inline::<16>(cancelled_reply, 31),
            Err(IpcError::InvalidHandle)
        );

        // Task teardown cancels virtual calls queued or in-flight.
        add_endpoint_receiver_waiter_with_capacity(endpoint, 90, 16).unwrap();
        let exit_custody = super::test_scheduling_custody(32);
        let (task_reply, _) = reserve_fast_endpoint_call_with_response_capacity(
            endpoint,
            320,
            32,
            b"exit",
            16,
            Some(exit_custody),
        )
        .unwrap();
        assert!(is_virtual_reply_token(task_reply.raw()));
        let _ = take_fast_endpoint_request_inline::<16>(endpoint, 90, 16)
            .unwrap()
            .unwrap();
        let mut released = std::vec::Vec::new();
        let count = cancel_endpoint_calls_for_task(
            32,
            |_| {},
            |reply, c| released.push((reply, c)),
        );
        assert_eq!(count, 1);
        assert_eq!(released, std::vec![(task_reply, exit_custody)]);
        assert_eq!(
            complete_fast_endpoint_reply_for_task(task_reply, 90, b"late"),
            Err(IpcError::InvalidHandle)
        );

        // Endpoint owner teardown fails in-flight virtual calls without allocation.
        add_endpoint_receiver_waiter_with_capacity(endpoint, 90, 16).unwrap();
        let fail_custody = super::test_scheduling_custody(33);
        let (failed_reply, _) = reserve_fast_endpoint_call_with_response_capacity(
            endpoint,
            330,
            33,
            b"fail",
            16,
            Some(fail_custody),
        )
        .unwrap();
        assert!(is_virtual_reply_token(failed_reply.raw()));
        let _ = take_fast_endpoint_request_inline::<16>(endpoint, 90, 16)
            .unwrap()
            .unwrap();
        let wake = fail_endpoints_owned_by_task(90, IpcError::PeerClosed);
        assert_eq!(wake.callers(), &[33]);
        assert_eq!(
            wake.scheduling_contexts().collect::<std::vec::Vec<_>>(),
            std::vec![ReplySchedulingContextReturn {
                reply: failed_reply,
                custody: fail_custody,
            }]
        );
        assert_eq!(
            take_fast_endpoint_response_inline::<16>(failed_reply, 33),
            Ok(FastEndpointResponseTake::Error(IpcError::PeerClosed))
        );
    });
}

#[test]
fn virtual_saturation_falls_back_cleanly_to_replies_slab() {
    with_isolated_ipc_test(|| {
        let mut pairs = std::vec::Vec::new();
        for i in 0..VIRTUAL_SLOT_COUNT {
            let endpoint = create_endpoint().unwrap();
            let receiver = 1000 + i as u64;
            let caller = 2000 + i as u64;
            add_endpoint_receiver_waiter_with_capacity(endpoint, receiver, 16).unwrap();
            let (reply, _) = reserve_fast_endpoint_call_with_response_capacity(
                endpoint, caller, caller, b"pin", 16, None,
            )
            .unwrap();
            assert!(is_virtual_reply_token(reply.raw()));
            pairs.push((reply, caller));
        }

        let spill_ep = create_endpoint().unwrap();
        add_endpoint_receiver_waiter_with_capacity(spill_ep, 9999, 16).unwrap();
        let (fallback_reply, _) = reserve_fast_endpoint_call_with_response_capacity(
            spill_ep, 8888, 8888, b"spill", 16, None,
        )
        .unwrap();
        assert!(!is_virtual_reply_token(fallback_reply.raw()));
        assert_eq!(REPLIES.occupied_count(), 1);
        take_fast_endpoint_request_inline::<16>(spill_ep, 9999, 16)
            .unwrap()
            .unwrap();
        complete_fast_endpoint_reply_for_task(fallback_reply, 9999, b"ok").unwrap();
        assert!(matches!(
            take_fast_endpoint_response_inline::<16>(fallback_reply, 8888),
            Ok(FastEndpointResponseTake::Response { response_len: 2, .. })
        ));
        assert_eq!(REPLIES.occupied_count(), 0);

        for (reply, caller) in pairs {
            cancel_endpoint_call_with_transfers(reply, caller).unwrap();
        }
    });
}

#[test]
fn concurrent_virtual_replies_publish_custody_once() {
    with_isolated_ipc_test(|| {
        let endpoint = create_endpoint().unwrap();
        add_endpoint_receiver_waiter_with_capacity(endpoint, 31, 16).unwrap();
        let custody = super::test_scheduling_custody(7);
        let (reply, _) = reserve_fast_endpoint_call_with_response_capacity(
            endpoint, 70, 7, b"race", 16, Some(custody),
        )
        .unwrap();
        assert!(is_virtual_reply_token(reply.raw()));
        take_fast_endpoint_request_inline::<16>(endpoint, 31, 16)
            .unwrap()
            .unwrap();
        let barrier = std::sync::Barrier::new(3);
        let results = std::thread::scope(|scope| {
            let a = scope.spawn(|| {
                barrier.wait();
                complete_fast_endpoint_reply_for_task(reply, 31, b"first")
            });
            let b = scope.spawn(|| {
                barrier.wait();
                complete_fast_endpoint_reply_for_task(reply, 31, b"second")
            });
            barrier.wait();
            [a.join().unwrap(), b.join().unwrap()]
        });
        assert_eq!(results.iter().filter(|r| r.is_ok()).count(), 1);
        assert_eq!(
            results
                .iter()
                .filter(|r| matches!(r, Err(IpcError::InvalidArgument)))
                .count(),
            1
        );
        let completion = results.into_iter().find_map(Result::ok).unwrap();
        assert_eq!(completion.completion.scheduling_context, Some(custody));
        let FastEndpointResponseTake::Response {
            response_len,
            response,
        } = take_fast_endpoint_response_inline::<16>(reply, 7).unwrap()
        else {
            panic!("missing winner response");
        };
        assert!(&response[..response_len] == b"first" || &response[..response_len] == b"second");
    });
}
