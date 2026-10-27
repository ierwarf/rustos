use super::*;

#[test]
fn queued_inline_copy_preserves_exact_bytes_padding_and_overflow() {
    for len in 0..=16 {
        let result = copy_inline_reply::<16>(&[0xab; 16][..len]).unwrap();
        assert_eq!(result.len, len);
        assert_eq!(&result.bytes[..len], &[0xab; 16][..len]);
        assert!(result.bytes[len..].iter().all(|byte| *byte == 0));
    }
    assert!(matches!(copy_inline_reply::<16>(&[0; 17]), Err(LINUX_EOVERFLOW)));
}

#[test]
fn inline_deadline_wins_before_and_during_take_without_polling_again() {
    use core::cell::Cell;
    let ticks = Cell::new(9);
    let takes = Cell::new(0);
    let take = || {
        takes.set(takes.get() + 1);
        ticks.set(10);
        Ok(Some(42))
    };
    assert_eq!(observe_inline_reply(9, || ticks.get(), take), Err(LINUX_ETIMEDOUT));
    assert_eq!(takes.get(), 0, "pre-expired calls must not consume a reply");
    assert_eq!(observe_inline_reply(10, || ticks.get(), take), Err(LINUX_ETIMEDOUT));
    assert_eq!(takes.get(), 1, "expiry during take must not publish the reply");
    assert_eq!(observe_inline_reply(11, || ticks.get(), take), Ok(Some(42)));
    assert_eq!(takes.get(), 2);
    assert_eq!(observe_inline_reply::<u64>(11, || 10, || Ok(None)), Ok(None));
    assert_eq!(observe_inline_reply::<u64>(11, || 10, || Err(LINUX_EINVAL)), Err(LINUX_EINVAL));
}

#[test]
fn inline_wait_has_two_polls_and_one_transport_independent_park() {
    let source = include_str!("../../ipc_ops.rs");
    let body = source.split("fn collect_inline_reply<const N: usize>(").nth(1).unwrap()
        .split("unsafe fn park_register_call(").next().unwrap();
    assert!(body.contains("for armed in [false, true]"));
    assert_eq!(body.matches("take_inline_reply::<N>(").count(), 1);
    assert!(body.contains("observe_inline_reply(deadline_tick, crate::arch::rtc::ticks"));
    let record = body.find("register_ipc_park_record(frame).write(").unwrap();
    let block = body.find("multitask::commit_block_current_task_direct(").unwrap();
    assert!(record < block);
    assert!(body.contains("InlineReplyTransport::Queued => crate::user::syscall::park_kind::CALL_QUEUED"));
    let entry = source.split("fn syscall_linux_rustos_ipc_call_with_timeout(").nth(1).unwrap()
        .split("fn try_fast_ipc_call(").next().unwrap();
    assert!(!entry.contains("timeout_ms == SERVICE_IPC_TIMEOUT_MS"));
    assert!(entry.contains(".min(call_deadline)"));
    let receive = source.split("fn recv_with_sender_blocking_prepared_inner<const N: usize>(")
        .nth(1).unwrap().split("fn recv_endpoint_once(").next().unwrap();
    let commit = receive.find("let committed = match").unwrap();
    let clear = receive.find("deferred_fast_reply = None;").unwrap();
    let retry = receive.find("Some(false) => continue").unwrap();
    assert!(commit < clear && clear < retry, "a wake-before-commit retry must not consume the reply twice");
    let fused = include_str!("../../ipc_reply_recv.rs");
    assert_eq!(fused.matches("fn finish_committed_reply_receive(").count(), 1);
    assert!(!fused.contains("fn finish_committed_fast_reply_receive("));
    assert!(fused.contains("recv_with_sender_blocking_prepared_after_fast_reply("));
}
