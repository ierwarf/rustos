use alloc::collections::BTreeMap;
use alloc::collections::VecDeque;
use alloc::vec::Vec;
#[cfg(not(test))]
use core::ptr;

use crate::ipc_core::SharedRegionHandle;
#[cfg(test)]
use crate::ipc_core::{
    ChannelHandle, EventHandle, IpcHeader, PORT_NAME_CAPACITY, PortHandle, PortName,
};
use kernel_object::api::handle::{HandleRights, HandleToken};
use spin::Mutex;
#[cfg(not(test))]
use x86_64::instructions::interrupts;

#[cfg(not(test))]
use crate::memory::{kernel_vm, phys};

#[cfg(not(test))]
const PAGE_SIZE: usize = 4096;
const INITIAL_PENDING_CHANNEL_CAPACITY: usize = 4;
#[cfg(test)]
const INITIAL_CHANNEL_QUEUE_CAPACITY: usize = 8;
#[cfg(test)]
const MAX_PENDING_CHANNELS_PER_PORT: usize = 256;
#[cfg(test)]
const MAX_CHANNEL_QUEUE_DEPTH: usize = 256;
#[cfg(test)]
const MAX_IPC_PAYLOAD_BYTES: usize = 64 * 1024;
#[cfg(test)]
const MAX_IPC_ATTACHED_HANDLES: usize = 16;
const MAX_ENDPOINT_INLINE_MESSAGE_BYTES: usize = rustos_user_abi::syscall::IPC_MAX_INLINE_BYTES;
const MAX_ENDPOINT_PENDING_MESSAGES: usize = 64;
const MAX_ENDPOINT_TRANSFER_HANDLES: usize = 16;
const MAX_ENDPOINT_WAITERS: usize = 64;
const MAX_SHARED_REGION_BYTES: usize = 256 * 1024 * 1024;

#[cfg(test)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ConsoleStreamKind {
    Input,
    Output,
    Error,
}

#[cfg(test)]
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum KernelHandle {
    Console(ConsoleStreamKind),
}

#[cfg(test)]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct KernelPortHandle {
    raw: PortHandle,
}

#[cfg(test)]
impl KernelPortHandle {
    pub const fn from_raw(raw: u64) -> Self {
        Self {
            raw: PortHandle(raw),
        }
    }

    pub const fn raw(&self) -> u64 {
        self.raw.0
    }
}

#[cfg(test)]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct KernelChannelHandle {
    raw: ChannelHandle,
}

#[cfg(test)]
impl KernelChannelHandle {
    pub const fn from_raw(raw: u64) -> Self {
        Self {
            raw: ChannelHandle(raw),
        }
    }

    pub const fn raw(&self) -> u64 {
        self.raw.0
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct KernelSharedRegionHandle {
    raw: SharedRegionHandle,
}

impl KernelSharedRegionHandle {
    pub const fn from_raw(raw: u64) -> Self {
        Self {
            raw: SharedRegionHandle(raw),
        }
    }

    pub const fn raw(&self) -> u64 {
        self.raw.0
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct KernelEndpointHandle {
    raw: u64,
}

impl KernelEndpointHandle {
    pub const fn from_raw(raw: u64) -> Self {
        Self { raw }
    }

    pub const fn raw(&self) -> u64 {
        self.raw
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct KernelReplyHandle {
    raw: u64,
}

impl KernelReplyHandle {
    pub const fn from_raw(raw: u64) -> Self {
        Self { raw }
    }

    pub const fn raw(&self) -> u64 {
        self.raw
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct KernelTransferredHandle {
    transfer_id: u64,
    token: HandleToken,
    rights: HandleRights,
}

impl KernelTransferredHandle {
    pub const fn new(transfer_id: u64, token: HandleToken, rights: HandleRights) -> Self {
        Self {
            transfer_id,
            token,
            rights,
        }
    }

    pub const fn transfer_id(self) -> u64 {
        self.transfer_id
    }

    pub const fn token(self) -> HandleToken {
        self.token
    }

    pub const fn rights(self) -> HandleRights {
        self.rights
    }

    pub const fn is_transferable(self) -> bool {
        self.transfer_id != 0 && self.rights.allows_transfer()
    }
}

#[cfg(test)]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct KernelEventHandle {
    raw: EventHandle,
}

#[cfg(test)]
impl KernelEventHandle {
    pub const fn from_raw(raw: u64) -> Self {
        Self {
            raw: EventHandle(raw),
        }
    }

    pub const fn raw(&self) -> u64 {
        self.raw.0
    }
}

#[cfg(test)]
#[derive(Clone, Debug)]
pub(crate) struct IpcMessage {
    pub header: IpcHeader,
    pub payload: Vec<u8>,
    pub attached_handles: Vec<KernelHandle>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum IpcError {
    InvalidHandle,
    PeerClosed,
    BufferTooSmall,
    InvalidArgument,
    NoMemory,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct EndpointWakeSet {
    pub callers: Vec<u64>,
    pub receivers: Vec<u64>,
}

impl EndpointWakeSet {
    fn push_caller(&mut self, task_id: u64) {
        if !self.callers.contains(&task_id) {
            self.callers.push(task_id);
        }
    }

    fn push_receiver(&mut self, task_id: u64) {
        if !self.receivers.contains(&task_id) {
            self.receivers.push(task_id);
        }
    }
}

#[cfg(test)]
#[derive(Default)]
struct PortObject {
    name: Option<PortName>,
    pending_channels: VecDeque<u64>,
}

#[cfg(test)]
#[derive(Default)]
struct ChannelObject {
    peer: Option<u64>,
    recv_queue: VecDeque<IpcMessage>,
    closed: bool,
    queued_for_accept: bool,
}

#[cfg(test)]
struct SharedRegionObject {
    byte_len: usize,
    bytes: Vec<u8>,
}

#[cfg(not(test))]
struct SharedRegionObject {
    byte_len: usize,
    phys_start: u64,
    page_count: usize,
}

#[derive(Default)]
struct EndpointObject {
    owner_task_id: Option<u64>,
    pending_messages: VecDeque<u64>,
    waiting_receivers: VecDeque<u64>,
}

struct EndpointMessageObject {
    endpoint_id: u64,
    caller_task_id: u64,
    request: Vec<u8>,
    attached_handles: Vec<KernelTransferredHandle>,
    response: Option<EndpointResponse>,
}

struct ReplyObject {
    message_id: u64,
    used: bool,
}

enum EndpointResponse {
    Data {
        bytes: Vec<u8>,
        attached_handles: Vec<KernelTransferredHandle>,
    },
    Error(IpcError),
}

#[cfg(test)]
#[derive(Default)]
struct EventObject {
    signal_count: u64,
}

#[derive(Default)]
struct IpcObjectTable {
    next_id: u64,
    #[cfg(test)]
    named_ports: BTreeMap<PortName, u64>,
    #[cfg(test)]
    ports: BTreeMap<u64, PortObject>,
    #[cfg(test)]
    channels: BTreeMap<u64, ChannelObject>,
    endpoints: BTreeMap<u64, EndpointObject>,
    replies: BTreeMap<u64, ReplyObject>,
    endpoint_messages: BTreeMap<u64, EndpointMessageObject>,
    shared_regions: BTreeMap<u64, SharedRegionObject>,
    #[cfg(test)]
    events: BTreeMap<u64, EventObject>,
}

impl IpcObjectTable {
    const fn new() -> Self {
        Self {
            next_id: 1,
            #[cfg(test)]
            named_ports: BTreeMap::new(),
            #[cfg(test)]
            ports: BTreeMap::new(),
            #[cfg(test)]
            channels: BTreeMap::new(),
            endpoints: BTreeMap::new(),
            replies: BTreeMap::new(),
            endpoint_messages: BTreeMap::new(),
            shared_regions: BTreeMap::new(),
            #[cfg(test)]
            events: BTreeMap::new(),
        }
    }

    fn allocate_id(&mut self) -> Result<u64, IpcError> {
        let id = self.next_id;
        let Some(next_id) = self.next_id.checked_add(1) else {
            return Err(IpcError::NoMemory);
        };
        self.next_id = next_id;
        Ok(id)
    }

    #[cfg(test)]
    fn allocate_channel_pair(&mut self) -> Result<(u64, u64), IpcError> {
        let left_id = self.allocate_id()?;
        let right_id = self.allocate_id()?;
        self.channels.insert(
            left_id,
            ChannelObject {
                peer: Some(right_id),
                recv_queue: VecDeque::with_capacity(INITIAL_CHANNEL_QUEUE_CAPACITY),
                closed: false,
                queued_for_accept: false,
            },
        );
        self.channels.insert(
            right_id,
            ChannelObject {
                peer: Some(left_id),
                recv_queue: VecDeque::with_capacity(INITIAL_CHANNEL_QUEUE_CAPACITY),
                closed: false,
                queued_for_accept: false,
            },
        );
        Ok((left_id, right_id))
    }
}

static IPC_OBJECTS: Mutex<IpcObjectTable> = Mutex::new(IpcObjectTable::new());

fn with_ipc_objects<R>(f: impl FnOnce(&mut IpcObjectTable) -> R) -> R {
    #[cfg(not(test))]
    {
        interrupts::without_interrupts(|| {
            let mut objects = IPC_OBJECTS.lock();
            f(&mut objects)
        })
    }

    #[cfg(test)]
    {
        let mut objects = IPC_OBJECTS.lock();
        f(&mut objects)
    }
}

fn with_ipc_objects_ref<R>(f: impl FnOnce(&IpcObjectTable) -> R) -> R {
    #[cfg(not(test))]
    {
        interrupts::without_interrupts(|| {
            let objects = IPC_OBJECTS.lock();
            f(&objects)
        })
    }

    #[cfg(test)]
    {
        let objects = IPC_OBJECTS.lock();
        f(&objects)
    }
}

#[cfg(test)]
pub fn create_port() -> Result<KernelPortHandle, IpcError> {
    create_named_port(None)
}

#[cfg(test)]
fn normalize_port_name(name: PortName) -> Result<PortName, IpcError> {
    let len = usize::from(name.len);
    if len > PORT_NAME_CAPACITY {
        return Err(IpcError::InvalidArgument);
    }

    let mut normalized = PortName::empty();
    normalized.bytes[..len].copy_from_slice(&name.bytes[..len]);
    normalized.len = name.len;
    Ok(normalized)
}

#[cfg(test)]
fn normalize_header(
    mut header: IpcHeader,
    payload_len: usize,
    handle_count: usize,
) -> Result<IpcHeader, IpcError> {
    let Ok(payload_len) = u32::try_from(payload_len) else {
        return Err(IpcError::InvalidArgument);
    };
    let Ok(handle_count) = u16::try_from(handle_count) else {
        return Err(IpcError::InvalidArgument);
    };
    header.payload_len = payload_len;
    header.handle_count = handle_count;
    header.reserved = 0;
    Ok(header)
}

#[cfg(test)]
fn queue_server_channel_for_accept(
    objects: &mut IpcObjectTable,
    port_id: u64,
    server_channel_id: u64,
) -> Result<(), IpcError> {
    let Some(port_object) = objects.ports.get(&port_id) else {
        return Err(IpcError::InvalidHandle);
    };
    if port_object.pending_channels.len() >= MAX_PENDING_CHANNELS_PER_PORT {
        return Err(IpcError::NoMemory);
    }

    {
        let Some(channel_object) = objects.channels.get_mut(&server_channel_id) else {
            return Err(IpcError::InvalidHandle);
        };
        if channel_object.closed || channel_object.peer.is_none() {
            return Err(IpcError::PeerClosed);
        }
        if channel_object.queued_for_accept {
            return Err(IpcError::InvalidArgument);
        }
        channel_object.queued_for_accept = true;
    }

    objects
        .ports
        .get_mut(&port_id)
        .expect("ipc: port disappeared while queueing accept")
        .pending_channels
        .push_back(server_channel_id);
    Ok(())
}

#[cfg(test)]
fn connect_port_locked(
    objects: &mut IpcObjectTable,
    port_id: u64,
) -> Result<KernelChannelHandle, IpcError> {
    let Some(port_object) = objects.ports.get(&port_id) else {
        return Err(IpcError::InvalidHandle);
    };
    if port_object.pending_channels.len() >= MAX_PENDING_CHANNELS_PER_PORT {
        return Err(IpcError::NoMemory);
    }

    let (client_id, server_id) = objects.allocate_channel_pair()?;
    queue_server_channel_for_accept(objects, port_id, server_id)?;
    Ok(KernelChannelHandle::from_raw(client_id))
}

#[cfg(test)]
pub fn create_named_port(name: Option<PortName>) -> Result<KernelPortHandle, IpcError> {
    let normalized_name = match name {
        Some(name) => Some(normalize_port_name(name)?),
        None => None,
    };

    with_ipc_objects(|objects| {
        if let Some(name) = normalized_name {
            if objects.named_ports.contains_key(&name) {
                return Err(IpcError::InvalidArgument);
            }
        }

        let id = objects.allocate_id()?;
        if let Some(name) = normalized_name {
            objects.named_ports.insert(name, id);
        }

        objects.ports.insert(
            id,
            PortObject {
                name: normalized_name,
                pending_channels: VecDeque::with_capacity(INITIAL_PENDING_CHANNEL_CAPACITY),
            },
        );
        Ok(KernelPortHandle::from_raw(id))
    })
}

#[cfg(test)]
pub fn lookup_named_port(name: PortName) -> Option<KernelPortHandle> {
    let name = normalize_port_name(name).ok()?;
    with_ipc_objects_ref(|objects| {
        objects
            .named_ports
            .get(&name)
            .copied()
            .map(KernelPortHandle::from_raw)
    })
}

#[cfg(test)]
pub fn create_channel_pair() -> Result<(KernelChannelHandle, KernelChannelHandle), IpcError> {
    with_ipc_objects(|objects| {
        let (left_id, right_id) = objects.allocate_channel_pair()?;
        Ok((
            KernelChannelHandle::from_raw(left_id),
            KernelChannelHandle::from_raw(right_id),
        ))
    })
}

#[cfg(test)]
pub fn queue_channel_for_accept(
    port: KernelPortHandle,
    server_channel: KernelChannelHandle,
) -> Result<(), IpcError> {
    with_ipc_objects(|objects| {
        queue_server_channel_for_accept(objects, port.raw(), server_channel.raw())
    })
}

#[cfg(test)]
pub fn accept_channel(port: KernelPortHandle) -> Result<Option<KernelChannelHandle>, IpcError> {
    with_ipc_objects(|objects| {
        let Some(port_object) = objects.ports.get_mut(&port.raw()) else {
            return Err(IpcError::InvalidHandle);
        };

        while let Some(channel_id) = port_object.pending_channels.pop_front() {
            let Some(channel_object) = objects.channels.get_mut(&channel_id) else {
                continue;
            };
            channel_object.queued_for_accept = false;
            if channel_object.closed || channel_object.peer.is_none() {
                continue;
            }
            return Ok(Some(KernelChannelHandle::from_raw(channel_id)));
        }

        Ok(None)
    })
}

#[cfg(test)]
pub fn connect_port(port: KernelPortHandle) -> Result<KernelChannelHandle, IpcError> {
    with_ipc_objects(|objects| connect_port_locked(objects, port.raw()))
}

#[cfg(test)]
pub fn connect_named_port(name: PortName) -> Result<KernelChannelHandle, IpcError> {
    let name = normalize_port_name(name)?;
    with_ipc_objects(|objects| {
        let Some(port_id) = objects.named_ports.get(&name).copied() else {
            return Err(IpcError::InvalidHandle);
        };
        connect_port_locked(objects, port_id)
    })
}

pub fn create_shared_region(byte_len: usize) -> Result<KernelSharedRegionHandle, IpcError> {
    if byte_len == 0 || byte_len > MAX_SHARED_REGION_BYTES {
        return Err(IpcError::InvalidArgument);
    }

    #[cfg(test)]
    let object = SharedRegionObject {
        byte_len,
        bytes: alloc::vec![0_u8; byte_len],
    };

    #[cfg(not(test))]
    let object = {
        let page_count = byte_len
            .checked_add(PAGE_SIZE - 1)
            .map(|len| len / PAGE_SIZE)
            .ok_or(IpcError::InvalidArgument)?;
        let alloc_len = page_count
            .checked_mul(PAGE_SIZE)
            .ok_or(IpcError::InvalidArgument)?;
        let Some(phys_start) = phys::alloc_contiguous(page_count) else {
            return Err(IpcError::NoMemory);
        };
        unsafe {
            ptr::write_bytes(
                kernel_vm::higher_half_addr(phys_start.as_u64()) as *mut u8,
                0,
                alloc_len,
            );
        }
        SharedRegionObject {
            byte_len,
            phys_start: phys_start.as_u64(),
            page_count,
        }
    };

    with_ipc_objects(|objects| {
        let id = objects.allocate_id()?;
        objects.shared_regions.insert(id, object);
        Ok(KernelSharedRegionHandle::from_raw(id))
    })
}

pub fn create_endpoint() -> Result<KernelEndpointHandle, IpcError> {
    create_endpoint_for_task(None)
}

pub fn create_endpoint_for_task(
    owner_task_id: Option<u64>,
) -> Result<KernelEndpointHandle, IpcError> {
    with_ipc_objects(|objects| {
        let id = objects.allocate_id()?;
        objects.endpoints.insert(
            id,
            EndpointObject {
                owner_task_id,
                pending_messages: VecDeque::with_capacity(INITIAL_PENDING_CHANNEL_CAPACITY),
                waiting_receivers: VecDeque::with_capacity(INITIAL_PENDING_CHANNEL_CAPACITY),
            },
        );
        Ok(KernelEndpointHandle::from_raw(id))
    })
}

fn validate_endpoint_transfer_handles(
    attached_handles: &[KernelTransferredHandle],
) -> Result<(), IpcError> {
    if attached_handles.len() > MAX_ENDPOINT_TRANSFER_HANDLES {
        return Err(IpcError::InvalidArgument);
    }
    if attached_handles
        .iter()
        .any(|handle| !handle.is_transferable())
    {
        return Err(IpcError::InvalidArgument);
    }
    Ok(())
}

pub fn enqueue_endpoint_call(
    endpoint: KernelEndpointHandle,
    caller_task_id: u64,
    request: &[u8],
) -> Result<(KernelReplyHandle, Option<u64>), IpcError> {
    enqueue_endpoint_call_with_handles(endpoint, caller_task_id, request, &[])
}

pub fn enqueue_endpoint_call_with_handles(
    endpoint: KernelEndpointHandle,
    caller_task_id: u64,
    request: &[u8],
    attached_handles: &[KernelTransferredHandle],
) -> Result<(KernelReplyHandle, Option<u64>), IpcError> {
    if request.is_empty() || request.len() > MAX_ENDPOINT_INLINE_MESSAGE_BYTES {
        return Err(IpcError::InvalidArgument);
    }
    validate_endpoint_transfer_handles(attached_handles)?;

    with_ipc_objects(|objects| {
        let Some(endpoint_object) = objects.endpoints.get(&endpoint.raw()) else {
            return Err(IpcError::InvalidHandle);
        };
        if endpoint_object.pending_messages.len() >= MAX_ENDPOINT_PENDING_MESSAGES {
            return Err(IpcError::NoMemory);
        }

        let message_id = objects.allocate_id()?;
        let reply_id = objects.allocate_id()?;
        objects.endpoint_messages.insert(
            message_id,
            EndpointMessageObject {
                endpoint_id: endpoint.raw(),
                caller_task_id,
                request: request.to_vec(),
                attached_handles: attached_handles.to_vec(),
                response: None,
            },
        );
        objects.replies.insert(
            reply_id,
            ReplyObject {
                message_id,
                used: false,
            },
        );

        let receiver_to_wake = {
            let endpoint_object = objects
                .endpoints
                .get_mut(&endpoint.raw())
                .expect("ipc endpoint disappeared while enqueueing call");
            endpoint_object.pending_messages.push_back(message_id);
            endpoint_object.waiting_receivers.pop_front()
        };

        Ok((KernelReplyHandle::from_raw(reply_id), receiver_to_wake))
    })
}

pub fn recv_endpoint(
    endpoint: KernelEndpointHandle,
) -> Result<Option<(KernelReplyHandle, Vec<u8>)>, IpcError> {
    recv_endpoint_with_limits(endpoint, usize::MAX)
}

pub fn recv_endpoint_with_limits(
    endpoint: KernelEndpointHandle,
    request_capacity: usize,
) -> Result<Option<(KernelReplyHandle, Vec<u8>)>, IpcError> {
    let Some((reply, request, _handles)) =
        recv_endpoint_with_limits_and_handles(endpoint, request_capacity, 0)?
    else {
        return Ok(None);
    };
    Ok(Some((reply, request)))
}

pub fn recv_endpoint_with_limits_and_handles(
    endpoint: KernelEndpointHandle,
    request_capacity: usize,
    handle_capacity: usize,
) -> Result<Option<(KernelReplyHandle, Vec<u8>, Vec<KernelTransferredHandle>)>, IpcError> {
    let Some((reply, request, attached_handles, _caller_task_id)) =
        recv_endpoint_with_sender_and_limits(endpoint, request_capacity, handle_capacity)?
    else {
        return Ok(None);
    };
    Ok(Some((reply, request, attached_handles)))
}

pub fn recv_endpoint_with_sender_and_limits(
    endpoint: KernelEndpointHandle,
    request_capacity: usize,
    handle_capacity: usize,
) -> Result<
    Option<(
        KernelReplyHandle,
        Vec<u8>,
        Vec<KernelTransferredHandle>,
        u64,
    )>,
    IpcError,
> {
    with_ipc_objects(|objects| {
        let Some(endpoint_object) = objects.endpoints.get_mut(&endpoint.raw()) else {
            return Err(IpcError::InvalidHandle);
        };
        let Some(message_id) = endpoint_object.pending_messages.front().copied() else {
            return Ok(None);
        };

        let Some(message) = objects.endpoint_messages.get(&message_id) else {
            return Err(IpcError::InvalidHandle);
        };
        if message.request.len() > request_capacity
            || message.attached_handles.len() > handle_capacity
        {
            return Err(IpcError::BufferTooSmall);
        }
        let caller_task_id = message.caller_task_id;
        let Some(reply_id) = objects
            .replies
            .iter()
            .find_map(|(reply_id, reply)| (reply.message_id == message_id).then_some(*reply_id))
        else {
            return Err(IpcError::InvalidHandle);
        };
        objects
            .endpoints
            .get_mut(&endpoint.raw())
            .expect("ipc endpoint disappeared while dequeuing call")
            .pending_messages
            .pop_front();
        // Move the request bytes and handles out of the message rather than cloning;
        // the server has consumed them and the message struct only needs to track
        // the reply-write path from here on.
        let message = objects
            .endpoint_messages
            .get_mut(&message_id)
            .expect("ipc endpoint_message disappeared while dequeuing call");
        let request = core::mem::take(&mut message.request);
        let attached_handles = core::mem::take(&mut message.attached_handles);
        Ok(Some((
            KernelReplyHandle::from_raw(reply_id),
            request,
            attached_handles,
            caller_task_id,
        )))
    })
}

/// Registers `task_id` as a receiver waiter on `endpoint`. Returns
/// `Ok(has_pending)` where `has_pending == true` means a message is already
/// queued on the endpoint at the moment of registration. Callers must use the
/// returned flag to skip blocking (and re-poll the queue) when `true`, closing
/// the recv→add-waiter→block race window where the producer queued a message
/// before our slot was visible and so issued no wake.
pub fn add_endpoint_receiver_waiter(
    endpoint: KernelEndpointHandle,
    task_id: u64,
) -> Result<bool, IpcError> {
    with_ipc_objects(|objects| {
        let Some(endpoint_object) = objects.endpoints.get_mut(&endpoint.raw()) else {
            return Err(IpcError::InvalidHandle);
        };
        let already_waiting = endpoint_object.waiting_receivers.contains(&task_id);
        if !already_waiting {
            if endpoint_object.waiting_receivers.len() >= MAX_ENDPOINT_WAITERS {
                return Err(IpcError::NoMemory);
            }
            endpoint_object.waiting_receivers.push_back(task_id);
        }
        Ok(!endpoint_object.pending_messages.is_empty())
    })
}

pub fn complete_endpoint_reply(reply: KernelReplyHandle, response: &[u8]) -> Result<u64, IpcError> {
    complete_endpoint_reply_with_handles(reply, response, &[])
}

pub fn complete_endpoint_reply_with_handles(
    reply: KernelReplyHandle,
    response: &[u8],
    attached_handles: &[KernelTransferredHandle],
) -> Result<u64, IpcError> {
    if response.len() > MAX_ENDPOINT_INLINE_MESSAGE_BYTES {
        return Err(IpcError::InvalidArgument);
    }
    validate_endpoint_transfer_handles(attached_handles)?;

    with_ipc_objects(|objects| {
        let Some(reply_object) = objects.replies.get_mut(&reply.raw()) else {
            return Err(IpcError::InvalidHandle);
        };
        if reply_object.used {
            return Err(IpcError::InvalidArgument);
        }

        let Some(message) = objects.endpoint_messages.get_mut(&reply_object.message_id) else {
            return Err(IpcError::InvalidHandle);
        };
        if message.response.is_some() {
            return Err(IpcError::InvalidHandle);
        }
        reply_object.used = true;
        message.response = Some(EndpointResponse::Data {
            bytes: response.to_vec(),
            attached_handles: attached_handles.to_vec(),
        });
        Ok(message.caller_task_id)
    })
}

pub fn take_endpoint_response(reply: KernelReplyHandle) -> Result<Option<Vec<u8>>, IpcError> {
    let Some((response, _handles)) = take_endpoint_response_with_handle_limit(reply, 0)? else {
        return Ok(None);
    };
    Ok(Some(response))
}

pub fn take_endpoint_response_with_handle_limit(
    reply: KernelReplyHandle,
    handle_capacity: usize,
) -> Result<Option<(Vec<u8>, Vec<KernelTransferredHandle>)>, IpcError> {
    with_ipc_objects(|objects| {
        let Some(reply_object) = objects.replies.get(&reply.raw()) else {
            return Err(IpcError::InvalidHandle);
        };
        let message_id = reply_object.message_id;
        let Some(message) = objects.endpoint_messages.get_mut(&message_id) else {
            return Err(IpcError::InvalidHandle);
        };
        match message.response.as_ref() {
            None => Ok(None),
            Some(EndpointResponse::Data {
                attached_handles, ..
            }) if attached_handles.len() > handle_capacity => Err(IpcError::BufferTooSmall),
            Some(EndpointResponse::Data { .. }) => {
                let Some(EndpointResponse::Data {
                    bytes,
                    attached_handles,
                }) = message.response.take()
                else {
                    unreachable!("response was checked above");
                };
                objects.endpoint_messages.remove(&message_id);
                objects.replies.remove(&reply.raw());
                Ok(Some((bytes, attached_handles)))
            }
            Some(EndpointResponse::Error(err)) => {
                let err = *err;
                message.response.take();
                objects.endpoint_messages.remove(&message_id);
                objects.replies.remove(&reply.raw());
                Err(err)
            }
        }
    })
}

pub fn cancel_endpoint_call(reply: KernelReplyHandle, caller_task_id: u64) -> Result<(), IpcError> {
    with_ipc_objects(|objects| {
        let Some(message_id) = objects
            .replies
            .get(&reply.raw())
            .map(|reply| reply.message_id)
        else {
            return Err(IpcError::InvalidHandle);
        };
        let Some(message) = objects.endpoint_messages.get(&message_id) else {
            return Err(IpcError::InvalidHandle);
        };
        if message.caller_task_id != caller_task_id {
            return Err(IpcError::InvalidArgument);
        }

        if let Some(endpoint) = objects.endpoints.get_mut(&message.endpoint_id) {
            endpoint
                .pending_messages
                .retain(|pending_message_id| *pending_message_id != message_id);
        }
        objects.endpoint_messages.remove(&message_id);
        objects.replies.remove(&reply.raw());
        Ok(())
    })
}

pub fn remove_endpoint_waiters_for_task(task_id: u64) -> usize {
    with_ipc_objects(|objects| {
        let mut removed = 0;
        for endpoint in objects.endpoints.values_mut() {
            let before = endpoint.waiting_receivers.len();
            endpoint
                .waiting_receivers
                .retain(|waiting_task_id| *waiting_task_id != task_id);
            removed += before.saturating_sub(endpoint.waiting_receivers.len());
        }
        removed
    })
}

pub fn fail_endpoints_owned_by_task(task_id: u64, err: IpcError) -> EndpointWakeSet {
    with_ipc_objects(|objects| {
        let endpoints = objects
            .endpoints
            .iter()
            .filter_map(|(endpoint_id, endpoint)| {
                (endpoint.owner_task_id == Some(task_id)).then_some(*endpoint_id)
            })
            .collect::<Vec<_>>();
        if endpoints.is_empty() {
            return EndpointWakeSet::default();
        }

        let mut wake_set = EndpointWakeSet::default();
        for endpoint_id in endpoints {
            if let Some(endpoint) = objects.endpoints.remove(&endpoint_id) {
                for receiver in endpoint.waiting_receivers {
                    wake_set.push_receiver(receiver);
                }
            }
            let message_ids = objects
                .endpoint_messages
                .iter()
                .filter_map(|(message_id, message)| {
                    (message.endpoint_id == endpoint_id).then_some(*message_id)
                })
                .collect::<Vec<_>>();
            for message_id in message_ids {
                let Some(message) = objects.endpoint_messages.get_mut(&message_id) else {
                    continue;
                };
                if message.response.is_none() {
                    message.response = Some(EndpointResponse::Error(err));
                    wake_set.push_caller(message.caller_task_id);
                    if let Some(reply_id) = objects.replies.iter().find_map(|(reply_id, reply)| {
                        (reply.message_id == message_id).then_some(*reply_id)
                    }) {
                        if let Some(reply) = objects.replies.get_mut(&reply_id) {
                            reply.used = true;
                        }
                    }
                }
            }
        }
        wake_set
    })
}

#[cfg(test)]
pub fn shared_region_len(region: KernelSharedRegionHandle) -> Option<usize> {
    with_ipc_objects_ref(|objects| {
        objects
            .shared_regions
            .get(&region.raw())
            .map(|object| object.byte_len)
    })
}

pub fn map_shared_region(region: KernelSharedRegionHandle) -> Option<(*mut u8, usize)> {
    with_ipc_objects_ref(|objects| {
        let object = objects.shared_regions.get(&region.raw())?;

        #[cfg(test)]
        {
            Some((object.bytes.as_ptr() as *mut u8, object.byte_len))
        }

        #[cfg(not(test))]
        {
            Some((
                kernel_vm::higher_half_addr(object.phys_start) as *mut u8,
                object.byte_len,
            ))
        }
    })
}

pub fn shared_region_frames(region: KernelSharedRegionHandle) -> Option<Vec<u64>> {
    with_ipc_objects_ref(|objects| {
        let object = objects.shared_regions.get(&region.raw())?;

        #[cfg(test)]
        {
            let _ = object;
            None
        }

        #[cfg(not(test))]
        {
            let mut frames = Vec::with_capacity(object.page_count);
            for page_index in 0..object.page_count {
                frames.push(object.phys_start + page_index as u64 * 4096);
            }
            Some(frames)
        }
    })
}

#[cfg(test)]
pub fn create_event() -> Result<KernelEventHandle, IpcError> {
    with_ipc_objects(|objects| {
        let id = objects.allocate_id()?;
        objects.events.insert(id, EventObject::default());
        Ok(KernelEventHandle::from_raw(id))
    })
}

#[cfg(test)]
pub fn signal_event(event: KernelEventHandle) -> Result<u64, IpcError> {
    with_ipc_objects(|objects| {
        let Some(object) = objects.events.get_mut(&event.raw()) else {
            return Err(IpcError::InvalidHandle);
        };
        object.signal_count = object.signal_count.saturating_add(1);
        Ok(object.signal_count)
    })
}

#[cfg(test)]
pub fn event_signal_count(event: KernelEventHandle) -> Option<u64> {
    with_ipc_objects_ref(|objects| {
        objects
            .events
            .get(&event.raw())
            .map(|object| object.signal_count)
    })
}

#[cfg(test)]
pub fn port_name(port: KernelPortHandle) -> Option<PortName> {
    with_ipc_objects_ref(|objects| {
        objects
            .ports
            .get(&port.raw())
            .and_then(|object| object.name)
    })
}

#[cfg(test)]
pub(crate) fn enqueue_message(
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

#[cfg(test)]
pub(crate) fn dequeue_message(
    channel: KernelChannelHandle,
) -> Result<Option<IpcMessage>, IpcError> {
    dequeue_message_with_limits(channel, usize::MAX, usize::MAX)
}

#[cfg(test)]
pub(crate) fn dequeue_message_with_limits(
    channel: KernelChannelHandle,
    payload_capacity: usize,
    handle_capacity: usize,
) -> Result<Option<IpcMessage>, IpcError> {
    with_ipc_objects(|objects| {
        let Some(channel_object) = objects.channels.get_mut(&channel.raw()) else {
            return Err(IpcError::InvalidHandle);
        };
        if let Some(message) = channel_object.recv_queue.front() {
            if message.payload.len() > payload_capacity
                || message.attached_handles.len() > handle_capacity
            {
                return Err(IpcError::BufferTooSmall);
            }
        }
        Ok(channel_object.recv_queue.pop_front())
    })
}

#[cfg(test)]
pub fn channel_peer(channel: KernelChannelHandle) -> Option<KernelChannelHandle> {
    with_ipc_objects_ref(|objects| {
        let channel_object = objects.channels.get(&channel.raw())?;
        channel_object.peer.map(KernelChannelHandle::from_raw)
    })
}

#[cfg(test)]
pub fn channel_queue_len(channel: KernelChannelHandle) -> Option<usize> {
    with_ipc_objects_ref(|objects| {
        let channel_object = objects.channels.get(&channel.raw())?;
        Some(channel_object.recv_queue.len())
    })
}

#[cfg(test)]
mod tests {
    use super::{
        ConsoleStreamKind, IpcError, IpcHeader, KernelHandle, accept_channel, connect_named_port,
        connect_port, create_channel_pair, create_event, create_named_port, create_shared_region,
        dequeue_message, dequeue_message_with_limits, enqueue_message, event_signal_count,
        lookup_named_port, map_shared_region, port_name, queue_channel_for_accept, recv_endpoint,
        recv_endpoint_with_limits, recv_endpoint_with_limits_and_handles, shared_region_len,
        signal_event,
    };
    use kernel_object::api::handle::{FileHandleRights, HandleOwner, HandleRights, HandleToken};
    use spin::Mutex;

    static IPC_TEST_GUARD: Mutex<()> = Mutex::new(());

    fn with_isolated_ipc_test(f: impl FnOnce()) {
        let _guard = IPC_TEST_GUARD.lock();
        super::with_ipc_objects(|objects| *objects = super::IpcObjectTable::new());
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(f));
        super::with_ipc_objects(|objects| *objects = super::IpcObjectTable::new());
        if let Err(payload) = result {
            std::panic::resume_unwind(payload);
        }
    }

    fn transferable_file_handle(id: u64) -> super::KernelTransferredHandle {
        super::KernelTransferredHandle::new(
            id,
            HandleToken::new(HandleOwner::Io, id),
            HandleRights::File(FileHandleRights::READ.union(FileHandleRights::TRANSFER)),
        )
    }

    fn non_transferable_file_handle(id: u64) -> super::KernelTransferredHandle {
        super::KernelTransferredHandle::new(
            id,
            HandleToken::new(HandleOwner::Io, id),
            HandleRights::File(FileHandleRights::READ),
        )
    }

    #[test]
    fn channel_messages_arrive_in_peer_queue_order() {
        with_isolated_ipc_test(|| {
            let (left, right) = create_channel_pair().expect("create channel pair");
            enqueue_message(
                left,
                IpcHeader {
                    opcode: 1,
                    ..IpcHeader::default()
                },
                b"hello",
                &[
                    KernelHandle::Console(ConsoleStreamKind::Input),
                    KernelHandle::Console(ConsoleStreamKind::Output),
                ],
            )
            .expect("enqueue first");
            enqueue_message(
                left,
                IpcHeader {
                    opcode: 2,
                    ..IpcHeader::default()
                },
                b"world",
                &[],
            )
            .expect("enqueue second");

            let first = dequeue_message(right)
                .expect("dequeue first")
                .expect("message present");
            let second = dequeue_message(right)
                .expect("dequeue second")
                .expect("message present");

            assert_eq!(first.header.opcode, 1);
            assert_eq!(first.payload, b"hello");
            assert_eq!(first.attached_handles.len(), 2);
            assert!(matches!(
                &first.attached_handles[0],
                KernelHandle::Console(ConsoleStreamKind::Input)
            ));
            assert!(matches!(
                &first.attached_handles[1],
                KernelHandle::Console(ConsoleStreamKind::Output)
            ));
            assert_eq!(second.header.opcode, 2);
            assert_eq!(second.payload, b"world");
        });
    }

    #[test]
    fn ports_accept_queued_server_channels() {
        with_isolated_ipc_test(|| {
            let port = create_named_port(None).expect("create port");
            let (_client, server) = create_channel_pair().expect("create channel pair");
            queue_channel_for_accept(port, server).expect("queue server channel");
            let accepted = accept_channel(port)
                .expect("accept")
                .expect("accepted channel");
            assert_eq!(accepted, server);
        });
    }

    #[test]
    fn events_and_shared_regions_track_basic_state() {
        with_isolated_ipc_test(|| {
            let event = create_event().expect("create event");
            assert_eq!(event_signal_count(event), Some(0));
            assert_eq!(signal_event(event), Ok(1));
            assert_eq!(event_signal_count(event), Some(1));

            let region = create_shared_region(8192).expect("create region");
            assert_eq!(shared_region_len(region), Some(8192));
            let (ptr, len) = map_shared_region(region).expect("map region");
            assert_eq!(len, 8192);
            assert!(!ptr.is_null());
        });
    }

    #[test]
    fn named_ports_retain_port_name() {
        with_isolated_ipc_test(|| {
            let mut name = crate::ipc_core::PortName::empty();
            name.bytes[..4].copy_from_slice(b"test");
            name.len = 4;
            let port = create_named_port(Some(name)).expect("create named port");
            assert_eq!(port_name(port), Some(name));
            assert_eq!(lookup_named_port(name), Some(port));
        });
    }

    #[test]
    fn connect_port_queues_server_channel_for_accept() {
        with_isolated_ipc_test(|| {
            let port = create_named_port(None).expect("create port");
            let client = connect_port(port).expect("connect port");
            let server = accept_channel(port)
                .expect("accept")
                .expect("server channel");
            enqueue_message(
                client,
                IpcHeader {
                    opcode: 99,
                    ..IpcHeader::default()
                },
                b"ping",
                &[],
            )
            .expect("enqueue");
            let received = dequeue_message(server).expect("dequeue").expect("message");
            assert_eq!(received.header.opcode, 99);
            assert_eq!(received.payload, b"ping");
        });
    }

    #[test]
    fn connect_named_port_finds_registered_port() {
        with_isolated_ipc_test(|| {
            let name = crate::ipc_core::PortName::try_from_str("display-host").expect("port name");
            let port = create_named_port(Some(name)).expect("create named port");
            let client = connect_named_port(name).expect("connect named port");
            let server = accept_channel(port)
                .expect("accept")
                .expect("server channel");
            enqueue_message(
                client,
                IpcHeader {
                    opcode: 7,
                    ..IpcHeader::default()
                },
                b"surface",
                &[
                    KernelHandle::Console(ConsoleStreamKind::Input),
                    KernelHandle::Console(ConsoleStreamKind::Output),
                    KernelHandle::Console(ConsoleStreamKind::Error),
                ],
            )
            .expect("enqueue");
            let received = dequeue_message(server).expect("dequeue").expect("message");
            assert_eq!(received.header.opcode, 7);
            assert_eq!(received.payload, b"surface");
            assert_eq!(received.attached_handles.len(), 3);
        });
    }

    #[test]
    fn attached_handles_are_cloned_into_messages() {
        with_isolated_ipc_test(|| {
            let (left, right) = create_channel_pair().expect("create channel pair");
            enqueue_message(
                left,
                IpcHeader {
                    opcode: 55,
                    ..IpcHeader::default()
                },
                b"",
                &[
                    KernelHandle::Console(ConsoleStreamKind::Input),
                    KernelHandle::Console(ConsoleStreamKind::Output),
                ],
            )
            .expect("enqueue");
            let received = dequeue_message(right).expect("dequeue").expect("message");
            assert_eq!(received.attached_handles.len(), 2);
            assert!(matches!(
                &received.attached_handles[0],
                KernelHandle::Console(ConsoleStreamKind::Input)
            ));
            assert!(matches!(
                &received.attached_handles[1],
                KernelHandle::Console(ConsoleStreamKind::Output)
            ));
        });
    }

    #[test]
    fn enqueue_message_normalizes_header_lengths() {
        with_isolated_ipc_test(|| {
            let (left, right) = create_channel_pair().expect("create channel pair");
            enqueue_message(
                left,
                IpcHeader {
                    opcode: 9,
                    reserved: u16::MAX,
                    ..IpcHeader::default()
                },
                b"hello",
                &[KernelHandle::Console(ConsoleStreamKind::Input)],
            )
            .expect("enqueue");

            let received = dequeue_message(right)
                .expect("dequeue")
                .expect("message present");
            assert_eq!(received.header.payload_len, 5);
            assert_eq!(received.header.handle_count, 1);
            assert_eq!(received.header.reserved, 0);
        });
    }

    #[test]
    fn duplicate_named_ports_are_rejected() {
        with_isolated_ipc_test(|| {
            let name = crate::ipc_core::PortName::try_from_str("display-host").expect("port name");
            create_named_port(Some(name)).expect("first named port");
            assert_eq!(
                create_named_port(Some(name)),
                Err(IpcError::InvalidArgument)
            );
        });
    }

    #[test]
    fn duplicate_pending_channel_is_rejected() {
        with_isolated_ipc_test(|| {
            let port = create_named_port(None).expect("create port");
            let (_client, server) = create_channel_pair().expect("create channel pair");
            queue_channel_for_accept(port, server).expect("queue once");
            assert_eq!(
                queue_channel_for_accept(port, server),
                Err(IpcError::InvalidArgument)
            );
        });
    }

    #[test]
    fn buffer_too_small_preserves_front_message() {
        with_isolated_ipc_test(|| {
            let (left, right) = create_channel_pair().expect("create channel pair");
            enqueue_message(
                left,
                IpcHeader {
                    opcode: 77,
                    ..IpcHeader::default()
                },
                b"hello",
                &[KernelHandle::Console(ConsoleStreamKind::Input)],
            )
            .expect("enqueue");

            assert!(matches!(
                dequeue_message_with_limits(right, 4, 1),
                Err(IpcError::BufferTooSmall)
            ));

            let message = dequeue_message(right)
                .expect("dequeue")
                .expect("message present");
            assert_eq!(message.payload, b"hello");
            assert_eq!(message.attached_handles.len(), 1);
        });
    }

    #[test]
    fn endpoint_call_recv_reply_completes_response() {
        with_isolated_ipc_test(|| {
            let endpoint = super::create_endpoint().expect("create endpoint");
            let (reply, receiver) =
                super::enqueue_endpoint_call(endpoint, 41, b"statx").expect("enqueue call");
            assert_eq!(receiver, None);

            let (server_reply, request) = recv_endpoint(endpoint)
                .expect("recv endpoint")
                .expect("message queued");
            assert_eq!(server_reply, reply);
            assert_eq!(request, b"statx");

            let caller = super::complete_endpoint_reply(reply, b"ok").expect("reply");
            assert_eq!(caller, 41);
            let response = super::take_endpoint_response(reply)
                .expect("take response")
                .expect("response present");
            assert_eq!(response, b"ok");
            assert_eq!(
                super::take_endpoint_response(reply),
                Err(IpcError::InvalidHandle)
            );
        });
    }

    #[test]
    fn endpoint_request_handles_require_explicit_receive_capacity() {
        with_isolated_ipc_test(|| {
            let endpoint = super::create_endpoint().expect("create endpoint");
            let handle = transferable_file_handle(11);
            let (reply, _) =
                super::enqueue_endpoint_call_with_handles(endpoint, 41, b"open", &[handle])
                    .expect("enqueue call with handle");

            assert_eq!(
                recv_endpoint_with_limits(endpoint, usize::MAX),
                Err(IpcError::BufferTooSmall)
            );

            let (server_reply, request, handles) =
                recv_endpoint_with_limits_and_handles(endpoint, usize::MAX, 1)
                    .expect("recv endpoint")
                    .expect("message queued");
            assert_eq!(server_reply, reply);
            assert_eq!(request, b"open");
            assert_eq!(handles, alloc::vec![handle]);
        });
    }

    #[test]
    fn endpoint_rejects_non_transferable_request_handles() {
        with_isolated_ipc_test(|| {
            let endpoint = super::create_endpoint().expect("create endpoint");
            assert_eq!(
                super::enqueue_endpoint_call_with_handles(
                    endpoint,
                    41,
                    b"open",
                    &[non_transferable_file_handle(12)],
                ),
                Err(IpcError::InvalidArgument)
            );
            assert_eq!(
                super::enqueue_endpoint_call_with_handles(
                    endpoint,
                    41,
                    b"open",
                    &[super::KernelTransferredHandle::new(
                        0,
                        HandleToken::new(HandleOwner::Io, 12),
                        HandleRights::File(
                            FileHandleRights::READ.union(FileHandleRights::TRANSFER),
                        ),
                    )],
                ),
                Err(IpcError::InvalidArgument)
            );
        });
    }

    #[test]
    fn endpoint_request_handle_limit_is_bounded() {
        with_isolated_ipc_test(|| {
            let endpoint = super::create_endpoint().expect("create endpoint");
            let mut handles = alloc::vec::Vec::new();
            for index in 0..=super::MAX_ENDPOINT_TRANSFER_HANDLES {
                handles.push(transferable_file_handle(index as u64 + 1));
            }

            assert_eq!(
                super::enqueue_endpoint_call_with_handles(endpoint, 41, b"open", &handles),
                Err(IpcError::InvalidArgument)
            );
        });
    }

    #[test]
    fn endpoint_reply_handles_require_explicit_take_capacity() {
        with_isolated_ipc_test(|| {
            let endpoint = super::create_endpoint().expect("create endpoint");
            let (reply, _) =
                super::enqueue_endpoint_call(endpoint, 41, b"request").expect("enqueue call");
            let (_server_reply, _request) = recv_endpoint(endpoint)
                .expect("recv endpoint")
                .expect("message queued");
            let handle = transferable_file_handle(21);

            assert_eq!(
                super::complete_endpoint_reply_with_handles(reply, b"ok", &[handle]),
                Ok(41)
            );
            assert_eq!(
                super::take_endpoint_response(reply),
                Err(IpcError::BufferTooSmall)
            );

            let (bytes, handles) = super::take_endpoint_response_with_handle_limit(reply, 1)
                .expect("take response")
                .expect("response present");
            assert_eq!(bytes, b"ok");
            assert_eq!(handles, alloc::vec![handle]);
            assert_eq!(
                super::take_endpoint_response_with_handle_limit(reply, 1),
                Err(IpcError::InvalidHandle)
            );
        });
    }

    #[test]
    fn malformed_reply_handles_do_not_consume_reply_cap() {
        with_isolated_ipc_test(|| {
            let endpoint = super::create_endpoint().expect("create endpoint");
            let (reply, _) =
                super::enqueue_endpoint_call(endpoint, 7, b"request").expect("enqueue call");
            let (_server_reply, _request) = recv_endpoint(endpoint)
                .expect("recv endpoint")
                .expect("message queued");

            assert_eq!(
                super::complete_endpoint_reply_with_handles(
                    reply,
                    b"bad",
                    &[non_transferable_file_handle(33)],
                ),
                Err(IpcError::InvalidArgument)
            );
            assert_eq!(super::complete_endpoint_reply(reply, b"first"), Ok(7));
        });
    }

    #[test]
    fn endpoint_reply_cap_is_one_shot() {
        with_isolated_ipc_test(|| {
            let endpoint = super::create_endpoint().expect("create endpoint");
            let (reply, _) =
                super::enqueue_endpoint_call(endpoint, 7, b"request").expect("enqueue call");
            assert_eq!(super::complete_endpoint_reply(reply, b"first"), Ok(7));
            assert_eq!(
                super::complete_endpoint_reply(reply, b"second"),
                Err(IpcError::InvalidArgument)
            );
        });
    }

    #[test]
    fn endpoint_receiver_waiter_is_woken_by_next_call() {
        with_isolated_ipc_test(|| {
            let endpoint = super::create_endpoint().expect("create endpoint");
            super::add_endpoint_receiver_waiter(endpoint, 99).expect("add waiter");
            let (_reply, receiver) =
                super::enqueue_endpoint_call(endpoint, 1, b"request").expect("enqueue call");
            assert_eq!(receiver, Some(99));
        });
    }

    #[test]
    fn endpoint_queue_limit_is_bounded() {
        with_isolated_ipc_test(|| {
            let endpoint = super::create_endpoint().expect("create endpoint");
            for index in 0..super::MAX_ENDPOINT_PENDING_MESSAGES {
                let request = [index as u8 + 1];
                super::enqueue_endpoint_call(endpoint, index as u64, &request)
                    .expect("enqueue within limit");
            }
            assert_eq!(
                super::enqueue_endpoint_call(endpoint, 1000, b"x"),
                Err(IpcError::NoMemory)
            );
        });
    }

    #[test]
    fn endpoint_recv_capacity_preserves_front_message() {
        with_isolated_ipc_test(|| {
            let endpoint = super::create_endpoint().expect("create endpoint");
            super::enqueue_endpoint_call(endpoint, 3, b"long-request").expect("enqueue call");
            assert_eq!(
                recv_endpoint_with_limits(endpoint, 4),
                Err(IpcError::BufferTooSmall)
            );
            let (_reply, request) = recv_endpoint(endpoint)
                .expect("recv endpoint")
                .expect("message queued");
            assert_eq!(request, b"long-request");
        });
    }

    #[test]
    fn endpoint_rejects_malformed_message_lengths() {
        with_isolated_ipc_test(|| {
            let endpoint = super::create_endpoint().expect("create endpoint");
            assert_eq!(
                super::enqueue_endpoint_call(endpoint, 1, b""),
                Err(IpcError::InvalidArgument)
            );
            let oversized = alloc::vec![0_u8; super::MAX_ENDPOINT_INLINE_MESSAGE_BYTES + 1];
            assert_eq!(
                super::enqueue_endpoint_call(endpoint, 1, oversized.as_slice()),
                Err(IpcError::InvalidArgument)
            );
        });
    }

    #[test]
    fn endpoint_owner_exit_fails_pending_callers() {
        with_isolated_ipc_test(|| {
            let endpoint = super::create_endpoint_for_task(Some(10)).expect("create endpoint");
            let (reply, _) =
                super::enqueue_endpoint_call(endpoint, 22, b"request").expect("enqueue call");

            let wake_set = super::fail_endpoints_owned_by_task(10, IpcError::PeerClosed);
            assert_eq!(wake_set.callers, alloc::vec![22]);
            assert!(wake_set.receivers.is_empty());
            assert_eq!(
                super::take_endpoint_response(reply),
                Err(IpcError::PeerClosed)
            );
            assert_eq!(
                super::enqueue_endpoint_call(endpoint, 23, b"request"),
                Err(IpcError::InvalidHandle)
            );
        });
    }

    #[test]
    fn endpoint_cancel_pending_call_removes_queued_message() {
        with_isolated_ipc_test(|| {
            let endpoint = super::create_endpoint().expect("create endpoint");
            let (reply, _) =
                super::enqueue_endpoint_call(endpoint, 22, b"request").expect("enqueue call");

            assert_eq!(super::cancel_endpoint_call(reply, 22), Ok(()));
            assert_eq!(recv_endpoint(endpoint), Ok(None));
            assert_eq!(
                super::take_endpoint_response(reply),
                Err(IpcError::InvalidHandle)
            );
            assert_eq!(
                super::complete_endpoint_reply(reply, b"late"),
                Err(IpcError::InvalidHandle)
            );
        });
    }

    #[test]
    fn endpoint_cancel_dequeued_call_invalidates_late_reply() {
        with_isolated_ipc_test(|| {
            let endpoint = super::create_endpoint().expect("create endpoint");
            let (reply, _) =
                super::enqueue_endpoint_call(endpoint, 22, b"request").expect("enqueue call");
            let (server_reply, request) = recv_endpoint(endpoint)
                .expect("recv endpoint")
                .expect("message queued");
            assert_eq!(server_reply, reply);
            assert_eq!(request, b"request");

            assert_eq!(super::cancel_endpoint_call(reply, 22), Ok(()));
            assert_eq!(
                super::complete_endpoint_reply(reply, b"late"),
                Err(IpcError::InvalidHandle)
            );
            assert_eq!(
                super::take_endpoint_response(reply),
                Err(IpcError::InvalidHandle)
            );
        });
    }

    #[test]
    fn endpoint_cancel_rejects_wrong_caller_without_consuming_reply() {
        with_isolated_ipc_test(|| {
            let endpoint = super::create_endpoint().expect("create endpoint");
            let (reply, _) =
                super::enqueue_endpoint_call(endpoint, 22, b"request").expect("enqueue call");

            assert_eq!(
                super::cancel_endpoint_call(reply, 23),
                Err(IpcError::InvalidArgument)
            );
            let (server_reply, request) = recv_endpoint(endpoint)
                .expect("recv endpoint")
                .expect("message queued");
            assert_eq!(server_reply, reply);
            assert_eq!(request, b"request");
            assert_eq!(super::complete_endpoint_reply(reply, b"ok"), Ok(22));
            assert_eq!(
                super::take_endpoint_response(reply),
                Ok(Some(alloc::vec![b'o', b'k']))
            );
        });
    }

    #[test]
    fn endpoint_owner_exit_wakes_receivers() {
        with_isolated_ipc_test(|| {
            let endpoint = super::create_endpoint_for_task(Some(10)).expect("create endpoint");
            super::add_endpoint_receiver_waiter(endpoint, 31).expect("add waiter");
            super::add_endpoint_receiver_waiter(endpoint, 32).expect("add waiter");

            let wake_set = super::fail_endpoints_owned_by_task(10, IpcError::PeerClosed);
            assert!(wake_set.callers.is_empty());
            assert_eq!(wake_set.receivers, alloc::vec![31, 32]);
        });
    }

    #[test]
    fn endpoint_owner_exit_fails_dequeued_call() {
        with_isolated_ipc_test(|| {
            let endpoint = super::create_endpoint_for_task(Some(10)).expect("create endpoint");
            let (reply, _) =
                super::enqueue_endpoint_call(endpoint, 22, b"request").expect("enqueue call");
            let (server_reply, _request) = recv_endpoint(endpoint)
                .expect("recv endpoint")
                .expect("message queued");
            assert_eq!(server_reply, reply);

            let wake_set = super::fail_endpoints_owned_by_task(10, IpcError::PeerClosed);
            assert_eq!(wake_set.callers, alloc::vec![22]);
            assert!(wake_set.receivers.is_empty());
            assert_eq!(
                super::complete_endpoint_reply(reply, b"late"),
                Err(IpcError::InvalidArgument)
            );
            assert_eq!(
                super::take_endpoint_response(reply),
                Err(IpcError::PeerClosed)
            );
        });
    }

    #[test]
    fn endpoint_remove_waiters_for_task_prunes_stale_waiters() {
        with_isolated_ipc_test(|| {
            let first = super::create_endpoint().expect("create first endpoint");
            let second = super::create_endpoint().expect("create second endpoint");
            super::add_endpoint_receiver_waiter(first, 9).expect("add first stale waiter");
            super::add_endpoint_receiver_waiter(first, 10).expect("add live waiter");
            super::add_endpoint_receiver_waiter(second, 9).expect("add second stale waiter");

            assert_eq!(super::remove_endpoint_waiters_for_task(9), 2);
            let (_reply, receiver_to_wake) =
                super::enqueue_endpoint_call(first, 22, b"request").expect("enqueue first");
            assert_eq!(receiver_to_wake, Some(10));
            let (_reply, receiver_to_wake) =
                super::enqueue_endpoint_call(second, 23, b"request").expect("enqueue second");
            assert_eq!(receiver_to_wake, None);
        });
    }
}
