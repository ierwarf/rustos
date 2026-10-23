//! Compact images exercise the SAME objects, generations and consume boundary.
use super::super::*;
use super::with_isolated_ipc_test;

#[test]
fn compact_fast_images_preserve_exact_authority_padding_and_one_shot_reply() {
    with_isolated_ipc_test(|| {
        assert!(
            core::mem::size_of::<FastEndpointReceived<16>>()
                < core::mem::size_of::<FastEndpointReceived>()
        );
        assert!(
            core::mem::size_of::<FastEndpointResponseTake<16>>()
                < core::mem::size_of::<FastEndpointResponseTake>()
        );
        let endpoint = create_endpoint().unwrap();
        for len in 1..=16 {
            assert!(matches!(
                receive_or_wait_inline::<16>(endpoint, 11, 16, 0, true),
                Ok(EndpointReceive::Waiting)
            ));
            let data = [len as u8; 16];
            let (reply, receiver) = reserve_fast_endpoint_call_with_response_capacity(
                endpoint,
                70,
                7,
                &data[..len],
                16,
                None,
            )
            .unwrap();
            assert_eq!(receiver, 11);
            assert_eq!(
                take_fast_endpoint_request_inline::<16>(endpoint, 12, 16),
                Err(IpcError::PermissionDenied)
            );
            let taken = take_fast_endpoint_request_inline::<16>(endpoint, 11, 16)
                .unwrap()
                .unwrap();
            assert_eq!(
                (taken.reply, taken.caller_process_id, taken.caller_task_id),
                (reply, 70, 7)
            );
            assert_eq!(taken.request_len, len);
            assert_eq!(&taken.request[..len], &data[..len]);
            assert!(taken.request[len..].iter().all(|&b| b == 0));
            complete_fast_endpoint_reply_for_task(reply, 11, &data[..len]).unwrap();
            assert!(matches!(
                take_fast_endpoint_response_inline::<16>(reply, 8),
                Err(IpcError::PermissionDenied)
            ));
            let FastEndpointResponseTake::Response {
                response_len,
                response,
            } = take_fast_endpoint_response_inline::<16>(reply, 7).unwrap()
            else {
                panic!("lost reply")
            };
            assert_eq!(response_len, len);
            assert_eq!(&response[..len], &data[..len]);
            assert!(response[len..].iter().all(|&b| b == 0));
            assert!(matches!(
                take_fast_endpoint_response_inline::<16>(reply, 7),
                Err(IpcError::InvalidHandle)
            ));
        }
    });
}

#[test]
fn compact_request_refusal_preserves_frame_and_response_overflow_retires_once() {
    with_isolated_ipc_test(|| {
        let endpoint = create_endpoint().unwrap();
        add_endpoint_receiver_waiter_with_capacity(endpoint, 11, 32).unwrap();
        let (reply, _) =
            reserve_fast_endpoint_call_with_response_capacity(endpoint, 70, 7, &[3; 17], 32, None)
                .unwrap();
        assert_eq!(
            take_fast_endpoint_request_inline::<16>(endpoint, 11, 32),
            Err(IpcError::BufferTooSmall)
        );
        assert!(matches!(
            receive_or_wait_inline::<16>(endpoint, 11, 32, 0, true),
            Err(IpcError::BufferTooSmall)
        ));
        let taken = take_fast_endpoint_request(endpoint, 11).unwrap().unwrap();
        assert_eq!((taken.reply, taken.request_len), (reply, 17));
        complete_fast_endpoint_reply_for_task(reply, 11, &[4; 17]).unwrap();
        assert!(matches!(
            take_fast_endpoint_response_inline::<16>(reply, 7),
            Err(IpcError::BufferTooSmall)
        ));
        assert!(matches!(
            take_fast_endpoint_response(reply, 7),
            Err(IpcError::InvalidHandle)
        ));
    });
}

#[test]
fn compact_queued_capacity_refusal_keeps_legacy_large_message_deliverable() {
    with_isolated_ipc_test(|| {
        let endpoint = create_endpoint().unwrap();
        let data = [9; 300];
        let (reply, _) = enqueue_endpoint_call(endpoint, 7, &data).unwrap();
        assert!(matches!(
            receive_or_wait_inline::<16>(endpoint, 11, 512, 0, true),
            Err(IpcError::BufferTooSmall)
        ));
        let EndpointReceive::Queued((received, request, handles, caller)) =
            receive_or_wait(endpoint, 11, 512, 0, true).unwrap()
        else {
            panic!("legacy queue lost")
        };
        assert_eq!((received, caller), (reply, 7));
        assert_eq!(request, data);
        assert!(handles.is_empty());
        complete_endpoint_reply(reply, &[]).unwrap();
        assert_eq!(take_endpoint_response(reply).unwrap(), Some(alloc::vec![]));
    });
}
