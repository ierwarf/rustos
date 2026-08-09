//! Phase-explicit byte-only reply + sender-authenticated receive transaction.
//!
//! - **Owner:** Compat owns the wire transaction; IPC runtime owns endpoint
//!   objects and kernel-ps owns blocking and synchronous handoff custody.
//! - **Boundary:** The argument block, response bytes, receive outputs,
//!   endpoint handle, and one-shot reply capability are untrusted.
//! - **Lifecycle:** Preflight retains the old reply; successful completion
//!   consumes it once, wakes the exact caller, then arms or completes receive.
//! - **Concurrency:** Reply wake and endpoint check-arm-recheck run in one
//!   syscall while scheduler custody remains in the bounded synchronous FIFO.
//! - **Failure:** Ordinary errno is pre-commit; tagged native errno is
//!   post-commit and therefore forbids retrying the old reply capability.
//! - **Forbidden:** No implicit handle transfer, policy routing, partial shape
//!   acceptance, receive-before-reply, or ambiguous retry outcome.
//! - **Evidence:** `ipc-reply-recv-transaction/IpcReplyRecvTransaction`, Kani
//!   wire proofs, source conformance, and exact implementation mutants.

use super::*;

pub(super) fn syscall_linux_rustos_ipc_reply_recv_with_sender(args_ptr: u64) -> u64 {
    let args = match usermem::read_current_user_struct::<
        rustos_user_abi::syscall::IpcReplyRecvWithSenderArgs,
    >(args_ptr)
    {
        Ok(args) => args,
        Err(err) => return linux_errno(address_space_error_to_linux_errno(err)),
    };
    if !rustos_user_abi::syscall::ipc_reply_recv_shape_valid(&args) {
        return linux_errno(LINUX_EINVAL);
    }

    // CONTRACT: every operation that can reject caller-controlled shape or
    // authority happens before the one-shot reply is consumed. This keeps an
    // ordinary errno unambiguously pre-commit and makes the tagged post-commit
    // range below the sole retry boundary.
    let (endpoint, receiver_task_id, receiver_process_id, request_capacity) =
        match prepare_recv_with_sender(
            args.endpoint,
            args.request_ptr,
            args.request_capacity,
            args.next_reply_cap_ptr,
            args.sender_pid_ptr,
            args.sender_tid_ptr,
        ) {
            Ok(prepared) => prepared,
            Err(errno) => return linux_errno(errno),
        };
    let start_ticks = crate::arch::rtc::ticks();
    let response = match copy_request_from_user(args.response_ptr, args.response_len) {
        Ok(response) => response,
        Err(errno) => return linux_errno(errno),
    };
    let copy_ticks = crate::arch::rtc::ticks();
    let caller_task_id = match kernel_ipc_runtime::api::complete_endpoint_reply_for_process(
        KernelReplyHandle::from_raw(args.reply_cap),
        receiver_process_id,
        response.as_slice(),
    ) {
        Ok(task_id) => task_id,
        Err(err) => {
            record_ipc_reply_rejection(args.reply_cap, receiver_process_id, err);
            return linux_errno(ipc_error_to_linux_errno(err));
        }
    };
    let reply_ticks = crate::arch::rtc::ticks();
    let handoff_queued = multitask::complete_ipc_reply_wake_handoff(args.reply_cap, caller_task_id);
    log_slow_ipc_reply(
        "reply-recv",
        args.reply_cap,
        start_ticks,
        copy_ticks,
        reply_ticks,
        response.len(),
    );

    // The endpoint receive keeps the existing check-arm-recheck protocol. If
    // it blocks, that software-schedule transition consumes the exact caller
    // hint. If a request was already queued, request one syscall-tail handoff
    // so the completed caller still receives its bounded direct turn.
    match recv_with_sender_blocking_prepared(
        endpoint,
        receiver_task_id,
        args.request_ptr,
        request_capacity,
        args.next_reply_cap_ptr,
        args.sender_pid_ptr,
        args.sender_tid_ptr,
    ) {
        Ok((received, yielded)) => {
            if handoff_queued && !yielded {
                multitask::request_deferred_reschedule();
            }
            received as u64
        }
        Err((errno, yielded)) => {
            if handoff_queued && !yielded {
                multitask::request_deferred_reschedule();
            }
            ipc_reply_recv_committed_error(errno)
        }
    }
}

fn ipc_reply_recv_committed_error(errno: i64) -> u64 {
    assert!(
        (1..rustos_user_abi::syscall::IPC_REPLY_RECV_COMMITTED_ERROR_BASE).contains(&errno),
        "reply-recv post-commit errno outside native ABI range: {errno}"
    );
    (-(rustos_user_abi::syscall::IPC_REPLY_RECV_COMMITTED_ERROR_BASE + errno)) as u64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reply_recv_post_commit_error_is_outside_linux_errno_space() {
        let tagged = ipc_reply_recv_committed_error(LINUX_EINVAL) as i64;
        assert_eq!(tagged, -(4096 + LINUX_EINVAL));
        assert!(tagged < -4095);
    }

    #[test]
    fn reply_recv_precommit_shape_is_exact_and_versioned() {
        let valid = rustos_user_abi::syscall::IpcReplyRecvWithSenderArgs {
            abi_version: rustos_user_abi::syscall::IPC_ABI_VERSION,
            ..rustos_user_abi::syscall::IpcReplyRecvWithSenderArgs::default()
        };
        assert!(rustos_user_abi::syscall::ipc_reply_recv_shape_valid(&valid));
        assert!(!rustos_user_abi::syscall::ipc_reply_recv_shape_valid(
            &rustos_user_abi::syscall::IpcReplyRecvWithSenderArgs {
                abi_version: valid.abi_version + 1,
                ..valid
            }
        ));
        assert!(!rustos_user_abi::syscall::ipc_reply_recv_shape_valid(
            &rustos_user_abi::syscall::IpcReplyRecvWithSenderArgs { flags: 1, ..valid }
        ));
        assert!(!rustos_user_abi::syscall::ipc_reply_recv_shape_valid(
            &rustos_user_abi::syscall::IpcReplyRecvWithSenderArgs {
                reserved1: 1,
                ..valid
            }
        ));
    }
}
