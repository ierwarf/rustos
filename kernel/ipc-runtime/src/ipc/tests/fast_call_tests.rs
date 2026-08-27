use super::with_isolated_ipc_test;

#[test]
fn fast_call_uses_fixed_frame_and_exact_receiver_caller_identities() {
    with_isolated_ipc_test(|| {
        let endpoint = super::super::create_endpoint().expect("create endpoint");
        assert_eq!(
            super::super::reserve_fast_endpoint_call(endpoint, 70, 7, b"getuid", None),
            Err(super::super::IpcError::NoMemory)
        );
        super::super::add_endpoint_receiver_waiter(endpoint, 7).expect("add self waiter");
        assert_eq!(
            super::super::reserve_fast_endpoint_call(endpoint, 70, 7, b"getuid", None),
            Err(super::super::IpcError::NoMemory)
        );
        assert_eq!(super::super::remove_endpoint_waiters_for_task(7), 1);
        super::super::add_endpoint_receiver_waiter(endpoint, 11).expect("add exact waiter");
        let (reply, receiver) =
            super::super::reserve_fast_endpoint_call(endpoint, 70, 7, b"getuid", None)
                .expect("reserve fixed call frame");
        assert_eq!(receiver, 11);
        assert_eq!(
            super::super::take_fast_endpoint_request(endpoint, 12),
            Err(super::super::IpcError::PermissionDenied)
        );
        let received = super::super::take_fast_endpoint_request(endpoint, 11)
            .expect("take fast request")
            .expect("published fast request");
        assert_eq!(received.reply, reply);
        assert_eq!(received.caller_process_id, 70);
        assert_eq!(received.caller_task_id, 7);
        assert_eq!(&received.request[..received.request_len], b"getuid");
        assert_eq!(
            super::super::complete_fast_endpoint_reply_for_task(reply, 12, b"1000"),
            Err(super::super::IpcError::PermissionDenied)
        );
        let completion = super::super::complete_fast_endpoint_reply_for_task(reply, 11, b"1000")
            .expect("complete fixed reply");
        assert_eq!(completion.completion.caller_task_id, 7);
        assert_eq!(completion.terminal_error, None);
        let response =
            super::super::take_fast_endpoint_response(reply, 7).expect("take fixed response");
        let super::super::FastEndpointResponseTake::Response {
            response_len,
            response,
        } = response
        else {
            panic!("completed fixed response remained pending");
        };
        assert_eq!(&response[..response_len], b"1000");
        assert_eq!(
            super::super::take_fast_endpoint_response(reply, 7),
            Err(super::super::IpcError::InvalidHandle)
        );
    });
}

#[test]
fn fast_call_rejects_oversize_and_ordinary_queue_without_partial_publication() {
    with_isolated_ipc_test(|| {
        let endpoint = super::super::create_endpoint().expect("create endpoint");
        assert_eq!(super::super::IPC_FAST_INLINE_BYTES, 256);
        super::super::add_endpoint_receiver_waiter_with_capacity(endpoint, 21, 3)
            .expect("add capacity-bounded waiter");
        assert_eq!(
            super::super::reserve_fast_endpoint_call(endpoint, 80, 8, b"four", None),
            Err(super::super::IpcError::NoMemory)
        );
        let oversized = [0_u8; 257];
        assert_eq!(
            super::super::reserve_fast_endpoint_call(endpoint, 80, 8, &oversized, None),
            Err(super::super::IpcError::InvalidArgument)
        );
        let (_slow_reply, receiver) =
            super::super::enqueue_endpoint_call(endpoint, 8, b"slow").expect("slow enqueue");
        assert_eq!(receiver, Some(21));
        assert_eq!(
            super::super::reserve_fast_endpoint_call(endpoint, 90, 9, b"fast", None),
            Err(super::super::IpcError::NoMemory)
        );
        let received = super::super::recv_endpoint(endpoint)
            .expect("slow receive")
            .expect("slow request retained");
        assert_eq!(received.1, b"slow");
    });
}

#[test]
fn fast_call_response_capacity_failure_is_terminal_and_wakes_the_caller() {
    with_isolated_ipc_test(|| {
        let endpoint = super::super::create_endpoint().expect("create endpoint");
        super::super::add_endpoint_receiver_waiter_with_capacity(endpoint, 51, 16)
            .expect("add waiter");
        let (reply, receiver) = super::super::reserve_fast_endpoint_call_with_response_capacity(
            endpoint, 500, 50, b"request", 2, None,
        )
        .expect("reserve bounded response frame");
        assert_eq!(receiver, 51);
        super::super::take_fast_endpoint_request(endpoint, receiver)
            .expect("take request")
            .expect("request published");
        let published =
            super::super::complete_fast_endpoint_reply_for_task(reply, receiver, b"four")
                .expect("publish terminal capacity failure");
        assert_eq!(published.completion.caller_task_id, 50);
        assert_eq!(
            published.terminal_error,
            Some(super::super::IpcError::BufferTooSmall)
        );
        assert_eq!(
            super::super::take_fast_endpoint_response(reply, 50),
            Ok(super::super::FastEndpointResponseTake::Error(
                super::super::IpcError::BufferTooSmall
            ))
        );
    });
}

#[test]
fn fast_call_rollback_restores_exact_front_waiter_and_custody() {
    with_isolated_ipc_test(|| {
        let endpoint = super::super::create_endpoint().expect("create endpoint");
        super::super::add_endpoint_receiver_waiter_with_capacity(endpoint, 41, 7)
            .expect("add first waiter");
        super::super::add_endpoint_receiver_waiter(endpoint, 42).expect("add second waiter");
        let custody = super::test_scheduling_custody(40);
        let (reply, receiver) =
            super::super::reserve_fast_endpoint_call(endpoint, 400, 40, b"seven12", Some(custody))
                .expect("reserve rollback frame");
        assert_eq!(receiver, 41);
        assert_eq!(
            super::super::rollback_fast_endpoint_call(endpoint, reply, 40, 42),
            Err(super::super::IpcError::PermissionDenied)
        );
        let rollback = super::super::rollback_fast_endpoint_call(endpoint, reply, 40, 41)
            .expect("rollback exact frame");
        assert_eq!(rollback.receiver_task_id, 41);
        assert_eq!(rollback.scheduling_context, Some(custody));
        assert_eq!(
            super::super::rollback_fast_endpoint_call(endpoint, reply, 40, 41),
            Err(super::super::IpcError::InvalidHandle)
        );

        assert_eq!(
            super::super::reserve_fast_endpoint_call(endpoint, 430, 43, b"eight123", None),
            Err(super::super::IpcError::NoMemory)
        );
        let (next_reply, next_receiver) =
            super::super::reserve_fast_endpoint_call(endpoint, 430, 43, b"seven12", None)
                .expect("retry uses restored front waiter");
        assert_eq!(next_receiver, 41);
        super::super::cancel_endpoint_call(next_reply, 43).expect("cancel retry");
    });
}

#[test]
fn fast_call_cancel_and_endpoint_failure_have_one_terminal_owner() {
    with_isolated_ipc_test(|| {
        let endpoint = super::super::create_endpoint_for_task(Some(90)).expect("create endpoint");
        super::super::add_endpoint_receiver_waiter(endpoint, 90).expect("add waiter");
        let (cancelled_reply, _) =
            super::super::reserve_fast_endpoint_call(endpoint, 310, 31, b"cancel", None)
                .expect("reserve cancellable frame");
        let cancelled = super::super::cancel_endpoint_call_with_transfers(cancelled_reply, 31)
            .expect("cancel exact fast frame");
        assert_eq!(
            cancelled.disposition,
            super::super::CancelledCallDisposition::Queued
        );
        assert_eq!(
            super::super::take_fast_endpoint_response(cancelled_reply, 31),
            Err(super::super::IpcError::InvalidHandle)
        );

        super::super::add_endpoint_receiver_waiter(endpoint, 90).expect("re-add waiter");
        let (failed_reply, _) =
            super::super::reserve_fast_endpoint_call(endpoint, 320, 32, b"fail", None)
                .expect("reserve fail frame");
        let wake =
            super::super::fail_endpoints_owned_by_task(90, super::super::IpcError::PeerClosed);
        assert_eq!(wake.callers(), &[32]);
        assert_eq!(
            super::super::take_fast_endpoint_response(failed_reply, 32),
            Ok(super::super::FastEndpointResponseTake::Error(
                super::super::IpcError::PeerClosed
            ))
        );
        assert_eq!(
            super::super::complete_fast_endpoint_reply_for_task(failed_reply, 90, b"late"),
            Err(super::super::IpcError::InvalidHandle)
        );
    });
}

/// Acquisitions of the endpoint slab lock during `body`.
///
/// A performance invariant needs a number: both forms below produce the same
/// waiter list, so nothing but a count distinguishes touching one object from
/// walking every slot of the slab.
fn endpoint_lock_acquisitions_during<R>(body: impl FnOnce() -> R) -> (R, u64) {
    let class = nucleus_core::util::lockdep::LockClass::IpcEndpoint as usize;
    let _ = nucleus_core::util::lockdep::work_budget::take_class_census();
    let value = body();
    let census = nucleus_core::util::lockdep::work_budget::take_class_census();
    (value, census[class])
}

/// The exact withdrawal enters the slab once; the whole-slab form enters it
/// once per slot, which is what made it the most-acquired endpoint site.
#[test]
fn an_exact_waiter_withdrawal_enters_the_slab_once() {
    with_isolated_ipc_test(|| {
        let endpoint = super::super::create_endpoint().expect("create endpoint");
        super::super::add_endpoint_receiver_waiter(endpoint, 31).expect("park");
        let (removed, exact) = endpoint_lock_acquisitions_during(|| {
            super::super::remove_endpoint_waiter_for_task(endpoint, 31)
        });
        assert_eq!(removed, 1);
        assert_eq!(exact, 1, "an exact withdrawal must enter the slab once");

        super::super::add_endpoint_receiver_waiter(endpoint, 31).expect("park again");
        let (removed, walked) = endpoint_lock_acquisitions_during(|| {
            super::super::remove_endpoint_waiters_for_task(31)
        });
        assert_eq!(removed, 1);
        assert!(
            walked > exact * 8,
            "the whole-slab form is expected to enter once per slot, got {walked}"
        );
    });
}

/// A receive that abandons its own wait knows which endpoint it was parked on.
/// The whole-slab form acquires the slab lock once per slot -- 512 of them --
/// and exists for a retiring task that does not know; taking it on the receive
/// path made it the most-acquired endpoint site in the system for a removal
/// that touches one object.
#[test]
fn withdrawing_one_waiter_touches_only_its_own_endpoint() {
    with_isolated_ipc_test(|| {
        let parked = super::super::create_endpoint().expect("create endpoint");
        let other = super::super::create_endpoint().expect("create second endpoint");
        super::super::add_endpoint_receiver_waiter(parked, 21).expect("park on the first");
        super::super::add_endpoint_receiver_waiter(other, 21).expect("park on the second");

        assert_eq!(super::super::remove_endpoint_waiter_for_task(parked, 21), 1);
        // The exact form must not reach the endpoint it was not given.
        assert_eq!(super::super::remove_endpoint_waiter_for_task(parked, 21), 0);
        assert_eq!(super::super::remove_endpoint_waiters_for_task(21), 1);

        // An unknown endpoint is a no-op, not a panic and not a slab walk.
        assert_eq!(
            super::super::remove_endpoint_waiter_for_task(
                super::super::KernelEndpointHandle::from_raw(0xdead_beef),
                21
            ),
            0
        );
    });
}
