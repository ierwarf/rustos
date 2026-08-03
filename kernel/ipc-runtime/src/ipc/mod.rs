//! Generational kernel IPC objects, replies, queues, and shared regions.
//!
//! - **Owner:** `kernel-ipc-runtime` owns object identity and transport
//!   mechanics; services own policy and request semantics.
//! - **Boundary:** Endpoint handles, message bytes, transferred capabilities,
//!   owner identities, and shared-region requests cross protection boundaries.
//! - **Lifecycle:** Reserve quota, allocate an unpublished generational slot,
//!   publish, settle reply/close/revoke, remove the exact generation, then
//!   reclaim backing outside the slot lock.
//! - **Concurrency:** Each production object slot has a tracked lock; guarded
//!   closures are bounded, non-blocking, allocation-free, and callback-free.
//! - **Failure:** Queue/capacity exhaustion, peer exit, timeout, duplicate
//!   reply, stale handle, and partial transfer converge without leaked quota or
//!   resurrected authority.
//! - **Forbidden:** No production global object-table lock, allocation under a
//!   raw slot lock, pointer-bearing wire record, or owner identity by PID alone.
//! - **Evidence:** `ipc-call`, `endpoint-lifecycle`,
//!   `ipc-handle-transfer`, `kernel-resource-lifecycle`, `root-authority`, and
//!   `service-call-authority`.
#[cfg(test)]
use alloc::collections::BTreeMap;
use alloc::collections::VecDeque;
use alloc::vec::Vec;
#[cfg(not(test))]
use core::ptr;
use core::sync::atomic::{AtomicUsize, Ordering};

mod endpoint_priority;
mod shared_region_hold;
mod slab;

pub use endpoint_priority::EndpointCallPriority;
use endpoint_priority::EndpointObject;
pub use shared_region_hold::KernelSharedRegionMappingHold;

use crate::ipc_core::SharedRegionHandle;
#[cfg(test)]
use crate::ipc_core::{
    ChannelHandle, EventHandle, IpcHeader, PORT_NAME_CAPACITY, PortHandle, PortName,
};
use kernel_object::api::handle::{HandleRights, HandleToken};
use nucleus_core::util::lockdep::{LockClass, TrackedSpinLock};
#[cfg(test)]
use spin::Mutex;

use slab::GenerationalSlab;

#[cfg(not(test))]
use crate::memory::{kernel_vm, phys};

#[cfg(not(test))]
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
const MAX_ENDPOINT_INLINE_MESSAGE_BYTES: usize = rustos_user_abi::syscall::IPC_MAX_INLINE_BYTES;
const MAX_ENDPOINT_PENDING_MESSAGES: usize = 64;
const MAX_ENDPOINT_TRANSFER_HANDLES: usize = 16;
const MAX_ENDPOINT_WAITERS: usize = 64;
const MAX_ENDPOINT_OBJECTS: usize = 512;
const MAX_ENDPOINT_MESSAGE_OBJECTS: usize = 128;
const MAX_REPLY_OBJECTS: usize = MAX_ENDPOINT_MESSAGE_OBJECTS;
const MAX_SHARED_REGION_OBJECTS: usize = 1024;
const MAX_OWNED_ENDPOINT_OBJECTS: usize = MAX_ENDPOINT_OBJECTS - 64;
const MAX_ENDPOINTS_PER_PROCESS: usize = 32;
const MAX_ENDPOINTS_PER_TASK: usize = 8;
pub const MAX_ENDPOINT_WAKE_TASKS: usize = 128;
const MAX_SHARED_REGION_BYTES: usize = 256 * 1024 * 1024;
const MAX_SHARED_REGION_TOTAL_BYTES: usize = 512 * 1024 * 1024;
const MAX_SHARED_REGIONS_PER_PROCESS: usize = 64;
const MAX_SHARED_REGION_BYTES_PER_PROCESS: usize = 128 * 1024 * 1024;

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

/// Integer-only capability ticket that may cross a Ring0/Ring3 byte boundary.
///
/// `KernelTransferredHandle` contains Rust enums and is therefore never a wire
/// type.  The random nonce also prevents a service from guessing a sequential
/// registry id or replaying a stale id after a future registry reuse.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct KernelTransferTicket {
    transfer_id: u64,
    nonce: u64,
    batch_generation: u64,
}

impl KernelTransferTicket {
    pub const fn new(transfer_id: u64, nonce: u64, batch_generation: u64) -> Option<Self> {
        if transfer_id == 0 || nonce == 0 || batch_generation == 0 {
            return None;
        }
        Some(Self {
            transfer_id,
            nonce,
            batch_generation,
        })
    }

    pub const fn transfer_id(self) -> u64 {
        self.transfer_id
    }

    pub const fn nonce(self) -> u64 {
        self.nonce
    }

    pub const fn batch_generation(self) -> u64 {
        self.batch_generation
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ProcessIdentity {
    pub pid: u64,
    pub generation: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ServiceIdentity {
    pub service_id: u64,
    pub epoch: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ChannelIdentity {
    pub channel_id: u64,
    pub generation: u64,
    pub receiver_side: u8,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TransferContext {
    pub source: ProcessIdentity,
    pub service: ServiceIdentity,
    pub channel: ChannelIdentity,
    pub stream_start: u64,
    pub stream_end: u64,
    pub intended_receiver: Option<ProcessIdentity>,
}

pub type EndpointReceived = (KernelReplyHandle, Vec<u8>, Vec<KernelTransferredHandle>);
pub type EndpointReceivedWithSender = (
    KernelReplyHandle,
    Vec<u8>,
    Vec<KernelTransferredHandle>,
    u64,
);
pub type EndpointResponseWithHandles = (Vec<u8>, Vec<KernelTransferredHandle>);

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum EndpointResponseTake {
    Pending,
    Response(EndpointResponseWithHandles),
    /// The endpoint owner failed before consuming request-attached handles.
    /// The caller's handle substrate must drop these descriptors exactly once
    /// before it reports `error` to userspace.
    Error {
        error: IpcError,
        discarded_request_handles: Vec<KernelTransferredHandle>,
    },
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
    PermissionDenied,
    PeerClosed,
    BufferTooSmall,
    InvalidArgument,
    NoMemory,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EndpointWakeSet {
    callers: [u64; MAX_ENDPOINT_WAKE_TASKS],
    caller_count: usize,
    receivers: [u64; MAX_ENDPOINT_WAKE_TASKS],
    receiver_count: usize,
}

impl Default for EndpointWakeSet {
    fn default() -> Self {
        Self {
            callers: [0; MAX_ENDPOINT_WAKE_TASKS],
            caller_count: 0,
            receivers: [0; MAX_ENDPOINT_WAKE_TASKS],
            receiver_count: 0,
        }
    }
}

impl EndpointWakeSet {
    fn push_caller(&mut self, task_id: u64) {
        if self.callers[..self.caller_count].contains(&task_id) {
            return;
        }
        assert!(
            self.caller_count < self.callers.len(),
            "endpoint caller wake set exceeds scheduler task capacity"
        );
        self.callers[self.caller_count] = task_id;
        self.caller_count += 1;
    }

    fn push_receiver(&mut self, task_id: u64) {
        if self.receivers[..self.receiver_count].contains(&task_id) {
            return;
        }
        assert!(
            self.receiver_count < self.receivers.len(),
            "endpoint receiver wake set exceeds scheduler task capacity"
        );
        self.receivers[self.receiver_count] = task_id;
        self.receiver_count += 1;
    }

    pub fn callers(&self) -> &[u64] {
        &self.callers[..self.caller_count]
    }

    pub fn receivers(&self) -> &[u64] {
        &self.receivers[..self.receiver_count]
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
    owner_process_id: Option<u64>,
    bytes: Vec<u8>,
    references: AtomicUsize,
}

#[cfg(not(test))]
struct SharedRegionObject {
    byte_len: usize,
    owner_process_id: Option<u64>,
    phys_start: u64,
    page_count: usize,
    references: AtomicUsize,
}

impl SharedRegionObject {
    fn try_retain(&self) -> bool {
        self.references
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |references| {
                (references != 0)
                    .then(|| references.checked_add(1))
                    .flatten()
            })
            .is_ok()
    }

    fn release(&self) -> bool {
        let previous = self.references.fetch_sub(1, Ordering::Release);
        assert!(previous != 0, "shared-region reference underflow");
        previous == 1
    }
}

struct SharedRegionReclaimQueue {
    entries: [Option<SharedRegionObject>; MAX_SHARED_REGION_OBJECTS],
    len: usize,
}

impl SharedRegionReclaimQueue {
    const fn new() -> Self {
        Self {
            entries: [const { None }; MAX_SHARED_REGION_OBJECTS],
            len: 0,
        }
    }

    fn push(&mut self, object: SharedRegionObject) {
        let slot = self
            .entries
            .get_mut(self.len)
            .expect("shared-region reclaim admission invariant violated");
        *slot = Some(object);
        self.len += 1;
    }

    fn pop_front(&mut self) -> Option<SharedRegionObject> {
        let object = self.entries.first_mut()?.take()?;
        for index in 1..self.len {
            self.entries[index - 1] = self.entries[index].take();
        }
        self.len -= 1;
        Some(object)
    }

    #[cfg(test)]
    fn clear(&mut self) {
        for slot in &mut self.entries[..self.len] {
            *slot = None;
        }
        self.len = 0;
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum EndpointOwner {
    Task(u64),
    Process(u64),
}

#[derive(Clone, Copy)]
struct EndpointQuota {
    owner: EndpointOwner,
    count: usize,
}

struct EndpointQuotaTable {
    entries: [Option<EndpointQuota>; MAX_ENDPOINT_OBJECTS],
    owned_total: usize,
}

impl EndpointQuotaTable {
    const fn new() -> Self {
        Self {
            entries: [None; MAX_ENDPOINT_OBJECTS],
            owned_total: 0,
        }
    }

    fn reserve(&mut self, owner: EndpointOwner) -> Result<(), IpcError> {
        if self.owned_total >= MAX_OWNED_ENDPOINT_OBJECTS {
            return Err(IpcError::NoMemory);
        }
        let per_owner_limit = match owner {
            EndpointOwner::Task(_) => MAX_ENDPOINTS_PER_TASK,
            EndpointOwner::Process(_) => MAX_ENDPOINTS_PER_PROCESS,
        };
        if let Some(entry) = self
            .entries
            .iter_mut()
            .flatten()
            .find(|entry| entry.owner == owner)
        {
            if entry.count >= per_owner_limit {
                return Err(IpcError::NoMemory);
            }
            entry.count += 1;
            self.owned_total += 1;
            return Ok(());
        }
        let Some(slot) = self.entries.iter_mut().find(|entry| entry.is_none()) else {
            return Err(IpcError::NoMemory);
        };
        *slot = Some(EndpointQuota { owner, count: 1 });
        self.owned_total += 1;
        Ok(())
    }

    fn release(&mut self, owner: EndpointOwner) {
        let Some(slot) = self
            .entries
            .iter_mut()
            .find(|entry| entry.is_some_and(|entry| entry.owner == owner))
        else {
            panic!("endpoint quota release lost owner accounting");
        };
        let entry = slot
            .as_mut()
            .expect("matched endpoint quota slot must be populated");
        if entry.count == 0 || self.owned_total == 0 {
            panic!("endpoint quota accounting underflow");
        }
        entry.count -= 1;
        self.owned_total -= 1;
        if entry.count == 0 {
            *slot = None;
        }
    }

    #[cfg(test)]
    fn clear(&mut self) {
        self.entries = [None; MAX_ENDPOINT_OBJECTS];
        self.owned_total = 0;
    }
}

#[derive(Clone, Copy)]
struct SharedRegionQuota {
    process_id: u64,
    object_count: usize,
    byte_count: usize,
}

struct SharedRegionQuotaTable {
    entries: [Option<SharedRegionQuota>; MAX_PROCESS_RESOURCE_OWNERS],
}

const MAX_PROCESS_RESOURCE_OWNERS: usize = 32;

impl SharedRegionQuotaTable {
    const fn new() -> Self {
        Self {
            entries: [None; MAX_PROCESS_RESOURCE_OWNERS],
        }
    }

    fn reserve(&mut self, process_id: u64, byte_len: usize) -> Result<(), IpcError> {
        if let Some(entry) = self
            .entries
            .iter_mut()
            .flatten()
            .find(|entry| entry.process_id == process_id)
        {
            if entry.object_count >= MAX_SHARED_REGIONS_PER_PROCESS
                || entry
                    .byte_count
                    .checked_add(byte_len)
                    .is_none_or(|bytes| bytes > MAX_SHARED_REGION_BYTES_PER_PROCESS)
            {
                return Err(IpcError::NoMemory);
            }
            entry.object_count += 1;
            entry.byte_count += byte_len;
            return Ok(());
        }
        if byte_len > MAX_SHARED_REGION_BYTES_PER_PROCESS {
            return Err(IpcError::NoMemory);
        }
        let Some(slot) = self.entries.iter_mut().find(|entry| entry.is_none()) else {
            return Err(IpcError::NoMemory);
        };
        *slot = Some(SharedRegionQuota {
            process_id,
            object_count: 1,
            byte_count: byte_len,
        });
        Ok(())
    }

    fn release(&mut self, process_id: u64, byte_len: usize) {
        let Some(slot) = self
            .entries
            .iter_mut()
            .find(|entry| entry.is_some_and(|entry| entry.process_id == process_id))
        else {
            panic!("shared-region quota release lost process accounting");
        };
        let entry = slot
            .as_mut()
            .expect("matched shared-region quota slot must be populated");
        if entry.object_count == 0 || entry.byte_count < byte_len {
            panic!("shared-region quota accounting underflow");
        }
        entry.object_count -= 1;
        entry.byte_count -= byte_len;
        if entry.object_count == 0 {
            assert_eq!(
                entry.byte_count, 0,
                "shared-region object/byte accounting diverged"
            );
            *slot = None;
        }
    }

    #[cfg(test)]
    fn clear(&mut self) {
        self.entries = [None; MAX_PROCESS_RESOURCE_OWNERS];
    }
}

struct EndpointMessageObject {
    endpoint_id: u64,
    reply_id: u64,
    caller_task_id: u64,
    published: bool,
    request: Vec<u8>,
    attached_handles: Vec<KernelTransferredHandle>,
    response: Option<EndpointResponse>,
}

struct ReplyObject {
    message_id: u64,
    receiver_owner: Option<EndpointOwner>,
    used: bool,
    consumed: bool,
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

#[cfg(test)]
#[derive(Default)]
struct IpcObjectTable {
    next_id: u64,
    named_ports: BTreeMap<PortName, u64>,
    ports: BTreeMap<u64, PortObject>,
    channels: BTreeMap<u64, ChannelObject>,
    events: BTreeMap<u64, EventObject>,
}

#[cfg(test)]
impl IpcObjectTable {
    const fn new() -> Self {
        Self {
            next_id: 1,
            named_ports: BTreeMap::new(),
            ports: BTreeMap::new(),
            channels: BTreeMap::new(),
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

static ENDPOINTS: GenerationalSlab<
    EndpointObject,
    MAX_ENDPOINT_OBJECTS,
    { LockClass::IpcEndpoint as u8 },
> = GenerationalSlab::new();
static ENDPOINT_QUOTAS: TrackedSpinLock<EndpointQuotaTable, { LockClass::IpcEndpointQuota as u8 }> =
    TrackedSpinLock::new(EndpointQuotaTable::new());
static ENDPOINT_MESSAGES: GenerationalSlab<
    EndpointMessageObject,
    MAX_ENDPOINT_MESSAGE_OBJECTS,
    { LockClass::IpcMessage as u8 },
> = GenerationalSlab::new();
static REPLIES: GenerationalSlab<ReplyObject, MAX_REPLY_OBJECTS, { LockClass::IpcReply as u8 }> =
    GenerationalSlab::new();
static SHARED_REGIONS: GenerationalSlab<
    SharedRegionObject,
    MAX_SHARED_REGION_OBJECTS,
    { LockClass::IpcRegion as u8 },
> = GenerationalSlab::new();
static SHARED_REGION_RECLAIMS: TrackedSpinLock<
    SharedRegionReclaimQueue,
    { LockClass::IpcRegionReclaim as u8 },
> = TrackedSpinLock::new(SharedRegionReclaimQueue::new());
static SHARED_REGION_ADMITTED: AtomicUsize = AtomicUsize::new(0);
static SHARED_REGION_BYTES_ADMITTED: AtomicUsize = AtomicUsize::new(0);
static SHARED_REGION_QUOTAS: TrackedSpinLock<
    SharedRegionQuotaTable,
    { LockClass::IpcRegionQuota as u8 },
> = TrackedSpinLock::new(SharedRegionQuotaTable::new());

#[cfg(test)]
static IPC_OBJECTS: Mutex<IpcObjectTable> = Mutex::new(IpcObjectTable::new());

#[cfg(test)]
fn with_ipc_objects<R>(f: impl FnOnce(&mut IpcObjectTable) -> R) -> R {
    let mut objects = IPC_OBJECTS.lock();
    f(&mut objects)
}

#[cfg(test)]
fn with_ipc_objects_ref<R>(f: impl FnOnce(&IpcObjectTable) -> R) -> R {
    let objects = IPC_OBJECTS.lock();
    f(&objects)
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
        if let Some(name) = normalized_name
            && objects.named_ports.contains_key(&name)
        {
            return Err(IpcError::InvalidArgument);
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

fn reserve_shared_region_admission(
    owner_process_id: Option<u64>,
    byte_len: usize,
) -> Result<(), IpcError> {
    if SHARED_REGION_ADMITTED
        .fetch_update(Ordering::AcqRel, Ordering::Acquire, |admitted| {
            if admitted >= MAX_SHARED_REGION_OBJECTS {
                return None;
            }
            Some(admitted + 1)
        })
        .is_err()
    {
        return Err(IpcError::NoMemory);
    }
    if SHARED_REGION_BYTES_ADMITTED
        .fetch_update(Ordering::AcqRel, Ordering::Acquire, |admitted| {
            admitted
                .checked_add(byte_len)
                .filter(|bytes| *bytes <= MAX_SHARED_REGION_TOTAL_BYTES)
        })
        .is_err()
    {
        SHARED_REGION_ADMITTED.fetch_sub(1, Ordering::AcqRel);
        return Err(IpcError::NoMemory);
    }
    if let Some(process_id) = owner_process_id
        && let Err(error) = SHARED_REGION_QUOTAS.lock().reserve(process_id, byte_len)
    {
        SHARED_REGION_BYTES_ADMITTED.fetch_sub(byte_len, Ordering::AcqRel);
        SHARED_REGION_ADMITTED.fetch_sub(1, Ordering::AcqRel);
        return Err(error);
    }
    Ok(())
}

fn release_shared_region_admission(owner_process_id: Option<u64>, byte_len: usize) {
    if let Some(process_id) = owner_process_id {
        SHARED_REGION_QUOTAS.lock().release(process_id, byte_len);
    }
    let previous_bytes = SHARED_REGION_BYTES_ADMITTED.fetch_sub(byte_len, Ordering::AcqRel);
    assert!(
        previous_bytes >= byte_len,
        "shared-region admission byte count underflow"
    );
    let previous = SHARED_REGION_ADMITTED.fetch_sub(1, Ordering::AcqRel);
    assert!(previous != 0, "shared-region admission count underflow");
}

pub fn create_shared_region(byte_len: usize) -> Result<KernelSharedRegionHandle, IpcError> {
    create_shared_region_with_owner(None, byte_len)
}

pub fn create_shared_region_for_process(
    owner_process_id: u64,
    byte_len: usize,
) -> Result<KernelSharedRegionHandle, IpcError> {
    create_shared_region_with_owner(Some(owner_process_id), byte_len)
}

fn create_shared_region_with_owner(
    owner_process_id: Option<u64>,
    byte_len: usize,
) -> Result<KernelSharedRegionHandle, IpcError> {
    if byte_len == 0 || byte_len > MAX_SHARED_REGION_BYTES {
        return Err(IpcError::InvalidArgument);
    }
    reserve_shared_region_admission(owner_process_id, byte_len)?;

    #[cfg(test)]
    let object_result = Ok::<SharedRegionObject, IpcError>(SharedRegionObject {
        byte_len,
        owner_process_id,
        bytes: alloc::vec![0_u8; byte_len],
        references: AtomicUsize::new(1),
    });

    #[cfg(not(test))]
    let object_result = (|| {
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
        Ok(SharedRegionObject {
            byte_len,
            owner_process_id,
            phys_start: phys_start.as_u64(),
            page_count,
            references: AtomicUsize::new(1),
        })
    })();

    let object = match object_result {
        Ok(object) => object,
        Err(error) => {
            release_shared_region_admission(owner_process_id, byte_len);
            return Err(error);
        }
    };
    match SHARED_REGIONS.insert(object) {
        Ok(raw) => Ok(KernelSharedRegionHandle::from_raw(raw)),
        Err(object) => {
            enqueue_shared_region_reclaim(object);
            Err(IpcError::NoMemory)
        }
    }
}

pub fn retain_shared_region(region: KernelSharedRegionHandle) -> bool {
    SHARED_REGIONS
        .with(region.raw(), SharedRegionObject::try_retain)
        .unwrap_or(false)
}

pub fn release_shared_region(region: KernelSharedRegionHandle) {
    let remove = SHARED_REGIONS
        .with(region.raw(), SharedRegionObject::release)
        .unwrap_or(false);
    if remove {
        let removed = SHARED_REGIONS.remove(region.raw());
        let removed = removed.expect("last shared-region reference lost its object");
        enqueue_shared_region_reclaim(removed);
    }
}

pub fn acquire_shared_region_mapping(
    region: KernelSharedRegionHandle,
) -> Option<KernelSharedRegionMappingHold> {
    retain_shared_region(region).then_some(KernelSharedRegionMappingHold::new(region))
}

fn enqueue_shared_region_reclaim(object: SharedRegionObject) {
    SHARED_REGION_RECLAIMS.lock().push(object);
}

pub fn service_deferred_shared_region_reclaims(max_pages: usize) -> usize {
    if max_pages == 0 {
        return 0;
    }
    let Some(mut object) = SHARED_REGION_RECLAIMS.lock().pop_front() else {
        return 0;
    };

    #[cfg(test)]
    let reclaimed = {
        object.bytes.clear();
        1
    };

    #[cfg(not(test))]
    let reclaimed = {
        let count = max_pages.min(object.page_count);
        for _ in 0..count {
            object.page_count -= 1;
            phys::free_frame(x86_64::PhysAddr::new(
                object.phys_start + object.page_count as u64 * PAGE_SIZE as u64,
            ));
        }
        count
    };

    #[cfg(test)]
    let complete = object.bytes.is_empty();
    #[cfg(not(test))]
    let complete = object.page_count == 0;

    if complete {
        release_shared_region_admission(object.owner_process_id, object.byte_len);
    } else {
        enqueue_shared_region_reclaim(object);
    }
    reclaimed
}

pub fn create_endpoint() -> Result<KernelEndpointHandle, IpcError> {
    create_endpoint_with_owner(None)
}

pub fn create_endpoint_for_task(
    owner_task_id: Option<u64>,
) -> Result<KernelEndpointHandle, IpcError> {
    create_endpoint_with_owner(owner_task_id.map(EndpointOwner::Task))
}

/// User-visible endpoints are process-owned: a service may safely receive and
/// reply on a worker thread, while another process cannot consume its queue or
/// forge a reply with a guessed raw capability.
pub fn create_endpoint_for_process(
    owner_process_id: u64,
) -> Result<KernelEndpointHandle, IpcError> {
    create_endpoint_with_owner(Some(EndpointOwner::Process(owner_process_id)))
}

fn create_endpoint_with_owner(
    owner: Option<EndpointOwner>,
) -> Result<KernelEndpointHandle, IpcError> {
    if let Some(owner) = owner {
        ENDPOINT_QUOTAS.lock().reserve(owner)?;
    }
    let endpoint = (|| {
        let mut pending_messages = VecDeque::new();
        pending_messages
            .try_reserve_exact(MAX_ENDPOINT_PENDING_MESSAGES)
            .map_err(|_| IpcError::NoMemory)?;
        let mut pending_system_messages = VecDeque::new();
        pending_system_messages
            .try_reserve_exact(MAX_ENDPOINT_PENDING_MESSAGES)
            .map_err(|_| IpcError::NoMemory)?;
        let mut waiting_receivers = VecDeque::new();
        waiting_receivers
            .try_reserve_exact(MAX_ENDPOINT_WAITERS)
            .map_err(|_| IpcError::NoMemory)?;
        Ok(EndpointObject::new(
            owner,
            // Reserve the complete admitted queue sizes before publication. No
            // endpoint operation can therefore enter the allocator while
            // holding the endpoint slot lock.
            pending_messages,
            pending_system_messages,
            waiting_receivers,
        ))
    })();
    let endpoint = match endpoint {
        Ok(endpoint) => endpoint,
        Err(error) => {
            if let Some(owner) = owner {
                ENDPOINT_QUOTAS.lock().release(owner);
            }
            return Err(error);
        }
    };
    match ENDPOINTS.insert(endpoint) {
        Ok(handle) => Ok(KernelEndpointHandle::from_raw(handle)),
        Err(endpoint) => {
            if let Some(owner) = endpoint.owner {
                ENDPOINT_QUOTAS.lock().release(owner);
            }
            Err(IpcError::NoMemory)
        }
    }
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
    enqueue_endpoint_call_with_handles_and_priority(
        endpoint,
        caller_task_id,
        request,
        attached_handles,
        EndpointCallPriority::Ordinary,
    )
}

pub fn enqueue_endpoint_call_with_handles_and_priority(
    endpoint: KernelEndpointHandle,
    caller_task_id: u64,
    request: &[u8],
    attached_handles: &[KernelTransferredHandle],
    priority: EndpointCallPriority,
) -> Result<(KernelReplyHandle, Option<u64>), IpcError> {
    enqueue_endpoint_call_with_handles_faultable(
        endpoint,
        caller_task_id,
        request,
        attached_handles,
        priority,
        rustos_fault_injection::should_fail("ipc.endpoint.enqueue"),
    )
}

fn enqueue_endpoint_call_with_handles_faultable(
    endpoint: KernelEndpointHandle,
    caller_task_id: u64,
    request: &[u8],
    attached_handles: &[KernelTransferredHandle],
    priority: EndpointCallPriority,
    injected_failure: bool,
) -> Result<(KernelReplyHandle, Option<u64>), IpcError> {
    if request.is_empty() || request.len() > MAX_ENDPOINT_INLINE_MESSAGE_BYTES {
        return Err(IpcError::InvalidArgument);
    }
    validate_endpoint_transfer_handles(attached_handles)?;
    if injected_failure {
        return Err(IpcError::NoMemory);
    }

    let receiver_owner = ENDPOINTS
        .with(endpoint.raw(), |endpoint| endpoint.owner)
        .ok_or(IpcError::InvalidHandle)?;

    let mut request_handles = Vec::new();
    request_handles
        .try_reserve_exact(MAX_ENDPOINT_TRANSFER_HANDLES.saturating_mul(2))
        .map_err(|_| IpcError::NoMemory)?;
    request_handles.extend_from_slice(attached_handles);
    let request = copy_endpoint_bytes(request)?;
    let message_id = ENDPOINT_MESSAGES
        .insert(EndpointMessageObject {
            endpoint_id: endpoint.raw(),
            reply_id: 0,
            caller_task_id,
            published: false,
            request,
            attached_handles: request_handles,
            response: None,
        })
        .map_err(|_| IpcError::NoMemory)?;
    let reply_id = match REPLIES.insert(ReplyObject {
        message_id,
        receiver_owner,
        used: false,
        consumed: false,
    }) {
        Ok(reply_id) => reply_id,
        Err(_) => {
            drop(ENDPOINT_MESSAGES.remove(message_id));
            return Err(IpcError::NoMemory);
        }
    };
    let _ = ENDPOINT_MESSAGES.with_mut(message_id, |message| message.reply_id = reply_id);

    let receiver_to_wake = ENDPOINTS.with_mut(endpoint.raw(), |endpoint_object| {
        if endpoint_object.pending_len() >= MAX_ENDPOINT_PENDING_MESSAGES {
            return None;
        }
        let published = ENDPOINT_MESSAGES.with_mut(message_id, |message| {
            if message.endpoint_id != endpoint.raw() || message.reply_id != reply_id {
                return false;
            }
            message.published = true;
            true
        });
        if published != Some(true) {
            return None;
        }
        endpoint_object.push_pending(priority, message_id);
        Some(
            endpoint_object
                .waiting_receivers
                .pop_front()
                .or(match endpoint_object.owner {
                    Some(EndpointOwner::Task(task_id)) => Some(task_id),
                    Some(EndpointOwner::Process(_)) | None => None,
                })
                .filter(|task_id| *task_id != caller_task_id),
        )
    });
    let Some(Some(receiver_to_wake)) = receiver_to_wake else {
        let _ = REPLIES.remove(reply_id);
        drop(ENDPOINT_MESSAGES.remove(message_id));
        return if ENDPOINTS.with(endpoint.raw(), |_| ()).is_some() {
            Err(IpcError::NoMemory)
        } else {
            Err(IpcError::InvalidHandle)
        };
    };

    Ok((KernelReplyHandle::from_raw(reply_id), receiver_to_wake))
}

/// Returns the process owner of the endpoint bound into a live reply
/// capability.  The scheduler uses this only to establish bounded priority
/// inheritance before a process-owned endpoint has selected a specific worker.
pub fn endpoint_receiver_process_for_reply(reply: KernelReplyHandle) -> Option<u64> {
    REPLIES
        .with(reply.raw(), |reply_object| {
            match reply_object.receiver_owner {
                Some(EndpointOwner::Process(process_id)) => Some(process_id),
                Some(EndpointOwner::Task(_)) | None => None,
            }
        })
        .flatten()
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
) -> Result<Option<EndpointReceived>, IpcError> {
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
) -> Result<Option<EndpointReceivedWithSender>, IpcError> {
    ENDPOINTS
        .with_mut(endpoint.raw(), |endpoint_object| {
            let Some((lane, message_id)) = endpoint_object.next_pending() else {
                return Ok(None);
            };

            let received = ENDPOINT_MESSAGES
                .with_mut(message_id, |message| {
                    if message.endpoint_id != endpoint.raw() {
                        return Err(IpcError::InvalidHandle);
                    }
                    if message.request.len() > request_capacity
                        || message.attached_handles.len() > handle_capacity
                    {
                        return Err(IpcError::BufferTooSmall);
                    }
                    let request = core::mem::take(&mut message.request);
                    let attached_handles = core::mem::take(&mut message.attached_handles);
                    Ok((
                        KernelReplyHandle::from_raw(message.reply_id),
                        request,
                        attached_handles,
                        message.caller_task_id,
                    ))
                })
                .ok_or(IpcError::InvalidHandle)??;
            endpoint_object.consume_pending(lane, message_id);
            Ok(Some(received))
        })
        .ok_or(IpcError::InvalidHandle)?
}

/// Confirms that a task-owned internal endpoint is received only by that task.
/// User-visible endpoints use `authorize_endpoint_receiver_for_process` so
/// workers in the owning process are valid receivers.
pub fn authorize_endpoint_receiver(
    endpoint: KernelEndpointHandle,
    receiver_task_id: u64,
) -> Result<(), IpcError> {
    ENDPOINTS
        .with(endpoint.raw(), |endpoint_object| {
            endpoint_owner_allows(endpoint_object.owner, EndpointOwner::Task(receiver_task_id))
        })
        .ok_or(IpcError::InvalidHandle)
        .and_then(|allowed| allowed.then_some(()).ok_or(IpcError::PermissionDenied))
}

pub fn authorize_endpoint_receiver_for_process(
    endpoint: KernelEndpointHandle,
    receiver_process_id: u64,
) -> Result<(), IpcError> {
    ENDPOINTS
        .with(endpoint.raw(), |endpoint_object| {
            endpoint_owner_allows(
                endpoint_object.owner,
                EndpointOwner::Process(receiver_process_id),
            )
        })
        .ok_or(IpcError::InvalidHandle)
        .and_then(|allowed| allowed.then_some(()).ok_or(IpcError::PermissionDenied))
}

fn endpoint_owner_allows(owner: Option<EndpointOwner>, receiver: EndpointOwner) -> bool {
    owner.is_none_or(|owner| owner == receiver)
}

/// Registers `task_id` as a receiver waiter on `endpoint`. Returns
/// `Ok(has_pending)` where `has_pending == true` means a message is already
/// queued on the endpoint at the moment of registration. Callers must use the
/// returned flag to skip blocking (and re-poll the queue) when `true`, closing
/// the recv→add-waiter→block race window where the producer queued a message
/// before our slot was visible and so issued no wake. A task is published as a
/// waiter only when the queue is still empty at this exact linearization point:
/// leaving a waiter behind while returning `has_pending` lets a later producer
/// consume stale wake/handoff authority for a receiver that never blocked.
pub fn add_endpoint_receiver_waiter(
    endpoint: KernelEndpointHandle,
    task_id: u64,
) -> Result<bool, IpcError> {
    ENDPOINTS
        .with_mut(endpoint.raw(), |endpoint_object| {
            if endpoint_object.has_pending() {
                return Ok(true);
            }
            let already_waiting = endpoint_object.waiting_receivers.contains(&task_id);
            if !already_waiting {
                if endpoint_object.waiting_receivers.len() >= MAX_ENDPOINT_WAITERS {
                    return Err(IpcError::NoMemory);
                }
                endpoint_object.waiting_receivers.push_back(task_id);
            }
            Ok(false)
        })
        .ok_or(IpcError::InvalidHandle)?
}

pub fn complete_endpoint_reply(reply: KernelReplyHandle, response: &[u8]) -> Result<u64, IpcError> {
    complete_endpoint_reply_with_handles(reply, response, &[])
}

pub fn complete_endpoint_reply_with_handles(
    reply: KernelReplyHandle,
    response: &[u8],
    attached_handles: &[KernelTransferredHandle],
) -> Result<u64, IpcError> {
    complete_endpoint_reply_with_handles_faultable(
        reply,
        response,
        attached_handles,
        rustos_fault_injection::should_fail("ipc.endpoint.reply"),
    )
}

fn complete_endpoint_reply_with_handles_faultable(
    reply: KernelReplyHandle,
    response: &[u8],
    attached_handles: &[KernelTransferredHandle],
    injected_failure: bool,
) -> Result<u64, IpcError> {
    if response.len() > MAX_ENDPOINT_INLINE_MESSAGE_BYTES {
        return Err(IpcError::InvalidArgument);
    }
    validate_endpoint_transfer_handles(attached_handles)?;
    if injected_failure {
        return Err(IpcError::NoMemory);
    }

    complete_endpoint_reply_prepared(
        reply,
        None,
        prepare_endpoint_response(response, attached_handles)?,
    )
}

/// Completes a reply obtained through a task-owned internal endpoint.
pub fn complete_endpoint_reply_for_task(
    reply: KernelReplyHandle,
    receiver_task_id: u64,
    response: &[u8],
) -> Result<u64, IpcError> {
    complete_endpoint_reply_with_handles_for_owner(
        reply,
        EndpointOwner::Task(receiver_task_id),
        response,
        &[],
    )
}

pub fn complete_endpoint_reply_with_handles_for_task(
    reply: KernelReplyHandle,
    receiver_task_id: u64,
    response: &[u8],
    attached_handles: &[KernelTransferredHandle],
) -> Result<u64, IpcError> {
    complete_endpoint_reply_with_handles_for_owner(
        reply,
        EndpointOwner::Task(receiver_task_id),
        response,
        attached_handles,
    )
}

/// Completes a reply obtained through a process-owned user endpoint. The
/// capability is bound to the destination process, allowing its worker
/// threads while rejecting unrelated processes.
pub fn complete_endpoint_reply_for_process(
    reply: KernelReplyHandle,
    receiver_process_id: u64,
    response: &[u8],
) -> Result<u64, IpcError> {
    complete_endpoint_reply_with_handles_for_owner(
        reply,
        EndpointOwner::Process(receiver_process_id),
        response,
        &[],
    )
}

pub fn complete_endpoint_reply_with_handles_for_process(
    reply: KernelReplyHandle,
    receiver_process_id: u64,
    response: &[u8],
    attached_handles: &[KernelTransferredHandle],
) -> Result<u64, IpcError> {
    complete_endpoint_reply_with_handles_for_owner(
        reply,
        EndpointOwner::Process(receiver_process_id),
        response,
        attached_handles,
    )
}

fn complete_endpoint_reply_with_handles_for_owner(
    reply: KernelReplyHandle,
    receiver_owner: EndpointOwner,
    response: &[u8],
    attached_handles: &[KernelTransferredHandle],
) -> Result<u64, IpcError> {
    complete_endpoint_reply_with_handles_for_owner_faultable(
        reply,
        receiver_owner,
        response,
        attached_handles,
        rustos_fault_injection::should_fail("ipc.endpoint.reply"),
    )
}

fn complete_endpoint_reply_with_handles_for_owner_faultable(
    reply: KernelReplyHandle,
    receiver_owner: EndpointOwner,
    response: &[u8],
    attached_handles: &[KernelTransferredHandle],
    injected_failure: bool,
) -> Result<u64, IpcError> {
    if response.len() > MAX_ENDPOINT_INLINE_MESSAGE_BYTES {
        return Err(IpcError::InvalidArgument);
    }
    validate_endpoint_transfer_handles(attached_handles)?;
    if injected_failure {
        return Err(IpcError::NoMemory);
    }

    complete_endpoint_reply_prepared(
        reply,
        Some(receiver_owner),
        prepare_endpoint_response(response, attached_handles)?,
    )
}

fn prepare_endpoint_response(
    response: &[u8],
    attached_handles: &[KernelTransferredHandle],
) -> Result<EndpointResponse, IpcError> {
    let mut handles = Vec::new();
    handles
        .try_reserve_exact(MAX_ENDPOINT_TRANSFER_HANDLES.saturating_mul(2))
        .map_err(|_| IpcError::NoMemory)?;
    handles.extend_from_slice(attached_handles);
    Ok(EndpointResponse::Data {
        bytes: copy_endpoint_bytes(response)?,
        attached_handles: handles,
    })
}

fn copy_endpoint_bytes(bytes: &[u8]) -> Result<Vec<u8>, IpcError> {
    let mut copy = Vec::new();
    copy.try_reserve_exact(bytes.len())
        .map_err(|_| IpcError::NoMemory)?;
    copy.extend_from_slice(bytes);
    Ok(copy)
}

fn complete_endpoint_reply_prepared(
    reply: KernelReplyHandle,
    required_owner: Option<EndpointOwner>,
    response: EndpointResponse,
) -> Result<u64, IpcError> {
    let message_id = REPLIES
        .with(reply.raw(), |reply_object| {
            if required_owner
                .is_some_and(|owner| !endpoint_owner_allows(reply_object.receiver_owner, owner))
            {
                return Err(IpcError::PermissionDenied);
            }
            if reply_object.consumed {
                return Err(IpcError::InvalidArgument);
            }
            Ok(reply_object.message_id)
        })
        .ok_or(IpcError::InvalidHandle)??;

    // Keep ownership outside both slot locks until the exact reply identity
    // has been revalidated. If the transaction loses a race, the backing
    // vectors are then dropped after the guards have been released.
    let mut response = Some(response);
    let result = ENDPOINT_MESSAGES
        .with_mut(message_id, |message| {
            REPLIES
                .with_mut(reply.raw(), |reply_object| {
                    if reply_object.message_id != message_id || reply_object.consumed {
                        return Err(IpcError::InvalidHandle);
                    }
                    if required_owner.is_some_and(|owner| {
                        !endpoint_owner_allows(reply_object.receiver_owner, owner)
                    }) {
                        return Err(IpcError::PermissionDenied);
                    }
                    if reply_object.used {
                        return Err(IpcError::InvalidArgument);
                    }
                    if message.response.is_some() {
                        return Err(IpcError::InvalidHandle);
                    }
                    let caller = message.caller_task_id;
                    message.response = response.take();
                    reply_object.used = true;
                    Ok(caller)
                })
                .ok_or(IpcError::InvalidHandle)?
        })
        .ok_or(IpcError::InvalidHandle)?;
    drop(response);
    result
}

#[cfg(test)]
pub fn take_endpoint_response(reply: KernelReplyHandle) -> Result<Option<Vec<u8>>, IpcError> {
    let Some((response, _handles)) = take_endpoint_response_with_handle_limit(reply, 0)? else {
        return Ok(None);
    };
    Ok(Some(response))
}

#[cfg(test)]
pub fn take_endpoint_response_with_handle_limit(
    reply: KernelReplyHandle,
    handle_capacity: usize,
) -> Result<Option<EndpointResponseWithHandles>, IpcError> {
    match take_endpoint_response_detailed(reply, handle_capacity)? {
        EndpointResponseTake::Pending => Ok(None),
        EndpointResponseTake::Response(response) => Ok(Some(response)),
        // TEST-HARNESS: Test-only callers never create request descriptors.
        // Production uses the detailed result and settles discarded transfer
        // descriptors in the owning handle substrate.
        EndpointResponseTake::Error { error, .. } => Err(error),
    }
}

pub fn take_endpoint_response_detailed(
    reply: KernelReplyHandle,
    handle_capacity: usize,
) -> Result<EndpointResponseTake, IpcError> {
    let message_id = REPLIES
        .with(reply.raw(), |reply_object| {
            (!reply_object.consumed).then_some(reply_object.message_id)
        })
        .flatten()
        .ok_or(IpcError::InvalidHandle)?;

    let (result, consumed) = ENDPOINT_MESSAGES
        .with_mut(message_id, |message| {
            REPLIES
                .with_mut(reply.raw(), |reply_object| {
                    if reply_object.message_id != message_id || reply_object.consumed {
                        return Err(IpcError::InvalidHandle);
                    }
                    match message.response.as_ref() {
                        None => Ok((EndpointResponseTake::Pending, false)),
                        Some(EndpointResponse::Data {
                            attached_handles, ..
                        }) if attached_handles.len() > handle_capacity => {
                            Err(IpcError::BufferTooSmall)
                        }
                        Some(EndpointResponse::Data { .. }) => {
                            let Some(EndpointResponse::Data {
                                bytes,
                                attached_handles,
                            }) = message.response.take()
                            else {
                                unreachable!("response was checked above");
                            };
                            reply_object.consumed = true;
                            Ok((
                                EndpointResponseTake::Response((bytes, attached_handles)),
                                true,
                            ))
                        }
                        Some(EndpointResponse::Error(err)) => {
                            let err = *err;
                            message.response.take();
                            let discarded_request_handles =
                                core::mem::take(&mut message.attached_handles);
                            reply_object.consumed = true;
                            Ok((
                                EndpointResponseTake::Error {
                                    error: err,
                                    discarded_request_handles,
                                },
                                true,
                            ))
                        }
                    }
                })
                .ok_or(IpcError::InvalidHandle)?
        })
        .ok_or(IpcError::InvalidHandle)??;
    if consumed {
        drop(ENDPOINT_MESSAGES.remove(message_id));
        let _ = REPLIES.remove(reply.raw());
    }
    Ok(result)
}

pub fn cancel_endpoint_call(reply: KernelReplyHandle, caller_task_id: u64) -> Result<(), IpcError> {
    cancel_endpoint_call_with_transfers(reply, caller_task_id).map(|_| ())
}

pub fn cancel_endpoint_call_with_transfers(
    reply: KernelReplyHandle,
    caller_task_id: u64,
) -> Result<Vec<KernelTransferredHandle>, IpcError> {
    let message_id = REPLIES
        .with(reply.raw(), |reply| {
            (!reply.consumed).then_some(reply.message_id)
        })
        .flatten()
        .ok_or(IpcError::InvalidHandle)?;
    let endpoint_id = ENDPOINT_MESSAGES
        .with(message_id, |message| {
            (message.caller_task_id == caller_task_id).then_some(message.endpoint_id)
        })
        .flatten()
        .ok_or(IpcError::InvalidArgument)?;

    let mut marked = false;
    if ENDPOINTS
        .with_mut(endpoint_id, |endpoint| {
            endpoint
                .pending_messages
                .retain(|pending_message_id| *pending_message_id != message_id);
            endpoint
                .pending_system_messages
                .retain(|pending_message_id| *pending_message_id != message_id);
            marked = mark_endpoint_call_consumed(message_id, reply.raw(), caller_task_id);
        })
        .is_none()
    {
        marked = mark_endpoint_call_consumed(message_id, reply.raw(), caller_task_id);
    }
    if !marked {
        return Err(IpcError::InvalidHandle);
    }

    let message = ENDPOINT_MESSAGES
        .remove(message_id)
        .ok_or(IpcError::InvalidHandle)?;
    let _ = REPLIES.remove(reply.raw());
    Ok(transfers_from_message(message))
}

/// Cancels every endpoint call owned by a task being retired.  The caller may
/// no longer reach the response path, so both request and already-queued reply
/// descriptors are returned to the process-handle substrate for disposal.
pub fn cancel_endpoint_calls_for_task(
    task_id: u64,
    mut release_transfers: impl FnMut(&[KernelTransferredHandle]),
) -> usize {
    let mut cancelled = 0usize;
    for _ in 0..MAX_ENDPOINT_MESSAGE_OBJECTS {
        let Some(message_id) = ENDPOINT_MESSAGES
            .find_handle(|message| message.published && message.caller_task_id == task_id)
        else {
            return cancelled;
        };
        let Some(reply_id) = ENDPOINT_MESSAGES.with(message_id, |message| message.reply_id) else {
            continue;
        };
        let transfers =
            cancel_endpoint_call_with_transfers(KernelReplyHandle::from_raw(reply_id), task_id)
                .expect("published task-owned endpoint call lost exact cancellation state");
        release_transfers(&transfers);
        cancelled += 1;
    }
    if ENDPOINT_MESSAGES
        .find_handle(|message| message.published && message.caller_task_id == task_id)
        .is_some()
    {
        panic!(
            "task {} endpoint cancellation exceeded global message capacity {}",
            task_id, MAX_ENDPOINT_MESSAGE_OBJECTS
        );
    }
    cancelled
}

fn transfers_from_message(mut message: EndpointMessageObject) -> Vec<KernelTransferredHandle> {
    let mut transfers = core::mem::take(&mut message.attached_handles);
    if let Some(EndpointResponse::Data {
        attached_handles: mut response_handles,
        ..
    }) = message.response.take()
    {
        if transfers.capacity() < transfers.len().saturating_add(response_handles.len()) {
            core::mem::swap(&mut transfers, &mut response_handles);
        }
        transfers.extend(response_handles);
    }
    transfers
}

pub fn remove_endpoint_waiters_for_task(task_id: u64) -> usize {
    let mut removed = 0;
    ENDPOINTS.visit_mut(|_, endpoint| {
        let before = endpoint.waiting_receivers.len();
        endpoint
            .waiting_receivers
            .retain(|waiting_task_id| *waiting_task_id != task_id);
        removed += before.saturating_sub(endpoint.waiting_receivers.len());
    });
    removed
}

pub fn fail_endpoints_owned_by_task(task_id: u64, err: IpcError) -> EndpointWakeSet {
    fail_endpoints_owned_by(EndpointOwner::Task(task_id), err)
}

pub fn fail_endpoints_owned_by_process(process_id: u64, err: IpcError) -> EndpointWakeSet {
    fail_endpoints_owned_by(EndpointOwner::Process(process_id), err)
}

fn fail_endpoints_owned_by(owner: EndpointOwner, err: IpcError) -> EndpointWakeSet {
    let mut wake_set = EndpointWakeSet::default();
    while let Some((endpoint_id, endpoint)) =
        ENDPOINTS.take_first_matching(|endpoint| endpoint.owner == Some(owner))
    {
        ENDPOINT_QUOTAS.lock().release(owner);
        for receiver in endpoint.waiting_receivers {
            wake_set.push_receiver(receiver);
        }
        ENDPOINT_MESSAGES.visit_mut(|message_id, message| {
            if !message.published
                || message.endpoint_id != endpoint_id
                || message.response.is_some()
            {
                return;
            }
            let failed = REPLIES.with_mut(message.reply_id, |reply| {
                if reply.message_id != message_id || reply.consumed {
                    return false;
                }
                reply.used = true;
                message.response = Some(EndpointResponse::Error(err));
                true
            });
            if failed == Some(true) {
                wake_set.push_caller(message.caller_task_id);
            }
        });
    }
    wake_set
}

fn mark_endpoint_call_consumed(message_id: u64, reply_id: u64, caller_task_id: u64) -> bool {
    ENDPOINT_MESSAGES
        .with_mut(message_id, |message| {
            if message.caller_task_id != caller_task_id || message.reply_id != reply_id {
                return false;
            }
            REPLIES
                .with_mut(reply_id, |reply| {
                    if reply.message_id != message_id || reply.consumed {
                        return false;
                    }
                    reply.consumed = true;
                    true
                })
                .unwrap_or(false)
        })
        .unwrap_or(false)
}

#[cfg(test)]
pub fn shared_region_len(region: KernelSharedRegionHandle) -> Option<usize> {
    SHARED_REGIONS.with(region.raw(), |object| object.byte_len)
}

pub fn map_shared_region(region: KernelSharedRegionHandle) -> Option<(*mut u8, usize)> {
    SHARED_REGIONS.with(region.raw(), |object| {
        #[cfg(test)]
        {
            (object.bytes.as_ptr() as *mut u8, object.byte_len)
        }

        #[cfg(not(test))]
        {
            (
                kernel_vm::higher_half_addr(object.phys_start) as *mut u8,
                object.byte_len,
            )
        }
    })
}

pub fn shared_region_frames(region: KernelSharedRegionHandle) -> Option<Vec<u64>> {
    #[cfg(test)]
    {
        let _ = region;
        None
    }

    #[cfg(not(test))]
    {
        let (phys_start, page_count) = SHARED_REGIONS.with(region.raw(), |object| {
            (object.phys_start, object.page_count)
        })?;
        let mut frames = Vec::with_capacity(page_count);
        for page_index in 0..page_count {
            frames.push(phys_start + page_index as u64 * PAGE_SIZE as u64);
        }
        Some(frames)
    }
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
        if let Some(message) = channel_object.recv_queue.front()
            && (message.payload.len() > payload_capacity
                || message.attached_handles.len() > handle_capacity)
        {
            return Err(IpcError::BufferTooSmall);
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
    mod receiver_waiter_tests;

    use alloc::vec::Vec;
    use core::sync::atomic::Ordering;

    use super::{
        ConsoleStreamKind, IpcError, IpcHeader, KernelHandle, accept_channel,
        acquire_shared_region_mapping, connect_named_port, connect_port, create_channel_pair,
        create_event, create_named_port, create_shared_region, dequeue_message,
        dequeue_message_with_limits, enqueue_message, event_signal_count, lookup_named_port,
        map_shared_region, port_name, queue_channel_for_accept, recv_endpoint,
        recv_endpoint_with_limits, recv_endpoint_with_limits_and_handles, release_shared_region,
        service_deferred_shared_region_reclaims, shared_region_len, signal_event,
    };
    use kernel_object::api::handle::{FileHandleRights, HandleOwner, HandleRights, HandleToken};
    use spin::Mutex;

    static IPC_TEST_GUARD: Mutex<()> = Mutex::new(());

    fn with_isolated_ipc_test(f: impl FnOnce()) {
        let _guard = IPC_TEST_GUARD.lock();
        super::with_ipc_objects(|objects| *objects = super::IpcObjectTable::new());
        super::ENDPOINTS.clear();
        super::ENDPOINT_QUOTAS.lock().clear();
        super::ENDPOINT_MESSAGES.clear();
        super::REPLIES.clear();
        super::SHARED_REGIONS.clear();
        super::SHARED_REGION_RECLAIMS.lock().clear();
        super::SHARED_REGION_ADMITTED.store(0, Ordering::Release);
        super::SHARED_REGION_BYTES_ADMITTED.store(0, Ordering::Release);
        super::SHARED_REGION_QUOTAS.lock().clear();
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(f));
        super::with_ipc_objects(|objects| *objects = super::IpcObjectTable::new());
        super::ENDPOINTS.clear();
        super::ENDPOINT_QUOTAS.lock().clear();
        super::ENDPOINT_MESSAGES.clear();
        super::REPLIES.clear();
        super::SHARED_REGIONS.clear();
        super::SHARED_REGION_RECLAIMS.lock().clear();
        super::SHARED_REGION_ADMITTED.store(0, Ordering::Release);
        super::SHARED_REGION_BYTES_ADMITTED.store(0, Ordering::Release);
        super::SHARED_REGION_QUOTAS.lock().clear();
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

            let mapping = acquire_shared_region_mapping(region).expect("retain mapping");
            let cloned_mapping = mapping.clone();
            release_shared_region(region);
            assert_eq!(shared_region_len(region), Some(8192));
            drop(mapping);
            assert_eq!(shared_region_len(region), Some(8192));
            drop(cloned_mapping);
            assert_eq!(shared_region_len(region), None);
            assert_eq!(service_deferred_shared_region_reclaims(64), 1);
        });
    }

    #[test]
    fn process_shared_region_quota_is_bounded_until_reclaim_completes() {
        with_isolated_ipc_test(|| {
            let mut regions = [None; super::MAX_SHARED_REGIONS_PER_PROCESS];
            for slot in &mut regions {
                *slot = Some(
                    super::create_shared_region_for_process(51, 1)
                        .expect("within process shared-region quota"),
                );
            }
            assert_eq!(
                super::create_shared_region_for_process(51, 1),
                Err(IpcError::NoMemory)
            );
            for region in regions.into_iter().flatten() {
                release_shared_region(region);
            }
            assert_eq!(
                super::create_shared_region_for_process(51, 1),
                Err(IpcError::NoMemory),
                "queued backing must remain charged until physical reclaim"
            );
            for _ in 0..super::MAX_SHARED_REGIONS_PER_PROCESS {
                assert_eq!(service_deferred_shared_region_reclaims(1), 1);
            }
            assert!(
                super::create_shared_region_for_process(51, 1).is_ok(),
                "completed reclaim must return process quota"
            );
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
    fn endpoint_fault_boundaries_fail_before_queue_or_reply_mutation() {
        with_isolated_ipc_test(|| {
            let endpoint = super::create_endpoint().expect("create endpoint");
            assert_eq!(
                super::enqueue_endpoint_call_with_handles_faultable(
                    endpoint,
                    41,
                    b"request",
                    &[],
                    super::EndpointCallPriority::Ordinary,
                    true,
                ),
                Err(IpcError::NoMemory)
            );
            assert_eq!(recv_endpoint(endpoint), Ok(None));

            let (reply, _) =
                super::enqueue_endpoint_call(endpoint, 41, b"request").expect("enqueue call");
            let (server_reply, _) = recv_endpoint(endpoint)
                .expect("receive result")
                .expect("receive queued request");
            assert_eq!(server_reply, reply);
            assert_eq!(
                super::complete_endpoint_reply_with_handles_faultable(
                    reply,
                    b"response",
                    &[],
                    true,
                ),
                Err(IpcError::NoMemory)
            );
            assert_eq!(super::take_endpoint_response(reply), Ok(None));
            assert_eq!(super::complete_endpoint_reply(reply, b"response"), Ok(41));
        });
    }

    #[test]
    fn owned_endpoint_rejects_foreign_receiver_and_reply_task() {
        with_isolated_ipc_test(|| {
            let endpoint = super::create_endpoint_for_task(Some(10)).expect("create endpoint");
            let (reply, _) =
                super::enqueue_endpoint_call(endpoint, 22, b"request").expect("enqueue call");

            assert_eq!(
                super::authorize_endpoint_receiver(endpoint, 11),
                Err(super::IpcError::PermissionDenied)
            );
            assert_eq!(super::authorize_endpoint_receiver(endpoint, 10), Ok(()));
            assert_eq!(
                super::complete_endpoint_reply_for_task(reply, 11, b"forged"),
                Err(super::IpcError::PermissionDenied)
            );
            assert_eq!(
                super::complete_endpoint_reply_for_task(reply, 10, b"ok"),
                Ok(22)
            );
        });
    }

    #[test]
    fn process_owned_endpoint_allows_worker_and_rejects_foreign_process() {
        with_isolated_ipc_test(|| {
            let endpoint = super::create_endpoint_for_process(10).expect("create endpoint");
            let (reply, _) =
                super::enqueue_endpoint_call(endpoint, 22, b"request").expect("enqueue call");

            assert_eq!(
                super::authorize_endpoint_receiver_for_process(endpoint, 11),
                Err(super::IpcError::PermissionDenied)
            );
            // A different task in the owning service process may receive and
            // reply; this is how uiserver's display-policy worker operates.
            assert_eq!(
                super::authorize_endpoint_receiver_for_process(endpoint, 10),
                Ok(())
            );
            assert_eq!(
                super::complete_endpoint_reply_for_process(reply, 11, b"forged"),
                Err(super::IpcError::PermissionDenied)
            );
            assert_eq!(
                super::complete_endpoint_reply_for_process(reply, 10, b"ok"),
                Ok(22)
            );
        });
    }

    #[test]
    fn process_endpoint_quota_is_bounded_and_returned_on_exit() {
        with_isolated_ipc_test(|| {
            for _ in 0..super::MAX_ENDPOINTS_PER_PROCESS {
                super::create_endpoint_for_process(41).expect("within process endpoint quota");
            }
            assert_eq!(
                super::create_endpoint_for_process(41),
                Err(IpcError::NoMemory)
            );

            let _ = super::fail_endpoints_owned_by_process(41, IpcError::PeerClosed);
            for _ in 0..super::MAX_ENDPOINTS_PER_PROCESS {
                super::create_endpoint_for_process(41).expect("quota returned after process exit");
            }
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
            assert_eq!(wake_set.callers(), &[22]);
            assert!(wake_set.receivers().is_empty());
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
    fn process_owner_exit_fails_pending_callers() {
        with_isolated_ipc_test(|| {
            let endpoint = super::create_endpoint_for_process(10).expect("create endpoint");
            let (reply, _) =
                super::enqueue_endpoint_call(endpoint, 22, b"request").expect("enqueue call");

            let wake_set = super::fail_endpoints_owned_by_process(10, IpcError::PeerClosed);
            assert_eq!(wake_set.callers(), &[22]);
            assert_eq!(
                super::take_endpoint_response(reply),
                Err(IpcError::PeerClosed)
            );
        });
    }

    #[test]
    fn endpoint_peer_close_returns_unreceived_request_handles_for_cleanup() {
        with_isolated_ipc_test(|| {
            let endpoint = super::create_endpoint_for_task(Some(10)).expect("create endpoint");
            let handle = transferable_file_handle(77);
            let (reply, _) =
                super::enqueue_endpoint_call_with_handles(endpoint, 22, b"request", &[handle])
                    .expect("enqueue call");

            let wake_set = super::fail_endpoints_owned_by_task(10, IpcError::PeerClosed);
            assert_eq!(wake_set.callers(), &[22]);
            assert_eq!(
                super::take_endpoint_response_detailed(reply, 0),
                Ok(super::EndpointResponseTake::Error {
                    error: IpcError::PeerClosed,
                    discarded_request_handles: alloc::vec![handle],
                })
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
    fn retiring_caller_returns_all_outstanding_transfer_batches() {
        with_isolated_ipc_test(|| {
            let endpoint = super::create_endpoint().expect("create endpoint");
            let first = transferable_file_handle(81);
            let second = transferable_file_handle(82);
            let (first_reply, _) =
                super::enqueue_endpoint_call_with_handles(endpoint, 22, b"first", &[first])
                    .expect("enqueue first");
            let (second_reply, _) =
                super::enqueue_endpoint_call_with_handles(endpoint, 22, b"second", &[second])
                    .expect("enqueue second");

            let mut discarded = Vec::new();
            assert_eq!(
                super::cancel_endpoint_calls_for_task(22, |batch| {
                    discarded.extend_from_slice(batch);
                }),
                2
            );
            assert_eq!(discarded, alloc::vec![first, second]);
            assert_eq!(recv_endpoint(endpoint), Ok(None));
            assert_eq!(
                super::take_endpoint_response(first_reply),
                Err(IpcError::InvalidHandle)
            );
            assert_eq!(
                super::take_endpoint_response(second_reply),
                Err(IpcError::InvalidHandle)
            );
        });
    }

    #[test]
    fn retiring_caller_may_consume_the_exact_global_message_capacity() {
        with_isolated_ipc_test(|| {
            let first = super::create_endpoint().expect("create first endpoint");
            let second = super::create_endpoint().expect("create second endpoint");
            for endpoint in [first, second] {
                for sequence in 0..super::MAX_ENDPOINT_PENDING_MESSAGES {
                    super::enqueue_endpoint_call(endpoint, 22, &[(sequence + 1) as u8])
                        .expect("enqueue within endpoint and global capacity");
                }
            }

            assert_eq!(
                super::cancel_endpoint_calls_for_task(22, |batch| {
                    assert!(batch.is_empty());
                }),
                super::MAX_ENDPOINT_MESSAGE_OBJECTS
            );
            assert_eq!(recv_endpoint(first), Ok(None));
            assert_eq!(recv_endpoint(second), Ok(None));
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
            assert!(wake_set.callers().is_empty());
            assert_eq!(wake_set.receivers(), &[31, 32]);
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
            assert_eq!(wake_set.callers(), &[22]);
            assert!(wake_set.receivers().is_empty());
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
