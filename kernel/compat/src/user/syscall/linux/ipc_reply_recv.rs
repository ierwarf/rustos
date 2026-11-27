//! Phase-explicit byte reply + authenticated byte/handle receive transaction.
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
    linux_result(reply_receive::<false>(args_ptr))
}

pub(super) fn syscall_linux_rustos_ipc_reply_recv_with_handles_and_sender(args_ptr: u64) -> u64 {
    linux_result(reply_receive::<true>(args_ptr))
}

fn validate_reply_recv_lengths(
    response_len: u64,
    request_capacity: u64,
) -> LinuxResult<(usize, usize)> {
    let response_len = usize::try_from(response_len).map_err(|_| LINUX_EINVAL)?;
    let request_capacity = usize::try_from(request_capacity).map_err(|_| LINUX_EINVAL)?;
    if response_len > rustos_user_abi::syscall::IPC_MAX_INLINE_BYTES
        || request_capacity > rustos_user_abi::syscall::IPC_MAX_INLINE_BYTES
    {
        return Err(LINUX_EINVAL);
    }
    Ok((response_len, request_capacity))
}

fn reply_receive<const WITH_HANDLES: bool>(args_ptr: u64) -> LinuxResult<u64> {
    let retained_mm =
        usermem::current_user_address_space().map_err(address_space_error_to_linux_errno)?;

    // CONTRACT: every operation that can reject caller-controlled shape or
    // authority happens before the one-shot reply is consumed. This keeps an
    // ordinary errno unambiguously pre-commit and makes the tagged post-commit
    // range below the sole retry boundary. The argument block, the response
    // bytes and every output range are admitted under one address-space bind.
    let mut inline_response = [0_u8; kernel_ipc_runtime::api::IPC_FAST_INLINE_BYTES];
    let (args, response_len, request_capacity, inline, handles) =
        usermem::with_retained_user_access(&retained_mm, |user| {
            let (args, handles) = if WITH_HANDLES {
                let outer = user
                    .read_struct::<rustos_user_abi::syscall::IpcReplyRecvWithHandlesAndSenderArgs>(
                        args_ptr,
                    )
                    .map_err(address_space_error_to_linux_errno)?;
                if !rustos_user_abi::syscall::ipc_handle_reply_recv_shape_valid(&outer) {
                    return Err(LINUX_EINVAL);
                }
                (
                    outer.reply_recv,
                    Some(handle_receive::HandleOutputs {
                        fds_ptr: outer.recv_fds_ptr,
                        count_ptr: outer.recv_fd_count_ptr,
                        capacity: usize::from(outer.recv_fd_capacity),
                    }),
                )
            } else {
                let args = user
                    .read_struct::<rustos_user_abi::syscall::IpcReplyRecvWithSenderArgs>(args_ptr)
                    .map_err(address_space_error_to_linux_errno)?;
                if !rustos_user_abi::syscall::ipc_reply_recv_shape_valid(&args) {
                    return Err(LINUX_EINVAL);
                }
                (args, None)
            };
            let (response_len, request_capacity) =
                validate_reply_recv_lengths(args.response_len, args.request_capacity)?;
            let inline = response_len <= inline_response.len();
            let output_buffers = [
                (args.request_ptr, request_capacity),
                (args.next_reply_cap_ptr, size_of::<u64>()),
                (args.sender_pid_ptr, size_of::<u64>()),
                (args.sender_tid_ptr, size_of::<u64>()),
            ];
            for (ptr, len) in output_buffers {
                user.validate_write(ptr, len)
                    .map_err(address_space_error_to_linux_errno)?;
            }
            if let Some(handles) = handles {
                if handles.capacity != 0 {
                    let bytes = handles
                        .capacity
                        .checked_mul(size_of::<i32>())
                        .ok_or(LINUX_EINVAL)?;
                    user.validate_write(handles.fds_ptr, bytes).map_err(address_space_error_to_linux_errno)?;
                }
                user.validate_write(handles.count_ptr, size_of::<u16>()).map_err(address_space_error_to_linux_errno)?;
            }
            if inline && response_len != 0 {
                user.read_exact(args.response_ptr, &mut inline_response[..response_len])
                    .map_err(address_space_error_to_linux_errno)?;
            }
            Ok((args, response_len, request_capacity, inline, handles))
        })
        .map_err(address_space_error_to_linux_errno)??;
    // Authority follows the shape and range admission above; it still precedes
    // the one-shot consume, so an ordinary errno remains pre-commit.
    let endpoint = KernelEndpointHandle::from_raw(args.endpoint);
    let receiver_process_id = retained_mm.process_id();
    kernel_ipc_runtime::api::authorize_endpoint_receiver_for_process(endpoint, receiver_process_id)
        .map_err(ipc_error_to_linux_errno)?;
    let receiver_task_id = retained_mm.thread_id();
    // Inline bytes were already snapshotted during preflight; a larger response is
    // copied once and serves both a fast frame's spill and a queued reply object.
    let mut large_response: Vec<u8> = Vec::new();
    let copy_mark = crate::user::syscall::linux::ipc_server_profile::now();
    if !inline {
        large_response = copy_request_from_user(args.response_ptr, args.response_len)?;
    }
    let _ = crate::user::syscall::linux::ipc_server_profile::charge(
        crate::user::syscall::linux::ipc_server_profile::IpcServerPhase::ReplyCopy,
        copy_mark,
    );
    if response_len <= kernel_ipc_runtime::api::IPC_FAST_MAX_BYTES {
        let reply_handle = KernelReplyHandle::from_raw(args.reply_cap);
        // A large response moves into the frame spill without a second copy; a
        // rejected one stays in `large_response` for the queued reply below.
        let completed = if inline {
            kernel_ipc_runtime::api::endpoint::complete_fast_reply_for_task(
                reply_handle,
                receiver_task_id,
                &inline_response[..response_len],
            )
        } else {
            kernel_ipc_runtime::api::endpoint::complete_fast_reply_moving(
                reply_handle,
                receiver_task_id,
                &mut large_response,
            )
        };
        match completed {
            Ok(published) => {
                note_fast_ipc(IpcFastCounter::FusedReplyPublished);
                if let Some(error) = published.terminal_error {
                    let _ = multitask::complete_fast_ipc_reply_wake_handoff_with_custody(
                        args.reply_cap,
                        published.completion,
                    );
                    return Ok(ipc_reply_recv_committed_error(ipc_error_to_linux_errno(
                        error,
                    )));
                }
                return Ok(finish_committed_reply_receive(
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
                    handles,
                ));
            }
            Err(kernel_ipc_runtime::api::IpcError::InvalidHandle) => {}
            Err(error) => {
                record_ipc_reply_rejection(args.reply_cap, receiver_process_id, error);
                return Err(ipc_error_to_linux_errno(error));
            }
        }
    }
    let response_len_bytes = if inline {
        response_len
    } else {
        large_response.len()
    };
    let response_payload = if inline {
        kernel_ipc_runtime::api::EndpointPayload::try_from_slice(&inline_response[..response_len])
            .map_err(ipc_error_to_linux_errno)?
    } else {
        kernel_ipc_runtime::api::EndpointPayload::from_vec(large_response)
    };
    // The fast branch above publishes in-kernel without a user copy and cannot
    // be slow; only this path pays for the slow-call timing reads.
    let start_ticks = crate::arch::rtc::ticks();
    let copy_ticks = crate::arch::rtc::ticks();
    let completion = kernel_ipc_runtime::api::complete_endpoint_reply_payload_for_process_with_custody(
        KernelReplyHandle::from_raw(args.reply_cap),
        receiver_process_id,
        response_payload,
    )
    .map_err(|err| {
        record_ipc_reply_rejection(args.reply_cap, receiver_process_id, err);
        ipc_error_to_linux_errno(err)
    })?;
    let reply_ticks = crate::arch::rtc::ticks();
    log_slow_ipc_reply(
        "reply-recv",
        args.reply_cap,
        start_ticks,
        copy_ticks,
        reply_ticks,
        response_len_bytes,
    );

    // Both representations carry the same reply/context custody into receive.
    // Its block settles and hands back in one boundary; an immediate receive
    // or error drops the deferred reply and wakes the caller exactly once.
    Ok(finish_committed_reply_receive(
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
        handles,
    ))
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
    handles: Option<handle_receive::HandleOutputs>,
) -> u64 {
    let received = if let Some(handles) = handles {
        recv_with_sender_blocking_prepared_inner::<{ kernel_ipc_runtime::api::IPC_FAST_INLINE_BYTES }>(
            endpoint, receiver_task_id, request_capacity, None,
            Some(fast_reply::DeferredFastReply::new(completed_reply, completion)),
            ReceiveDestination::UserWithHandles(handle_receive::ReceiveOutputs::new(
                retained_mm,
                request_ptr,
                next_reply_cap_ptr,
                Some((sender_pid_ptr, sender_tid_ptr)),
                Some(handles),
            )),
        )
    } else {
        recv_with_sender_blocking_prepared_after_fast_reply(
            endpoint, receiver_task_id, retained_mm, request_ptr, request_capacity,
            next_reply_cap_ptr, sender_pid_ptr, sender_tid_ptr, completed_reply, completion,
        )
    };
    match received {
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

    #[test]
    fn reply_recv_lengths_reject_oversized_response_and_admit_empty_reply() {
        let max = rustos_user_abi::syscall::IPC_MAX_INLINE_BYTES as u64;
        assert_eq!(validate_reply_recv_lengths(0, max), Ok((0, max as usize)));
        assert_eq!(validate_reply_recv_lengths(max, 0), Ok((max as usize, 0)));
        assert_eq!(validate_reply_recv_lengths(max + 1, 64), Err(LINUX_EINVAL));
        assert_eq!(validate_reply_recv_lengths(64, max + 1), Err(LINUX_EINVAL));
        assert_eq!(validate_reply_recv_lengths(u64::MAX, 64), Err(LINUX_EINVAL));
    }
}
