use alloc::collections::BTreeMap;
use alloc::vec::Vec;
#[cfg(not(test))]
use core::ptr;

#[cfg(test)]
use alloc::collections::VecDeque;

use crate::ipc_core::SharedRegionHandle;
#[cfg(test)]
use crate::ipc_core::{
    ChannelHandle, EventHandle, IpcHeader, PORT_NAME_CAPACITY, PortHandle, PortName,
};
use spin::Mutex;
#[cfg(not(test))]
use x86_64::instructions::interrupts;

#[cfg(not(test))]
use crate::memory::{kernel_vm, phys};

const PAGE_SIZE: usize = 4096;
#[cfg(test)]
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
        connect_port, create_channel_pair, create_event, create_named_port,
        create_shared_region, dequeue_message, dequeue_message_with_limits, enqueue_message,
        event_signal_count, lookup_named_port, map_shared_region, port_name,
        queue_channel_for_accept, shared_region_len, signal_event,
    };
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
}
