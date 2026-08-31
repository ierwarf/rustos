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
