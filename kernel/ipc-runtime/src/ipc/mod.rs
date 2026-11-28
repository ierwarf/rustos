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
use alloc::collections::VecDeque;
use alloc::vec::Vec;
#[cfg(not(test))]
use core::ptr;
use core::sync::atomic::{AtomicUsize, Ordering};

mod endpoint_priority;
mod endpoint_receive;
mod reply_publication;
mod shared_region_hold;
mod slab;
mod virtual_rendezvous;

pub use endpoint_priority::EndpointCallPriority;
use endpoint_priority::EndpointObject;
pub use endpoint_receive::{
    EndpointReceive, EndpointReceivePayload, published_request_retains_custody, receive_or_wait,
    receive_or_wait_inline, receive_or_wait_inline_payload,
    received_request_custody_conditions, received_request_retains_custody,
};
pub use shared_region_hold::KernelSharedRegionMappingHold;

use kernel_object::api::{
    handle::{HandleRights, HandleToken},
    identity::{ObjectIdentity, ObjectKind, ObjectOwner},
};
use nucleus_core::util::lockdep::{LockClass, TrackedSpinLock};
#[cfg(test)]
use spin::Mutex;

use reply_publication::{insert_fast_reply_object, insert_reply_object, published_reply_message_id};
use slab::GenerationalSlab;

#[cfg(not(test))]
use crate::memory::{kernel_vm, phys};

#[cfg(not(test))]
const PAGE_SIZE: usize = 4096;
const MAX_ENDPOINT_INLINE_MESSAGE_BYTES: usize = rustos_user_abi::syscall::IPC_MAX_INLINE_BYTES;
const MAX_ENDPOINT_PENDING_MESSAGES: usize = 64;
const MAX_ENDPOINT_TRANSFER_HANDLES: usize = 16;
const MAX_ENDPOINT_WAITERS: usize = 64;
const MAX_ENDPOINT_OBJECTS: usize = 512;
const MAX_ENDPOINT_MESSAGE_OBJECTS: usize = 512;
const MAX_REPLY_OBJECTS: usize = MAX_ENDPOINT_MESSAGE_OBJECTS;
const MAX_SHARED_REGION_OBJECTS: usize = 1024;
const MAX_OWNED_ENDPOINT_OBJECTS: usize = MAX_ENDPOINT_OBJECTS - 64;
const MAX_ENDPOINTS_PER_PROCESS: usize = 32;
const MAX_ENDPOINTS_PER_TASK: usize = 8;
const MAX_SHARED_REGION_BYTES: usize = 256 * 1024 * 1024;
const MAX_SHARED_REGION_TOTAL_BYTES: usize = 512 * 1024 * 1024;
const MAX_SHARED_REGIONS_PER_PROCESS: usize = 64;
const MAX_SHARED_REGION_BYTES_PER_PROCESS: usize = 128 * 1024 * 1024;

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct KernelSharedRegionHandle {
    raw: u64,
}

impl KernelSharedRegionHandle {
    pub const fn from_raw(raw: u64) -> Self {
        Self { raw }
    }

    pub const fn raw(&self) -> u64 {
        self.raw
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

    /// Reconstructs only a valid-shaped generational endpoint handle from an
    /// externally stored stable identity. Liveness is revalidated by every
    /// endpoint operation.
    pub fn from_identity(slot: u64, generation: u64) -> Option<Self> {
        let index = usize::try_from(slot.checked_sub(1)?).ok()?;
        if index >= MAX_ENDPOINT_OBJECTS || generation == 0 || generation > (u64::MAX >> 16) {
            return None;
        }
        let handle = Self {
            raw: (generation << 16) | slot,
        };
        let identity = handle.identity()?;
        (identity.slot() == slot && identity.generation() == generation).then_some(handle)
    }

    pub const fn raw(&self) -> u64 {
        self.raw
    }

    /// Stable endpoint identity decoded from the generational wire handle.
    ///
    /// `Some` means the raw value has a valid shape for the endpoint slab;
    /// it does not establish that the endpoint remains live. Every operation
    /// continues to validate this generation against `ENDPOINTS`.
    pub fn identity(&self) -> Option<ObjectIdentity> {
        let (slot, generation) = slab::identity_components::<MAX_ENDPOINT_OBJECTS>(self.raw)?;
        ObjectIdentity::new(ObjectOwner::Ipc, ObjectKind::Endpoint, slot, generation)
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

    /// Stable reply identity decoded from the generational wire handle.
    ///
    /// `Some` means the raw value has a valid shape for the reply slab; live
    /// ownership and consumption remain checked by the reply operation.
    pub fn identity(&self) -> Option<ObjectIdentity> {
        let (slot, generation) = slab::identity_components::<MAX_REPLY_OBJECTS>(self.raw)?;
        ObjectIdentity::new(ObjectOwner::Ipc, ObjectKind::Reply, slot, generation)
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
    identity: ObjectIdentity,
    nonce: u64,
}

impl KernelTransferTicket {
    pub const fn new(transfer_id: u64, nonce: u64, batch_generation: u64) -> Option<Self> {
        let Some(identity) = ObjectIdentity::new(
            ObjectOwner::Ipc,
            ObjectKind::Transfer,
            transfer_id,
            batch_generation,
        ) else {
            return None;
        };
        if nonce == 0 {
            return None;
        }
        Some(Self { identity, nonce })
    }

    pub const fn transfer_id(self) -> u64 {
        self.identity.slot()
    }

    pub const fn nonce(self) -> u64 {
        self.nonce
    }

    pub const fn batch_generation(self) -> u64 {
        self.identity.generation()
    }

    /// Internal object identity; this is never serialized as a userspace
    /// layout. The wire ticket remains the same opaque integer tuple.
    pub const fn identity(self) -> ObjectIdentity {
        self.identity
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
    /// Receiving open-description token, zero until the first claim binds it.
    ///
    /// A shared AF_UNIX open description has no single receiver process at send
    /// time, so `intended_receiver` alone leaves the batch claimable by any
    /// process holding that description. Recording the exact open description
    /// that first presented itself narrows the authority from "some member of
    /// the receiver set" to "this exact description", and a later dup, fork, or
    /// close cannot substitute a different one.
    pub receiver_open_description: u64,
}

pub type EndpointReceived = (KernelReplyHandle, Vec<u8>, Vec<KernelTransferredHandle>);
pub const IPC_QUEUED_INLINE_BYTES: usize = 16;

/// Compact payload carrier (`size_of == 24`, identical to `Vec<u8>`) avoiding
/// heap allocation for small (<= 16-byte) queued IPC messages while moving larger
/// `Vec<u8>` buffers by ownership without copying or enlarging slab slots.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum EndpointPayload {
    Inline {
        len: u8,
        bytes: [u8; IPC_QUEUED_INLINE_BYTES],
    },
    Heap(Vec<u8>),
}

const _: () = assert!(core::mem::size_of::<EndpointPayload>() <= 32);

impl Default for EndpointPayload {
    fn default() -> Self {
        Self::Inline {
            len: 0,
            bytes: [0; IPC_QUEUED_INLINE_BYTES],
        }
    }
}

impl EndpointPayload {
    #[inline]
    pub const fn empty() -> Self {
        Self::Inline {
            len: 0,
            bytes: [0; IPC_QUEUED_INLINE_BYTES],
        }
    }

    #[inline]
    pub fn len(&self) -> usize {
        match self {
            Self::Inline { len, .. } => usize::from(*len),
            Self::Heap(vec) => vec.len(),
        }
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    #[inline]
    pub fn as_slice(&self) -> &[u8] {
        match self {
            Self::Inline { len, bytes } => &bytes[..usize::from(*len)],
            Self::Heap(vec) => vec.as_slice(),
        }
    }

    #[inline]
    pub fn from_inline(bytes: &[u8]) -> Option<Self> {
        if bytes.len() > IPC_QUEUED_INLINE_BYTES {
            return None;
        }
        let mut buf = [0_u8; IPC_QUEUED_INLINE_BYTES];
        if !bytes.is_empty() {
            buf[..bytes.len()].copy_from_slice(bytes);
        }
        Some(Self::Inline {
            len: bytes.len() as u8,
            bytes: buf,
        })
    }

    #[inline]
    pub fn from_vec(bytes: Vec<u8>) -> Self {
        if bytes.is_empty() {
            Self::empty()
        } else {
            Self::Heap(bytes)
        }
    }

    pub fn try_from_slice(bytes: &[u8]) -> Result<Self, IpcError> {
        if let Some(inline) = Self::from_inline(bytes) {
            return Ok(inline);
        }
        copy_endpoint_bytes(bytes).map(Self::Heap)
    }

    pub fn into_vec(self) -> Vec<u8> {
        match self {
            Self::Inline { len, bytes } => {
                let len = usize::from(len);
                if len == 0 {
                    Vec::new()
                } else {
                    bytes[..len].to_vec()
                }
            }
            Self::Heap(vec) => vec,
        }
    }
}

impl core::ops::Deref for EndpointPayload {
    type Target = [u8];

    #[inline]
    fn deref(&self) -> &[u8] {
        self.as_slice()
    }
}

impl AsRef<[u8]> for EndpointPayload {
    #[inline]
    fn as_ref(&self) -> &[u8] {
        self.as_slice()
    }
}

pub type EndpointReceivedWithSender = (
    KernelReplyHandle,
    Vec<u8>,
    Vec<KernelTransferredHandle>,
    u64,
);
pub type EndpointReceivedPayloadWithSender = (
    KernelReplyHandle,
    EndpointPayload,
    Vec<KernelTransferredHandle>,
    u64,
);
pub type EndpointResponseWithHandles = (Vec<u8>, Vec<KernelTransferredHandle>);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct EnqueuedCall {
    pub reply: KernelReplyHandle,
    pub receiver_to_wake: Option<u64>,
    pub receiver_owner_process: Option<u64>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum EndpointPayloadTake {
    Pending,
    Response((EndpointPayload, Vec<KernelTransferredHandle>)),
    Error {
        error: IpcError,
        discarded_request_handles: Vec<KernelTransferredHandle>,
    },
}

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

    /// Derive an IPC descriptor with no authority beyond the source typed
    /// rights. The opaque transfer id and provider token are retained, so this
    /// is attenuation rather than a new object or a wire-visible capability.
    pub const fn attenuate(self, rights: HandleRights) -> Option<Self> {
        if !self.rights.can_attenuate_to(rights) {
            return None;
        }
        Some(Self { rights, ..self })
    }

    pub const fn is_transferable(self) -> bool {
        self.transfer_id != 0 && self.rights.allows_transfer()
    }
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

/// Scheduling authority carried by one synchronous call until its reply
/// reaches a terminal transition.  The IPC runtime owns only this neutral,
/// integer-only custody token; budget policy and charging remain in
/// `kernel-ps` and, ultimately, the user-mode scheduler policy service.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ReplySchedulingContextCustody {
    identity: ObjectIdentity,
    /// Task whose scheduling-context object supplies temporal authority.
    /// This differs from `caller_task_id` across a nested passive-server call.
    context_owner_task_id: u64,
    /// Immediate caller that must receive this custody on terminal reply.
    caller_task_id: u64,
}

impl ReplySchedulingContextCustody {
    pub fn new(identity: ObjectIdentity, caller_task_id: u64) -> Option<Self> {
        Self::new_with_owner(identity, caller_task_id, caller_task_id)
    }

    pub fn new_with_owner(
        identity: ObjectIdentity,
        context_owner_task_id: u64,
        caller_task_id: u64,
    ) -> Option<Self> {
        if identity.owner() != ObjectOwner::Ps
            || identity.kind() != ObjectKind::SchedulingContext
            || context_owner_task_id == 0
            || caller_task_id == 0
        {
            return None;
        }
        Some(Self {
            identity,
            context_owner_task_id,
            caller_task_id,
        })
    }

    pub const fn identity(self) -> ObjectIdentity {
        self.identity
    }

    pub const fn donor_task_id(self) -> u64 {
        self.caller_task_id
    }

    pub const fn context_owner_task_id(self) -> u64 {
        self.context_owner_task_id
    }

    pub const fn caller_task_id(self) -> u64 {
        self.caller_task_id
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ReplyCompletion {
    pub caller_task_id: u64,
    pub scheduling_context: Option<ReplySchedulingContextCustody>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ReplySchedulingContextReturn {
    pub reply: KernelReplyHandle,
    pub custody: ReplySchedulingContextCustody,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct EndpointWakeSet {
    callers: Vec<u64>,
    receivers: Vec<u64>,
    scheduling_contexts: Vec<ReplySchedulingContextReturn>,
}

impl EndpointWakeSet {
    fn reserve_for_endpoint_teardown(&mut self, reply_bound: usize) {
        let bound = reply_bound.min(MAX_REPLY_OBJECTS);
        if bound != 0 {
            // Callers and reply scheduling contexts are bounded by live reply objects.
            self.callers.reserve(bound);
            self.scheduling_contexts.reserve(bound);
        }
    }

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

    fn push_scheduling_context(
        &mut self,
        reply: KernelReplyHandle,
        custody: ReplySchedulingContextCustody,
    ) {
        self.scheduling_contexts
            .push(ReplySchedulingContextReturn { reply, custody });
    }

    pub fn callers(&self) -> &[u64] {
        &self.callers
    }

    pub fn receivers(&self) -> &[u64] {
        &self.receivers
    }

    pub fn scheduling_contexts(&self) -> impl Iterator<Item = ReplySchedulingContextReturn> + '_ {
        self.scheduling_contexts.iter().copied()
    }
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
        // ORDERING: every retain/release runs inside `SHARED_REGIONS.with`, i.e. under
        // the IpcRegion slab lock, and the last-reference teardown re-takes that lock in
        // `remove`; the lock already orders all holders' accesses before the reclaim.
        // Release is kept so the count stays correct if used outside the lock.
        let previous = self.references.fetch_sub(1, Ordering::Release);
        assert!(previous != 0, "shared-region reference underflow");
        previous == 1
    }
}

struct SharedRegionReclaimQueue {
    entries: [Option<SharedRegionObject>; MAX_SHARED_REGION_OBJECTS],
    head: usize,
    len: usize,
}

impl SharedRegionReclaimQueue {
    const fn new() -> Self {
        Self {
            entries: [const { None }; MAX_SHARED_REGION_OBJECTS],
            head: 0,
            len: 0,
        }
    }

    fn push(&mut self, object: SharedRegionObject) {
        assert!(
            self.len < MAX_SHARED_REGION_OBJECTS,
            "shared-region reclaim admission invariant violated"
        );
        let tail = (self.head + self.len) % MAX_SHARED_REGION_OBJECTS;
        self.entries[tail] = Some(object);
        self.len += 1;
    }

    /// O(1) under the reclaim lock: the previous shift-down moved up to
    /// `MAX_SHARED_REGION_OBJECTS` entries on every serviced reclaim step.
    fn pop_front(&mut self) -> Option<SharedRegionObject> {
        if self.len == 0 {
            return None;
        }
        let object = self.entries[self.head].take();
        self.head = (self.head + 1) % MAX_SHARED_REGION_OBJECTS;
        self.len -= 1;
        object
    }

    #[cfg(test)]
    fn clear(&mut self) {
        for slot in &mut self.entries {
            *slot = None;
        }
        self.head = 0;
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

const MAX_PROCESS_RESOURCE_OWNERS: usize = 128;

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
    request: EndpointPayload,
    attached_handles: Vec<KernelTransferredHandle>,
    // A broker may reserve a descriptor after the request was delivered but
    // before its service completes the exact reply.  It remains invisible
    // here until the normal reply transition moves it into response handles.
    prepared_reply_handles: Option<Vec<KernelTransferredHandle>>,
    response: Option<EndpointResponse>,
    /// Sender Job group and bytes billed as `Class::IpcMessage` while this
    /// message object exists; refunded by `Drop`, which covers every removal
    /// path (completion, cancellation, endpoint failure, publication refusal).
    kmem: Option<(usize, u64)>,
}

impl Drop for EndpointMessageObject {
    fn drop(&mut self) {
        if let Some((group, bytes)) = self.kmem.take() {
            kernel_mm::api::kmem_charge::uncharge(
                kernel_mm::api::kmem_charge::Class::IpcMessage,
                group,
                bytes,
            );
        }
    }
}

/// Refunds a not-yet-moved message charge on an early return.
struct MessageKmemGuard(Option<(usize, u64)>);

impl Drop for MessageKmemGuard {
    fn drop(&mut self) {
        if let Some((group, bytes)) = self.0.take() {
            kernel_mm::api::kmem_charge::uncharge(
                kernel_mm::api::kmem_charge::Class::IpcMessage,
                group,
                bytes,
            );
        }
    }
}

/// Bytes one queued call pins: the object, its request copy and the handle
/// vector reserved for the request and its response transfers.
fn message_kmem_bytes(request_len: usize) -> u64 {
    (core::mem::size_of::<EndpointMessageObject>()
        + request_len
        + MAX_ENDPOINT_TRANSFER_HANDLES.saturating_mul(2)
            * core::mem::size_of::<KernelTransferredHandle>()) as u64
}

#[cfg(test)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum EndpointEnqueueBindingFault {
    EndpointId,
    ReplyId,
}

struct ReplyObject {
    message_id: u64,
    receiver_owner: Option<EndpointOwner>,
    used: bool,
    consumed: bool,
    scheduling_context: Option<ReplySchedulingContextCustody>,
    fast_frame: Option<FastCallFrame>,
}

pub const IPC_FAST_INLINE_BYTES: usize = 256;

/// Largest request or response a rendezvous frame carries. Bytes beyond the inline
/// image ride a spill buffer the frame owns (allocated for that call only), so a
/// large message takes the same reserve / hand off / reply transaction as a small
/// one instead of the queued message and reply objects.
pub const IPC_FAST_MAX_BYTES: usize = rustos_user_abi::syscall::IPC_MAX_INLINE_BYTES;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum FastCallState {
    RequestReady,
    RequestTaken,
    ResponseReady,
    ErrorReady,
}

#[derive(Clone)]
struct FastCallFrame {
    endpoint_id: u64,
    caller_process_id: u64,
    caller_task_id: u64,
    receiver_task_id: u64,
    receiver_request_capacity: usize,
    caller_response_capacity: usize,
    request_len: usize,
    response_len: usize,
    state: FastCallState,
    terminal_error: Option<IpcError>,
    request: [core::mem::MaybeUninit<u8>; IPC_FAST_INLINE_BYTES],
    response: [core::mem::MaybeUninit<u8>; IPC_FAST_INLINE_BYTES],
    /// Metadata for bytes outside the 256-byte images. The two Vec headers
    /// live in the fixed reply frame even when the spill is absent: this costs
    /// some slot memory but avoids a separate infallible Box allocation on the
    /// OOM-sensitive kernel IPC path. Spill byte allocations remain fallible.
    spill: Option<FastSpill>,
}

#[derive(Clone, Default)]
struct FastSpill {
    request: Vec<u8>,
    response: Vec<u8>,
}

#[inline]
pub(super) fn write_uninit_bytes(
    dst: &mut [core::mem::MaybeUninit<u8>],
    src: &[u8],
) {
    assert!(src.len() <= dst.len());
    // SAFETY: `src` and `dst` are valid for `src.len()` non-overlapping bytes.
    unsafe {
        core::ptr::copy_nonoverlapping(src.as_ptr(), dst.as_mut_ptr().cast::<u8>(), src.len());
    }
}

#[inline]
pub(super) fn copy_uninit_prefix(
    src: &[core::mem::MaybeUninit<u8>],
    len: usize,
    dst: &mut [u8],
) {
    assert!(len <= src.len() && len <= dst.len());
    // SAFETY: callers maintain that `src[..len]` was initialized by `write_uninit_bytes`.
    unsafe {
        core::ptr::copy_nonoverlapping(src.as_ptr().cast::<u8>(), dst.as_mut_ptr(), len);
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FastEndpointReceived<const N: usize = IPC_FAST_INLINE_BYTES> {
    pub reply: KernelReplyHandle,
    pub caller_process_id: u64,
    pub caller_task_id: u64,
    pub request_len: usize,
    pub request: [u8; N],
    /// The whole request when it exceeds `IPC_FAST_INLINE_BYTES`; `request` is then
    /// unused.
    pub spill: Option<Vec<u8>>,
}

impl<const N: usize> FastEndpointReceived<N> {
    /// The request bytes wherever they live.
    pub fn bytes(&self) -> &[u8] {
        match &self.spill {
            Some(spill) => spill,
            None => &self.request[..self.request_len],
        }
    }
}

/// Exact custody returned when a published fast reservation is rolled back
/// before its receiver is allowed to run.  The caller must either restore the
/// scheduling context to its admission reservation or settle it terminally;
/// dropping it would lose temporal-accounting authority.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct FastEndpointRollback {
    pub receiver_task_id: u64,
    pub scheduling_context: Option<ReplySchedulingContextCustody>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct FastReplyCompletion {
    pub completion: ReplyCompletion,
    pub terminal_error: Option<IpcError>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum FastEndpointResponseTake<const N: usize = IPC_FAST_INLINE_BYTES> {
    Pending,
    Response {
        response_len: usize,
        response: [u8; N],
    },
    /// A response larger than the inline image.
    Spilled(Vec<u8>),
    Error(IpcError),
}

enum EndpointResponse {
    Data {
        bytes: EndpointPayload,
        attached_handles: Vec<KernelTransferredHandle>,
        prepared_handles: Option<Vec<KernelTransferredHandle>>,
    },
    Error(IpcError),
}

/// A rejected prepared-reply bind returns ownership of its descriptors to the
/// broker.  The broker can then reclaim an unpublished provider object
/// without routing a synthetic close through an unrelated process.
#[derive(Debug, Eq, PartialEq)]
pub struct PreparedReplyHandleBindError {
    pub error: IpcError,
    pub handles: Vec<KernelTransferredHandle>,
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
static ENDPOINT_ENQUEUE_BINDING_FAULT: Mutex<Option<EndpointEnqueueBindingFault>> =
    Mutex::new(None);

#[cfg(test)]
static ENDPOINT_CANCEL_REPLY_BINDING_FAULT: Mutex<bool> = Mutex::new(false);

#[cfg(test)]
static ENDPOINT_RECV_STALE_HEAD_FAULT: Mutex<Option<u64>> = Mutex::new(None);

#[cfg(test)]
fn set_endpoint_enqueue_binding_fault(fault: Option<EndpointEnqueueBindingFault>) {
    *ENDPOINT_ENQUEUE_BINDING_FAULT.lock() = fault;
}

#[cfg(test)]
fn set_endpoint_cancel_reply_binding_fault(enabled: bool) {
    *ENDPOINT_CANCEL_REPLY_BINDING_FAULT.lock() = enabled;
}

#[cfg(test)]
fn set_endpoint_recv_stale_head_fault(enabled: bool) {
    *ENDPOINT_RECV_STALE_HEAD_FAULT.lock() = enabled.then_some(0);
}

#[cfg(test)]
fn inject_endpoint_recv_stale_head_fault(message_id: u64) {
    let invalidate = {
        let mut fault = ENDPOINT_RECV_STALE_HEAD_FAULT.lock();
        match *fault {
            Some(0) => {
                *fault = Some(message_id);
                true
            }
            Some(stale_message_id) if stale_message_id == message_id => {
                panic!("stale endpoint queue head was not consumed")
            }
            _ => false,
        }
    };
    if invalidate {
        assert!(
            ENDPOINT_MESSAGES.remove(message_id).is_some(),
            "faulted queue head must name a live endpoint message"
        );
    }
}

fn reserve_shared_region_admission(
    owner_process_id: Option<u64>,
    byte_len: usize,
) -> Result<(), IpcError> {
    if SHARED_REGION_ADMITTED
        // ORDERING: AcqRel CAS-loop reserves one object slot against the hard cap. The
        // counter only bounds admission; the object itself is published later by the
        // slab insert under its lock, so no other data rides on this ordering.
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
        // ORDERING: AcqRel CAS-loop reserves the byte budget against the global cap;
        // admission accounting only, paired with the AcqRel decrement in release.
        .fetch_update(Ordering::AcqRel, Ordering::Acquire, |admitted| {
            admitted
                .checked_add(byte_len)
                .filter(|bytes| *bytes <= MAX_SHARED_REGION_TOTAL_BYTES)
        })
        .is_err()
    {
        // ORDERING: AcqRel rollback of the object-slot reservation taken above when the
        // byte budget is refused; undoes exactly this caller's increment.
        SHARED_REGION_ADMITTED.fetch_sub(1, Ordering::AcqRel);
        return Err(IpcError::NoMemory);
    }
    if let Some(process_id) = owner_process_id
        && let Err(error) = SHARED_REGION_QUOTAS.lock().reserve(process_id, byte_len)
    {
        // ORDERING: AcqRel rollback of this caller's byte reservation when the
        // per-process quota refuses; counters are admission-only accounting.
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
    // ORDERING: AcqRel decrement returns this region's byte budget; the previous
    // value is only checked for underflow, and no data depends on it.
    let previous_bytes = SHARED_REGION_BYTES_ADMITTED.fetch_sub(byte_len, Ordering::AcqRel);
    assert!(
        previous_bytes >= byte_len,
        "shared-region admission byte count underflow"
    );
    // ORDERING: AcqRel decrement returns the object slot; checked for underflow
    // only, and the freed capacity carries no other published state.
    let previous = SHARED_REGION_ADMITTED.fetch_sub(1, Ordering::AcqRel);
    assert!(previous != 0, "shared-region admission count underflow");
}

#[cfg(test)]
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
        // SAFETY: `phys_start..+alloc_len` was just returned by `alloc_contiguous` for
        // `page_count` frames, so it is exclusively owned and no handle exists yet. The
        // higher-half direct map covers all RAM; `u8` writes need no alignment.
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
    if let Some(((), Some(removed))) =
        SHARED_REGIONS.with_mut_remove_if(region.raw(), |object| ((), object.release()))
    {
        enqueue_shared_region_reclaim(removed);
    }
}

pub fn acquire_shared_region_mapping(
    region: KernelSharedRegionHandle,
) -> Option<KernelSharedRegionMappingHold> {
    retain_shared_region(region).then(|| KernelSharedRegionMappingHold::new(region))
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

#[cfg(test)]
pub fn create_endpoint() -> Result<KernelEndpointHandle, IpcError> {
    create_endpoint_with_owner(None, None)
}

pub fn create_endpoint_for_task(
    owner_task_id: Option<u64>,
) -> Result<KernelEndpointHandle, IpcError> {
    create_endpoint_with_owner(owner_task_id.map(EndpointOwner::Task), None)
}

/// User-visible endpoints are process-owned: a service may safely receive and
/// reply on a worker thread, while another process cannot consume its queue or
/// forge a reply with a guessed raw capability.
pub fn create_endpoint_for_process(
    owner_process_id: u64,
) -> Result<KernelEndpointHandle, IpcError> {
    create_endpoint_with_owner(Some(EndpointOwner::Process(owner_process_id)), None)
}

#[cfg(test)]
/// As `create_endpoint_for_process`, additionally billing the endpoint's
/// reserved queue storage to the owner's Job charge group (lock-free; refunded
/// when the owner's endpoints are failed at exit). The caller resolves the
/// group in its own context so this crate takes no process-table lock.
pub fn create_endpoint_for_process_in_group(
    owner_process_id: u64,
    kmem_group: usize,
) -> Result<KernelEndpointHandle, IpcError> {
    create_endpoint_with_owner(Some(EndpointOwner::Process(owner_process_id)), Some(kmem_group))
}

/// The receiver declares its immutable transport capacity before any caller
/// can publish a request. Byte-only services choose zero; heterogeneous
/// endpoints may retain the legacy full transfer capacity.
pub fn create_endpoint_for_process_with_limits(
    owner_process_id: u64,
    max_request_bytes: usize,
    max_request_handles: usize,
    kmem_group: Option<usize>,
) -> Result<KernelEndpointHandle, IpcError> {
    create_endpoint_with_owner_and_limits(Some(EndpointOwner::Process(owner_process_id)), kmem_group, max_request_bytes, max_request_handles)
}

fn create_endpoint_with_owner(
    owner: Option<EndpointOwner>,
    kmem_group: Option<usize>,
) -> Result<KernelEndpointHandle, IpcError> {
    create_endpoint_with_owner_and_limits(owner, kmem_group, MAX_ENDPOINT_INLINE_MESSAGE_BYTES, MAX_ENDPOINT_TRANSFER_HANDLES)
}

fn create_endpoint_with_owner_and_limits(
    owner: Option<EndpointOwner>,
    kmem_group: Option<usize>,
    max_request_bytes: usize,
    max_request_handles: usize,
) -> Result<KernelEndpointHandle, IpcError> {
    if max_request_handles > MAX_ENDPOINT_TRANSFER_HANDLES || max_request_bytes == 0 || max_request_bytes > MAX_ENDPOINT_INLINE_MESSAGE_BYTES {
        return Err(IpcError::InvalidArgument);
    }
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
    let mut endpoint = match endpoint {
        Ok(endpoint) => endpoint,
        Err(error) => {
            if let Some(owner) = owner {
                ENDPOINT_QUOTAS.lock().release(owner);
            }
            return Err(error);
        }
    };
    endpoint.max_request_handles = max_request_handles;
    endpoint.max_request_bytes = max_request_bytes;
    if let Some(group) = kmem_group {
        let bytes = endpoint.reserved_queue_bytes();
        if !kernel_mm::api::kmem_charge::try_charge(
            kernel_mm::api::kmem_charge::Class::IpcEndpoint,
            group,
            bytes,
        ) {
            if let Some(owner) = owner {
                ENDPOINT_QUOTAS.lock().release(owner);
            }
            return Err(IpcError::NoMemory);
        }
        endpoint.kmem = Some((group, bytes));
    }
    match ENDPOINTS.insert(endpoint) {
        Ok(handle) => Ok(KernelEndpointHandle::from_raw(handle)),
        Err(endpoint) => {
            if let Some(owner) = endpoint.owner {
                ENDPOINT_QUOTAS.lock().release(owner);
            }
            endpoint.refund_kmem();
            Err(IpcError::NoMemory)
        }
    }
}

pub fn validate_endpoint_request_shape(endpoint: KernelEndpointHandle, request_len: usize, count: usize) -> Result<(), IpcError> {
    ENDPOINTS.with(endpoint.raw(), |object| {
        if request_len == 0 || request_len > object.max_request_bytes { return Err(IpcError::InvalidArgument); }
        if count > object.max_request_handles { Err(IpcError::InvalidArgument) } else { Ok(()) }
    }).ok_or(IpcError::InvalidHandle)?
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
    enqueue_endpoint_call_with_handles_priority_and_optional_custody(
        endpoint,
        caller_task_id,
        request,
        attached_handles,
        priority,
        None,
        None,
    )
}

pub fn enqueue_endpoint_call_with_handles_priority_and_custody(
    endpoint: KernelEndpointHandle,
    caller_task_id: u64,
    request: &[u8],
    attached_handles: &[KernelTransferredHandle],
    priority: EndpointCallPriority,
    scheduling_context: ReplySchedulingContextCustody,
) -> Result<(KernelReplyHandle, Option<u64>), IpcError> {
    enqueue_endpoint_call_with_handles_priority_custody_in_group(
        endpoint,
        caller_task_id,
        request,
        attached_handles,
        priority,
        scheduling_context,
        None,
    )
}

/// As the unbilled call, additionally billing the queued message to the
/// sender's Job charge group (`Class::IpcMessage`). A refused charge fails the
/// call with `NoMemory` before any message storage is allocated. The caller
/// resolves the group in its own context, as for endpoint creation.
pub fn enqueue_endpoint_call_with_handles_priority_custody_in_group(
    endpoint: KernelEndpointHandle,
    caller_task_id: u64,
    request: &[u8],
    attached_handles: &[KernelTransferredHandle],
    priority: EndpointCallPriority,
    scheduling_context: ReplySchedulingContextCustody,
    kmem_group: Option<usize>,
) -> Result<(KernelReplyHandle, Option<u64>), IpcError> {
    if scheduling_context.donor_task_id() != caller_task_id {
        return Err(IpcError::InvalidArgument);
    }
    enqueue_endpoint_call_with_handles_priority_and_optional_custody(
        endpoint,
        caller_task_id,
        request,
        attached_handles,
        priority,
        Some(scheduling_context),
        kmem_group,
    )
}

fn enqueue_endpoint_call_with_handles_priority_and_optional_custody(
    endpoint: KernelEndpointHandle,
    caller_task_id: u64,
    request: &[u8],
    attached_handles: &[KernelTransferredHandle],
    priority: EndpointCallPriority,
    scheduling_context: Option<ReplySchedulingContextCustody>,
    kmem_group: Option<usize>,
) -> Result<(KernelReplyHandle, Option<u64>), IpcError> {
    enqueue_endpoint_call_with_handles_faultable(
        endpoint,
        caller_task_id,
        request,
        attached_handles,
        priority,
        scheduling_context,
        kmem_group,
        rustos_fault_injection::should_fail("ipc.endpoint.enqueue"),
    )
}

fn endpoint_queue_is_full(endpoint: &EndpointObject) -> bool {
    endpoint.pending_len() >= MAX_ENDPOINT_PENDING_MESSAGES
}

pub fn enqueue_endpoint_call_with_handles_priority_custody_owned(
    endpoint: KernelEndpointHandle,
    caller_task_id: u64,
    request: EndpointPayload,
    attached_handles: &[KernelTransferredHandle],
    priority: EndpointCallPriority,
    scheduling_context: ReplySchedulingContextCustody,
    kmem_group: Option<usize>,
) -> Result<EnqueuedCall, (IpcError, bool, EndpointPayload)> {
    if scheduling_context.donor_task_id() != caller_task_id {
        return Err((IpcError::InvalidArgument, false, request));
    }
    enqueue_endpoint_call_with_handles_owned_faultable(
        endpoint,
        caller_task_id,
        request,
        attached_handles,
        priority,
        Some(scheduling_context),
        kmem_group,
        rustos_fault_injection::should_fail("ipc.endpoint.enqueue"),
    )
}

fn enqueue_endpoint_call_with_handles_faultable(
    endpoint: KernelEndpointHandle,
    caller_task_id: u64,
    request: &[u8],
    attached_handles: &[KernelTransferredHandle],
    priority: EndpointCallPriority,
    scheduling_context: Option<ReplySchedulingContextCustody>,
    kmem_group: Option<usize>,
    injected_failure: bool,
) -> Result<(KernelReplyHandle, Option<u64>), IpcError> {
    let payload = EndpointPayload::try_from_slice(request)?;
    enqueue_endpoint_call_with_handles_owned_faultable(
        endpoint,
        caller_task_id,
        payload,
        attached_handles,
        priority,
        scheduling_context,
        kmem_group,
        injected_failure,
    )
    .map(|enqueued| (enqueued.reply, enqueued.receiver_to_wake))
    .map_err(|(err, _, _)| err)
}

fn enqueue_endpoint_call_with_handles_owned_faultable(
    endpoint: KernelEndpointHandle,
    caller_task_id: u64,
    request: EndpointPayload,
    attached_handles: &[KernelTransferredHandle],
    priority: EndpointCallPriority,
    scheduling_context: Option<ReplySchedulingContextCustody>,
    kmem_group: Option<usize>,
    injected_failure: bool,
) -> Result<EnqueuedCall, (IpcError, bool, EndpointPayload)> {
    if request.is_empty() || request.len() > MAX_ENDPOINT_INLINE_MESSAGE_BYTES {
        return Err((IpcError::InvalidArgument, false, request));
    }
    if let Err(err) = validate_endpoint_transfer_handles(attached_handles) {
        return Err((err, false, request));
    }
    if injected_failure {
        return Err((IpcError::NoMemory, false, request));
    }
    // Bill the sender's Job before any message storage is allocated. The
    // charge moves into the message object, whose Drop refunds it on every
    // removal path; until then `charge` refunds it on an early return.
    let message_kmem = match kmem_group {
        Some(group) => {
            let bytes = message_kmem_bytes(request.len());
            if !kernel_mm::api::kmem_charge::try_charge(
                kernel_mm::api::kmem_charge::Class::IpcMessage,
                group,
                bytes,
            ) {
                return Err((IpcError::NoMemory, false, request));
            }
            Some((group, bytes))
        }
        None => None,
    };
    let mut kmem_guard = MessageKmemGuard(message_kmem);

    let receiver_owner = match ENDPOINTS.with_mut(endpoint.raw(), |endpoint| {
        if request.len() > endpoint.max_request_bytes {
            return Err((IpcError::InvalidArgument, false));
        }
        if attached_handles.len() > endpoint.max_request_handles {
            return Err((IpcError::InvalidArgument, false));
        }
        if endpoint_queue_is_full(endpoint) {
            endpoint.queue_full_hits = endpoint.queue_full_hits.wrapping_add(1);
            return Err((IpcError::NoMemory, true));
        }
        Ok(endpoint.owner)
    }) {
        Some(Ok(owner)) => owner,
        Some(Err((err, queue_full))) => return Err((err, queue_full, request)),
        None => return Err((IpcError::InvalidHandle, false, request)),
    };
    let receiver_owner_process = match receiver_owner {
        Some(EndpointOwner::Process(pid)) => Some(pid),
        _ => None,
    };

    let mut request_handles = Vec::new();
    if !attached_handles.is_empty() {
        // Retirement may collect this request and a maximum-sized reply.
        if request_handles
            .try_reserve_exact(attached_handles.len() + MAX_ENDPOINT_TRANSFER_HANDLES)
            .is_err()
        {
            return Err((IpcError::NoMemory, false, request));
        }
        request_handles.extend_from_slice(attached_handles);
    }
    let message_id = match ENDPOINT_MESSAGES.insert(EndpointMessageObject {
        endpoint_id: endpoint.raw(),
        reply_id: 0,
        caller_task_id,
        published: false,
        request,
        attached_handles: request_handles,
        prepared_reply_handles: None,
        response: None,
        kmem: kmem_guard.0.take(),
    }) {
        Ok(message_id) => message_id,
        Err(mut rejected_msg) => {
            return Err((IpcError::NoMemory, false, core::mem::take(&mut rejected_msg.request)));
        }
    };
    let reply_id = match insert_reply_object(ReplyObject {
        message_id,
        receiver_owner,
        used: false,
        consumed: false,
        scheduling_context,
        fast_frame: None,
    }) {
        Ok(reply_id) => reply_id,
        Err(_) => {
            let removed_req = ENDPOINT_MESSAGES
                .remove(message_id)
                .map(|mut msg| core::mem::take(&mut msg.request))
                .unwrap_or_default();
            return Err((IpcError::NoMemory, false, removed_req));
        }
    };
    #[cfg(test)]
    let binding_fault = *ENDPOINT_ENQUEUE_BINDING_FAULT.lock();

    let receiver_to_wake = ENDPOINTS.with_mut(endpoint.raw(), |endpoint_object| {
        if endpoint_queue_is_full(endpoint_object) {
            endpoint_object.queue_full_hits = endpoint_object.queue_full_hits.wrapping_add(1);
            return Err(true);
        }
        let published = ENDPOINT_MESSAGES.with_mut(message_id, |message| {
            message.reply_id = reply_id;
            #[cfg(test)]
            match binding_fault {
                Some(EndpointEnqueueBindingFault::EndpointId) => {
                    message.endpoint_id = message.endpoint_id.wrapping_add(1);
                }
                Some(EndpointEnqueueBindingFault::ReplyId) => {
                    message.reply_id = message.reply_id.wrapping_add(1);
                }
                None => {}
            }
            if message.endpoint_id != endpoint.raw() || message.reply_id != reply_id {
                return false;
            }
            message.published = true;
            true
        });
        if published != Some(true) {
            return Err(false);
        }
        endpoint_object.push_pending(priority, message_id);
        let depth = endpoint_object.pending_len() as u64;
        nucleus_core::debug::ipc_tail::record_task_tail_event(
            nucleus_core::debug::ipc_tail::IpcTailEventKind::EnqueueReq,
            priority as u8,
            caller_task_id as u32,
            endpoint.raw(),
            depth,
        );
        Ok(
            endpoint_object
                .waiting_receivers
                .pop_front()
                .map(|waiter| waiter.task_id)
                .or(match endpoint_object.owner {
                    Some(EndpointOwner::Task(task_id)) => Some(task_id),
                    Some(EndpointOwner::Process(_)) | None => None,
                })
                .filter(|task_id| *task_id != caller_task_id),
        )
    });
    let receiver_to_wake = match receiver_to_wake {
        Some(Ok(wake)) => wake,
        Some(Err(queue_full)) => {
            reply_publication::clear_reply_publication(reply_id);
            let _ = REPLIES.remove(reply_id);
            let removed_req = ENDPOINT_MESSAGES
                .remove(message_id)
                .map(|mut msg| core::mem::take(&mut msg.request))
                .unwrap_or_default();
            return Err((IpcError::NoMemory, queue_full, removed_req));
        }
        None => {
            reply_publication::clear_reply_publication(reply_id);
            let _ = REPLIES.remove(reply_id);
            let removed_req = ENDPOINT_MESSAGES
                .remove(message_id)
                .map(|mut msg| core::mem::take(&mut msg.request))
                .unwrap_or_default();
            return Err((IpcError::InvalidHandle, false, removed_req));
        }
    };

    Ok(EnqueuedCall {
        reply: KernelReplyHandle::from_raw(reply_id),
        receiver_to_wake,
        receiver_owner_process,
    })
}

/// Reports whether a live endpoint's queued request backlog has reached
/// `MAX_ENDPOINT_PENDING_MESSAGES`, distinguishing transient queue saturation
/// from quota/allocator exhaustion on synchronous IPC wait paths.
pub fn endpoint_pending_queue_full(endpoint: KernelEndpointHandle) -> bool {
    ENDPOINTS.with(endpoint.raw(), |endpoint_object| {
        endpoint_queue_is_full(endpoint_object)
    }) == Some(true)
}

/// Returns the monotonic count of queue-full rejections observed on a live
/// endpoint, closing the TOCTOU window between `NoMemory` return and inspection.
pub fn endpoint_queue_full_hits(endpoint: KernelEndpointHandle) -> Option<u64> {
    ENDPOINTS.with(endpoint.raw(), |endpoint_object| endpoint_object.queue_full_hits)
}

#[cfg(test)]
/// Reserves the allocation-free byte-only rendezvous transport. Publication
/// succeeds only for the exact front waiter on an otherwise idle endpoint;
/// every failure removes the unpublished reply slot before returning.
pub fn reserve_fast_endpoint_call(
    endpoint: KernelEndpointHandle,
    caller_process_id: u64,
    caller_task_id: u64,
    request: &[u8],
    scheduling_context: Option<ReplySchedulingContextCustody>,
) -> Result<(KernelReplyHandle, u64), IpcError> {
    reserve_fast_endpoint_call_with_response_capacity(
        endpoint,
        caller_process_id,
        caller_task_id,
        request,
        IPC_FAST_INLINE_BYTES,
        scheduling_context,
    )
}

pub fn reserve_fast_endpoint_call_with_response_capacity(
    endpoint: KernelEndpointHandle,
    caller_process_id: u64,
    caller_task_id: u64,
    request: &[u8],
    response_capacity: usize,
    scheduling_context: Option<ReplySchedulingContextCustody>,
) -> Result<(KernelReplyHandle, u64), IpcError> {
    if request.is_empty() || request.len() > IPC_FAST_MAX_BYTES
        || response_capacity > IPC_FAST_MAX_BYTES
    {
        return Err(IpcError::InvalidArgument);
    }
    if scheduling_context.is_some_and(|custody| custody.caller_task_id() != caller_task_id) {
        return Err(IpcError::InvalidArgument);
    }
    // Allocated before any lock: a failure is the ordinary "no frame" fallback.
    let spill = if request.len() > IPC_FAST_INLINE_BYTES {
        let mut bytes = Vec::new();
        bytes.try_reserve_exact(request.len()).map_err(|_| IpcError::NoMemory)?;
        bytes.extend_from_slice(request);
        Some(FastSpill { request: bytes, response: Vec::new() })
    } else {
        None
    };
    let mut spill = spill;

    ENDPOINTS
        .with_mut(endpoint.raw(), |endpoint_object| {
            if request.len() > endpoint_object.max_request_bytes { return Err(IpcError::InvalidArgument); }
            if endpoint_object.has_pending() {
                return Err(IpcError::NoMemory);
            }
            let receiver_waiter = endpoint_object
                .waiting_receivers
                .front()
                .copied()
                .filter(|waiter| {
                    waiter.task_id != caller_task_id && request.len() <= waiter.request_capacity
                })
                .ok_or(IpcError::NoMemory)?;

            // LOCK ORDER: IpcEndpoint precedes IpcReply, matching request
            // consumption below. One endpoint transaction binds the exact
            // front waiter to the reply frame; no advisory read/reacquisition
            // can race a receiver cancellation between selection and publish.
            let receiver_owner = endpoint_object.owner;
            let mut scheduling_context = scheduling_context;
            let reply_id = if let Some(virtual_id) = virtual_rendezvous::try_reserve(
                endpoint.raw(),
                receiver_owner,
                caller_process_id,
                caller_task_id,
                receiver_waiter.task_id,
                receiver_waiter.request_capacity,
                response_capacity,
                request,
                &mut scheduling_context,
            ) {
                virtual_id
            } else {
                insert_fast_reply_object(caller_task_id, || {
                    let mut frame = FastCallFrame {
                        endpoint_id: endpoint.raw(),
                        caller_process_id,
                        caller_task_id,
                        receiver_task_id: receiver_waiter.task_id,
                        receiver_request_capacity: receiver_waiter.request_capacity,
                        caller_response_capacity: response_capacity,
                        request_len: request.len(),
                        response_len: 0,
                        state: FastCallState::RequestReady,
                        terminal_error: None,
                        request: [const { core::mem::MaybeUninit::uninit() }; IPC_FAST_INLINE_BYTES],
                        response: [const { core::mem::MaybeUninit::uninit() }; IPC_FAST_INLINE_BYTES],
                        spill: spill.take(),
                    };
                    if frame.spill.is_none() {
                        write_uninit_bytes(&mut frame.request, request);
                    }
                    ReplyObject {
                        message_id: 0,
                        receiver_owner,
                        used: false,
                        consumed: false,
                        scheduling_context,
                        fast_frame: Some(frame),
                    }
                })
                .ok_or(IpcError::NoMemory)?
            };

            endpoint_object.waiting_receivers.pop_front();
            endpoint_object.fast_reply = Some(reply_id);
            Ok((
                KernelReplyHandle::from_raw(reply_id),
                receiver_waiter.task_id,
            ))
        })
        .ok_or(IpcError::InvalidHandle)?
}

/// Removes an unpublished-to-userspace fast rendezvous and restores the exact
/// receiver to the front of the endpoint wait queue.  This is deliberately
/// narrower than cancellation: it succeeds only while the request remains in
/// `RequestReady`, so a receiver that took the frame and a rollback can never
/// both own the same request.  `pop_front` during reservation left capacity in
/// the bounded deque, making the matching `push_front` allocation-free.
pub fn rollback_fast_endpoint_call(
    endpoint: KernelEndpointHandle,
    reply: KernelReplyHandle,
    caller_task_id: u64,
    receiver_task_id: u64,
) -> Result<FastEndpointRollback, IpcError> {
    let is_virtual = virtual_rendezvous::is_virtual_reply_token(reply.raw());
    let rollback = ENDPOINTS
        .with_mut(endpoint.raw(), |endpoint_object| {
            if endpoint_object.fast_reply != Some(reply.raw()) {
                let exists = if is_virtual {
                    virtual_rendezvous::reply_diagnostic(reply.raw()).is_some()
                } else {
                    REPLIES.with(reply.raw(), |_| ()).is_some()
                };
                return Err(if exists {
                    IpcError::PermissionDenied
                } else {
                    IpcError::InvalidHandle
                });
            }
            // Revalidate and consume under Endpoint -> Reply, before changing
            // endpoint publication. Cancellation and receive cannot split this
            // transaction or restore a waiter for a cancelled frame.
            let (rollback, capacity) = if is_virtual {
                virtual_rendezvous::rollback(
                    endpoint.raw(),
                    reply.raw(),
                    caller_task_id,
                    receiver_task_id,
                )?
            } else {
                REPLIES
                    .with_mut(reply.raw(), |reply_object| {
                        let frame = reply_object.fast_frame.as_ref().ok_or(IpcError::InvalidHandle)?;
                        if reply_object.used || reply_object.consumed
                            || frame.endpoint_id != endpoint.raw()
                            || frame.caller_task_id != caller_task_id
                            || frame.receiver_task_id != receiver_task_id
                            || frame.state != FastCallState::RequestReady
                        {
                            return Err(IpcError::PermissionDenied);
                        }
                        let capacity = frame.receiver_request_capacity;
                        reply_object.consumed = true;
                        Ok((FastEndpointRollback {
                            receiver_task_id,
                            scheduling_context: reply_object.scheduling_context.take(),
                        }, capacity))
                    })
                    .ok_or(IpcError::InvalidHandle)??
            };
            endpoint_object.fast_reply = None;
            if let Some(waiter) = endpoint_object.waiting_receivers.iter_mut()
                .find(|waiter| waiter.task_id == receiver_task_id)
            {
                waiter.request_capacity = capacity;
            } else {
                endpoint_object.waiting_receivers.push_front(
                    endpoint_priority::EndpointReceiverWaiter {
                        task_id: receiver_task_id,
                        request_capacity: capacity,
                    },
                );
            }
            Ok(rollback)
        })
        .ok_or(IpcError::InvalidHandle)??;
    if !is_virtual {
        drop(REPLIES.remove(reply.raw()));
    }
    Ok(rollback)
}

/// True when a receiver already took (or answered) the caller's fast request,
/// so it can no longer be rolled back and its response will still arrive.
/// The caller must then wait for that response rather than report a failure
/// for a request that the server is executing.
pub fn fast_endpoint_request_owned_by_receiver(reply: KernelReplyHandle, caller_task_id: u64) -> bool {
    if virtual_rendezvous::is_virtual_reply_token(reply.raw()) {
        return virtual_rendezvous::fast_request_owned_by_receiver(reply.raw(), caller_task_id);
    }
    REPLIES
        .with(reply.raw(), |reply_object| {
            !reply_object.consumed
                && reply_object.fast_frame.as_ref().is_some_and(|frame| {
                    frame.caller_task_id == caller_task_id
                        && frame.state != FastCallState::RequestReady
                })
        })
        .unwrap_or(false)
}

pub fn take_fast_endpoint_request(
    endpoint: KernelEndpointHandle,
    receiver_task_id: u64,
) -> Result<Option<FastEndpointReceived>, IpcError> {
    take_fast_endpoint_request_within(endpoint, receiver_task_id, IPC_FAST_INLINE_BYTES)
}

/// `take_fast_endpoint_request` for a receiver whose buffer holds
/// `request_capacity` bytes: a larger request is refused without being consumed.
pub fn take_fast_endpoint_request_within(
    endpoint: KernelEndpointHandle,
    receiver_task_id: u64,
    request_capacity: usize,
) -> Result<Option<FastEndpointReceived>, IpcError> {
    take_fast_endpoint_request_inline::<IPC_FAST_INLINE_BYTES>(endpoint, receiver_task_id, request_capacity)
}

/// Same atomic receive transaction with a bounded in-kernel return image.
/// Short register IPC must not copy a full 256-byte frame between Rust layers.
pub fn take_fast_endpoint_request_inline<const N: usize>(
    endpoint: KernelEndpointHandle,
    receiver_task_id: u64,
    request_capacity: usize,
) -> Result<Option<FastEndpointReceived<N>>, IpcError> {
    ENDPOINTS
        .with_mut(endpoint.raw(), |endpoint_object| {
            let received = endpoint_receive::take_fast_request_inline::<N>(
                endpoint_object,
                endpoint,
                receiver_task_id,
                request_capacity.min(IPC_FAST_MAX_BYTES),
            )?;
            // Unlike receive-or-wait, a direct fast-frame take is an exact
            // receiver operation, not a search for other available work.
            if received.is_none() && endpoint_object.fast_reply.is_some() {
                return Err(IpcError::PermissionDenied);
            }
            Ok(received)
        })
        .ok_or(IpcError::InvalidHandle)?
}

/// Where a fast reply's bytes come from. A response above the inline image is
/// stored in the frame's spill; `Moving` hands the caller's own allocation over
/// so the spill needs neither a second reservation nor a second copy.
enum FastReplyBytes<'a> {
    Borrowed(&'a [u8]),
    Moving(&'a mut Vec<u8>),
}

impl FastReplyBytes<'_> {
    fn as_slice(&self) -> &[u8] {
        match self {
            Self::Borrowed(bytes) => bytes,
            Self::Moving(bytes) => bytes.as_slice(),
        }
    }
}

pub fn complete_fast_endpoint_reply_for_task(
    reply: KernelReplyHandle,
    receiver_task_id: u64,
    response: &[u8],
) -> Result<FastReplyCompletion, IpcError> {
    complete_fast_endpoint_reply(reply, receiver_task_id, FastReplyBytes::Borrowed(response))
}

/// [`complete_fast_endpoint_reply_for_task`] for a response the caller already
/// owns in a `Vec` whose length is exactly the response length. When the
/// response is larger than the inline image and is accepted, the allocation
/// itself becomes the frame spill and `response` is left empty; on any error
/// (including `InvalidHandle` for a non-fast reply) `response` is returned
/// untouched so the caller can retry on another transport. A response that fits
/// inline is copied and `response` is left as is.
pub fn complete_fast_endpoint_reply_moving(
    reply: KernelReplyHandle,
    receiver_task_id: u64,
    response: &mut Vec<u8>,
) -> Result<FastReplyCompletion, IpcError> {
    complete_fast_endpoint_reply(reply, receiver_task_id, FastReplyBytes::Moving(response))
}

fn complete_fast_endpoint_reply(
    reply: KernelReplyHandle,
    receiver_task_id: u64,
    mut response: FastReplyBytes<'_>,
) -> Result<FastReplyCompletion, IpcError> {
    if virtual_rendezvous::is_virtual_reply_token(reply.raw()) {
        return virtual_rendezvous::complete_reply(
            reply.raw(),
            receiver_task_id,
            response.as_slice(),
        );
    }
    let response_len = response.as_slice().len();
    // Validate before preparing a potentially large buffer. This observation
    // grants no authority: cancellation, teardown or another reply may win
    // while allocation is in progress, so the commit repeats every check.
    let capacity = if response_len > IPC_FAST_INLINE_BYTES {
        Some(
            REPLIES.with(reply.raw(), |object| {
                if object.used || object.consumed {
                    return Err(IpcError::InvalidArgument);
                }
                let frame = object.fast_frame.as_ref().ok_or(IpcError::InvalidHandle)?;
                if frame.receiver_task_id != receiver_task_id
                    || frame.state != FastCallState::RequestTaken
                {
                    return Err(IpcError::PermissionDenied);
                }
                Ok(frame.caller_response_capacity)
            }).ok_or(IpcError::InvalidHandle)??,
        )
    } else {
        None
    };

    let mut terminal_error = capacity
        .is_some_and(|capacity| response_len > IPC_FAST_MAX_BYTES || response_len > capacity)
        .then_some(IpcError::BufferTooSmall);
    let mut prepared_spill = None;
    if terminal_error.is_none() && response_len > IPC_FAST_INLINE_BYTES {
        match &mut response {
            FastReplyBytes::Moving(owned) => {
                prepared_spill = Some(FastSpill {
                    request: Vec::new(),
                    response: core::mem::take(*owned),
                });
            }
            FastReplyBytes::Borrowed(borrowed) => {
                let mut bytes = Vec::new();
                if bytes.try_reserve_exact(response_len).is_err() {
                    terminal_error = Some(IpcError::NoMemory);
                } else {
                    bytes.extend_from_slice(borrowed);
                    prepared_spill = Some(FastSpill {
                        request: Vec::new(),
                        response: bytes,
                    });
                }
            }
        }
    }
    let mut replaced_spill = None;
    let result = REPLIES
        .with_mut(reply.raw(), |reply_object| {
            if reply_object.used || reply_object.consumed {
                return Err(IpcError::InvalidArgument);
            }
            let frame = reply_object.fast_frame.as_mut().ok_or(IpcError::InvalidHandle)?;
            if frame.receiver_task_id != receiver_task_id || frame.state != FastCallState::RequestTaken {
                return Err(IpcError::PermissionDenied);
            }
            // The capacity belongs to this exact reply generation and is never
            // mutated; still check it as part of the authoritative transition.
            if capacity.is_some_and(|capacity| frame.caller_response_capacity != capacity) {
                return Err(IpcError::InvalidHandle);
            }
            if response_len > frame.caller_response_capacity {
                terminal_error = Some(IpcError::BufferTooSmall);
            }
            if let Some(error) = terminal_error {
                frame.response_len = 0;
                frame.terminal_error = Some(error);
                frame.state = FastCallState::ErrorReady;
            } else {
                if let Some(spill) = prepared_spill.take() {
                    replaced_spill = frame.spill.replace(spill);
                } else {
                    write_uninit_bytes(&mut frame.response, response.as_slice());
                }
                frame.response_len = response_len;
                frame.state = FastCallState::ResponseReady;
            }
            reply_object.used = true;
            Ok(FastReplyCompletion {
                completion: ReplyCompletion {
                    caller_task_id: frame.caller_task_id,
                    scheduling_context: reply_object.scheduling_context.take(),
                },
                terminal_error,
            })
        })
        .ok_or(IpcError::InvalidHandle)?;
    // A rejected moved response goes back to its owner; the owner's own bytes
    // are never destroyed by a refusal. A response terminated by BufferTooSmall
    // was consumed by the reply and is dropped below.
    if result.is_err()
        && let (Some(spill), FastReplyBytes::Moving(owned)) = (prepared_spill.take(), &mut response)
    {
        **owned = spill.response;
    }
    // Neither replaced nor rejected storage may be destroyed under a slot lock.
    drop(replaced_spill);
    drop(prepared_spill);
    result
}

#[cfg(test)]
pub fn take_fast_endpoint_response(
    reply: KernelReplyHandle,
    caller_task_id: u64,
) -> Result<FastEndpointResponseTake, IpcError> {
    take_fast_endpoint_response_inline::<IPC_FAST_INLINE_BYTES>(reply, caller_task_id)
}

/// A compact return image; exact caller, one-shot consume, terminal errors and
/// retirement use the very same reply-slot transaction as buffer IPC.
pub fn take_fast_endpoint_response_inline<const N: usize>(
    reply: KernelReplyHandle,
    caller_task_id: u64,
) -> Result<FastEndpointResponseTake<N>, IpcError> {
    if virtual_rendezvous::is_virtual_reply_token(reply.raw()) {
        return virtual_rendezvous::take_response::<N>(reply.raw(), caller_task_id);
    }
    // One acquisition where `with_mut` then `remove` took this slot's lock
    // twice; `GenerationalSlab::with_mut_take` says why the object comes back.
    let (result, retired) = REPLIES
        .with_mut_take(reply.raw(), |reply_object| 'take: {
            let Some(frame) = reply_object.fast_frame.as_ref() else {
                break 'take (Err(IpcError::InvalidHandle), false);
            };
            if frame.caller_task_id != caller_task_id || reply_object.consumed {
                break 'take (Err(IpcError::PermissionDenied), false);
            }
            if frame.state == FastCallState::ErrorReady {
                reply_object.consumed = true;
                break 'take match frame.terminal_error {
                    Some(error) => (Ok(FastEndpointResponseTake::Error(error)), true),
                    None => (Err(IpcError::InvalidHandle), false),
                };
            }
            if frame.state != FastCallState::ResponseReady {
                break 'take (Ok(FastEndpointResponseTake::Pending), false);
            }
            reply_object.consumed = true;
            if frame.response_len > IPC_FAST_INLINE_BYTES {
                let spilled = reply_object
                    .fast_frame
                    .as_mut()
                    .and_then(|frame| frame.spill.as_mut())
                    .map(|spill| core::mem::take(&mut spill.response));
                break 'take match spilled {
                    Some(bytes) if N >= IPC_FAST_INLINE_BYTES => {
                        (Ok(FastEndpointResponseTake::Spilled(bytes)), true)
                    }
                    _ => (Err(IpcError::BufferTooSmall), true),
                };
            }
            if frame.response_len > N {
                break 'take (Err(IpcError::BufferTooSmall), true);
            }
            let mut response = [0; N];
            copy_uninit_prefix(&frame.response, frame.response_len, &mut response[..frame.response_len]);
            (
                Ok(FastEndpointResponseTake::Response {
                    response_len: frame.response_len,
                    response,
                }),
                true,
            )
        })
        .ok_or(IpcError::InvalidHandle)?;
    drop(retired);
    result
}

/// Returns the process owner of the endpoint bound into a live reply
/// capability.  The scheduler uses this only to establish bounded priority
/// inheritance before a process-owned endpoint has selected a specific worker.
pub fn endpoint_receiver_process_for_reply(reply: KernelReplyHandle) -> Option<u64> {
    if virtual_rendezvous::is_virtual_reply_token(reply.raw()) {
        return virtual_rendezvous::endpoint_receiver_process_for_reply(reply.raw());
    }
    REPLIES
        .with(reply.raw(), |reply_object| {
            match reply_object.receiver_owner {
                Some(EndpointOwner::Process(process_id)) => Some(process_id),
                Some(EndpointOwner::Task(_)) | None => None,
            }
        })
        .flatten()
}


/// Read-only diagnostic summary of a reply: `(endpoint id, receiver owner,
/// state bits)`. Bits: 1 live message published to the endpoint queue, 2
/// request taken by a receiver (`used`), 4 response present, 8 consumed,
/// 16 fast frame. Grants nothing; for stall reports only.
pub fn reply_diagnostic(reply: KernelReplyHandle) -> Option<(u64, u64, u64)> {
    if virtual_rendezvous::is_virtual_reply_token(reply.raw()) {
        return virtual_rendezvous::reply_diagnostic(reply.raw());
    }
    let (message_id, receiver, mut bits) = REPLIES.with(reply.raw(), |object| {
        let receiver = match object.receiver_owner {
            Some(EndpointOwner::Process(id)) => id,
            Some(EndpointOwner::Task(id)) => id | (1 << 63),
            None => 0,
        };
        let bits = u64::from(object.used) << 1
            | u64::from(object.consumed) << 3
            | u64::from(object.fast_frame.is_some()) << 4;
        (object.message_id, receiver, bits)
    })?;
    let endpoint = if let Some(frame_endpoint) =
        REPLIES.with(reply.raw(), |object| object.fast_frame.as_ref().map(|frame| frame.endpoint_id)).flatten()
    {
        frame_endpoint
    } else {
        ENDPOINT_MESSAGES
            .with(message_id, |message| {
                bits |= u64::from(message.published) | u64::from(message.response.is_some()) << 2;
                message.endpoint_id
            })
            .unwrap_or(0)
    };
    Some((endpoint, receiver, bits))
}

/// Read-only endpoint summary for stall reports: `(pending messages,
/// parked receivers, first parked receiver task, fast reply present)`.
pub fn endpoint_diagnostic(endpoint: u64) -> Option<(u64, u64, u64, bool)> {
    ENDPOINTS.with(endpoint, |object| {
        (
            object.pending_len() as u64,
            object.waiting_receivers.len() as u64,
            object.waiting_receivers.front().map_or(0, |waiter| waiter.task_id),
            object.fast_reply.is_some(),
        )
    })
}

/// Confirms that the exact live reply already crossed its terminal reply
/// transition and returned scheduling-context custody to `caller_task_id`.
///
/// This is a narrow race discriminator for the sender-side admission commit:
/// a receiver on another CPU may bind and answer between endpoint publication
/// and the sender attaching its scheduler reservation. `used` is published in
/// the same reply transition that takes custody, while the message identity
/// keeps an unrelated or recycled reply from satisfying the check.
pub fn endpoint_reply_custody_returned(reply: KernelReplyHandle, caller_task_id: u64) -> bool {
    let Some((message_id, returned)) = REPLIES.with(reply.raw(), |reply_object| {
        (
            reply_object.message_id,
            reply_object.used && reply_object.scheduling_context.is_none(),
        )
    }) else {
        return false;
    };
    returned
        && ENDPOINT_MESSAGES
            .with(message_id, |message| {
                message.reply_id == reply.raw() && message.caller_task_id == caller_task_id
            })
            .unwrap_or(false)
}

/// Atomically binds already-registered descriptors to one live reply owned by
/// `receiver_process_id`.
///
/// The caller retains the vector on every rejection.  This boundary does not
/// create, duplicate, or release provider references; it only establishes
/// reply ownership.  Cancellation, caller exit, endpoint failure, and a late
/// take subsequently return the same descriptors through the normal endpoint
/// transfer cleanup result.
pub fn bind_prepared_reply_handles_for_process(
    reply: KernelReplyHandle,
    receiver_process_id: u64,
    handles: Vec<KernelTransferredHandle>,
) -> Result<(), PreparedReplyHandleBindError> {
    if receiver_process_id == 0 || handles.is_empty() {
        return Err(PreparedReplyHandleBindError {
            error: IpcError::InvalidArgument,
            handles,
        });
    }
    if let Err(error) = validate_endpoint_transfer_handles(handles.as_slice()) {
        return Err(PreparedReplyHandleBindError { error, handles });
    }

    let mut handles = Some(handles);
    let result = REPLIES
        .with(reply.raw(), |reply_object| {
            if reply_object.receiver_owner != Some(EndpointOwner::Process(receiver_process_id)) {
                return Err(IpcError::PermissionDenied);
            }
            if reply_object.used || reply_object.consumed {
                return Err(IpcError::InvalidArgument);
            }
            Ok(reply_object.message_id)
        })
        .ok_or(IpcError::InvalidHandle)
        .and_then(|result| result)
        .and_then(|message_id| {
            ENDPOINT_MESSAGES
                .with_mut(message_id, |message| {
                    REPLIES
                        .with_mut(reply.raw(), |reply_object| {
                            if reply_object.message_id != message_id
                                || reply_object.receiver_owner
                                    != Some(EndpointOwner::Process(receiver_process_id))
                            {
                                return Err(IpcError::PermissionDenied);
                            }
                            if reply_object.used || reply_object.consumed {
                                return Err(IpcError::InvalidArgument);
                            }
                            if !message.published || message.reply_id != reply.raw() {
                                return Err(IpcError::InvalidHandle);
                            }
                            if message.response.is_some()
                                || message.prepared_reply_handles.is_some()
                            {
                                return Err(IpcError::InvalidArgument);
                            }
                            message.prepared_reply_handles = handles.take();
                            Ok(())
                        })
                        .ok_or(IpcError::InvalidHandle)?
                })
                .ok_or(IpcError::InvalidHandle)?
        });
    match result {
        Ok(()) => Ok(()),
        Err(error) => Err(PreparedReplyHandleBindError {
            error,
            handles: handles.expect("rejected reply-handle bind lost descriptor ownership"),
        }),
    }
}

#[cfg(test)]
pub fn recv_endpoint(
    endpoint: KernelEndpointHandle,
) -> Result<Option<(KernelReplyHandle, Vec<u8>)>, IpcError> {
    recv_endpoint_with_limits(endpoint, usize::MAX)
}

#[cfg(test)]
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

#[cfg(test)]
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
            endpoint_receive::take_queued_request(
                endpoint_object,
                endpoint,
                request_capacity,
                handle_capacity,
            )
        })
        .ok_or(IpcError::InvalidHandle)?
}

#[cfg(test)]
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

#[cfg(test)]
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
    add_endpoint_receiver_waiter_with_capacity(endpoint, task_id, IPC_FAST_INLINE_BYTES)
}

pub fn add_endpoint_receiver_waiter_with_capacity(
    endpoint: KernelEndpointHandle,
    task_id: u64,
    request_capacity: usize,
) -> Result<bool, IpcError> {
    ENDPOINTS
        .with_mut(endpoint.raw(), |endpoint_object| {
            if endpoint_object.has_pending() {
                return Ok(true);
            }
            endpoint_receive::register_receiver(endpoint_object, task_id, request_capacity)?;
            Ok(false)
        })
        .ok_or(IpcError::InvalidHandle)?
}

#[cfg(test)]
pub fn complete_endpoint_reply(reply: KernelReplyHandle, response: &[u8]) -> Result<u64, IpcError> {
    complete_endpoint_reply_with_handles(reply, response, &[])
}

#[cfg(test)]
pub fn complete_endpoint_reply_with_handles(
    reply: KernelReplyHandle,
    response: &[u8],
    attached_handles: &[KernelTransferredHandle],
) -> Result<u64, IpcError> {
    ensure_reply_has_no_scheduling_context(reply)?;
    complete_endpoint_reply_with_handles_faultable(
        reply,
        response,
        attached_handles,
        rustos_fault_injection::should_fail("ipc.endpoint.reply"),
    )
}

#[cfg(test)]
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
    .map(|completion| completion.caller_task_id)
}

#[cfg(test)]
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

#[cfg(test)]
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

pub fn complete_endpoint_reply_for_process_with_custody(
    reply: KernelReplyHandle,
    receiver_process_id: u64,
    response: &[u8],
) -> Result<ReplyCompletion, IpcError> {
    complete_endpoint_reply_with_handles_for_owner_with_custody(
        reply,
        EndpointOwner::Process(receiver_process_id),
        response,
        &[],
    )
}

pub fn complete_endpoint_reply_payload_for_process_with_custody(
    reply: KernelReplyHandle,
    receiver_process_id: u64,
    response: EndpointPayload,
) -> Result<ReplyCompletion, IpcError> {
    complete_endpoint_reply_with_handles_payload_for_process_with_custody(
        reply,
        receiver_process_id,
        response,
        &[],
    )
}

pub fn complete_endpoint_reply_with_handles_for_process_with_custody(
    reply: KernelReplyHandle,
    receiver_process_id: u64,
    response: &[u8],
    attached_handles: &[KernelTransferredHandle],
) -> Result<ReplyCompletion, IpcError> {
    complete_endpoint_reply_with_handles_for_owner_with_custody(
        reply,
        EndpointOwner::Process(receiver_process_id),
        response,
        attached_handles,
    )
}

pub fn complete_endpoint_reply_with_handles_payload_for_process_with_custody(
    reply: KernelReplyHandle,
    receiver_process_id: u64,
    response: EndpointPayload,
    attached_handles: &[KernelTransferredHandle],
) -> Result<ReplyCompletion, IpcError> {
    if response.len() > MAX_ENDPOINT_INLINE_MESSAGE_BYTES {
        return Err(IpcError::InvalidArgument);
    }
    validate_endpoint_transfer_handles(attached_handles)?;
    if rustos_fault_injection::should_fail("ipc.endpoint.reply") {
        return Err(IpcError::NoMemory);
    }
    complete_endpoint_reply_prepared(
        reply,
        Some(EndpointOwner::Process(receiver_process_id)),
        prepare_endpoint_response_payload(response, attached_handles)?,
    )
}

#[cfg(test)]
fn ensure_reply_has_no_scheduling_context(reply: KernelReplyHandle) -> Result<(), IpcError> {
    if virtual_rendezvous::is_virtual_reply_token(reply.raw()) {
        return (!virtual_rendezvous::has_scheduling_context(reply.raw())?)
            .then_some(())
            .ok_or(IpcError::InvalidArgument);
    }
    REPLIES
        .with(reply.raw(), |object| object.scheduling_context.is_none())
        .ok_or(IpcError::InvalidHandle)?
        .then_some(())
        .ok_or(IpcError::InvalidArgument)
}

#[cfg(test)]
fn complete_endpoint_reply_with_handles_for_owner(
    reply: KernelReplyHandle,
    receiver_owner: EndpointOwner,
    response: &[u8],
    attached_handles: &[KernelTransferredHandle],
) -> Result<u64, IpcError> {
    ensure_reply_has_no_scheduling_context(reply)?;
    complete_endpoint_reply_with_handles_for_owner_faultable(
        reply,
        receiver_owner,
        response,
        attached_handles,
        rustos_fault_injection::should_fail("ipc.endpoint.reply"),
    )
}

fn complete_endpoint_reply_with_handles_for_owner_with_custody(
    reply: KernelReplyHandle,
    receiver_owner: EndpointOwner,
    response: &[u8],
    attached_handles: &[KernelTransferredHandle],
) -> Result<ReplyCompletion, IpcError> {
    if response.len() > MAX_ENDPOINT_INLINE_MESSAGE_BYTES {
        return Err(IpcError::InvalidArgument);
    }
    validate_endpoint_transfer_handles(attached_handles)?;
    if rustos_fault_injection::should_fail("ipc.endpoint.reply") {
        return Err(IpcError::NoMemory);
    }
    complete_endpoint_reply_prepared(
        reply,
        Some(receiver_owner),
        prepare_endpoint_response(response, attached_handles)?,
    )
}

#[cfg(test)]
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
    .map(|completion| completion.caller_task_id)
}

fn prepare_endpoint_response_payload(
    bytes: EndpointPayload,
    attached_handles: &[KernelTransferredHandle],
) -> Result<EndpointResponse, IpcError> {
    let mut handles = Vec::new();
    if !attached_handles.is_empty() {
        // Reserve for any receiver-prepared batch before taking slot locks.
        handles
            .try_reserve_exact(MAX_ENDPOINT_TRANSFER_HANDLES)
            .map_err(|_| IpcError::NoMemory)?;
        handles.extend_from_slice(attached_handles);
    }
    Ok(EndpointResponse::Data {
        bytes,
        attached_handles: handles,
        prepared_handles: None,
    })
}

fn prepare_endpoint_response(
    response: &[u8],
    attached_handles: &[KernelTransferredHandle],
) -> Result<EndpointResponse, IpcError> {
    prepare_endpoint_response_payload(EndpointPayload::try_from_slice(response)?, attached_handles)
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
) -> Result<ReplyCompletion, IpcError> {
    let message_id = published_reply_message_id(reply.raw()).ok_or(IpcError::InvalidHandle)?;
    nucleus_core::debug::ipc_tail::record_task_tail_event(
        nucleus_core::debug::ipc_tail::IpcTailEventKind::ReplyPublish,
        0,
        0,
        reply.raw(),
        message_id,
    );

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
                    if let (
                        Some(EndpointResponse::Data {
                            attached_handles, ..
                        }),
                        Some(prepared_handles),
                    ) = (response.as_mut(), message.prepared_reply_handles.as_ref())
                    {
                        if attached_handles
                            .len()
                            .saturating_add(prepared_handles.len())
                            > MAX_ENDPOINT_TRANSFER_HANDLES
                        {
                            return Err(IpcError::InvalidArgument);
                        }
                    }
                    if let Some(EndpointResponse::Data {
                        prepared_handles: response_prepared,
                        ..
                    }) = response.as_mut()
                    {
                        // Keep batches separate under the locks and merge them
                        // afterward using already reserved capacity.
                        *response_prepared = message.prepared_reply_handles.take();
                    }
                    let caller_task_id = message.caller_task_id;
                    message.response = response.take();
                    reply_object.used = true;
                    reply_publication::note_reply_published(reply.raw());
                    Ok(ReplyCompletion {
                        caller_task_id,
                        scheduling_context: reply_object.scheduling_context.take(),
                    })
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

pub fn endpoint_reply_is_known_pending(reply: KernelReplyHandle) -> bool {
    reply_publication::reply_is_known_pending(reply.raw())
}

pub fn take_endpoint_response_detailed(
    reply: KernelReplyHandle,
    handle_capacity: usize,
) -> Result<EndpointResponseTake, IpcError> {
    match take_endpoint_response_payload_detailed(reply, handle_capacity)? {
        EndpointPayloadTake::Pending => Ok(EndpointResponseTake::Pending),
        EndpointPayloadTake::Response((bytes, handles)) => {
            Ok(EndpointResponseTake::Response((bytes.into_vec(), handles)))
        }
        EndpointPayloadTake::Error {
            error,
            discarded_request_handles,
        } => Ok(EndpointResponseTake::Error {
            error,
            discarded_request_handles,
        }),
    }
}

pub fn take_endpoint_response_payload_detailed(
    reply: KernelReplyHandle,
    handle_capacity: usize,
) -> Result<EndpointPayloadTake, IpcError> {
    enum RawTake {
        Pending,
        Response {
            bytes: EndpointPayload,
            attached_handles: Vec<KernelTransferredHandle>,
            prepared_handles: Option<Vec<KernelTransferredHandle>>,
        },
        Error {
            error: IpcError,
            attached_handles: Vec<KernelTransferredHandle>,
            prepared_handles: Option<Vec<KernelTransferredHandle>>,
        },
    }

    let message_id = published_reply_message_id(reply.raw()).ok_or(IpcError::InvalidHandle)?;

    let ((result, retired_reply), retired_message) = ENDPOINT_MESSAGES
        .with_mut_remove_if(message_id, |message| {
            let Some((inner, retired_reply)) = REPLIES.with_mut_remove_if(reply.raw(), |reply_object| {
                if reply_object.message_id != message_id || reply_object.consumed {
                    return (Err(IpcError::InvalidHandle), false);
                }
                match message.response.as_ref() {
                    None => (Ok(RawTake::Pending), false),
                    Some(EndpointResponse::Data {
                        attached_handles, ..
                    }) if attached_handles.len() > handle_capacity => {
                        (Err(IpcError::BufferTooSmall), false)
                    }
                    Some(EndpointResponse::Data {
                        attached_handles,
                        prepared_handles,
                        ..
                    }) => {
                        let remaining = handle_capacity.saturating_sub(attached_handles.len());
                        if prepared_handles.as_ref().is_some_and(|handles| handles.len() > remaining) {
                            return (Err(IpcError::BufferTooSmall), false);
                        }
                        let Some(EndpointResponse::Data {
                            bytes,
                            attached_handles,
                            prepared_handles,
                        }) = message.response.take()
                        else {
                            unreachable!("response was checked above");
                        };
                        reply_object.consumed = true;
                        (
                            Ok(RawTake::Response {
                                bytes,
                                attached_handles,
                                prepared_handles,
                            }),
                            true,
                        )
                    }
                    Some(EndpointResponse::Error(err)) => {
                        let err = *err;
                        message.response.take();
                        let attached_handles = core::mem::take(&mut message.attached_handles);
                        let prepared_handles = message.prepared_reply_handles.take();
                        reply_object.consumed = true;
                        (
                            Ok(RawTake::Error {
                                error: err,
                                attached_handles,
                                prepared_handles,
                            }),
                            true,
                        )
                    }
                }
            }) else {
                return ((Err(IpcError::InvalidHandle), None), false);
            };
            if retired_reply.is_some() {
                reply_publication::clear_reply_publication(reply.raw());
            }
            let retire_message = retired_reply.is_some();
            ((inner, retired_reply), retire_message)
        })
        .ok_or(IpcError::InvalidHandle)?;
    drop(retired_reply);
    drop(retired_message);

    let merge_handles =
        |mut primary: Vec<KernelTransferredHandle>,
         prepared: Option<Vec<KernelTransferredHandle>>| {
            if let Some(mut extra) = prepared {
                if primary.is_empty() {
                    primary = extra;
                } else if !extra.is_empty() {
                    assert!(primary.capacity() - primary.len() >= extra.len());
                    primary.append(&mut extra);
                }
            }
            primary
        };

    match result? {
        RawTake::Pending => Ok(EndpointPayloadTake::Pending),
        RawTake::Response {
            bytes,
            attached_handles,
            prepared_handles,
        } => Ok(EndpointPayloadTake::Response((
            bytes,
            merge_handles(attached_handles, prepared_handles),
        ))),
        RawTake::Error {
            error,
            attached_handles: request_handles,
            prepared_handles,
        } => Ok(EndpointPayloadTake::Error {
            error,
            discarded_request_handles: merge_handles(request_handles, prepared_handles),
        }),
    }
}

#[cfg(test)]
pub fn cancel_endpoint_call(reply: KernelReplyHandle, caller_task_id: u64) -> Result<(), IpcError> {
    ensure_reply_has_no_scheduling_context(reply)?;
    cancel_endpoint_call_with_transfers(reply, caller_task_id).map(|_| ())
}

/// What a cancellation actually reclaimed.
///
/// The two cases cost the system entirely different amounts and only one of
/// them can compound. A call still queued is reclaimed whole: no receiver ever
/// saw it and nothing was computed for it. A call already delivered leaves the
/// service producing a reply that will be rejected on arrival, so the work is
/// spent and the endpoint's next request starts that much later - the shape
/// QNX addresses by delivering an unblock pulse to the server. Reporting the
/// two as one success is what let ninety-eight consecutive rejected replies
/// look like ordinary timeout handling in a measured run.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CancelledCallDisposition {
    /// Reclaimed from the endpoint queue before any receiver took it.
    Queued,
    /// Already delivered; the receiver is still working on it.
    InFlight,
}

#[derive(Debug)]
pub struct CancelledCall {
    pub transfers: Vec<KernelTransferredHandle>,
    pub disposition: CancelledCallDisposition,
    pub scheduling_context: Option<ReplySchedulingContextCustody>,
}

pub fn cancel_endpoint_call_with_transfers(
    reply: KernelReplyHandle,
    caller_task_id: u64,
) -> Result<CancelledCall, IpcError> {
    if virtual_rendezvous::is_virtual_reply_token(reply.raw()) {
        return virtual_rendezvous::cancel_call(reply.raw(), caller_task_id);
    }
    enum CancelTarget {
        Fast {
            endpoint_id: u64,
            bound_caller: u64,
            request_was_published: bool,
        },
        Queued {
            message_id: u64,
        },
    }

    let target = REPLIES
        .with(reply.raw(), |reply_object| {
            if let Some(frame) = reply_object.fast_frame.as_ref() {
                Ok(CancelTarget::Fast {
                    endpoint_id: frame.endpoint_id,
                    bound_caller: frame.caller_task_id,
                    request_was_published: frame.state == FastCallState::RequestReady,
                })
            } else if !reply_object.consumed {
                Ok(CancelTarget::Queued {
                    message_id: reply_object.message_id,
                })
            } else {
                Err(IpcError::InvalidHandle)
            }
        })
        .ok_or(IpcError::InvalidHandle)??;

    let message_id = match target {
        CancelTarget::Fast {
            endpoint_id,
            bound_caller,
            request_was_published,
        } => {
            if bound_caller != caller_task_id {
                return Err(IpcError::InvalidArgument);
            }
            let mut was_queued = false;
            let _ = ENDPOINTS.with_mut(endpoint_id, |endpoint| {
                if endpoint.fast_reply == Some(reply.raw()) {
                    endpoint.fast_reply.take();
                    was_queued = request_was_published;
                }
            });
            let (scheduling_context, retired) = REPLIES
                .with_mut_remove_if(reply.raw(), |reply_object| {
                    if reply_object.consumed {
                        return (Err(IpcError::InvalidHandle), false);
                    }
                    reply_object.consumed = true;
                    (Ok(reply_object.scheduling_context.take()), true)
                })
                .ok_or(IpcError::InvalidHandle)?;
            drop(retired);
            let scheduling_context = scheduling_context?;
            return Ok(CancelledCall {
                transfers: Vec::new(),
                disposition: if was_queued {
                    CancelledCallDisposition::Queued
                } else {
                    CancelledCallDisposition::InFlight
                },
                scheduling_context,
            });
        }
        CancelTarget::Queued { message_id } => message_id,
    };

    #[cfg(test)]
    let binding_reply_id = if *ENDPOINT_CANCEL_REPLY_BINDING_FAULT.lock() {
        reply.raw().wrapping_add(1)
    } else {
        reply.raw()
    };
    #[cfg(not(test))]
    let binding_reply_id = reply.raw();

    let endpoint_id = ENDPOINT_MESSAGES
        .with(message_id, |message| {
            let endpoint_id = (message.caller_task_id == caller_task_id).then_some(message.endpoint_id)
                .ok_or(IpcError::InvalidArgument)?;
            let binding_matches = message.reply_id == binding_reply_id;
            if !binding_matches {
                return Err(IpcError::InvalidHandle);
            }
            Ok(endpoint_id)
        })
        // A response taker can retire the exact message between the two
        // slab lookups. Absence is terminal, not a caller mismatch.
        .ok_or(IpcError::InvalidHandle)??;

    let mut marked = false;
    let mut was_queued = false;
    if ENDPOINTS
        .with_mut(endpoint_id, |endpoint| {
            let before = endpoint.pending_len();
            endpoint
                .pending_messages
                .retain(|pending_message_id| *pending_message_id != message_id);
            endpoint
                .pending_system_messages
                .retain(|pending_message_id| *pending_message_id != message_id);
            was_queued = endpoint.pending_len() != before;
            marked = mark_endpoint_call_consumed(message_id, binding_reply_id, caller_task_id);
        })
        .is_none()
    {
        marked = mark_endpoint_call_consumed(message_id, binding_reply_id, caller_task_id);
    }
    if !marked {
        return Err(IpcError::InvalidHandle);
    }

    let message = ENDPOINT_MESSAGES
        .remove(message_id)
        .ok_or(IpcError::InvalidHandle)?;
    reply_publication::clear_reply_publication(reply.raw());
    let reply_object = REPLIES.remove(reply.raw()).ok_or(IpcError::InvalidHandle)?;
    Ok(CancelledCall {
        transfers: transfers_from_message(message),
        disposition: if was_queued {
            CancelledCallDisposition::Queued
        } else {
            CancelledCallDisposition::InFlight
        },
        scheduling_context: reply_object.scheduling_context,
    })
}

/// Cancels every endpoint call owned by a task being retired.  The caller may
/// no longer reach the response path, so both request and already-queued reply
/// descriptors are returned to the process-handle substrate for disposal.
pub fn cancel_endpoint_calls_for_task(
    task_id: u64,
    mut release_transfers: impl FnMut(&[KernelTransferredHandle]),
    mut release_scheduling_context: impl FnMut(KernelReplyHandle, ReplySchedulingContextCustody),
) -> usize {
    let mut cancelled = 0usize;
    // A single pass over occupied slots avoids O(K * N) rescanning and avoids
    // revisiting entries whose reply already crossed a concurrent terminal
    // transition on another CPU.
    let mut next_message_index = 0usize;
    while let Some(message_id) = ENDPOINT_MESSAGES.find_handle_from(next_message_index, |message| {
        message.published && message.caller_task_id == task_id
    }) {
        let Some(slot_index) = slab::slot_index::<MAX_ENDPOINT_MESSAGE_OBJECTS>(message_id) else {
            break;
        };
        next_message_index = slot_index + 1;
        let Some(reply_id) = ENDPOINT_MESSAGES.with(message_id, |message| message.reply_id) else {
            continue;
        };
        let cancelled_call = match cancel_endpoint_call_with_transfers(
            KernelReplyHandle::from_raw(reply_id), task_id,
        ) {
            Ok(call) => call,
            Err(IpcError::InvalidHandle) => {
                // Either a concurrent terminal consumer already set `reply.consumed`
                // and/or removed one or both slab objects. Check `REPLIES` first,
                // then recheck `ENDPOINT_MESSAGES` in case `REPLIES` was removed
                // before `ENDPOINT_MESSAGES`.
                if REPLIES.with(reply_id, |reply| reply.consumed) == Some(true)
                    || ENDPOINT_MESSAGES.with(message_id, |_| ()).is_none()
                {
                    continue;
                }
                panic!(
                    "published task-owned endpoint call lost live cancellation binding task={task_id} reply={reply_id}"
                );
            }
            Err(error) => panic!(
                "published task-owned endpoint call cancellation failed task={task_id} reply={reply_id} error={error:?}"
            ),
        };
        release_transfers(&cancelled_call.transfers);
        if let Some(custody) = cancelled_call.scheduling_context {
            release_scheduling_context(KernelReplyHandle::from_raw(reply_id), custody);
        }
        cancelled += 1;
    }

    let mut next_reply_index = 0usize;
    while let Some(reply_id) = REPLIES.find_handle_from(next_reply_index, |reply| {
        reply
            .fast_frame
            .as_ref()
            .is_some_and(|frame| frame.caller_task_id == task_id && !reply.consumed)
    }) {
        let Some(slot_index) = slab::slot_index::<MAX_REPLY_OBJECTS>(reply_id) else {
            break;
        };
        next_reply_index = slot_index + 1;
        let cancelled_call = match cancel_endpoint_call_with_transfers(
            KernelReplyHandle::from_raw(reply_id), task_id,
        ) {
            Ok(call) => call,
            // The exact fast reply may have been consumed and removed since
            // find_handle_from. The predicate excludes every consumed reply.
            Err(IpcError::InvalidHandle) => continue,
            Err(error) => panic!(
                "published task-owned fast call cancellation failed task={task_id} reply={reply_id} error={error:?}"
            ),
        };
        if let Some(custody) = cancelled_call.scheduling_context {
            release_scheduling_context(KernelReplyHandle::from_raw(reply_id), custody);
        }
        cancelled += 1;
    }
    cancelled += virtual_rendezvous::cancel_calls_for_task(
        task_id,
        &mut release_scheduling_context,
    );
    cancelled
}

fn transfers_from_message(mut message: EndpointMessageObject) -> Vec<KernelTransferredHandle> {
    let mut transfers = core::mem::take(&mut message.attached_handles);
    if let Some(mut prepared_handles) = message.prepared_reply_handles.take() {
        if transfers.capacity() < transfers.len().saturating_add(prepared_handles.len()) {
            core::mem::swap(&mut transfers, &mut prepared_handles);
        }
        transfers.extend(prepared_handles);
    }
    if let Some(EndpointResponse::Data {
        attached_handles: mut response_handles,
        prepared_handles,
        ..
    }) = message.response.take()
    {
        if transfers.capacity() < transfers.len().saturating_add(response_handles.len()) {
            core::mem::swap(&mut transfers, &mut response_handles);
        }
        transfers.extend(response_handles);
        if let Some(mut prepared) = prepared_handles {
            if transfers.capacity() < transfers.len().saturating_add(prepared.len()) {
                core::mem::swap(&mut transfers, &mut prepared);
            }
            transfers.extend(prepared);
        }
    }
    transfers
}

/// Withdraws `task_id`'s receive waiter from one exact endpoint.
///
/// The whole-slab form below acquires the slab lock once per slot -- 512 of
/// them -- because a retiring task does not know which endpoints it was parked
/// on. A receive that is abandoning its own wait does know: it is the endpoint
/// it just tried to receive from. Taking the general form there cost that
/// receive 512 acquisitions and made it the most-acquired endpoint site in the
/// system, for a removal that touches one object.
pub fn remove_endpoint_waiter_for_task(endpoint: KernelEndpointHandle, task_id: u64) -> usize {
    ENDPOINTS
        .with_mut(endpoint.raw(), |endpoint_object| {
            let before = endpoint_object.waiting_receivers.len();
            endpoint_object
                .waiting_receivers
                .retain(|waiter| waiter.task_id != task_id);
            before.saturating_sub(endpoint_object.waiting_receivers.len())
        })
        .unwrap_or(0)
}

/// Republishes a receive waiter that this call's own enqueue popped when the
/// scheduler then refused to wake it. Mirrors `rollback_fast_endpoint_call`:
/// without it the receiver commits its block outside the waiter list and no
/// later call can ever wake it. Capacity 0 keeps the fast path off this entry
/// until the receiver re-registers its real capacity on its next receive.
pub fn restore_endpoint_receiver_waiter(
    endpoint: KernelEndpointHandle,
    task_id: u64,
) -> Result<bool, IpcError> {
    ENDPOINTS
        .with_mut(endpoint.raw(), |object| {
            if object.waiting_receivers.iter().any(|w| w.task_id == task_id) {
                return Ok(false);
            }
            if !object.waiter_capacity_available() {
                return Err(IpcError::NoMemory);
            }
            object
                .waiting_receivers
                .push_front(endpoint_priority::EndpointReceiverWaiter {
                    task_id,
                    request_capacity: 0,
                });
            Ok(true)
        })
        .ok_or(IpcError::InvalidHandle)?
}

pub fn remove_endpoint_waiters_for_task(task_id: u64) -> usize {
    let mut removed = 0;
    ENDPOINTS.visit_mut(|_, endpoint| {
        let before = endpoint.waiting_receivers.len();
        endpoint
            .waiting_receivers
            .retain(|waiter| waiter.task_id != task_id);
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
    let mut owned_inline = [0_u64; MAX_ENDPOINTS_PER_PROCESS];
    let mut owned_inline_len = 0usize;
    let mut owned_overflow = Vec::new();

    ENDPOINTS.drain_matching(
        |endpoint| endpoint.owner == Some(owner),
        |endpoint_id, endpoint| {
            ENDPOINT_QUOTAS.lock().release(owner);
            endpoint.refund_kmem();
            wake_set.receivers.reserve(endpoint.waiting_receivers.len());
            for receiver in endpoint.waiting_receivers {
                wake_set.push_receiver(receiver.task_id);
            }
            if owned_inline_len < owned_inline.len() {
                owned_inline[owned_inline_len] = endpoint_id;
                owned_inline_len += 1;
            } else {
                owned_overflow.push(endpoint_id);
            }
        },
    );

    if owned_inline_len == 0 {
        return wake_set;
    }

    let owned_contains = |endpoint_id: u64| {
        owned_inline[..owned_inline_len].contains(&endpoint_id)
            || owned_overflow.contains(&endpoint_id)
    };

    wake_set.reserve_for_endpoint_teardown(
        REPLIES.occupied_count() + virtual_rendezvous::occupied_count_estimate(),
    );

    virtual_rendezvous::fail_endpoints(&owned_contains, err, &mut wake_set);

    // A receiver removes fast_reply when it takes the request, but the
    // live reply still belongs to this endpoint until completion. Fail both
    // published and already-taken frames when the owner exits.
    REPLIES.visit_mut(|reply_id, reply| {
        let Some(frame) = reply.fast_frame.as_mut() else {
            return;
        };
        if reply.used || reply.consumed || !owned_contains(frame.endpoint_id) {
            return;
        }
        frame.state = FastCallState::ErrorReady;
        frame.terminal_error = Some(err);
        reply.used = true;
        wake_set.push_caller(frame.caller_task_id);
        if let Some(custody) = reply.scheduling_context.take() {
            wake_set.push_scheduling_context(KernelReplyHandle::from_raw(reply_id), custody);
        }
    });
    ENDPOINT_MESSAGES.visit_mut(|message_id, message| {
        if !message.published
            || message.response.is_some()
            || !owned_contains(message.endpoint_id)
        {
            return;
        }
        let failed = REPLIES.with_mut(message.reply_id, |reply| {
            if reply.message_id != message_id || reply.consumed {
                return (false, None);
            }
            reply.used = true;
            message.response = Some(EndpointResponse::Error(err));
            reply_publication::note_reply_published(message.reply_id);
            (true, reply.scheduling_context.take())
        });
        if let Some((true, scheduling_context)) = failed {
            wake_set.push_caller(message.caller_task_id);
            if let Some(custody) = scheduling_context {
                wake_set.push_scheduling_context(
                    KernelReplyHandle::from_raw(message.reply_id),
                    custody,
                );
            }
        }
    });
    wake_set
}

fn endpoint_cancel_binding_matches(
    message: &EndpointMessageObject,
    reply_id: u64,
    caller_task_id: u64,
) -> bool {
    if message.caller_task_id != caller_task_id || message.reply_id != reply_id {
        return false;
    }
    true
}

fn mark_endpoint_call_consumed(message_id: u64, reply_id: u64, caller_task_id: u64) -> bool {
    ENDPOINT_MESSAGES
        .with_mut(message_id, |message| {
            if !endpoint_cancel_binding_matches(message, reply_id, caller_task_id) {
                return false;
            }
            #[cfg(test)]
            let reply_lookup_id = if *ENDPOINT_CANCEL_REPLY_BINDING_FAULT.lock() {
                message.reply_id
            } else {
                reply_id
            };
            #[cfg(not(test))]
            let reply_lookup_id = reply_id;
            REPLIES
                .with_mut(reply_lookup_id, |reply| {
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
        // A fallible reservation: the page count is bounded but a 256 MiB
        // region still needs a 512 KiB vector, and an allocation failure must
        // refuse the request instead of aborting the kernel.
        let mut frames = Vec::new();
        frames.try_reserve_exact(page_count).ok()?;
        for page_index in 0..page_count {
            frames.push(phys_start + page_index as u64 * PAGE_SIZE as u64);
        }
        Some(frames)
    }
}

#[cfg(test)]
mod tests;
