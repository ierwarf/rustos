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
                received.bytes(),
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
    dispatch_with(frame, None)
}

/// The identity the entry already observed (together with the pending-signal
/// hint) for the register operation's own authentication, so it is not read twice.
pub(super) type EntryIdentity = (u64, multitask::ProcessIdentity);

fn entry_or_current(entry: Option<EntryIdentity>) -> Result<EntryIdentity, i64> {
    match entry {
        Some(entry) => Ok(entry),
        None => multitask::current_user_ipc_identity().ok_or(LINUX_EINVAL),
    }
}

pub(super) fn dispatch_with(frame: &mut SyscallFrame, entry: Option<EntryIdentity>) -> u64 {
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
        SYS_RUSTOS_IPC_REGISTER_CALL => call(frame, endpoint, payload, entry),
        SYS_RUSTOS_IPC_REGISTER_RECV => receive(frame, endpoint, None, entry),
        SYS_RUSTOS_IPC_REGISTER_REPLY_RECV => reply_receive(frame, endpoint, reply, payload, entry),
        SYS_RUSTOS_IPC_REGISTER_REPLY => reply_only(endpoint, payload, entry),
        _ => unreachable!(),
    };
    result.unwrap_or_else(linux_errno)
}

fn call(
    frame: &mut SyscallFrame, raw: u64, request: &[u8], entry: Option<EntryIdentity>,
) -> Result<u64, i64> {
    let (task_id, identity) = entry_or_current(entry)?;
    let service = authorize_ipc_call_for_process(raw, identity.process_id())?;
    let endpoint = KernelEndpointHandle::from_raw(raw);
    let deadline = service_reply_deadline_tick_for_authorized_service(
        service, request, SERVICE_IPC_TIMEOUT_MS,
    );
    if let Some(response) = try_fast_ipc_call_bytes::<IPC_REGISTER_BYTES>(
        endpoint,
        identity.process_id(),
        task_id,
        request,
        IPC_REGISTER_BYTES,
        deadline,
        // The call may block through the direct schedule: its continuation is
        // this syscall frame plus a park record, see `complete_parked_register_ipc`.
        Some(core::ptr::from_mut::<SyscallFrame>(frame)),
    )? {
        return write_register_result(frame, response.data(), 0, 0, 0)
            .map(|len| len as u64);
    }
    // A declined reservation was rolled back by the existing helper. Enqueue
    // exactly once; lack of a parked receiver is not an ABI-level failure.
    if reply_deadline_expired(deadline) {
        return Err(LINUX_ETIMEDOUT);
    }
    let reply = enqueue_call_and_wake(endpoint, request)?;
    let response = collect_inline_reply::<IPC_REGISTER_BYTES>(
        reply, task_id, deadline, IPC_REGISTER_BYTES, phase_mark(),
        InlineReplyTransport::Queued, Some(core::ptr::from_mut(frame)),
    )?;
    write_register_result(frame, response.data(), 0, 0, 0).map(|len| len as u64)
}

/// Finish a register `CALL` or `REPLY_RECV` that blocked through the direct
/// schedule. Entered from `fast_ipc_parked_tail` when the scheduler resumes the
/// task at its syscall frame; every Rust frame the call had is gone, and `record`
/// is what it left behind. Does what the in-place code would have done after
/// waking, then the generic syscall epilogue for pending signals.
pub(in crate::user::syscall) fn complete_parked_register_ipc(
    frame: &mut SyscallFrame,
    record: &crate::user::syscall::ParkRecord,
) -> u64 {
    use crate::user::syscall::park_kind;
    let result = match record.kind {
        park_kind::CALL | park_kind::CALL_QUEUED => complete_parked_call(frame, record),
        park_kind::RECV | park_kind::RECV_INITIAL => complete_parked_recv(frame, record),
        other => panic!("parked register IPC resumed with unknown record kind {other}"),
    };
    // `dispatch_linux_syscall` runs this after every syscall body.
    if deliver_pending_signals_if_needed(frame) {
        return frame.rax;
    }
    result
}

fn complete_parked_call(frame: &mut SyscallFrame, record: &crate::user::syscall::ParkRecord) -> u64 {
    let task_id = multitask::current_task_id().expect("parked register call lost its task");
    let reply = KernelReplyHandle::from_raw(record.reply);
    disarm_reply_deadline_waiter(task_id);
    let transport = if record.kind == crate::user::syscall::park_kind::CALL_QUEUED {
        InlineReplyTransport::Queued
    } else {
        note_fast_ipc(IpcFastCounter::HandoffCommitted);
        InlineReplyTransport::Rendezvous
    };
    collect_inline_reply::<IPC_REGISTER_BYTES>(
        reply,
        task_id,
        record.deadline_tick,
        IPC_REGISTER_BYTES,
        phase_mark(),
        transport,
        Some(core::ptr::from_mut(frame)),
    )
    .and_then(|response| {
        write_register_result(frame, response.data(), 0, 0, 0).map(|len| len as u64)
    })
    .unwrap_or_else(linux_errno)
}

fn complete_parked_recv(frame: &mut SyscallFrame, record: &crate::user::syscall::ParkRecord) -> u64 {
    let task_id = multitask::current_task_id().expect("parked register receive lost its task");
    let endpoint = KernelEndpointHandle::from_raw(record.endpoint);
    let error = |errno| {
        if record.kind == crate::user::syscall::park_kind::RECV {
            ipc_reply_recv::ipc_reply_recv_committed_error(errno)
        } else {
            linux_errno(errno)
        }
    };
    // Woken by the handoff: the sender's request is already addressed to this
    // task, so take it directly, as the in-place receive loop does first.
    if let Ok(Some(received)) = kernel_ipc_runtime::api::endpoint::take_fast_request_inline::<
        IPC_REGISTER_BYTES,
    >(endpoint, task_id, IPC_REGISTER_BYTES)
    {
        note_fast_ipc(IpcFastCounter::ReceiverTaken);
        return match (ReceiveDestination::Registers(frame)).write_fast(received) {
            Ok(len) => len as u64,
            Err(errno) => error(errno),
        };
    }
    // Anything else (a wake that carried no request) takes the full receive.
    match recv_with_sender_blocking_prepared_inner::<IPC_REGISTER_BYTES>(
        endpoint,
        task_id,
        IPC_REGISTER_BYTES,
        None,
        None,
        ReceiveDestination::Registers(frame),
    ) {
        Ok((len, _)) => len as u64,
        Err((errno, _)) => error(errno),
    }
}

fn receive(
    frame: &mut SyscallFrame,
    raw: u64,
    deferred: Option<fast_reply::DeferredFastReply>,
    entry: Option<EntryIdentity>,
) -> Result<u64, i64> {
    let (task_id, identity) = entry_or_current(entry)?;
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
    entry: Option<EntryIdentity>,
) -> Result<u64, i64> {
    // Authenticate the next endpoint BEFORE consuming the old reply. The
    // post-commit tagged error range is identical to the buffer ABI.
    let (task_id, identity) = entry_or_current(entry)?;
    let endpoint = KernelEndpointHandle::from_raw(raw);
    kernel_ipc_runtime::api::authorize_endpoint_receiver_for_process(
        endpoint,
        identity.process_id(),
    )
    .map_err(ipc_error_to_linux_errno)?;
    let handle = KernelReplyHandle::from_raw(reply);
    let deferred = match kernel_ipc_runtime::api::endpoint::complete_fast_reply_for_task(
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
            deferred
        }
        Err(kernel_ipc_runtime::api::IpcError::InvalidHandle) => {
            let completion =
                kernel_ipc_runtime::api::complete_endpoint_reply_for_process_with_custody(
                    handle,
                    identity.process_id(),
                    response,
                )
                .map_err(ipc_error_to_linux_errno)?;
            // Queued calls carry the same exact caller/context custody. Fuse
            // settlement with receive's park just as for a rendezvous reply.
            fast_reply::DeferredFastReply::new(reply, completion)
        }
        Err(error) => return Err(ipc_error_to_linux_errno(error)),
    };
    let result = recv_with_sender_blocking_prepared_inner::<IPC_REGISTER_BYTES>(
        endpoint,
        task_id,
        IPC_REGISTER_BYTES,
        None,
        Some(deferred),
        ReceiveDestination::Registers(frame),
    );
    Ok(match result {
        Ok((len, _)) => len as u64,
        Err((errno, _)) => ipc_reply_recv::ipc_reply_recv_committed_error(errno),
    })
}

fn reply_only(raw: u64, response: &[u8], entry: Option<EntryIdentity>) -> Result<u64, i64> {
    let (task_id, identity) = entry_or_current(entry)?;
    let handle = KernelReplyHandle::from_raw(raw);
    match kernel_ipc_runtime::api::endpoint::complete_fast_reply_for_task(handle, task_id, response)
    {
        Ok(published) => {
            if multitask::publish_fast_ipc_reply(raw, published.completion) {
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
            if multitask::publish_ipc_reply(raw, completion) {
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
    fn direct_blocks_describe_themselves_before_they_can_abandon_their_frames() {
        let source = include_str!("../ipc_ops.rs");
        let call = source
            .find("park_register_call(frame, endpoint.raw(), reply.raw(), deadline_tick);")
            .expect("call park record");
        let commit = source
            .find("multitask::commit_fast_ipc_call_handoff_direct(")
            .expect("direct call commit");
        assert!(call < commit, "the park record must exist before the call can commit");
        let recv = source.find("park_register_recv(frame, endpoint.raw());").expect("receive park record");
        let block = source.find("deferred.commit_block_and_yield_direct(").expect("direct receive commit");
        assert!(recv < block, "the park record must exist before the receive can commit");
        // Only register ABI operations may block directly: the buffer wrappers and
        // the legacy handlers keep their in-place continuation.
        assert_eq!(source.matches("commit_fast_ipc_call_handoff_direct(").count(), 1);
        assert_eq!(source.matches("commit_block_and_yield_direct(").count(), 1);
    }

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
