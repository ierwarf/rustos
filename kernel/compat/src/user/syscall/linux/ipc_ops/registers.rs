//! Pointer-free short IPC over the existing authenticated transport/custody.
//! No new scheduler state: receive uses the same arm/register/recheck/commit,
//! call uses the same reservation rollback and deadline, and reply consumes
//! the same generation-bound one-shot capability. Slow/remote paths stay live.

mod buffer;

use super::*;
use rustos_user_abi::syscall::{
    IPC_REGISTER_BYTES, SYS_RUSTOS_IPC_REGISTER_CALL, SYS_RUSTOS_IPC_REGISTER_RECV,
    SYS_RUSTOS_IPC_REGISTER_REPLY, SYS_RUSTOS_IPC_REGISTER_REPLY_RECV, ipc_register_pack,
    ipc_register_shape_valid, ipc_register_unpack,
};

pub(super) enum ReceiveDestination<'a> {
    User {
        retained_mm: &'a multitask::RetainedCurrentUserAddressSpace,
        request_ptr: u64,
        reply_cap_ptr: u64,
        sender_pid_ptr: u64,
        sender_tid_ptr: u64,
    },
    Registers(&'a mut SyscallFrame),
}

impl ReceiveDestination<'_> {
    pub(super) fn write_fast<const N: usize>(
        &mut self,
        received: kernel_ipc_runtime::api::FastEndpointReceived<N>,
    ) -> Result<usize, i64> {
        match self {
            Self::User {
                retained_mm,
                request_ptr,
                reply_cap_ptr,
                sender_pid_ptr,
                sender_tid_ptr,
            } => write_fast_endpoint_request_with_sender(
                received,
                retained_mm,
                *request_ptr,
                *reply_cap_ptr,
                *sender_pid_ptr,
                *sender_tid_ptr,
            ),
            Self::Registers(frame) => write_register_result(
                frame,
                &received.request[..received.request_len],
                received.reply.raw(),
                received.caller_process_id,
                received.caller_task_id,
            ),
        }
    }

    pub(super) fn write_queued(
        &mut self,
        reply: KernelReplyHandle,
        request: &[u8],
        caller: u64,
    ) -> Result<usize, i64> {
        let (pid, tid) = multitask::user_log_ids_for_task(caller).unwrap_or((0, 0));
        match self {
            Self::Registers(frame) => write_register_result(frame, request, reply.raw(), pid, tid),
            Self::User {
                retained_mm,
                request_ptr,
                reply_cap_ptr,
                sender_pid_ptr,
                sender_tid_ptr,
            } => {
                let (cap, pid, tid) = (
                    reply.raw().to_ne_bytes(),
                    pid.to_ne_bytes(),
                    tid.to_ne_bytes(),
                );
                usermem::write_retained_user_bytes_batch(
                    retained_mm,
                    &[
                        (*request_ptr, request),
                        (*reply_cap_ptr, &cap),
                        (*sender_pid_ptr, &pid),
                        (*sender_tid_ptr, &tid),
                    ],
                )
                .map_err(address_space_error_to_linux_errno)?;
                Ok(request.len())
            }
        }
    }
}

fn clear_register_result(frame: &mut SyscallFrame) {
    frame.rdx = 0;
    frame.r10 = 0;
    frame.r8 = 0;
    frame.r9 = 0;
    frame.rsi = 0;
}

fn write_register_result(
    frame: &mut SyscallFrame,
    bytes: &[u8],
    reply: u64,
    pid: u64,
    tid: u64,
) -> Result<usize, i64> {
    let words = ipc_register_pack(bytes).ok_or(LINUX_EOVERFLOW)?;
    frame.rdx = words[0];
    frame.r10 = words[1];
    frame.r8 = reply;
    frame.r9 = pid;
    frame.rsi = tid;
    Ok(bytes.len())
}

pub(super) fn dispatch(frame: &mut SyscallFrame) -> u64 {
    let (operation, endpoint, len, word0, word1, reply, reserved) = (
        frame.rax, frame.rdi, frame.rsi, frame.rdx, frame.r10, frame.r8, frame.r9,
    );
    clear_register_result(frame);
    // Shape validation precedes any authority consumption. Unused payload bytes
    // are never delivered; outputs are padded with zeros, including on failure.
    if !ipc_register_shape_valid(operation, len, word0, word1, reply, reserved) {
        return linux_errno(LINUX_EINVAL);
    }
    if reserved == rustos_user_abi::syscall::IPC_REGISTER_BUFFER_MODE {
        return buffer::dispatch(frame, operation, endpoint);
    }
    let bytes = ipc_register_unpack([word0, word1]);
    let payload = &bytes[..len as usize];
    let result = match operation {
        SYS_RUSTOS_IPC_REGISTER_CALL => call(frame, endpoint, payload),
        SYS_RUSTOS_IPC_REGISTER_RECV => receive(frame, endpoint, None),
        SYS_RUSTOS_IPC_REGISTER_REPLY_RECV => reply_receive(frame, endpoint, reply, payload),
        SYS_RUSTOS_IPC_REGISTER_REPLY => reply_only(endpoint, payload),
        _ => unreachable!(),
    };
    result.unwrap_or_else(linux_errno)
}

fn call(frame: &mut SyscallFrame, raw: u64, request: &[u8]) -> Result<u64, i64> {
    let (task_id, identity) = multitask::current_user_ipc_identity().ok_or(LINUX_EINVAL)?;
    let service = authorize_ipc_call_for_process(raw, identity.process_id())?;
    let endpoint = KernelEndpointHandle::from_raw(raw);
    if let Some(response) = try_fast_ipc_call_bytes::<IPC_REGISTER_BYTES>(
        endpoint,
        identity.process_id(),
        task_id,
        request,
        IPC_REGISTER_BYTES,
        SERVICE_IPC_TIMEOUT_MS,
        service,
    )? {
        return write_register_result(frame, &response.bytes[..response.len], 0, 0, 0)
            .map(|len| len as u64);
    }
    // A declined reservation was rolled back by the existing helper. Enqueue
    // exactly once; lack of a parked receiver is not an ABI-level failure.
    let deadline = service_reply_deadline_tick_for_authorized_service(
        service,
        request,
        SERVICE_IPC_TIMEOUT_MS,
    );
    let reply = enqueue_call_and_wake(endpoint, request)?;
    let response =
        wait_for_service_reply_with_deadline_tick(reply, deadline, SERVICE_IPC_TIMEOUT_MS)?;
    write_register_result(frame, &response, 0, 0, 0).map(|len| len as u64)
}

fn receive(
    frame: &mut SyscallFrame,
    raw: u64,
    deferred: Option<fast_reply::DeferredFastReply>,
) -> Result<u64, i64> {
    let (task_id, identity) = multitask::current_user_ipc_identity().ok_or(LINUX_EINVAL)?;
    let endpoint = KernelEndpointHandle::from_raw(raw);
    kernel_ipc_runtime::api::authorize_endpoint_receiver_for_process(
        endpoint,
        identity.process_id(),
    )
    .map_err(ipc_error_to_linux_errno)?;
    recv_with_sender_blocking_prepared_inner::<IPC_REGISTER_BYTES>(
        endpoint,
        task_id,
        IPC_REGISTER_BYTES,
        None,
        deferred,
        ReceiveDestination::Registers(frame),
    )
    .map(|(len, _)| len as u64)
    .map_err(|(errno, _)| errno)
}

fn reply_receive(
    frame: &mut SyscallFrame,
    raw: u64,
    reply: u64,
    response: &[u8],
) -> Result<u64, i64> {
    // Authenticate the next endpoint BEFORE consuming the old reply. The
    // post-commit tagged error range is identical to the buffer ABI.
    let (task_id, identity) = multitask::current_user_ipc_identity().ok_or(LINUX_EINVAL)?;
    let endpoint = KernelEndpointHandle::from_raw(raw);
    kernel_ipc_runtime::api::authorize_endpoint_receiver_for_process(
        endpoint,
        identity.process_id(),
    )
    .map_err(ipc_error_to_linux_errno)?;
    let handle = KernelReplyHandle::from_raw(reply);
    let (deferred, handoff) = match kernel_ipc_runtime::api::endpoint::complete_fast_reply_for_task(
        handle, task_id, response,
    ) {
        Ok(published) => {
            let deferred = fast_reply::DeferredFastReply::new(reply, published.completion);
            if let Some(error) = published.terminal_error {
                drop(deferred);
                return Ok(ipc_reply_recv::ipc_reply_recv_committed_error(
                    ipc_error_to_linux_errno(error),
                ));
            }
            (Some(deferred), false)
        }
        Err(kernel_ipc_runtime::api::IpcError::InvalidHandle) => {
            let completion =
                kernel_ipc_runtime::api::complete_endpoint_reply_for_process_with_custody(
                    handle,
                    identity.process_id(),
                    response,
                )
                .map_err(ipc_error_to_linux_errno)?;
            (
                None,
                multitask::complete_ipc_reply_wake_handoff_with_custody(reply, completion),
            )
        }
        Err(error) => return Err(ipc_error_to_linux_errno(error)),
    };
    let result = recv_with_sender_blocking_prepared_inner::<IPC_REGISTER_BYTES>(
        endpoint,
        task_id,
        IPC_REGISTER_BYTES,
        None,
        deferred,
        ReceiveDestination::Registers(frame),
    );
    let yielded = match &result {
        Ok((_, yielded)) | Err((_, yielded)) => *yielded,
    };
    if handoff && !yielded {
        multitask::request_deferred_reschedule();
    }
    Ok(match result {
        Ok((len, _)) => len as u64,
        Err((errno, _)) => ipc_reply_recv::ipc_reply_recv_committed_error(errno),
    })
}

fn reply_only(raw: u64, response: &[u8]) -> Result<u64, i64> {
    let (task_id, identity) = multitask::current_user_ipc_identity().ok_or(LINUX_EINVAL)?;
    let handle = KernelReplyHandle::from_raw(raw);
    match kernel_ipc_runtime::api::endpoint::complete_fast_reply_for_task(handle, task_id, response)
    {
        Ok(published) => {
            if multitask::complete_fast_ipc_reply_wake_handoff_with_custody(
                raw,
                published.completion,
            ) {
                multitask::request_deferred_reschedule();
            }
            published
                .terminal_error
                .map_or(Ok(0), |error| Err(ipc_error_to_linux_errno(error)))
        }
        Err(kernel_ipc_runtime::api::IpcError::InvalidHandle) => {
            let completion =
                kernel_ipc_runtime::api::complete_endpoint_reply_for_process_with_custody(
                    handle,
                    identity.process_id(),
                    response,
                )
                .map_err(ipc_error_to_linux_errno)?;
            if multitask::complete_ipc_reply_wake_handoff_with_custody(raw, completion) {
                multitask::request_deferred_reschedule();
            }
            Ok(0)
        }
        Err(error) => Err(ipc_error_to_linux_errno(error)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn invalid_register_shape_rejects_before_identity_and_clears_outputs() {
        for operation in [
            SYS_RUSTOS_IPC_REGISTER_CALL,
            SYS_RUSTOS_IPC_REGISTER_RECV,
            SYS_RUSTOS_IPC_REGISTER_REPLY_RECV,
            SYS_RUSTOS_IPC_REGISTER_REPLY,
        ] {
            let mut frame: SyscallFrame = unsafe { core::mem::zeroed() };
            frame.rax = operation;
            frame.rsi = 17;
            frame.rdx = u64::MAX;
            frame.r10 = u64::MAX;
            frame.r8 = u64::MAX;
            frame.r9 = u64::MAX;
            assert_eq!(dispatch(&mut frame) as i64, -LINUX_EINVAL);
            assert_eq!(
                (frame.rdx, frame.r10, frame.r8, frame.r9, frame.rsi),
                (0, 0, 0, 0, 0)
            );
        }
    }

    #[test]
    fn register_output_has_exact_bytes_metadata_and_no_stale_tail() {
        // SyscallFrame is all u64 words, so zero is a valid initial image.
        let mut frame: SyscallFrame = unsafe { core::mem::zeroed() };
        assert_eq!(
            write_register_result(&mut frame, &[0xaa; 9], 17, 23, 42),
            Ok(9)
        );
        assert_eq!(frame.rdx, 0xaaaa_aaaa_aaaa_aaaa);
        assert_eq!(frame.r10, 0xaa);
        assert_eq!((frame.r8, frame.r9, frame.rsi), (17, 23, 42));
        clear_register_result(&mut frame);
        assert_eq!(
            (frame.rdx, frame.r10, frame.r8, frame.r9, frame.rsi),
            (0, 0, 0, 0, 0)
        );
        assert_eq!(
            write_register_result(&mut frame, &[0; 17], 1, 2, 3),
            Err(LINUX_EOVERFLOW)
        );
        assert_eq!(frame.r8, 0);
    }
}
