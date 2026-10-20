//! Bounded reply-edge custody outside the scheduler task catalog.
//!
//! - **Owner:** this ledger owns one pending or live IPC donation edge per
//!   donor task; original participant loans independently qualify new Current entries.
//! - **Lifecycle:** reserve -> attach/bind -> release. A bound edge increments
//!   exactly one receiver-slot inheritance count and terminal release removes
//!   it before forgetting the reply identity.
//! - **Concurrency:** `SchedulerDonation` serializes edge mutation. Dispatch
//!   reads only an Acquire receiver-slot counter and never scans this ledger.
//! - **Failure:** duplicate reservation, stale reply, capacity exhaustion, and
//!   unmatched terminal release fail closed without changing a receiver class.
//!   Owned publication revalidates original source custody under this same leaf;
//!   candidate snapshots are never execution/interval grants.
//! - **Evidence:** `ipc-priority-inheritance` and the focused ledger tests.

use core::sync::atomic::{AtomicU8, AtomicU16, AtomicU64, Ordering};

use nucleus_core::util::lockdep::LockClass;
use crate::multitask::irq_leaf::IrqLeaf;

use super::{
    MAX_TASK,
    ipc_donation::{DonationNamespace, IpcDonationTarget, IpcPriorityDonation},
};

#[derive(Clone, Copy)]
struct LedgerEntry {
    donation: IpcPriorityDonation,
    receiver_slot: Option<usize>,
    funding: Option<FundingBasis>,
}

/// A candidate for original participant capture, never an execution grant.
/// Commit revalidates the exact originating reply edge under the ledger leaf.
#[derive(Clone, Copy, Eq, PartialEq)]
pub(super) struct FundingBasis {
    donor_slot: usize,
    donor_id: u64,
    root_slot: usize,
    root_id: u64,
    source: Option<FundingSource>,
}
#[derive(Clone, Copy, Eq, PartialEq)]
struct FundingSource {
    reply: u64,
    namespace: DonationNamespace,
    donor_id: u64,
    receiver_id: u64,
    receiver_slot: usize,
    root_id: u64,
    root_slot: usize,
}
impl FundingBasis {
    pub(super) fn root(&self) -> (usize, u64) { (self.root_slot, self.root_id) }
}

struct DonationLedger {
    entries: [Option<LedgerEntry>; MAX_TASK],
    len: usize,
}

impl DonationLedger {
    const fn new() -> Self {
        Self {
            entries: [None; MAX_TASK],
            len: 0,
        }
    }

    fn insert(&mut self, entry: LedgerEntry) -> bool {
        if self.len == MAX_TASK {
            return false;
        }
        self.entries[self.len] = Some(entry);
        self.len += 1;
        true
    }

    fn remove(&mut self, index: usize) -> LedgerEntry {
        assert!(index < self.len, "scheduler donation index exceeds ledger");
        self.len -= 1;
        let removed = self.entries[index]
            .take()
            .expect("scheduler donation ledger prefix contains an empty entry");
        if index != self.len {
            self.entries[index] = self.entries[self.len].take();
        }
        removed
    }

    fn find_reservation(&self, donor_task_id: u64) -> Option<usize> {
        self.entries[..self.len].iter().position(|entry| {
            entry.is_some_and(|entry| {
                entry.donation.reply == 0 && entry.donation.donor_task_id == donor_task_id
            })
        })
    }

    fn find_reply(&self, reply: u64, namespace: DonationNamespace) -> Option<usize> {
        self.entries[..self.len].iter().position(|entry| {
            entry.is_some_and(|entry| {
                entry.donation.reply == reply && entry.donation.namespace == namespace
            })
        })
    }
}

static DONATION_LEDGER: IrqLeaf<DonationLedger, { LockClass::SchedulerDonation as u8 }> =
    IrqLeaf::new(DonationLedger::new());
static INHERITED_SYSTEM: [AtomicU8; MAX_TASK] = [const { AtomicU8::new(0) }; MAX_TASK];
static CLASS_PUBLICATION_SEQUENCE: AtomicU64 = AtomicU64::new(0);
const NO_BORROWED_CONTEXT: u16 = u16::MAX;
static BORROWED_CONTEXT_OWNER: [AtomicU16; MAX_TASK] =
    [const { AtomicU16::new(NO_BORROWED_CONTEXT) }; MAX_TASK];
static BORROWED_CONTEXT_NAMESPACE: [AtomicU8; MAX_TASK] = [const { AtomicU8::new(0) }; MAX_TASK];
static BORROWED_CONTEXT_REPLY: [AtomicU64; MAX_TASK] = [const { AtomicU64::new(0) }; MAX_TASK];

const fn namespace_tag(namespace: DonationNamespace) -> u8 {
    match namespace {
        DonationNamespace::IpcReply => 1,
        DonationNamespace::PagerFault => 2,
    }
}

fn bind_entry_to_receiver(entry: &mut LedgerEntry, receiver_task_id: u64, receiver_slot: usize) {
    entry.donation.target = IpcDonationTarget::BoundWorker(receiver_task_id);
    entry.donation.custody_active = true;
    entry.receiver_slot = Some(receiver_slot);
    publish_charge_token(
        receiver_slot,
        entry.donation.context_owner_slot,
        entry.donation.reply,
        entry.donation.namespace,
    );
    if entry.donation.priority_donated {
        increment_receiver(receiver_slot);
    }
}

pub(super) fn reset() {
    let mut ledger = DONATION_LEDGER.lock();
    CLASS_PUBLICATION_SEQUENCE.fetch_add(1, Ordering::AcqRel);
    *ledger = DonationLedger::new();
    for inherited in &INHERITED_SYSTEM {
        inherited.store(0, Ordering::Release);
    }
    for owner in &BORROWED_CONTEXT_OWNER {
        owner.store(NO_BORROWED_CONTEXT, Ordering::Release);
    }
    for namespace in &BORROWED_CONTEXT_NAMESPACE {
        namespace.store(0, Ordering::Release);
    }
    for reply in &BORROWED_CONTEXT_REPLY {
        reply.store(0, Ordering::Release);
    }
    CLASS_PUBLICATION_SEQUENCE.fetch_add(1, Ordering::AcqRel);
}

fn source_live(ledger: &DonationLedger, source: FundingSource) -> bool {
    ledger.find_reply(source.reply, source.namespace).is_some_and(|index| {
        let entry = ledger.entries[index].expect("original funding source vanished");
        entry.donation.custody_active
            && entry.donation.donor_task_id == source.donor_id
            && entry.donation.context_owner_task_id == source.root_id
            && entry.donation.context_owner_slot == source.root_slot
            && entry.receiver_slot == Some(source.receiver_slot)
            && entry.donation.target == IpcDonationTarget::BoundWorker(source.receiver_id)
    })
}
fn basis_locked(ledger: &DonationLedger, slot: usize, id: u64) -> Option<FundingBasis> {
    let Some((root_slot, reply)) = borrowed_context_charge_token(slot) else {
        return Some(FundingBasis { donor_slot: slot, donor_id: id,
            root_slot: slot, root_id: id, source: None });
    };
    let tag = BORROWED_CONTEXT_NAMESPACE.get(slot)?.load(Ordering::Acquire);
    let namespace = match tag { 1 => DonationNamespace::IpcReply,
        2 => DonationNamespace::PagerFault, _ => return None };
    let entry = ledger.entries[ledger.find_reply(reply, namespace)?]?;
    let source = FundingSource { reply, namespace,
        donor_id: entry.donation.donor_task_id, receiver_id: id, receiver_slot: slot,
        root_slot, root_id: entry.donation.context_owner_task_id };
    source_live(ledger, source).then_some(FundingBasis { donor_slot: slot,
        donor_id: id, root_slot, root_id: source.root_id, source: Some(source) })
}
pub(super) fn funding_basis(slot: usize, id: u64) -> Option<FundingBasis> {
    basis_locked(&DONATION_LEDGER.lock(), slot, id)
}
fn basis_live(ledger: &DonationLedger, basis: &FundingBasis) -> bool {
    match basis.source {
        Some(source) => source_live(ledger, source),
        None => basis.root_slot == basis.donor_slot && basis.root_id == basis.donor_id,
    }
}
fn current_basis_live(ledger: &DonationLedger, basis: &FundingBasis) -> bool {
    basis_live(ledger, basis)
        && basis_locked(ledger, basis.donor_slot, basis.donor_id).as_ref() == Some(basis)
}
pub(super) fn reserve_owned(loans: &super::donation_publication::Loans,
    priority: bool) -> bool {
    let basis = loans.basis();
    let mut ledger = DONATION_LEDGER.lock();
    if !current_basis_live(&ledger, basis) { return false; }
    reserve_locked(&mut ledger, basis.donor_id, basis.root_id, basis.root_slot,
        priority, Some(*basis))
}


fn reserve_locked(
    ledger: &mut DonationLedger,
    donor_task_id: u64,
    context_owner_task_id: u64,
    context_owner_slot: usize,
    priority_donated: bool,
    funding: Option<FundingBasis>,
) -> bool {
    if context_owner_slot >= MAX_TASK {
        return false;
    }
    if ledger.find_reservation(donor_task_id).is_some() {
        return false;
    }
    ledger.insert(LedgerEntry {
        donation: IpcPriorityDonation {
            reply: 0,
            namespace: DonationNamespace::IpcReply,
            donor_task_id,
            context_owner_task_id,
            context_owner_slot,
            priority_donated,
            custody_active: false,
            target: IpcDonationTarget::AwaitingReceiver,
        },
        receiver_slot: None,
        funding,
    })
}

pub(super) fn cancel_reservation(donor_task_id: u64) -> bool {
    let mut ledger = DONATION_LEDGER.lock();
    let Some(index) = ledger.find_reservation(donor_task_id) else {
        return false;
    };
    let removed = ledger.remove(index);
    assert!(
        removed.receiver_slot.is_none(),
        "unbound donation carried a receiver"
    );
    true
}

/// Exact original queued-reply custody, not permission to bind any worker.
/// A stale receiver waiter may be ignored only while THIS reply still awaits
/// a receiver and is funded by this retained original donor's live basis.
pub(super) fn retains_awaiting_reply(
    donor: &super::super::current_identity::execution::BindingPin, reply: u64,
) -> bool {
    if reply == 0 { return false; }
    let ledger = DONATION_LEDGER.lock();
    let Some(index) = ledger.find_reply(reply, DonationNamespace::IpcReply) else { return false; };
    let entry = ledger.entries[index].as_ref().unwrap();
    entry.donation.donor_task_id == donor.id()
        && entry.receiver_slot.is_none() && !entry.donation.custody_active
        && matches!(entry.donation.target, IpcDonationTarget::AwaitingReceiver)
        && entry.funding.as_ref().is_some_and(|basis| basis.donor_slot == donor.slot()
            && basis.donor_id == donor.id() && current_basis_live(&ledger, basis))
}

/// Observation for the original sender's reply wait only. An actual receiver
/// may already have bound this published reply on another CPU. This confirms
/// that exact admission, never rebinds/wakes the stale waiter or lends a worker
/// frame/execution right. The caller retains its original Current/binding loan.
pub(super) fn reply_already_dispatched(
    donor: &super::super::current_identity::execution::BindingPin, reply: u64,
) -> bool {
    if reply == 0 { return false; }
    let ledger = DONATION_LEDGER.lock();
    let Some(index) = ledger.find_reply(reply, DonationNamespace::IpcReply) else { return false; };
    let entry = ledger.entries[index].as_ref().unwrap();
    entry.donation.donor_task_id == donor.id()
        && entry.donation.custody_active && entry.receiver_slot.is_some()
        && matches!(entry.donation.target, IpcDonationTarget::BoundWorker(_))
        && entry.funding.as_ref().is_some_and(|basis| basis.donor_slot == donor.slot()
            && basis.donor_id == donor.id() && current_basis_live(&ledger, basis))
}

pub(super) fn attach(reply: u64, namespace: DonationNamespace, donor_task_id: u64) -> bool {
    if reply == 0 {
        return false;
    }
    let mut ledger = DONATION_LEDGER.lock();
    if let Some(index) = ledger.find_reply(reply, namespace) {
        // A running receiver may bind the published reply before the sender
        // attaches its reservation. Confirm the exact donor edge instead of
        // treating that legal publication race as lost custody.
        return ledger.entries[index].is_some_and(|entry| {
            entry.donation.donor_task_id == donor_task_id
                && (entry.receiver_slot.is_none() || entry.donation.custody_active)
        });
    }
    let Some(index) = ledger.find_reservation(donor_task_id) else {
        return false;
    };
    ledger.entries[index]
        .as_mut()
        .expect("scheduler donation reservation disappeared")
        .donation
        .reply = reply;
    true
}

fn bind_basis_locked(ledger: &DonationLedger, reply: u64, namespace: DonationNamespace,
    slot: usize, id: u64) -> Result<Option<FundingBasis>, ()> {
    let Some(index) = ledger.find_reply(reply, namespace).or_else(|| ledger.find_reservation(id))
    else { return Ok(None); };
    let entry = ledger.entries[index].ok_or(())?;
    if entry.donation.donor_task_id != id { return Err(()); }
    let basis = match entry.funding {
        Some(basis) => basis,
        None if entry.donation.context_owner_task_id == id
            && entry.donation.context_owner_slot == slot => FundingBasis {
                donor_slot: slot, donor_id: id, root_slot: slot, root_id: id, source: None },
        None => return Err(()),
    };
    if basis.donor_slot != slot || basis.donor_id != id { return Err(()); }
    Ok(Some(basis))
}
pub(super) fn bind_basis(reply: u64, namespace: DonationNamespace,
    slot: usize, id: u64) -> Result<Option<FundingBasis>, ()> {
    bind_basis_locked(&DONATION_LEDGER.lock(), reply, namespace, slot, id)
}
pub(super) fn bind_owned(loans: &super::donation_publication::ReceiveLoans,
    reply: u64, namespace: DonationNamespace) -> bool {
    let basis = loans.donor.basis();
    let mut ledger = DONATION_LEDGER.lock();
    if !basis_live(&ledger, basis)
        || bind_basis_locked(&ledger, reply, namespace, basis.donor_slot, basis.donor_id)
            != Ok(Some(*basis))
    { return false; }
    bind_reserved_locked(&mut ledger, reply, namespace, basis.donor_id,
        loans.receiver.id(), loans.receiver.slot())
}
/// A successful IPC_RECV, unlike a sender's wake candidate, identifies the
/// actual consumer. Move ONLY the retained reservation's original context and
/// priority to that worker; a later sender hint remains idempotent and cannot
/// move it back. The original donor/root/receiver loans enclose this leaf.
pub(super) fn bind_received_owned(loans: &super::donation_publication::ReceiveLoans,
    reply: u64, namespace: DonationNamespace) -> bool {
    let basis = loans.donor.basis();
    let mut ledger = DONATION_LEDGER.lock();
    if !basis_live(&ledger, basis)
        || bind_basis_locked(&ledger, reply, namespace, basis.donor_slot, basis.donor_id)
            != Ok(Some(*basis))
    { return false; }
    if let Some(index) = ledger.find_reply(reply, namespace) {
        let previous = ledger.entries[index].expect("received reply lost its original reservation");
        if previous.donation.custody_active
            && (previous.receiver_slot != Some(loans.receiver.slot())
                || previous.donation.target != IpcDonationTarget::BoundWorker(loans.receiver.id()))
        {
            return upsert_locked(&mut ledger, reply, namespace, basis.donor_id,
                loans.receiver.id(), loans.receiver.slot(), basis.root_id, basis.root_slot,
                previous.donation.priority_donated, Some(*basis));
        }
    }
    bind_reserved_locked(&mut ledger, reply, namespace, basis.donor_id,
        loans.receiver.id(), loans.receiver.slot())
}

pub(super) fn upsert_owned(loans: &super::donation_publication::ReceiveLoans,
    reply: u64, namespace: DonationNamespace) -> bool {
    let basis = loans.donor.basis();
    let mut ledger = DONATION_LEDGER.lock();
    if !current_basis_live(&ledger, basis) { return false; }
    // A racing receive/attach owns the original reservation; a fresh upsert
    // must never overwrite that admitted worker or revive cancelled source custody.
    if namespace == DonationNamespace::IpcReply
        && (ledger.find_reply(reply, namespace).is_some()
            || ledger.find_reservation(basis.donor_id).is_some())
    { return false; }
    if let Some(index) = ledger.find_reply(reply, namespace) {
        if ledger.entries[index].is_none_or(|entry| entry.donation.donor_task_id != basis.donor_id)
        { return false; }
    }
    upsert_locked(&mut ledger, reply, namespace, basis.donor_id,
        loans.receiver.id(), loans.receiver.slot(), basis.root_id, basis.root_slot,
        loans.donor.system_class(), Some(*basis))
}


fn bind_reserved_locked(
    ledger: &mut DonationLedger,
    reply: u64,
    namespace: DonationNamespace,
    donor_task_id: u64,
    receiver_task_id: u64,
    receiver_slot: usize,
) -> bool {
    if reply == 0 || donor_task_id == receiver_task_id || receiver_slot >= MAX_TASK {
        return false;
    }
    if let Some(index) = ledger.find_reply(reply, namespace) {
        // The receiver can consume a just-published request on another CPU
        // before the sender commits its wake/handoff. That receive path binds
        // the reservation to the actual worker. Confirm that exact edge as
        // already admitted instead of looking only for the now-consumed
        // `reply == 0` reservation; rebinding it to the sender's stale waiter
        // would transfer custody to the wrong worker.
        let entry = ledger.entries[index]
            .as_mut()
            .expect("scheduler donation reply disappeared during bind");
        if entry.donation.donor_task_id != donor_task_id {
            return false;
        }
        if entry.donation.custody_active {
            return entry.receiver_slot.is_some();
        }
        bind_entry_to_receiver(entry, receiver_task_id, receiver_slot);
        return true;
    }
    let Some(index) = ledger.find_reservation(donor_task_id) else {
        return false;
    };
    let entry = ledger.entries[index]
        .as_mut()
        .expect("scheduler donation reservation disappeared");
    entry.donation.reply = reply;
    entry.donation.namespace = namespace;
    bind_entry_to_receiver(entry, receiver_task_id, receiver_slot);
    true
}


fn upsert_locked(
    ledger: &mut DonationLedger,
    reply: u64,
    namespace: DonationNamespace,
    donor_task_id: u64,
    receiver_task_id: u64,
    receiver_slot: usize,
    context_owner_task_id: u64,
    context_owner_slot: usize,
    priority_donated: bool,
    funding: Option<FundingBasis>,
) -> bool {
    if reply == 0
        || donor_task_id == receiver_task_id
        || receiver_slot >= MAX_TASK
        || context_owner_slot >= MAX_TASK
    {
        return false;
    }
    if let Some(index) = ledger.find_reply(reply, namespace) {
        let previous = ledger.entries[index].expect("scheduler donation reply disappeared");
        let entry = ledger.entries[index]
            .as_mut()
            .expect("scheduler donation reply disappeared");
        entry.receiver_slot = Some(receiver_slot);
        entry.funding = funding;
        entry.donation.donor_task_id = donor_task_id;
        entry.donation.context_owner_task_id = context_owner_task_id;
        entry.donation.context_owner_slot = context_owner_slot;
        entry.donation.priority_donated = priority_donated;
        entry.donation.custody_active = true;
        entry.donation.target = IpcDonationTarget::BoundWorker(receiver_task_id);
        if let Some(previous_slot) = previous.receiver_slot {
            restore_context_owner_after_release(
                &ledger,
                previous_slot,
                previous.donation.context_owner_slot,
                previous.donation.reply,
                previous.donation.namespace,
            );
            if previous.donation.priority_donated {
                decrement_receiver(previous_slot);
            }
        }
        publish_charge_token(receiver_slot, context_owner_slot, reply, namespace);
        if priority_donated {
            increment_receiver(receiver_slot);
        }
        return true;
    }
    if !ledger.insert(LedgerEntry {
        donation: IpcPriorityDonation {
            reply,
            namespace,
            donor_task_id,
            context_owner_task_id,
            context_owner_slot,
            priority_donated,
            custody_active: true,
            target: IpcDonationTarget::BoundWorker(receiver_task_id),
        },
        receiver_slot: Some(receiver_slot),
        funding,
    }) {
        return false;
    }
    publish_charge_token(receiver_slot, context_owner_slot, reply, namespace);
    if priority_donated {
        increment_receiver(receiver_slot);
    }
    true
}

pub(super) fn release_reply(reply: u64, namespace: DonationNamespace) -> bool {
    if reply == 0 {
        return false;
    }
    let mut ledger = DONATION_LEDGER.lock();
    let Some(index) = ledger.find_reply(reply, namespace) else {
        return false;
    };
    let removed = ledger.remove(index);
    let revoke_chain = removed.donation.donor_task_id == removed.donation.context_owner_task_id;
    let context_owner_slot = removed.donation.context_owner_slot;
    release_entry(&ledger, removed);
    if revoke_chain {
        let mut index = 0;
        while index < ledger.len {
            let entry = ledger.entries[index]
                .expect("scheduler donation ledger prefix contains an empty entry");
            if entry.donation.context_owner_slot == context_owner_slot
                && entry.donation.custody_active
            {
                let receiver_slot = entry.receiver_slot;
                let owner_slot = entry.donation.context_owner_slot;
                let priority_donated = entry.donation.priority_donated;
                ledger.entries[index]
                    .as_mut()
                    .expect("scheduler donation descendant disappeared")
                    .donation
                    .custody_active = false;
                if let Some(receiver_slot) = receiver_slot {
                    restore_context_owner_after_release(
                        &ledger,
                        receiver_slot,
                        owner_slot,
                        entry.donation.reply,
                        entry.donation.namespace,
                    );
                    if priority_donated {
                        decrement_receiver(receiver_slot);
                    }
                }
                index += 1;
            } else {
                index += 1;
            }
        }
    }
    true
}

pub(super) fn release_task(task_id: u64) {
    let mut ledger = DONATION_LEDGER.lock();
    let mut index = 0;
    while index < ledger.len {
        let entry = ledger.entries[index]
            .expect("scheduler donation ledger prefix contains an empty entry");
        if entry.donation.donor_task_id == task_id
            || entry.donation.context_owner_task_id == task_id
            || matches!(entry.donation.target, IpcDonationTarget::BoundWorker(target) if target == task_id)
        {
            let removed = ledger.remove(index);
            release_entry(&ledger, removed);
        } else {
            index += 1;
        }
    }
}

#[inline]
pub(super) fn inherited_system(slot: usize) -> bool {
    INHERITED_SYSTEM
        .get(slot)
        .is_some_and(|inherited| inherited.load(Ordering::Acquire) != 0)
}

pub(super) fn class_publication_sequence() -> u64 {
    CLASS_PUBLICATION_SEQUENCE.load(Ordering::Acquire)
}

/// Observation is NOT lifetime custody. The caller must retain its original
/// turn/frame/funder and borrow this leaf only for the bounded final claim.
/// All live base-class and inherited-class producers serialize here.
pub(super) fn with_class_claim<T>(
    expected: u64, claim: impl FnOnce() -> Option<T>,
) -> Option<T> {
    let _ledger = DONATION_LEDGER.lock();
    if expected & 1 != 0 || class_publication_sequence() != expected {
        return None;
    }
    claim()
}

#[cfg(test)]
pub(super) fn test_class_excluded() -> bool {
    DONATION_LEDGER.try_lock().is_none()
}

/// Self-demotion participates in the SAME class exclusion as ledger edges.
/// Unpublished initialization and drained terminal clearing are not live
/// class replacements; their existing physical publication seals remain owners.
pub(super) fn publish_base_class(effect: impl FnOnce()) {
    let _ledger = DONATION_LEDGER.lock();
    CLASS_PUBLICATION_SEQUENCE.fetch_add(1, Ordering::AcqRel);
    effect();
    CLASS_PUBLICATION_SEQUENCE.fetch_add(1, Ordering::AcqRel);
}

#[inline]
pub(super) fn borrowed_context_owner_slot(slot: usize) -> Option<usize> {
    let encoded = BORROWED_CONTEXT_OWNER.get(slot)?.load(Ordering::Acquire);
    (encoded != NO_BORROWED_CONTEXT).then_some(usize::from(encoded))
}

#[inline]
pub(super) fn borrowed_context_charge_token(slot: usize) -> Option<(usize, u64)> {
    let reply = BORROWED_CONTEXT_REPLY.get(slot)?.load(Ordering::Acquire);
    if reply == 0 {
        return None;
    }
    let encoded = BORROWED_CONTEXT_OWNER.get(slot)?.load(Ordering::Acquire);
    (encoded != NO_BORROWED_CONTEXT).then_some((usize::from(encoded), reply))
}

pub(super) fn live_len() -> usize {
    DONATION_LEDGER.lock().len
}

fn increment_receiver(slot: usize) {
    CLASS_PUBLICATION_SEQUENCE.fetch_add(1, Ordering::AcqRel);
    let previous = INHERITED_SYSTEM[slot].fetch_add(1, Ordering::Release);
    assert!(
        previous != u8::MAX,
        "scheduler donation receiver count overflow"
    );
    CLASS_PUBLICATION_SEQUENCE.fetch_add(1, Ordering::AcqRel);
}

fn decrement_receiver(slot: usize) {
    CLASS_PUBLICATION_SEQUENCE.fetch_add(1, Ordering::AcqRel);
    let previous = INHERITED_SYSTEM[slot].fetch_sub(1, Ordering::Release);
    assert!(previous != 0, "scheduler donation receiver count underflow");
    CLASS_PUBLICATION_SEQUENCE.fetch_add(1, Ordering::AcqRel);
}

fn release_entry(ledger: &DonationLedger, entry: LedgerEntry) {
    if entry.donation.custody_active
        && let Some(receiver_slot) = entry.receiver_slot
    {
        restore_context_owner_after_release(
            ledger,
            receiver_slot,
            entry.donation.context_owner_slot,
            entry.donation.reply,
            entry.donation.namespace,
        );
        if entry.donation.priority_donated {
            decrement_receiver(receiver_slot);
        }
    }
}

fn publish_charge_token(
    receiver_slot: usize,
    context_owner_slot: usize,
    reply: u64,
    namespace: DonationNamespace,
) {
    assert!(
        reply != 0,
        "borrowed scheduling context requires a live reply"
    );
    let encoded = u16::try_from(context_owner_slot).expect("context owner slot exceeds u16");
    BORROWED_CONTEXT_OWNER[receiver_slot].store(encoded, Ordering::Relaxed);
    BORROWED_CONTEXT_NAMESPACE[receiver_slot].store(namespace_tag(namespace), Ordering::Relaxed);
    BORROWED_CONTEXT_REPLY[receiver_slot].store(reply, Ordering::Release);
}

fn restore_context_owner_after_release(
    ledger: &DonationLedger,
    receiver_slot: usize,
    context_owner_slot: usize,
    reply: u64,
    namespace: DonationNamespace,
) {
    let encoded = u16::try_from(context_owner_slot).expect("context owner slot exceeds u16");
    let replacement = ledger.entries[..ledger.len]
        .iter()
        .rev()
        .flatten()
        .find(|entry| entry.donation.custody_active && entry.receiver_slot == Some(receiver_slot))
        .map(|entry| {
            (
                entry.donation.context_owner_slot,
                entry.donation.reply,
                entry.donation.namespace,
            )
        });
    if BORROWED_CONTEXT_OWNER[receiver_slot].load(Ordering::Relaxed) != encoded
        || BORROWED_CONTEXT_NAMESPACE[receiver_slot].load(Ordering::Relaxed)
            != namespace_tag(namespace)
        || BORROWED_CONTEXT_REPLY[receiver_slot].load(Ordering::Relaxed) != reply
    {
        return;
    }
    if let Some((owner_slot, reply, namespace)) = replacement {
        publish_charge_token(receiver_slot, owner_slot, reply, namespace);
    } else {
        BORROWED_CONTEXT_OWNER[receiver_slot].store(NO_BORROWED_CONTEXT, Ordering::Relaxed);
        BORROWED_CONTEXT_NAMESPACE[receiver_slot].store(0, Ordering::Relaxed);
        BORROWED_CONTEXT_REPLY[receiver_slot].store(0, Ordering::Release);
    }
}

/// Negative native teardown observation: all actual publication words must be
/// empty, not merely masked by an empty owner. Confers no funding authority.
#[cfg(test)]
pub(super) fn test_charge_publication_is_empty(slot: usize) -> bool {
    let _ledger = DONATION_LEDGER.lock();
    BORROWED_CONTEXT_OWNER[slot].load(Ordering::Acquire) == NO_BORROWED_CONTEXT
        && BORROWED_CONTEXT_NAMESPACE[slot].load(Ordering::Acquire) == 0
        && BORROWED_CONTEXT_REPLY[slot].load(Ordering::Acquire) == 0
}

#[cfg(test)]
mod tests {
    use super::super::donation_publication::tests::{ledger_witnesses as owned, legacy_witnesses};
    #[test]
    fn a_fault_token_never_aliases_an_equal_reply_handle() {
        owned::namespaces(true);
    }
    #[test]
    fn colliding_namespace_cannot_clear_worker_charge_publication() {
        owned::namespaces(false);
    }
    #[test]
    fn bound_reply_inheritance_is_exactly_once_and_rebinds_without_leak() {
        owned::exact_bind();
    }
    #[test]
    fn attached_reply_releases_when_either_endpoint_task_retires() {
        owned::endpoint_retire();
    }
    #[test]
    fn ordinary_nested_calls_borrow_one_root_context_without_system_promotion() {
        owned::ordinary();
    }
    #[test]
    fn root_reply_revokes_every_nested_priority_boost_exactly_once() {
        legacy_witnesses::transitive();
    }
    #[test]
    fn multithreaded_server_charge_tokens_restore_the_previous_live_reply() {
        owned::restore_previous();
    }
}
