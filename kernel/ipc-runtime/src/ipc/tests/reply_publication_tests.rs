use super::with_isolated_ipc_test;

#[test]
fn pending_detailed_response_takes_one_reply_lock() {
    with_isolated_ipc_test(|| {
        let endpoint = super::super::create_endpoint().expect("create endpoint");
        let (reply, _) =
            super::super::enqueue_endpoint_call(endpoint, 41, b"request").expect("enqueue call");
        let class = nucleus_core::util::lockdep::LockClass::IpcReply as usize;
        let _ = nucleus_core::util::lockdep::work_budget::take_class_census();

        assert_eq!(
            super::super::take_endpoint_response_detailed(reply, 0),
            Ok(super::super::EndpointResponseTake::Pending)
        );
        let census = nucleus_core::util::lockdep::work_budget::take_class_census();
        assert_eq!(
            census[class], 1,
            "the published message-id hint must remove the preliminary reply-slot lookup"
        );
    });
}

#[test]
fn retiring_caller_does_not_panic_when_another_cpu_owns_terminal_reply() {
    with_isolated_ipc_test(|| {
        let endpoint = super::super::create_endpoint().expect("endpoint");
        let (reply, _) = super::super::enqueue_endpoint_call(endpoint, 419, b"request")
            .expect("published call");
        let reply_id = reply.raw();
        let message_id = super::super::REPLIES
            .with(reply_id, |reply| reply.message_id)
            .expect("reply binding");
        // Witness the narrow window after another CPU won terminal
        // consumption but before it withdrew the message/reply slab entries.
        assert!(super::super::mark_endpoint_call_consumed(message_id, reply_id, 419));
        assert_eq!(
            super::super::cancel_endpoint_calls_for_task(
                419,
                |_| panic!("terminal winner owns request transfers"),
                |_, _| panic!("terminal winner owns scheduling context"),
            ),
            0,
            "a concurrent terminal owner must not look like leaked custody",
        );
        assert!(super::super::ENDPOINT_MESSAGES.with(message_id, |_| ()).is_some());
        drop(super::super::ENDPOINT_MESSAGES.remove(message_id));
        drop(super::super::REPLIES.remove(reply_id));
    });
}

#[test]
fn retiring_caller_races_concurrent_response_take_without_panicking() {
    with_isolated_ipc_test(|| {
        let endpoint = super::super::create_endpoint().expect("endpoint");
        for round in 0..64_u64 {
            let task_id = 420 + round;
            let (reply, _) =
                super::super::enqueue_endpoint_call(endpoint, task_id, b"request")
                    .expect("published call");
            let _ = super::super::recv_endpoint(endpoint)
                .expect("recv")
                .expect("message");
            assert_eq!(
                super::super::complete_endpoint_reply(reply, b"ok"),
                Ok(task_id)
            );

            let barrier = std::sync::Barrier::new(2);
            let (taken, cancelled) = std::thread::scope(|scope| {
                let taker = scope.spawn(|| {
                    barrier.wait();
                    super::super::take_endpoint_response_detailed(reply, 0)
                });
                barrier.wait();
                let cancelled = super::super::cancel_endpoint_calls_for_task(
                    task_id,
                    |_| {},
                    |_, _| panic!("test call unexpectedly carried scheduling custody"),
                );
                (taker.join().unwrap(), cancelled)
            });

            match taken {
                Ok(super::super::EndpointResponseTake::Response((bytes, _))) => {
                    assert_eq!(bytes, b"ok");
                    assert_eq!(cancelled, 0);
                }
                Err(super::super::IpcError::InvalidHandle) => {
                    assert_eq!(cancelled, 1);
                }
                other => panic!("unexpected take outcome: {other:?}"),
            }
        }
    });
}
