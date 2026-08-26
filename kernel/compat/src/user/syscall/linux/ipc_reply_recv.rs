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
    let retained_mm = match usermem::current_user_address_space() {
        Ok(retained) => retained,
        Err(err) => return linux_errno(address_space_error_to_linux_errno(err)),
    };
    let args = match usermem::read_retained_user_struct::<
        rustos_user_abi::syscall::IpcReplyRecvWithSenderArgs,
    >(&retained_mm, args_ptr)
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
    let response_len = match usize::try_from(args.response_len) {
        Ok(len) => len,
        Err(_) => return linux_errno(LINUX_EINVAL),
    };
    let mut inline_response = [0_u8; kernel_ipc_runtime::api::IPC_FAST_INLINE_BYTES];
    let inline = response_len <= inline_response.len();
    let (endpoint, receiver_task_id, receiver_process_id, request_capacity, retained_mm) =
        match prepare_recv_identity(args.endpoint, args.request_capacity, retained_mm) {
            Ok(prepared) => prepared,
            Err(errno) => return linux_errno(errno),
        };
    let output_buffers = [
        (args.request_ptr, request_capacity),
        (args.next_reply_cap_ptr, size_of::<u64>()),
        (args.sender_pid_ptr, size_of::<u64>()),
        (args.sender_tid_ptr, size_of::<u64>()),
    ];
    if inline {
        if let Err(error) = usermem::copy_from_retained_user_and_validate_writes(
            &retained_mm,
            args.response_ptr,
            &mut inline_response[..response_len],
            &output_buffers,
        ) {
            return linux_errno(address_space_error_to_linux_errno(error));
        }
    } else if let Err(error) =
        usermem::validate_retained_user_write_buffers(&retained_mm, &output_buffers)
    {
        return linux_errno(address_space_error_to_linux_errno(error));
    }
    let start_ticks = crate::arch::rtc::ticks();
    if inline {
        let copy_ticks = crate::arch::rtc::ticks();
        match kernel_ipc_runtime::api::endpoint::complete_fast_reply_for_task(
            KernelReplyHandle::from_raw(args.reply_cap),
            receiver_task_id,
            &inline_response[..response_len],
        ) {
            Ok(published) => {
                note_fast_ipc(IpcFastCounter::FusedReplyPublished);
                let reply_ticks = crate::arch::rtc::ticks();
                let handoff_queued = multitask::complete_fast_ipc_reply_wake_handoff_with_custody(
                    args.reply_cap,
                    published.completion,
                );
                log_slow_ipc_reply(
                    "reply-recv-fast",
                    args.reply_cap,
                    start_ticks,
                    copy_ticks,
                    reply_ticks,
                    response_len,
                );
                if let Some(error) = published.terminal_error {
                    return ipc_reply_recv_committed_error(ipc_error_to_linux_errno(error));
                }
                return finish_committed_reply_receive(
                    endpoint,
                    receiver_task_id,
                    &retained_mm,
                    args.request_ptr,
                    request_capacity,
                    args.next_reply_cap_ptr,
                    args.sender_pid_ptr,
                    args.sender_tid_ptr,
                    handoff_queued,
                );
            }
            Err(kernel_ipc_runtime::api::IpcError::InvalidHandle) => {}
            Err(error) => return linux_errno(ipc_error_to_linux_errno(error)),
        }
    }
    let response = match copy_request_from_user(args.response_ptr, args.response_len) {
        Ok(response) => response,
        Err(errno) => return linux_errno(errno),
    };
    let copy_ticks = crate::arch::rtc::ticks();
    let completion = match kernel_ipc_runtime::api::complete_endpoint_reply_for_process_with_custody(
        KernelReplyHandle::from_raw(args.reply_cap),
        receiver_process_id,
        response.as_slice(),
    ) {
        Ok(completion) => completion,
        Err(err) => {
            record_ipc_reply_rejection(args.reply_cap, receiver_process_id, err);
            return linux_errno(ipc_error_to_linux_errno(err));
        }
    };
    let reply_ticks = crate::arch::rtc::ticks();
    let handoff_queued =
        multitask::complete_ipc_reply_wake_handoff_with_custody(args.reply_cap, completion);
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
    finish_committed_reply_receive(
        endpoint,
        receiver_task_id,
        &retained_mm,
        args.request_ptr,
        request_capacity,
        args.next_reply_cap_ptr,
        args.sender_pid_ptr,
        args.sender_tid_ptr,
        handoff_queued,
    )
}

#[allow(clippy::too_many_arguments)]
fn finish_committed_reply_receive(
    endpoint: KernelEndpointHandle,
    receiver_task_id: u64,
    retained_mm: &multitask::RetainedCurrentUserAddressSpace,
    request_ptr: u64,
    request_capacity: usize,
    next_reply_cap_ptr: u64,
    sender_pid_ptr: u64,
    sender_tid_ptr: u64,
    handoff_queued: bool,
) -> u64 {
    match recv_with_sender_blocking_prepared(
        endpoint,
        receiver_task_id,
        retained_mm,
        request_ptr,
        request_capacity,
        next_reply_cap_ptr,
        sender_pid_ptr,
        sender_tid_ptr,
        // Reply-and-receive belongs to a single-endpoint server, which has
        // nothing else to service and so has no reason to wake early.
        None,
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
