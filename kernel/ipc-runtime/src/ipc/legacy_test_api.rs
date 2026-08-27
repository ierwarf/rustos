//! Host-test-only legacy port, channel, and event mechanics.
//!
//! This module is owned by `kernel-ipc-runtime` tests and is never compiled
//! into the product. It isolates the old in-memory compatibility objects from
//! the production generational endpoint, reply, and shared-region paths.

use super::*;

pub(super) fn create_event() -> Result<KernelEventHandle, IpcError> {
    with_ipc_objects(|objects| {
        let id = objects.allocate_id()?;
        objects.events.insert(id, EventObject::default());
        Ok(KernelEventHandle::from_raw(id))
    })
}

pub(super) fn signal_event(event: KernelEventHandle) -> Result<u64, IpcError> {
    with_ipc_objects(|objects| {
        let Some(object) = objects.events.get_mut(&event.raw()) else {
            return Err(IpcError::InvalidHandle);
        };
        object.signal_count = object.signal_count.saturating_add(1);
        Ok(object.signal_count)
    })
}

pub(super) fn event_signal_count(event: KernelEventHandle) -> Option<u64> {
    with_ipc_objects_ref(|objects| {
        objects
            .events
            .get(&event.raw())
            .map(|object| object.signal_count)
    })
}

pub(super) fn port_name(port: KernelPortHandle) -> Option<PortName> {
    with_ipc_objects_ref(|objects| {
        objects
            .ports
            .get(&port.raw())
            .and_then(|object| object.name)
    })
}

pub(super) fn enqueue_message(
    channel: KernelChannelHandle,
    header: IpcHeader,
    payload: &[u8],
    attached_handles: &[KernelHandle],
) -> Result<(), IpcError> {
    if payload.len() > MAX_IPC_PAYLOAD_BYTES || attached_handles.len() > MAX_IPC_ATTACHED_HANDLES {
        return Err(IpcError::InvalidArgument);
    }

    let header = normalize_header(header, payload.len(), attached_handles.len())?;
    let message = IpcMessage {
        header,
        payload: payload.to_vec(),
        attached_handles: attached_handles.to_vec(),
    };
    with_ipc_objects(|objects| {
        let peer_id = {
            let Some(channel_object) = objects.channels.get(&channel.raw()) else {
                return Err(IpcError::InvalidHandle);
            };
            if channel_object.closed {
                return Err(IpcError::PeerClosed);
            }
            channel_object.peer.ok_or(IpcError::PeerClosed)?
        };

        let Some(peer_object) = objects.channels.get_mut(&peer_id) else {
            return Err(IpcError::PeerClosed);
        };
        if peer_object.closed {
            return Err(IpcError::PeerClosed);
        }
        if peer_object.recv_queue.len() >= MAX_CHANNEL_QUEUE_DEPTH {
            return Err(IpcError::NoMemory);
        }

        peer_object.recv_queue.push_back(message);
        Ok(())
    })
}

pub(super) fn dequeue_message(
    channel: KernelChannelHandle,
) -> Result<Option<IpcMessage>, IpcError> {
    dequeue_message_with_limits(channel, usize::MAX, usize::MAX)
}

pub(super) fn dequeue_message_with_limits(
    channel: KernelChannelHandle,
    payload_capacity: usize,
    handle_capacity: usize,
) -> Result<Option<IpcMessage>, IpcError> {
    with_ipc_objects(|objects| {
        let Some(channel_object) = objects.channels.get_mut(&channel.raw()) else {
            return Err(IpcError::InvalidHandle);
        };
        if let Some(message) = channel_object.recv_queue.front()
            && (message.payload.len() > payload_capacity
                || message.attached_handles.len() > handle_capacity)
        {
            return Err(IpcError::BufferTooSmall);
        }
        Ok(channel_object.recv_queue.pop_front())
    })
}

pub(super) fn channel_peer(channel: KernelChannelHandle) -> Option<KernelChannelHandle> {
    with_ipc_objects_ref(|objects| {
        let channel_object = objects.channels.get(&channel.raw())?;
        channel_object.peer.map(KernelChannelHandle::from_raw)
    })
}

pub(super) fn channel_queue_len(channel: KernelChannelHandle) -> Option<usize> {
    with_ipc_objects_ref(|objects| {
        let channel_object = objects.channels.get(&channel.raw())?;
        Some(channel_object.recv_queue.len())
    })
}
