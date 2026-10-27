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

    // CONTRACT: every operation that can reject caller-controlled shape or
    // authority happens before the one-shot reply is consumed. This keeps an
    // ordinary errno unambiguously pre-commit and makes the tagged post-commit
    // range below the sole retry boundary. The argument block, the response
    // bytes and every output range are admitted under one address-space bind.
    let mut inline_response = [0_u8; kernel_ipc_runtime::api::IPC_FAST_INLINE_BYTES];
    let staged = usermem::with_retained_user_access(&retained_mm, |user| {
        let args = user
            .read_struct::<rustos_user_abi::syscall::IpcReplyRecvWithSenderArgs>(args_ptr)
            .map_err(|err| address_space_error_to_linux_errno(err))?;
        if !rustos_user_abi::syscall::ipc_reply_recv_shape_valid(&args) {
            return Err(LINUX_EINVAL);
        }
        let response_len = usize::try_from(args.response_len).map_err(|_| LINUX_EINVAL)?;
        let request_capacity = usize::try_from(args.request_capacity).map_err(|_| LINUX_EINVAL)?;
        if request_capacity > rustos_user_abi::syscall::IPC_MAX_INLINE_BYTES {
            return Err(LINUX_EINVAL);
        }
        let inline = response_len <= inline_response.len();
        let output_buffers = [
            (args.request_ptr, request_capacity),
            (args.next_reply_cap_ptr, size_of::<u64>()),
            (args.sender_pid_ptr, size_of::<u64>()),
            (args.sender_tid_ptr, size_of::<u64>()),
        ];
        for (ptr, len) in output_buffers {
            user.validate_write(ptr, len)
                .map_err(|err| address_space_error_to_linux_errno(err))?;
        }
        if inline {
            user.read_exact(args.response_ptr, &mut inline_response[..response_len])
                .map_err(|err| address_space_error_to_linux_errno(err))?;
        }
        Ok((args, response_len, request_capacity, inline))
    });
    let (args, response_len, request_capacity, inline) = match staged {
        Ok(Ok(staged)) => staged,
        Ok(Err(errno)) => return linux_errno(errno),
        Err(err) => return linux_errno(address_space_error_to_linux_errno(err)),
    };
    // Authority follows the shape and range admission above; it still precedes
    // the one-shot consume, so an ordinary errno remains pre-commit.
    let (endpoint, receiver_task_id, receiver_process_id) = {
        let endpoint = KernelEndpointHandle::from_raw(args.endpoint);
        let process_id = retained_mm.process_id();
        if let Err(err) =
            kernel_ipc_runtime::api::authorize_endpoint_receiver_for_process(endpoint, process_id)
        {
            return linux_errno(ipc_error_to_linux_errno(err));
        }
        (endpoint, retained_mm.thread_id(), process_id)
    };
    let retained_mm = retained_mm;
    if inline {
        match kernel_ipc_runtime::api::endpoint::complete_fast_reply_for_task(
            KernelReplyHandle::from_raw(args.reply_cap),
            receiver_task_id,
            &inline_response[..response_len],
        ) {
            Ok(published) => {
                note_fast_ipc(IpcFastCounter::FusedReplyPublished);
                if let Some(error) = published.terminal_error {
                    let _ = multitask::complete_fast_ipc_reply_wake_handoff_with_custody(
                        args.reply_cap,
                        published.completion,
                    );
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
                    args.reply_cap,
                    published.completion,
                );
            }
            Err(kernel_ipc_runtime::api::IpcError::InvalidHandle) => {}
            Err(error) => return linux_errno(ipc_error_to_linux_errno(error)),
        }
    }
    // The fast branch above publishes in-kernel without a user copy and cannot
    // be slow; only this path pays for the slow-call timing reads.
    let start_ticks = crate::arch::rtc::ticks();
    // Inline bytes were already snapshotted during preflight. A queued reply
    // does not need another user copy or a temporary Vec for those same bytes.
    let large_response;
    let response = if inline {
        &inline_response[..response_len]
    } else {
        large_response = match copy_request_from_user(args.response_ptr, args.response_len) {
            Ok(response) => response,
            Err(errno) => return linux_errno(errno),
        };
        large_response.as_slice()
    };
    let copy_ticks = crate::arch::rtc::ticks();
    let completion = match kernel_ipc_runtime::api::complete_endpoint_reply_for_process_with_custody(
        KernelReplyHandle::from_raw(args.reply_cap),
        receiver_process_id,
        response,
    ) {
        Ok(completion) => completion,
        Err(err) => {
            record_ipc_reply_rejection(args.reply_cap, receiver_process_id, err);
            return linux_errno(ipc_error_to_linux_errno(err));
        }
    };
    let reply_ticks = crate::arch::rtc::ticks();
    log_slow_ipc_reply(
        "reply-recv",
        args.reply_cap,
        start_ticks,
        copy_ticks,
        reply_ticks,
        response.len(),
    );

    // Both representations carry the same reply/context custody into receive.
    // Its block settles and hands back in one boundary; an immediate receive
    // or error drops the deferred reply and wakes the caller exactly once.
    finish_committed_reply_receive(
        endpoint,
        receiver_task_id,
        &retained_mm,
        args.request_ptr,
        request_capacity,
        args.next_reply_cap_ptr,
        args.sender_pid_ptr,
        args.sender_tid_ptr,
        args.reply_cap,
        completion,
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
    completed_reply: u64,
    completion: kernel_ipc_runtime::api::ReplyCompletion,
) -> u64 {
    match recv_with_sender_blocking_prepared_after_fast_reply(
        endpoint,
        receiver_task_id,
        retained_mm,
        request_ptr,
        request_capacity,
        next_reply_cap_ptr,
        sender_pid_ptr,
        sender_tid_ptr,
        completed_reply,
        completion,
    ) {
        Ok((received, _)) => received as u64,
        Err((errno, _)) => ipc_reply_recv_committed_error(errno),
    }
}

pub(super) fn ipc_reply_recv_committed_error(errno: i64) -> u64 {
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
