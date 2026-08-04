//! Opaque IPC transfer registry and its receiver authority.
//!
//! - **Owner:** this module owns pending transfer objects, their opaque
//!   tickets, and the binding that decides who may claim them.
//! - **Boundary:** tickets, service and channel identities, stream ranges, and
//!   open-description tokens all arrive untrusted and are validated here.
//! - **Lifecycle:** register, mint tickets, commit to a queue, bind a receiver,
//!   claim exactly once, then drop on cancellation, service-epoch revoke, or
//!   task exit.
//! - **Concurrency:** one tracked lock serializes the registry; no cross
//!   subsystem lock is taken while it is held.
//! - **Failure:** any identity, range, or ownership mismatch is a binding
//!   mismatch rather than a partial claim.
//! - **Forbidden:** no claim by an open description other than the one a batch
//!   is bound to, no second claim, and no reverse dependency on compat.
//! - **Evidence:** `ipc-transfer-authority`.
//!
//! A shared AF_UNIX open description has no single receiver process at send
//! time, so the intended receiver alone would leave a batch claimable by any
//! process holding that description. The first claim therefore also binds the
//! exact open description it arrived on, and no other description in the
//! receiver set can take the batch afterwards.

use super::*;

// The pending transfer registry belongs to the process/handle substrate rather
// than compat. Endpoint cancellation and task exit run below compat, so they
// must be able to discard opaque transfer descriptors without a reverse
// kernel_ps -> kernel_compat dependency.

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum IpcTransferRegistryError {
    Exhausted,
    BindingMismatch,
    InvalidDescriptor,
    InvalidState,
    StaleDescriptor,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum IpcTransferState {
    Vacant,
    Registered,
    Exported,
    Enqueued,
}

struct IpcTransferSlot {
    transfer_id: u64,
    nonce: u64,
    batch_generation: u64,
    context: Option<TransferContext>,
    state: IpcTransferState,
    entry: Option<TransferredHandleEntry>,
}

impl IpcTransferSlot {
    const fn empty() -> Self {
        Self {
            transfer_id: 0,
            nonce: 0,
            batch_generation: 0,
            context: None,
            state: IpcTransferState::Vacant,
            entry: None,
        }
    }
}

struct IpcTransferRegistry {
    slots: [IpcTransferSlot; MAX_PENDING_IPC_TRANSFER_OBJECTS],
    len: usize,
}

impl IpcTransferRegistry {
    const fn new() -> Self {
        Self {
            slots: [const { IpcTransferSlot::empty() }; MAX_PENDING_IPC_TRANSFER_OBJECTS],
            len: 0,
        }
    }

    fn len(&self) -> usize {
        self.len
    }

    fn contains(&self, transfer_id: u64) -> bool {
        self.slots
            .iter()
            .any(|slot| slot.transfer_id == transfer_id && slot.entry.is_some())
    }

    fn get(&self, transfer_id: u64) -> Option<&TransferredHandleEntry> {
        self.slots
            .iter()
            .find(|slot| slot.transfer_id == transfer_id)
            .and_then(|slot| slot.entry.as_ref())
    }

    fn insert(&mut self, transfer_id: u64, nonce: u64, entry: TransferredHandleEntry) -> bool {
        if transfer_id == 0 || nonce == 0 || self.contains(transfer_id) {
            return false;
        }
        let Some(slot) = self
            .slots
            .iter_mut()
            .find(|slot| slot.state == IpcTransferState::Vacant)
        else {
            return false;
        };
        slot.transfer_id = transfer_id;
        slot.nonce = nonce;
        slot.state = IpcTransferState::Registered;
        slot.entry = Some(entry);
        self.len += 1;
        true
    }

    fn remove(&mut self, transfer_id: u64) -> Option<TransferredHandleEntry> {
        let slot = self
            .slots
            .iter_mut()
            .find(|slot| slot.transfer_id == transfer_id)?;
        let entry = slot.entry.take()?;
        *slot = IpcTransferSlot::empty();
        self.len -= 1;
        Some(entry)
    }

    fn ticket_matches(&self, ticket: KernelTransferTicket) -> bool {
        self.slots.iter().any(|slot| {
            slot.entry.is_some()
                && slot.transfer_id == ticket.transfer_id()
                && slot.nonce == ticket.nonce()
                && slot.batch_generation == ticket.batch_generation()
        })
    }
}

struct DeferredTransferDropQueue {
    entries: [Option<TransferredHandleEntry>; MAX_PENDING_IPC_TRANSFER_OBJECTS],
    len: usize,
}

impl DeferredTransferDropQueue {
    const fn new() -> Self {
        Self {
            entries: [const { None }; MAX_PENDING_IPC_TRANSFER_OBJECTS],
            len: 0,
        }
    }

    fn len(&self) -> usize {
        self.len
    }

    fn push(&mut self, entry: TransferredHandleEntry) {
        let slot = self
            .entries
            .get_mut(self.len)
            .expect("deferred transfer-drop admission invariant violated");
        *slot = Some(entry);
        self.len += 1;
    }

    fn take_prefix(&mut self, limit: usize, output: &mut Vec<TransferredHandleEntry>) {
        let count = limit.min(self.len);
        for index in 0..count {
            output.push(
                self.entries[index]
                    .take()
                    .expect("deferred transfer-drop queue contains a hole"),
            );
        }
        for index in count..self.len {
            self.entries[index - count] = self.entries[index].take();
        }
        self.len -= count;
    }
}

static IPC_TRANSFER_OBJECTS: TrackedSpinLock<
    IpcTransferRegistry,
    { LockClass::IpcTransferRegistry as u8 },
> = TrackedSpinLock::new(IpcTransferRegistry::new());
static IPC_DEFERRED_TRANSFER_DROPS: TrackedSpinLock<
    DeferredTransferDropQueue,
    { LockClass::IpcDeferredDrop as u8 },
> =
    // The queue shares the same admission ceiling as the live registry and owns
    // fixed storage, so boot and task-exit bursts cannot enter the allocator while
    // either registry lock is held. Static construction also avoids copying this
    // large fixed store through a first-use stack frame.
    TrackedSpinLock::new(DeferredTransferDropQueue::new());

static NEXT_IPC_TRANSFER_ID: AtomicU64 = AtomicU64::new(1);
static NEXT_IPC_TRANSFER_BATCH_GENERATION: AtomicU64 = AtomicU64::new(1);

pub fn register_ipc_transfer_entries(
    entries: Vec<TransferredHandleEntry>,
) -> Result<Vec<KernelTransferredHandle>, IpcTransferRegistryError> {
    // Allocation is not part of the registry transaction. Keeping it outside
    // both spin locks prevents heap contention from extending the global
    // transfer publication critical section.
    let mut inserted_ids = Vec::with_capacity(entries.len());
    let mut descriptors = Vec::with_capacity(entries.len());
    let mut rollback = Vec::with_capacity(entries.len());
    let mut nonces = Vec::with_capacity(entries.len());
    for _ in 0..entries.len() {
        nonces.push(fresh_ipc_transfer_nonce());
    }
    let mut objects = IPC_TRANSFER_OBJECTS.lock();
    let deferred = IPC_DEFERRED_TRANSFER_DROPS.lock();
    if objects
        .len()
        .saturating_add(deferred.len())
        .saturating_add(entries.len())
        > MAX_PENDING_IPC_TRANSFER_OBJECTS
    {
        return Err(IpcTransferRegistryError::Exhausted);
    }
    drop(deferred);

    for (entry, nonce) in entries.into_iter().zip(nonces) {
        let Some(transfer_id) = allocate_ipc_transfer_id(&objects) else {
            for transfer_id in inserted_ids {
                if let Some(entry) = objects.remove(transfer_id) {
                    rollback.push(entry);
                }
            }
            drop(objects);
            drop(rollback);
            return Err(IpcTransferRegistryError::Exhausted);
        };
        let Some(descriptor) = entry.ipc_descriptor(transfer_id) else {
            for transfer_id in inserted_ids {
                if let Some(entry) = objects.remove(transfer_id) {
                    rollback.push(entry);
                }
            }
            drop(objects);
            drop(rollback);
            return Err(IpcTransferRegistryError::InvalidDescriptor);
        };
        assert!(
            objects.insert(transfer_id, nonce, entry),
            "validated IPC transfer slot disappeared before publication"
        );
        inserted_ids.push(transfer_id);
        descriptors.push(descriptor);
    }
    Ok(descriptors)
}

/// Convert kernel-only typed descriptors into opaque integer tickets for one
/// service byte protocol. No enum layout or padding crosses the boundary.
pub fn bind_ipc_transfer_tickets(
    descriptors: &[KernelTransferredHandle],
    context: TransferContext,
) -> Result<Vec<KernelTransferTicket>, IpcTransferRegistryError> {
    if descriptors.is_empty()
        || context.source.pid == 0
        || context.source.generation == 0
        || context.service.service_id == 0
        || context.service.epoch == 0
        || context.channel.channel_id == 0
        || context.channel.generation == 0
        || context.channel.receiver_side == 0
        || context.stream_start >= context.stream_end
    {
        return Err(IpcTransferRegistryError::BindingMismatch);
    }
    let mut tickets = Vec::with_capacity(descriptors.len());
    let batch_generation = NEXT_IPC_TRANSFER_BATCH_GENERATION
        .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |current| {
            (current != 0).then(|| current.checked_add(1)).flatten()
        })
        .map_err(|_| IpcTransferRegistryError::Exhausted)?;
    let mut objects = IPC_TRANSFER_OBJECTS.lock();
    for (index, descriptor) in descriptors.iter().enumerate() {
        if descriptors[..index]
            .iter()
            .any(|prior| prior.transfer_id() == descriptor.transfer_id())
        {
            return Err(IpcTransferRegistryError::InvalidDescriptor);
        }
        let Some(slot) = objects
            .slots
            .iter()
            .find(|slot| slot.transfer_id == descriptor.transfer_id())
        else {
            return Err(IpcTransferRegistryError::StaleDescriptor);
        };
        let Some(entry) = slot.entry.as_ref() else {
            return Err(IpcTransferRegistryError::StaleDescriptor);
        };
        if entry.ipc_descriptor(descriptor.transfer_id()) != Some(*descriptor) {
            return Err(IpcTransferRegistryError::InvalidDescriptor);
        }
        if slot.state != IpcTransferState::Registered {
            return Err(IpcTransferRegistryError::InvalidState);
        }
    }
    for descriptor in descriptors {
        let slot = objects
            .slots
            .iter_mut()
            .find(|slot| slot.transfer_id == descriptor.transfer_id())
            .expect("validated transfer registry entry disappeared before binding");
        slot.batch_generation = batch_generation;
        slot.context = Some(context);
        slot.state = IpcTransferState::Exported;
        tickets.push(
            KernelTransferTicket::new(slot.transfer_id, slot.nonce, batch_generation)
                .expect("validated transfer batch produced an invalid ticket"),
        );
    }
    Ok(tickets)
}

pub fn commit_ipc_transfer_enqueue(
    tickets: &[KernelTransferTicket],
) -> Result<(), IpcTransferRegistryError> {
    let mut objects = IPC_TRANSFER_OBJECTS.lock();
    validate_ticket_batch(&objects, tickets, IpcTransferState::Exported)?;
    for ticket in tickets {
        let slot = objects
            .slots
            .iter_mut()
            .find(|slot| slot.transfer_id == ticket.transfer_id())
            .expect("validated transfer ticket disappeared before enqueue commit");
        slot.state = IpcTransferState::Enqueued;
    }
    Ok(())
}

/// Bind an enqueued Unix-stream transfer batch to the concrete process that
/// actually received its control record. The caller reaches this boundary
/// only after the exact service/channel/side/stream position has been checked;
/// a guessed ticket or a different socket cannot acquire receiver authority.
pub fn bind_ipc_transfer_receiver_by_tickets(
    tickets: &[KernelTransferTicket],
    receiver: ProcessIdentity,
    service: ServiceIdentity,
    channel: ChannelIdentity,
    stream_pos: u64,
    receiver_open_description: u64,
) -> Result<(), IpcTransferRegistryError> {
    let mut objects = IPC_TRANSFER_OBJECTS.lock();
    validate_ticket_batch(&objects, tickets, IpcTransferState::Enqueued)?;
    let context = objects
        .slots
        .iter()
        .find(|slot| slot.transfer_id == tickets[0].transfer_id())
        .and_then(|slot| slot.context)
        .ok_or(IpcTransferRegistryError::InvalidState)?;
    if context.service != service
        || context.channel != channel
        || context.stream_start != stream_pos
        || context.stream_end <= context.stream_start
        || context
            .intended_receiver
            .is_some_and(|intended| intended != receiver)
        // A batch already bound to one open description cannot be rebound to
        // another, so dup, fork, or close within the receiver set cannot move
        // it to a different description.
        || (context.receiver_open_description != 0
            && context.receiver_open_description != receiver_open_description)
        || receiver_open_description == 0
    {
        return Err(IpcTransferRegistryError::BindingMismatch);
    }
    for ticket in tickets {
        let slot = objects
            .slots
            .iter_mut()
            .find(|slot| slot.transfer_id == ticket.transfer_id())
            .expect("validated transfer ticket disappeared during receiver bind");
        let mut bound = slot
            .context
            .expect("validated transfer ticket lost its binding context");
        bound.intended_receiver = Some(receiver);
        bound.receiver_open_description = receiver_open_description;
        slot.context = Some(bound);
    }
    Ok(())
}

pub fn claim_ipc_transfer_entries_by_tickets(
    tickets: &[KernelTransferTicket],
    receiver: ProcessIdentity,
    service: ServiceIdentity,
    channel: ChannelIdentity,
    stream_pos: u64,
    receiver_open_description: u64,
) -> Result<Vec<TransferredHandleEntry>, IpcTransferRegistryError> {
    let mut entries = Vec::with_capacity(tickets.len());
    let mut objects = IPC_TRANSFER_OBJECTS.lock();
    validate_ticket_batch(&objects, tickets, IpcTransferState::Enqueued)?;
    let context = objects
        .slots
        .iter()
        .find(|slot| slot.transfer_id == tickets[0].transfer_id())
        .and_then(|slot| slot.context)
        .ok_or(IpcTransferRegistryError::InvalidState)?;
    if context.service != service
        || context.channel != channel
        || context.stream_start != stream_pos
        || context.stream_end <= context.stream_start
        || context.intended_receiver != Some(receiver)
        // Zero is never an authority. A claim must present the exact open
        // description it received on, and once a batch is bound to one, no
        // other description in the receiver set can take it.
        || receiver_open_description == 0
        || (context.receiver_open_description != 0
            && context.receiver_open_description != receiver_open_description)
    {
        return Err(IpcTransferRegistryError::BindingMismatch);
    }
    for ticket in tickets {
        entries.push(
            objects
                .remove(ticket.transfer_id())
                .expect("validated transfer ticket disappeared while claiming"),
        );
    }
    Ok(entries)
}

fn validate_ticket_batch(
    objects: &IpcTransferRegistry,
    tickets: &[KernelTransferTicket],
    expected_state: IpcTransferState,
) -> Result<(), IpcTransferRegistryError> {
    let Some(first) = tickets.first() else {
        return Err(IpcTransferRegistryError::InvalidDescriptor);
    };
    for (index, ticket) in tickets.iter().enumerate() {
        if ticket.batch_generation() != first.batch_generation()
            || tickets[..index]
                .iter()
                .any(|prior| prior.transfer_id() == ticket.transfer_id())
        {
            return Err(IpcTransferRegistryError::InvalidDescriptor);
        }
        let Some(slot) = objects.slots.iter().find(|slot| {
            slot.transfer_id == ticket.transfer_id()
                && slot.nonce == ticket.nonce()
                && slot.batch_generation == ticket.batch_generation()
        }) else {
            return Err(IpcTransferRegistryError::StaleDescriptor);
        };
        if slot.state != expected_state || slot.context.is_none() || slot.entry.is_none() {
            return Err(IpcTransferRegistryError::InvalidState);
        }
    }
    Ok(())
}

pub fn drop_ipc_transfer_tickets(tickets: &[KernelTransferTicket]) {
    if tickets.is_empty() {
        return;
    }
    let mut objects = IPC_TRANSFER_OBJECTS.lock();
    let mut deferred = IPC_DEFERRED_TRANSFER_DROPS.lock();
    for ticket in tickets {
        if objects.ticket_matches(*ticket)
            && let Some(entry) = objects.remove(ticket.transfer_id())
        {
            deferred.push(entry);
        }
    }
}

pub fn drop_ipc_transfers_for_service_epoch(service_id: u64, epoch: u64) {
    if service_id == 0 || epoch == 0 {
        return;
    }
    let mut objects = IPC_TRANSFER_OBJECTS.lock();
    let mut deferred = IPC_DEFERRED_TRANSFER_DROPS.lock();
    for index in 0..objects.slots.len() {
        let matches = objects.slots[index].context.is_some_and(|context| {
            context.service.service_id == service_id && context.service.epoch == epoch
        });
        if matches {
            let transfer_id = objects.slots[index].transfer_id;
            if let Some(entry) = objects.remove(transfer_id) {
                deferred.push(entry);
            }
        }
    }
}

pub fn take_ipc_transfer_entries(
    descriptors: &[KernelTransferredHandle],
) -> Result<Vec<TransferredHandleEntry>, IpcTransferRegistryError> {
    let mut entries = Vec::with_capacity(descriptors.len());
    let mut objects = IPC_TRANSFER_OBJECTS.lock();
    for (index, descriptor) in descriptors.iter().enumerate() {
        if descriptors[..index]
            .iter()
            .any(|prior| prior.transfer_id() == descriptor.transfer_id())
        {
            return Err(IpcTransferRegistryError::InvalidDescriptor);
        }
        let Some(entry) = objects.get(descriptor.transfer_id()) else {
            return Err(IpcTransferRegistryError::StaleDescriptor);
        };
        if entry.ipc_descriptor(descriptor.transfer_id()) != Some(*descriptor) {
            return Err(IpcTransferRegistryError::InvalidDescriptor);
        }
    }

    for descriptor in descriptors {
        let entry = objects
            .remove(descriptor.transfer_id())
            .expect("validated IPC transfer descriptor disappeared while taking");
        entries.push(entry);
    }
    Ok(entries)
}

pub fn drop_ipc_transfer_descriptors(descriptors: &[KernelTransferredHandle]) {
    if descriptors.is_empty() {
        return;
    }
    let mut objects = IPC_TRANSFER_OBJECTS.lock();
    let mut deferred = IPC_DEFERRED_TRANSFER_DROPS.lock();
    for descriptor in descriptors {
        let matches = objects.get(descriptor.transfer_id()).is_some_and(|entry| {
            entry.ipc_descriptor(descriptor.transfer_id()) == Some(*descriptor)
        });
        if matches && let Some(entry) = objects.remove(descriptor.transfer_id()) {
            // Registration accounts pending and deferred entries against one
            // shared ceiling, so moving ownership here cannot exceed it.
            deferred.push(entry);
        }
    }
}

pub fn take_deferred_ipc_transfer_drops(limit: usize) -> Vec<TransferredHandleEntry> {
    let mut taken = Vec::with_capacity(limit.min(MAX_PENDING_IPC_TRANSFER_OBJECTS));
    let mut deferred = IPC_DEFERRED_TRANSFER_DROPS.lock();
    deferred.take_prefix(limit, &mut taken);
    taken
}

fn allocate_ipc_transfer_id(objects: &IpcTransferRegistry) -> Option<u64> {
    for _ in 0..MAX_PENDING_IPC_TRANSFER_OBJECTS {
        let id = NEXT_IPC_TRANSFER_ID.fetch_add(1, Ordering::Relaxed);
        if id != 0 && !objects.contains(id) {
            return Some(id);
        }
    }
    None
}

#[cfg(test)]
mod transfer_registry_tests {
    /// Open-description token the receiving side presents in these tests.
    const RECEIVER_OPEN_DESCRIPTION: u64 = 0x5eed_0001;

    use super::*;

    #[test]
    fn authority_identity_exhaustion_fails_closed_before_wrap() {
        let counter = AtomicU64::new(1);
        assert_eq!(allocate_nonwrapping_identity(&counter), Some(1));
        let exhausted = AtomicU64::new(u64::MAX);
        assert_eq!(allocate_nonwrapping_identity(&exhausted), None);
        assert_eq!(exhausted.load(Ordering::Relaxed), u64::MAX);
    }
    use crate::io::device::{DeviceAccessKind, DeviceId};

    #[test]
    fn cancelled_transfer_moves_its_open_description_to_deferred_cleanup() {
        let token = u64::MAX - 701;
        let device =
            DeviceHandle::from_parts_with_token(DeviceId::Input, DeviceAccessKind::Evdev, token);
        let entry = HandleEntry::new(KernelHandle::Device(device), 0, linux_abi::O_RDONLY);
        let transferred = TransferredHandleEntry::from_entry(entry).expect("transferable input");
        let descriptors =
            register_ipc_transfer_entries(alloc::vec![transferred]).expect("register transfer");
        drop_ipc_transfer_descriptors(&descriptors);
        let dropped = take_deferred_ipc_transfer_drops(1);
        assert_eq!(dropped.len(), 1);
        assert_eq!(dropped[0].entry().handle().device_handle(), Some(device));
    }

    #[test]
    fn opaque_transfer_ticket_is_exact_one_shot_and_nonce_bound() {
        let token = u64::MAX - 702;
        let device =
            DeviceHandle::from_parts_with_token(DeviceId::Input, DeviceAccessKind::Evdev, token);
        let entry = HandleEntry::new(KernelHandle::Device(device), 0, linux_abi::O_RDONLY);
        let transferred = TransferredHandleEntry::from_entry(entry).expect("transferable input");
        let descriptors =
            register_ipc_transfer_entries(alloc::vec![transferred]).expect("register transfer");
        let source = ProcessIdentity {
            pid: 7,
            generation: 3,
        };
        let receiver = ProcessIdentity {
            pid: 11,
            generation: 5,
        };
        let service = ServiceIdentity {
            service_id: 4,
            epoch: 9,
        };
        let channel = ChannelIdentity {
            channel_id: 13,
            generation: 13,
            receiver_side: 2,
        };
        let context = TransferContext {
            source,
            service,
            channel,
            stream_start: 0,
            stream_end: 1,
            intended_receiver: Some(receiver),
            receiver_open_description: 0,
        };
        let tickets =
            bind_ipc_transfer_tickets(&descriptors, context).expect("mint transfer ticket");
        assert_eq!(tickets.len(), 1);
        commit_ipc_transfer_enqueue(&tickets).expect("publish queue ownership");

        for rejected in [
            claim_ipc_transfer_entries_by_tickets(
                &tickets,
                ProcessIdentity {
                    pid: receiver.pid,
                    generation: receiver.generation + 1,
                },
                service,
                channel,
                context.stream_start,
                RECEIVER_OPEN_DESCRIPTION,
            ),
            claim_ipc_transfer_entries_by_tickets(
                &tickets,
                receiver,
                ServiceIdentity {
                    service_id: service.service_id,
                    epoch: service.epoch + 1,
                },
                channel,
                context.stream_start,
                RECEIVER_OPEN_DESCRIPTION,
            ),
            claim_ipc_transfer_entries_by_tickets(
                &tickets,
                receiver,
                service,
                ChannelIdentity {
                    receiver_side: channel.receiver_side + 1,
                    ..channel
                },
                context.stream_start,
                RECEIVER_OPEN_DESCRIPTION,
            ),
            claim_ipc_transfer_entries_by_tickets(
                &tickets,
                receiver,
                service,
                channel,
                context.stream_start + 1,
                RECEIVER_OPEN_DESCRIPTION,
            ),
        ] {
            assert!(matches!(
                rejected,
                Err(IpcTransferRegistryError::BindingMismatch)
            ));
        }

        let forged_nonce = tickets[0].nonce().wrapping_add(1).max(1);
        let forged = KernelTransferTicket::new(
            tickets[0].transfer_id(),
            forged_nonce,
            tickets[0].batch_generation(),
        )
        .expect("nonzero forged ticket shape");
        assert!(matches!(
            claim_ipc_transfer_entries_by_tickets(
                &[forged],
                receiver,
                service,
                channel,
                context.stream_start,
                RECEIVER_OPEN_DESCRIPTION,
            ),
            Err(IpcTransferRegistryError::StaleDescriptor)
        ));

        let entries = claim_ipc_transfer_entries_by_tickets(
            &tickets,
            receiver,
            service,
            channel,
            context.stream_start,
            RECEIVER_OPEN_DESCRIPTION,
        )
        .expect("take exact ticket");
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].entry().handle().device_handle(), Some(device));
        assert!(matches!(
            claim_ipc_transfer_entries_by_tickets(
                &tickets,
                receiver,
                service,
                channel,
                context.stream_start,
                RECEIVER_OPEN_DESCRIPTION,
            ),
            Err(IpcTransferRegistryError::StaleDescriptor)
        ));
    }

    #[test]
    fn unbound_stream_transfer_requires_exact_receive_time_process_binding() {
        let token = u64::MAX - 703;
        let device =
            DeviceHandle::from_parts_with_token(DeviceId::Input, DeviceAccessKind::Evdev, token);
        let entry = HandleEntry::new(KernelHandle::Device(device), 0, linux_abi::O_RDONLY);
        let transferred = TransferredHandleEntry::from_entry(entry).expect("transferable input");
        let descriptors =
            register_ipc_transfer_entries(alloc::vec![transferred]).expect("register transfer");
        let receiver = ProcessIdentity {
            pid: 21,
            generation: 8,
        };
        let service = ServiceIdentity {
            service_id: 4,
            epoch: 12,
        };
        let channel = ChannelIdentity {
            channel_id: 19,
            generation: 19,
            receiver_side: 2,
        };
        let context = TransferContext {
            source: ProcessIdentity {
                pid: 17,
                generation: 6,
            },
            service,
            channel,
            stream_start: 9,
            stream_end: 10,
            intended_receiver: None,
            receiver_open_description: 0,
        };
        let tickets = bind_ipc_transfer_tickets(&descriptors, context).expect("mint tickets");
        commit_ipc_transfer_enqueue(&tickets).expect("enqueue tickets");

        assert!(matches!(
            claim_ipc_transfer_entries_by_tickets(
                &tickets,
                receiver,
                service,
                channel,
                context.stream_start,
                RECEIVER_OPEN_DESCRIPTION,
            ),
            Err(IpcTransferRegistryError::BindingMismatch)
        ));
        assert!(matches!(
            bind_ipc_transfer_receiver_by_tickets(
                &tickets,
                receiver,
                service,
                ChannelIdentity {
                    receiver_side: 1,
                    ..channel
                },
                context.stream_start,
                RECEIVER_OPEN_DESCRIPTION,
            ),
            Err(IpcTransferRegistryError::BindingMismatch)
        ));
        bind_ipc_transfer_receiver_by_tickets(
            &tickets,
            receiver,
            service,
            channel,
            context.stream_start,
            RECEIVER_OPEN_DESCRIPTION,
        )
        .expect("bind exact receiving process");
        assert!(matches!(
            claim_ipc_transfer_entries_by_tickets(
                &tickets,
                ProcessIdentity {
                    generation: receiver.generation + 1,
                    ..receiver
                },
                service,
                channel,
                context.stream_start,
                RECEIVER_OPEN_DESCRIPTION,
            ),
            Err(IpcTransferRegistryError::BindingMismatch)
        ));
        let entries = claim_ipc_transfer_entries_by_tickets(
            &tickets,
            receiver,
            service,
            channel,
            context.stream_start,
            RECEIVER_OPEN_DESCRIPTION,
        )
        .expect("claim exact receive-time binding");
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].entry().handle().device_handle(), Some(device));
    }

    #[test]
    fn console_token_liveness_tracks_descriptor_references_not_snapshots() {
        let handle = ConsoleHandle::new(ConsoleStreamKind::Input);
        let token = handle.token_id();
        assert!(ConsoleHandle::token_is_live(token));

        handle.acquire_descriptor_reference();
        let duplicated = handle.clone();
        assert!(!handle.release_descriptor_reference());
        drop(handle);
        assert!(ConsoleHandle::token_is_live(token));
        assert_eq!(
            ConsoleHandle::stream_for_token(token),
            Some(ConsoleStreamKind::Input)
        );

        assert!(duplicated.release_descriptor_reference());
        assert!(!duplicated.try_acquire_descriptor_reference());
        drop(duplicated);
        assert!(!ConsoleHandle::token_is_live(token));
        assert_eq!(ConsoleHandle::stream_for_token(token), None);
    }
}
