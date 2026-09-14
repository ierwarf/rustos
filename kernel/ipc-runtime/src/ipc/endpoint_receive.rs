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
pub enum EndpointReceive {
    Fast(FastEndpointReceived),
    Queued(EndpointReceivedWithSender),
    Waiting,
    Empty,
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
    ENDPOINTS
        .with_mut(endpoint.raw(), |object| {
            if let Some(received) =
                take_fast_request(object, endpoint, receiver_task_id, request_capacity)?
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

pub(super) fn take_fast_request(
    object: &mut EndpointObject,
    endpoint: KernelEndpointHandle,
    receiver_task_id: u64,
    request_capacity: usize,
) -> Result<Option<FastEndpointReceived>, IpcError> {
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
            if frame.request_len > request_capacity {
                return Err(IpcError::BufferTooSmall);
            }
            frame.state = FastCallState::RequestTaken;
            Ok(FastEndpointReceived {
                reply: KernelReplyHandle::from_raw(reply_id),
                caller_process_id: frame.caller_process_id,
                caller_task_id: frame.caller_task_id,
                request_len: frame.request_len,
                request: frame.request,
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

