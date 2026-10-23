//! Endpoint receive and waiter publication under one object-slot guard.
//!
//! - **Owner:** IPC runtime owns transport; the scheduler owns arm/commit.
//! - **Boundary:** callers authenticate the endpoint and receiver before entry.
//! - **Lifecycle:** after scheduler arm, consume one request or publish one
//!   bounded waiter. A non-waiting result requires the caller to cancel its arm.
//! - **Concurrency:** Endpoint -> Message/Reply; no scheduler or user copy here.
//! - **Failure:** capacity/identity rejection preserves the live request.
//! - **Forbidden:** no allocation, blocking, or policy decisions under the guard.
//! - **Evidence:** endpoint-receiver-wakeup and the receive transaction tests.

use super::*;

/// `Waiting` is endpoint publication, not a scheduler block commit. The caller
/// must still commit its arm; a concurrent wake may already have withdrawn it.
#[derive(Debug)]
pub enum EndpointReceive<const N: usize = IPC_FAST_INLINE_BYTES> {
    Fast(FastEndpointReceived<N>),
    Queued(EndpointReceivedWithSender),
    Waiting,
    Empty,
}

/// Read-only observation of an already consumed QUEUED request, not a wake or
/// funding grant. A nonempty request was required at enqueue; only actual recv
/// takes it. The advisory message id never authorizes a recycled reply: both
/// exact objects are rejoined under Message -> Reply leaves before accepting.
pub fn received_request_retains_custody(reply: KernelReplyHandle, caller: u64) -> bool {
    // A fast frame is received once its receiver took it (`RequestTaken` or
    // later) and is still owed a response while the reply is neither used nor
    // consumed. Without this, a call handed off through a fast frame whose
    // receiver had already taken it read as "custody lost" and was cancelled
    // under that receiver; a service launch failing this way stalled boot.
    let fast = REPLIES
        .with(reply.raw(), |object| {
            object.fast_frame.as_ref().map(|frame| {
                frame.caller_task_id == caller
                    && frame.state != FastCallState::RequestReady
                    && !(object.used || object.consumed)
                    && object.scheduling_context.is_some_and(|held| held.caller_task_id() == caller)
            })
        })
        .flatten();
    if let Some(fast) = fast {
        return fast;
    }
    let Some(message_id) = REPLIES.with(reply.raw(), |object| object.message_id) else {
        return false;
    };
    ENDPOINT_MESSAGES.with(message_id, |message| {
        if message.reply_id != reply.raw() || message.caller_task_id != caller
            || !message.published || !message.request.is_empty()
        {
            return false;
        }
        REPLIES.with(reply.raw(), |object| {
            object.message_id == message_id && !object.used && !object.consumed
                && object.fast_frame.is_none()
                && object.scheduling_context.is_some_and(|custody| custody.caller_task_id() == caller)
        }).unwrap_or(false)
    }).unwrap_or(false)
}

/// Read-only observation of a request still QUEUED on its endpoint with this
/// caller's scheduling-context custody on the live reply. A refused directed
/// handoff leaves such a request exactly where an enqueue with no parked
/// receiver leaves it: the next receive takes it and binds that custody, so the
/// caller waits for its reply instead of cancelling it. The check and the
/// eventual take race only toward `received_request_retains_custody`.
pub fn published_request_retains_custody(reply: KernelReplyHandle, caller: u64) -> bool {
    let Some(message_id) = REPLIES.with(reply.raw(), |object| object.message_id) else {
        return false;
    };
    ENDPOINT_MESSAGES.with(message_id, |message| {
        message.reply_id == reply.raw() && message.caller_task_id == caller && message.published
            // Bytes still present: queued, or received by a copying receive.
            // A taking receive is `received_request_retains_custody`'s case.
            && message.request.len() != 0
    }).unwrap_or(false)
        && REPLIES.with(reply.raw(), |object| {
            object.message_id == message_id
                && !(object.used || object.consumed)
                && object.fast_frame.is_none()
                && object.scheduling_context.is_some_and(|held| held.caller_task_id() == caller)
        }).unwrap_or(false)
}

/// Failure-only evidence: which `received_request_retains_custody` condition
/// held, one bit each (message found, identity, published, taken, reply joined,
/// unused, unconsumed, not fast, custody caller). Never an authority.
pub fn received_request_custody_conditions(reply: KernelReplyHandle, caller: u64) -> u16 {
    let Some(message_id) = REPLIES.with(reply.raw(), |object| object.message_id) else {
        return 0;
    };
    let message = ENDPOINT_MESSAGES.with(message_id, |message| {
        (message.reply_id == reply.raw() && message.caller_task_id == caller,
            message.published, message.request.is_empty())
    });
    let Some((identity, published, taken)) = message else {
        return 0;
    };
    let reply_bits = REPLIES.with(reply.raw(), |object| {
        u16::from(object.message_id == message_id) << 4
            | u16::from(!object.used) << 5
            | u16::from(!(object.consumed)) << 6
            | u16::from(object.fast_frame.is_none()) << 7
            | u16::from(object.scheduling_context.is_some_and(|c| c.caller_task_id() == caller)) << 8
    }).unwrap_or(0);
    1 | u16::from(identity) << 1 | u16::from(published) << 2 | u16::from(taken) << 3 | reply_bits
}

/// The receiver must arm its scheduler wait before calling with `may_wait`.
/// A queued request is considered even when `may_wait` is false (e.g. expiry).
/// All output bytes remain kernel-owned until the endpoint guard has dropped.
pub fn receive_or_wait(
    endpoint: KernelEndpointHandle,
    receiver_task_id: u64,
    request_capacity: usize,
    handle_capacity: usize,
    may_wait: bool,
) -> Result<EndpointReceive, IpcError> {
    receive_or_wait_inline::<IPC_FAST_INLINE_BYTES>(endpoint, receiver_task_id, request_capacity, handle_capacity, may_wait)
}

/// Select a small fixed return image without changing transport or custody.
pub fn receive_or_wait_inline<const N: usize>(
    endpoint: KernelEndpointHandle,
    receiver_task_id: u64,
    request_capacity: usize,
    handle_capacity: usize,
    may_wait: bool,
) -> Result<EndpointReceive<N>, IpcError> {
    // The legacy 256-byte fast image also receives larger queued byte messages.
    // Only a smaller compact image narrows the queued admission bound.
    let request_capacity = if N < IPC_FAST_INLINE_BYTES { request_capacity.min(N) } else { request_capacity };
    ENDPOINTS
        .with_mut(endpoint.raw(), |object| {
            if let Some(received) =
                take_fast_request_inline::<N>(object, endpoint, receiver_task_id, request_capacity)?
            {
                return Ok(EndpointReceive::Fast(received));
            }
            if let Some(received) =
                take_queued_request(object, endpoint, request_capacity, handle_capacity)?
            {
                return Ok(EndpointReceive::Queued(received));
            }
            if !may_wait {
                return Ok(EndpointReceive::Empty);
            }
            register_receiver(object, receiver_task_id, request_capacity)?;
            Ok(EndpointReceive::Waiting)
        })
        .ok_or(IpcError::InvalidHandle)?
}

pub(super) fn take_fast_request_inline<const N: usize>(
    object: &mut EndpointObject,
    endpoint: KernelEndpointHandle,
    receiver_task_id: u64,
    request_capacity: usize,
) -> Result<Option<FastEndpointReceived<N>>, IpcError> {
    let Some(reply_id) = object.fast_reply else {
        return Ok(None);
    };
    // LOCK ORDER: Endpoint -> Reply. The frame and endpoint publication are
    // validated and consumed together; no advisory read authorizes a take.
    let received = REPLIES
        .with_mut(reply_id, |reply_object| {
            let frame = reply_object
                .fast_frame
                .as_mut()
                .ok_or(IpcError::InvalidHandle)?;
            if frame.endpoint_id != endpoint.raw()
                || frame.receiver_task_id != receiver_task_id
                || frame.state != FastCallState::RequestReady
            {
                return Err(IpcError::PermissionDenied);
            }
            if frame.request_len > request_capacity.min(N) {
                return Err(IpcError::BufferTooSmall);
            }
            let mut request = [0; N];
            request[..frame.request_len].copy_from_slice(&frame.request[..frame.request_len]);
            frame.state = FastCallState::RequestTaken;
            Ok(FastEndpointReceived {
                reply: KernelReplyHandle::from_raw(reply_id),
                caller_process_id: frame.caller_process_id,
                caller_task_id: frame.caller_task_id,
                request_len: frame.request_len,
                request,
            })
        })
        .ok_or(IpcError::PermissionDenied)??;
    object.fast_reply = None;
    Ok(Some(received))
}

pub(super) fn take_queued_request(
    object: &mut EndpointObject,
    endpoint: KernelEndpointHandle,
    request_capacity: usize,
    handle_capacity: usize,
) -> Result<Option<EndpointReceivedWithSender>, IpcError> {
    // Stale identities cannot wedge a lane. Byte-capacity rejection preserves
    // its head; handle rejection consumes the lane entry, retaining handles in
    // the caller's cancellable message object as on the standalone path.
    loop {
        let Some((lane, message_id)) = object.next_pending() else {
            return Ok(None);
        };
        #[cfg(test)]
        inject_endpoint_recv_stale_head_fault(message_id);
        let outcome = ENDPOINT_MESSAGES.with_mut(message_id, |message| {
            if message.endpoint_id != endpoint.raw() {
                return (Err(IpcError::InvalidHandle), false);
            }
            let request_too_large = message.request.len() > request_capacity;
            if request_too_large || message.attached_handles.len() > handle_capacity {
                return (Err(IpcError::BufferTooSmall), request_too_large);
            }
            let request = core::mem::take(&mut message.request);
            let attached_handles = core::mem::take(&mut message.attached_handles);
            (
                Ok((
                    KernelReplyHandle::from_raw(message.reply_id),
                    request,
                    attached_handles,
                    message.caller_task_id,
                )),
                false,
            )
        });
        let Some((outcome, preserve_queue)) = outcome else {
            object.consume_pending(lane, message_id);
            continue;
        };
        if preserve_queue {
            return outcome.map(Some);
        }
        object.consume_pending(lane, message_id);
        return outcome.map(Some);
    }
}

pub(super) fn register_receiver(
    object: &mut EndpointObject,
    task_id: u64,
    request_capacity: usize,
) -> Result<(), IpcError> {
    if let Some(waiter) = object
        .waiting_receivers
        .iter_mut()
        .find(|w| w.task_id == task_id)
    {
        waiter.request_capacity = request_capacity;
    } else {
        if object.waiting_receivers.len() >= MAX_ENDPOINT_WAITERS {
            return Err(IpcError::NoMemory);
        }
        object
            .waiting_receivers
            .push_back(endpoint_priority::EndpointReceiverWaiter {
                task_id,
                request_capacity,
            });
    }
    Ok(())
}

