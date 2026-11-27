//! Bounded reply-edge custody outside the scheduler task catalog.
//!
//! - **Owner:** this ledger owns one pending or live IPC donation edge per
//!   donor task; original participant loans independently qualify new Current entries.
//! - **Lifecycle:** reserve -> attach/bind -> release. A bound edge increments
//!   exactly one receiver-slot inheritance count and terminal release removes
//!   it before forgetting the reply identity.
//! - **Concurrency:** `SchedulerDonation` serializes edge mutation. Dispatch
//!   reads only an Acquire receiver-slot counter and never scans this ledger.
//!   Class claims exclude class publication without this leaf: each claim
//!   raises its CPU's indicator and revalidates the sequence; each publisher
//!   (always under the leaf) makes the sequence odd and waits for every raised
//!   indicator before changing a class. Claims on different CPUs therefore no
//!   longer serialize on one global lock per dispatch.
//! - **Failure:** duplicate reservation, stale reply, capacity exhaustion, and
//!   unmatched terminal release fail closed without changing a receiver class.
//!   Owned publication revalidates original source custody under this same leaf;
//!   candidate snapshots are never execution/interval grants.
//! - **Evidence:** `ipc-priority-inheritance` and the focused ledger tests.

use alloc::vec::Vec;
use crate::multitask::task_table;
use core::sync::atomic::{AtomicU32, AtomicU64, AtomicUsize, Ordering};

use nucleus_core::util::cache_padded::CachePadded;
use nucleus_core::util::lockdep::{LockClass, MAX_TRACKED_CPUS};
use crate::multitask::irq_leaf::IrqLeaf;

use super::{
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

const EMPTY_BUCKET: u32 = u32::MAX;

#[inline]
const fn hash_u64(mut x: u64) -> usize {
    x ^= x >> 30;
    x = x.wrapping_mul(0xbf58_476d_1ce4_e5b9);
    x ^= x >> 27;
    x = x.wrapping_mul(0x94d0_49bb_1331_11eb);
    x ^= x >> 31;
    x as usize
}

#[inline]
const fn hash_reply(reply: u64, namespace: DonationNamespace) -> usize {
    hash_u64(reply ^ ((namespace_tag(namespace) as u64) << 56))
}

/// Zero-allocation-under-lock open-addressed hash table with backward-shift
/// deletion (no tombstones ever accumulate). Sized to `>= 2 * entries.len()`
/// in unmasked `ensure_capacity`, keeping load factor `<= 0.5`.
struct IndexTable {
    buckets: Vec<u32>,
}

impl IndexTable {
    const fn new() -> Self {
        Self { buckets: Vec::new() }
    }

    fn clear(&mut self) {
        self.buckets.fill(EMPTY_BUCKET);
    }

    fn reset_capacity(&mut self, power_of_two_cap: usize) {
        debug_assert!(power_of_two_cap.is_power_of_two());
        self.buckets.clear();
        self.buckets.resize(power_of_two_cap, EMPTY_BUCKET);
    }

    #[inline]
    fn find(&self, hash: usize, matches: impl Fn(usize) -> bool) -> Option<usize> {
        let len = self.buckets.len();
        if len == 0 {
            return None;
        }
        let mask = len - 1;
        let mut pos = hash & mask;
        loop {
            let idx = self.buckets[pos];
            if idx == EMPTY_BUCKET {
                return None;
            }
            if matches(idx as usize) {
                return Some(idx as usize);
            }
            pos = (pos + 1) & mask;
        }
    }

    #[inline]
    fn insert(&mut self, hash: usize, entry_idx: usize) {
        let mask = self.buckets.len() - 1;
        let mut pos = hash & mask;
        while self.buckets[pos] != EMPTY_BUCKET {
            pos = (pos + 1) & mask;
        }
        self.buckets[pos] = entry_idx as u32;
    }

    #[inline]
    fn remove(&mut self, hash: usize, entry_idx: usize, hash_of_entry: impl Fn(usize) -> usize) {
        let mask = self.buckets.len() - 1;
        let target = entry_idx as u32;
        let mut pos = hash & mask;
        loop {
            let cur = self.buckets[pos];
            assert_ne!(cur, EMPTY_BUCKET, "donation index missing entry={entry_idx}");
            if cur == target {
                break;
            }
            pos = (pos + 1) & mask;
        }
        let mut hole = pos;
        let mut cur = (hole + 1) & mask;
        loop {
            let next_idx = self.buckets[cur];
            if next_idx == EMPTY_BUCKET {
                self.buckets[hole] = EMPTY_BUCKET;
                return;
            }
            let ideal = hash_of_entry(next_idx as usize) & mask;
            if hole.wrapping_sub(ideal) & mask < cur.wrapping_sub(ideal) & mask {
                self.buckets[hole] = next_idx;
                hole = cur;
            }
            cur = (cur + 1) & mask;
        }
    }

    #[inline]
    fn repoint(&mut self, hash: usize, old_idx: usize, new_idx: usize) {
        let mask = self.buckets.len() - 1;
        let target = old_idx as u32;
        let mut pos = hash & mask;
        loop {
            let cur = self.buckets[pos];
            assert_ne!(cur, EMPTY_BUCKET, "donation index missing moved entry={old_idx}");
            if cur == target {
                self.buckets[pos] = new_idx as u32;
                return;
            }
            pos = (pos + 1) & mask;
        }
    }
}

struct DonationLedger {
    /// One donation per task slot; sized once by `provision`, empty before.
    entries: Vec<Option<LedgerEntry>>,
    len: usize,
    /// O(1) lookup for unbound reservations (`reply == 0`) keyed by `donor_task_id`.
    reservation_index: IndexTable,
    /// O(1) lookup for attached/bound replies (`reply != 0`) keyed by `(reply, namespace)`.
    reply_index: IndexTable,
}

impl DonationLedger {
    const fn new() -> Self {
        Self {
            entries: Vec::new(),
            len: 0,
            reservation_index: IndexTable::new(),
            reply_index: IndexTable::new(),
        }
    }

    /// Drops every donation and keeps the provisioned storage.
    fn clear(&mut self) {
        self.entries.fill(None);
        self.len = 0;
        self.reservation_index.clear();
        self.reply_index.clear();
    }

    fn ensure_capacity(&mut self, slots: usize) {
        if self.entries.len() < slots {
            self.entries.resize(slots, None);
        }
        let target_buckets = self.entries.len().max(16).saturating_mul(2).next_power_of_two();
        if self.reservation_index.buckets.len() < target_buckets {
            self.reservation_index.reset_capacity(target_buckets);
            self.reply_index.reset_capacity(target_buckets);
            for idx in 0..self.len {
                if let Some(entry) = self.entries[idx] {
                    if entry.donation.reply == 0 {
                        self.reservation_index
                            .insert(hash_u64(entry.donation.donor_task_id), idx);
                    } else {
                        self.reply_index
                            .insert(hash_reply(entry.donation.reply, entry.donation.namespace), idx);
                    }
                }
            }
        }
    }

    fn insert(&mut self, entry: LedgerEntry) -> bool {
        if self.len == self.entries.len() {
            return false;
        }
        let idx = self.len;
        self.entries[idx] = Some(entry);
        self.len += 1;
        if entry.donation.reply == 0 {
            self.reservation_index
                .insert(hash_u64(entry.donation.donor_task_id), idx);
        } else {
            self.reply_index
                .insert(hash_reply(entry.donation.reply, entry.donation.namespace), idx);
        }
        true
    }

    fn remove(&mut self, index: usize) -> LedgerEntry {
        assert!(index < self.len, "scheduler donation index exceeds ledger");
        let entry = self.entries[index]
            .expect("scheduler donation ledger prefix contains an empty entry");
        if entry.donation.reply == 0 {
            let entries = &self.entries;
            self.reservation_index
                .remove(hash_u64(entry.donation.donor_task_id), index, |i| {
                    hash_u64(
                        entries[i]
                            .expect("indexed reservation missing")
                            .donation
                            .donor_task_id,
                    )
                });
        } else {
            let entries = &self.entries;
            self.reply_index.remove(
                hash_reply(entry.donation.reply, entry.donation.namespace),
                index,
                |i| {
                    let d = entries[i].expect("indexed reply missing").donation;
                    hash_reply(d.reply, d.namespace)
                },
            );
        }
        self.len -= 1;
        let removed = self.entries[index]
            .take()
            .expect("scheduler donation ledger prefix contains an empty entry");
        if index != self.len {
            let moved = self.entries[self.len]
                .take()
                .expect("scheduler donation ledger tail missing");
            self.entries[index] = Some(moved);
            if moved.donation.reply == 0 {
                self.reservation_index.repoint(
                    hash_u64(moved.donation.donor_task_id),
                    self.len,
                    index,
                );
            } else {
                self.reply_index.repoint(
                    hash_reply(moved.donation.reply, moved.donation.namespace),
                    self.len,
                    index,
                );
            }
        }
        removed
    }

    fn promote_reservation(
        &mut self,
        index: usize,
        reply: u64,
        namespace: Option<DonationNamespace>,
    ) -> &mut LedgerEntry {
        debug_assert!(reply != 0);
        let donor_id = self.entries[index]
            .expect("scheduler donation reservation disappeared")
            .donation
            .donor_task_id;
        {
            let entries = &self.entries;
            self.reservation_index
                .remove(hash_u64(donor_id), index, |i| {
                    hash_u64(
                        entries[i]
                            .expect("indexed reservation missing")
                            .donation
                            .donor_task_id,
                    )
                });
        }
        let entry = self.entries[index]
            .as_mut()
            .expect("scheduler donation reservation disappeared");
        entry.donation.reply = reply;
        if let Some(ns) = namespace {
            entry.donation.namespace = ns;
        }
        let final_ns = entry.donation.namespace;
        self.reply_index.insert(hash_reply(reply, final_ns), index);
        self.entries[index]
            .as_mut()
            .expect("scheduler donation reservation disappeared")
    }

    fn find_reservation(&self, donor_task_id: u64) -> Option<usize> {
        self.reservation_index.find(hash_u64(donor_task_id), |idx| {
            self.entries[idx].is_some_and(|entry| {
                entry.donation.reply == 0 && entry.donation.donor_task_id == donor_task_id
            })
        })
    }

    fn find_reply(&self, reply: u64, namespace: DonationNamespace) -> Option<usize> {
        if reply == 0 {
            return None;
        }
        self.reply_index.find(hash_reply(reply, namespace), |idx| {
            self.entries[idx].is_some_and(|entry| {
                entry.donation.reply == reply && entry.donation.namespace == namespace
            })
        })
    }
}

static DONATION_LEDGER: IrqLeaf<DonationLedger, { LockClass::SchedulerDonation as u8 }> =
    IrqLeaf::new(DonationLedger::new());

mod lazy;
pub(in crate::multitask) use lazy::Edge as LazyDonationEdge;

/// The ledger leaf with every lazily published own-funded edge already inserted,
/// so a reader never observes less than the eager protocol would have produced.
fn locked() -> impl core::ops::DerefMut<Target = DonationLedger> + 'static {
    let mut ledger = DONATION_LEDGER.lock();
    lazy::settle(&mut ledger);
    ledger
}

/// Reserve the donor's edge without taking the ledger leaf when it is an ordinary
/// class, self-funded edge; otherwise exactly `reserve_own`.
#[cfg(any(rustos_boot_image, test))]
pub(super) fn reserve_own_lazy(slot: usize, id: u64, priority: bool) -> bool {
    if !priority && borrowed_context_charge_token(slot).is_none() && lazy::reserve(slot, id) {
        return true;
    }
    reserve_own(slot, id, priority)
}

/// Lane bind of a lazily reserved self-funded edge; `None` means bind through the ledger.
pub(super) fn bind_own_lazy(
    donor_slot: usize, donor_id: u64, reply: u64, receiver_slot: usize, receiver_id: u64,
) -> Option<bool> {
    lazy::bind(donor_slot, donor_id, reply, receiver_slot, receiver_id)
}

#[cfg(test)]
pub(super) fn lazy_state_for_tests(slot: usize) -> u64 {
    lazy::state(slot)
}
#[cfg(test)]
pub(super) fn lazy_reserve_for_tests(slot: usize, id: u64, priority: bool) -> bool {
    !priority && lazy::reserve(slot, id)
}

/// Lane release of the reply's edge: lock-free when it was published lazily.
pub(super) fn release_reply_lane(donor_slot: usize, donor_id: u64, reply: u64) -> bool {
    lazy::release(donor_slot, donor_id, reply)
        || release_reply(reply, DonationNamespace::IpcReply)
}
static CLASS_PUBLICATION_SEQUENCE: AtomicU64 = AtomicU64::new(0);
/// Number of currently active priority-donated edges across the kernel.
/// Zero whenever no User task holds inherited System priority, enabling O(1)
/// short-circuits in local overdue/hint scans.
static LIVE_INHERITED_SYSTEM_EDGES: AtomicUsize = AtomicUsize::new(0);
/// Read side of the class exclusion: claims in progress on each CPU. A count
/// rather than a flag only so native fixtures, which run many test threads as
/// one modeled CPU, cannot clear each other's section.
static CLASS_CLAIMS: [CachePadded<AtomicU32>; MAX_TRACKED_CPUS] =
    [const { CachePadded::new(AtomicU32::new(0)) }; MAX_TRACKED_CPUS];
/// A claim is a bounded critical section; a publisher that waits longer than
/// this on one is a deadlock, not contention.
const CLASS_CLAIM_WAIT_SPINS: u64 = 1 << 28;

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
    lazy::reset();
    begin_class_publication();
    ledger.clear();
    LIVE_INHERITED_SYSTEM_EDGES.store(0, Ordering::Release);
    for slot in task_table::allocated_slots() {
        let Some(task) = task_table::try_get(slot) else { continue; };
        // ORDERING: Release resets class/context metadata before test runs resume.
        task.inherited_system.store(0, Ordering::Release);
        // ORDERING: Release resets borrowed context owner slot.
        task.borrowed_ctx_owner.store(task_table::NO_BORROWED_OWNER, Ordering::Release);
        // ORDERING: Release resets borrowed context namespace tag.
        task.borrowed_ctx_namespace.store(0, Ordering::Release);
        // ORDERING: Release resets borrowed context reply word.
        task.borrowed_ctx_reply.store(0, Ordering::Release);
    }
    end_class_publication();
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
    // ORDERING: Acquire pairs with Release context token stores.
    let tag = task_table::try_get(slot)?.borrowed_ctx_namespace.load(Ordering::Acquire);
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
    basis_locked(&locked(), slot, id)
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
    let mut ledger = locked();
    if !current_basis_live(&ledger, basis) { return false; }
    reserve_locked(&mut ledger, basis.donor_id, basis.root_id, basis.root_slot,
        priority, Some(*basis))
}
/// `reserve_owned` for a donor funded by its own context: the basis is the
/// unborrowed one by construction, so it is not captured in a separate leaf
/// acquisition. A charge token published since the caller's lock-free check
/// refuses here exactly as `current_basis_live` refuses a moved basis.
#[cfg(any(rustos_boot_image, test))]
pub(super) fn reserve_own(slot: usize, id: u64, priority: bool) -> bool {
    let mut ledger = locked();
    if borrowed_context_charge_token(slot).is_some() { return false; }
    let basis = FundingBasis { donor_slot: slot, donor_id: id, root_slot: slot, root_id: id, source: None };
    reserve_locked(&mut ledger, id, id, slot, priority, Some(basis))
}

fn reserve_locked(
    ledger: &mut DonationLedger,
    donor_task_id: u64,
    context_owner_task_id: u64,
    context_owner_slot: usize,
    priority_donated: bool,
    funding: Option<FundingBasis>,
) -> bool {
    if task_table::try_get(context_owner_slot).is_none() {
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
    let mut ledger = locked();
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
    let ledger = locked();
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
    let ledger = locked();
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
    let mut ledger = locked();
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
    let _ = ledger.promote_reservation(index, reply, None);
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
    bind_basis_locked(&locked(), reply, namespace, slot, id)
}
/// `bind_basis` + `bind_owned` for a donor whose funding is its own context
/// (no borrowed source, so no root pin or revalidation beyond the entry itself),
/// in one ledger acquisition. `None` means the full two-step bind must decide:
/// a borrowed basis, a missing reservation or a mismatch.
pub(super) fn bind_own_funding(reply: u64, donor_slot: usize, donor_id: u64,
    receiver_slot: usize, receiver_id: u64) -> Option<bool> {
    let mut ledger = locked();
    let basis = match bind_basis_locked(&ledger, reply, DonationNamespace::IpcReply, donor_slot, donor_id) {
        Ok(Some(basis)) => basis,
        _ => return None,
    };
    if basis.source.is_some() || basis.root_slot != donor_slot || basis.root_id != donor_id {
        return None;
    }
    Some(bind_reserved_locked(&mut ledger, reply, DonationNamespace::IpcReply, donor_id,
        receiver_id, receiver_slot))
}

pub(super) fn bind_owned(loans: &super::donation_publication::ReceiveLoans,
    reply: u64, namespace: DonationNamespace) -> bool {
    let basis = loans.donor.basis();
    let mut ledger = locked();
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
    let mut ledger = locked();
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
    let mut ledger = locked();
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
    if reply == 0 || donor_task_id == receiver_task_id || task_table::try_get(receiver_slot).is_none() {
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
    let entry = ledger.promote_reservation(index, reply, Some(namespace));
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
        || task_table::try_get(receiver_slot).is_none()
        || task_table::try_get(context_owner_slot).is_none()
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
    let mut ledger = locked();
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
    let mut ledger = locked();
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
pub(super) fn live_inherited_system_count() -> usize {
    // ORDERING: Acquire pairs with increment/decrement Release under class exclusion.
    LIVE_INHERITED_SYSTEM_EDGES.load(Ordering::Acquire)
}

#[inline]
pub(super) fn inherited_system(slot: usize) -> bool {
    task_table::try_get(slot)
        // ORDERING: Acquire pairs with increment/decrement Release under class exclusion.
        .is_some_and(|task| task.inherited_system.load(Ordering::Acquire) != 0)
}

pub(super) fn class_publication_sequence() -> u64 {
    CLASS_PUBLICATION_SEQUENCE.load(Ordering::Acquire)
}

/// Observation is NOT lifetime custody. The caller must retain its original
/// turn/frame/funder and run only the bounded final claim here. Every live
/// base-class and inherited-class producer is excluded for the claim's whole
/// duration: either this claim observes the producer's odd/advanced sequence
/// and refuses, or the producer observes this CPU's raised indicator and waits
/// for the claim to finish before changing any class (SeqCst store-then-load
/// on both sides). Claims on other CPUs proceed concurrently.
pub(super) fn with_class_claim<T>(
    expected: u64, claim: impl FnOnce() -> Option<T>,
) -> Option<T> {
    if expected & 1 != 0 {
        return None;
    }
    let indicator = &CLASS_CLAIMS[nucleus_core::util::lockdep::current_cpu_index()];
    // A publisher must never preempt a claim on its own CPU and then wait on
    // that CPU's indicator, so the claim runs with local interrupts excluded.
    let excluded = || {
        // ORDERING: SeqCst indicator raise before the SeqCst sequence load,
        // pairing with the publisher's SeqCst bump before its indicator scan.
        indicator.fetch_add(1, Ordering::SeqCst);
        // ORDERING: SeqCst; see the indicator raise above.
        if CLASS_PUBLICATION_SEQUENCE.load(Ordering::SeqCst) != expected {
            // ORDERING: Release ends this CPU's (empty) read section.
            indicator.fetch_sub(1, Ordering::Release);
            return None;
        }
        let claimed = claim();
        // ORDERING: Release publishes the claim's effects before a waiting
        // publisher may change a class.
        indicator.fetch_sub(1, Ordering::Release);
        claimed
    };
    #[cfg(rustos_boot_image)]
    {
        x86_64::instructions::interrupts::without_interrupts(excluded)
    }
    #[cfg(not(rustos_boot_image))]
    {
        excluded()
    }
}

/// Write side of the class exclusion; the caller holds `DONATION_LEDGER`.
fn begin_class_publication() {
    // ORDERING: SeqCst bump to odd before scanning claim indicators (Dekker
    // pair with `with_class_claim`).
    CLASS_PUBLICATION_SEQUENCE.fetch_add(1, Ordering::SeqCst);
    for indicator in &CLASS_CLAIMS {
        let mut spins = 0_u64;
        // ORDERING: Acquire joins the claim's Release end-of-section update.
        while indicator.load(Ordering::Acquire) != 0 {
            spins += 1;
            assert!(spins < CLASS_CLAIM_WAIT_SPINS, "class publication waited on a stalled claim");
            core::hint::spin_loop();
        }
    }
}

/// Ends a class publication; the caller still holds `DONATION_LEDGER`.
fn end_class_publication() {
    // ORDERING: AcqRel publishes the class change with the even sequence.
    CLASS_PUBLICATION_SEQUENCE.fetch_add(1, Ordering::AcqRel);
}

#[cfg(test)]
pub(super) fn test_class_excluded() -> bool {
    CLASS_CLAIMS
        .iter()
        // ORDERING: native observation of the read side only.
        .any(|indicator| indicator.load(Ordering::Acquire) != 0)
}

/// Self-demotion participates in the SAME class exclusion as ledger edges.
/// Unpublished initialization and drained terminal clearing are not live
/// class replacements; their existing physical publication seals remain owners.
pub(super) fn publish_base_class(effect: impl FnOnce()) {
    let _ledger = DONATION_LEDGER.lock();
    begin_class_publication();
    effect();
    end_class_publication();
}

#[inline]
#[cfg(test)]
pub(super) fn borrowed_context_owner_slot(slot: usize) -> Option<usize> {
    // ORDERING: Acquire observes Release publication of borrowed context owner/reply.
    let encoded = task_table::try_get(slot)?.borrowed_ctx_owner.load(Ordering::Acquire);
    (encoded != task_table::NO_BORROWED_OWNER).then_some(encoded as usize)
}

#[inline]
pub(super) fn borrowed_context_charge_token(slot: usize) -> Option<(usize, u64)> {
    let task = task_table::try_get(slot)?;
    // ORDERING: Acquire observes Release store of reply word before owner load.
    let reply = task.borrowed_ctx_reply.load(Ordering::Acquire);
    if reply == 0 {
        return None;
    }
    // ORDERING: Acquire observes owner slot written prior to reply publication.
    let encoded = task.borrowed_ctx_owner.load(Ordering::Acquire);
    (encoded != task_table::NO_BORROWED_OWNER).then_some((encoded as usize, reply))
}

pub(super) fn live_len() -> usize {
    locked().len
}

#[cfg(test)]
pub(super) fn capacity() -> usize {
    locked().entries.len()
}

fn increment_receiver(slot: usize) {
    begin_class_publication();
    // ORDERING: Release publishes increased inherited_system count after class exclusion.
    let previous = task_table::get(slot).inherited_system.fetch_add(1, Ordering::Release);
    assert!(
        previous != u8::MAX,
        "scheduler donation receiver count overflow"
    );
    LIVE_INHERITED_SYSTEM_EDGES.fetch_add(1, Ordering::Release);
    end_class_publication();
}

fn decrement_receiver(slot: usize) {
    begin_class_publication();
    // ORDERING: Release publishes decreased inherited_system count after class exclusion.
    let previous = task_table::get(slot).inherited_system.fetch_sub(1, Ordering::Release);
    assert!(previous != 0, "scheduler donation receiver count underflow");
    LIVE_INHERITED_SYSTEM_EDGES.fetch_sub(1, Ordering::Release);
    end_class_publication();
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
    let encoded = u32::try_from(context_owner_slot).expect("context owner slot exceeds u32");
    let task = task_table::get(receiver_slot);
    task.borrowed_ctx_owner.store(encoded, Ordering::Relaxed);
    task.borrowed_ctx_namespace.store(namespace_tag(namespace), Ordering::Relaxed);
    // ORDERING: Release publishes owner/namespace words together with non-zero reply.
    task.borrowed_ctx_reply.store(reply, Ordering::Release);
}

fn restore_context_owner_after_release(
    ledger: &DonationLedger,
    receiver_slot: usize,
    context_owner_slot: usize,
    reply: u64,
    namespace: DonationNamespace,
) {
    let encoded = u32::try_from(context_owner_slot).expect("context owner slot exceeds u32");
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
    let task = task_table::get(receiver_slot);
    if task.borrowed_ctx_owner.load(Ordering::Relaxed) != encoded
        || task.borrowed_ctx_namespace.load(Ordering::Relaxed)
            != namespace_tag(namespace)
        || task.borrowed_ctx_reply.load(Ordering::Relaxed) != reply
    {
        return;
    }
    if let Some((owner_slot, reply, namespace)) = replacement {
        publish_charge_token(receiver_slot, owner_slot, reply, namespace);
    } else {
        task.borrowed_ctx_owner.store(task_table::NO_BORROWED_OWNER, Ordering::Relaxed);
        task.borrowed_ctx_namespace.store(0, Ordering::Relaxed);
        // ORDERING: Release clears borrowed reply after owner/namespace cleared.
        task.borrowed_ctx_reply.store(0, Ordering::Release);
    }
}

/// Negative native teardown observation: all actual publication words must be
/// empty, not merely masked by an empty owner. Confers no funding authority.
#[cfg(test)]
pub(super) fn test_charge_publication_is_empty(slot: usize) -> bool {
    let _ledger = DONATION_LEDGER.lock();
    let Some(task) = task_table::try_get(slot) else {
        return true;
    };
    // ORDERING: Acquire reads observed context words under ledger lock.
    task.borrowed_ctx_owner.load(Ordering::Acquire) == task_table::NO_BORROWED_OWNER
        // ORDERING: Acquire reads observed namespace word under ledger lock.
        && task.borrowed_ctx_namespace.load(Ordering::Acquire) == 0
        // ORDERING: Acquire reads observed reply word under ledger lock.
        && task.borrowed_ctx_reply.load(Ordering::Acquire) == 0
}

#[cfg(test)]
mod tests {
    use super::super::donation_publication::tests::ledger_witnesses as owned;

    #[test]
    fn class_claims_on_different_cpus_do_not_wait_for_each_other() {
        use nucleus_core::util::lockdep::with_native_cpu;
        // A single global claim lock made the second CPU's claim wait for the
        // first one's entire callback; it must now return while the first is
        // still inside its claim.
        let returned_inside = with_native_cpu(0, || loop {
            let expected = super::class_publication_sequence();
            let outcome = super::with_class_claim(expected, || {
                let (sender, receiver) = std::sync::mpsc::channel();
                let peer = std::thread::spawn(move || {
                    with_native_cpu(1, || {
                        let _ = super::with_class_claim(expected, || Some(()));
                        sender.send(()).unwrap();
                    })
                });
                let returned = receiver
                    .recv_timeout(std::time::Duration::from_secs(5))
                    .is_ok();
                peer.join().unwrap();
                Some(returned)
            });
            if let Some(returned) = outcome {
                break returned;
            }
        });
        assert!(returned_inside);
    }

    #[test]
    fn class_publication_waits_for_an_in_progress_claim() {
        use core::sync::atomic::{AtomicBool, Ordering::SeqCst};
        use nucleus_core::util::lockdep::with_native_cpu;
        use std::sync::Arc;
        // Publishing advances the shared class sequence; exclude the fixtures
        // whose claims validate it, in their own guard order.
        let _runtime = super::super::scheduling_runtime::test_serial_guard();
        let _queue = super::super::runqueue::test_serial_guard();
        let published = Arc::new(AtomicBool::new(false));
        with_native_cpu(2, || loop {
            let expected = super::class_publication_sequence();
            let claimed = super::with_class_claim(expected, || {
                let observed = Arc::clone(&published);
                let publisher = std::thread::spawn(move || {
                    with_native_cpu(3, || super::publish_base_class(|| observed.store(true, SeqCst)))
                });
                std::thread::sleep(std::time::Duration::from_millis(50));
                // The class may not change under a claim that validated it.
                assert!(!published.load(SeqCst));
                Some(publisher)
            });
            if let Some(publisher) = claimed {
                publisher.join().unwrap();
                break;
            }
        });
        assert!(published.load(SeqCst));
    }
    #[test]
    fn lazy_edge_settles_to_the_eager_entries() {
        owned::lazy_edges();
    }
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
    fn multithreaded_server_charge_tokens_restore_the_previous_live_reply() {
        owned::restore_previous();
    }

    #[test]
    fn donation_index_tables_handle_insert_promote_swap_remove_and_resize() {
        use super::{DonationLedger, DonationNamespace, IpcDonationTarget, IpcPriorityDonation, LedgerEntry};

        let mut ledger = DonationLedger::new();
        ledger.ensure_capacity(16);

        let make_res = |donor: u64| LedgerEntry {
            donation: IpcPriorityDonation {
                reply: 0,
                namespace: DonationNamespace::IpcReply,
                donor_task_id: donor,
                context_owner_task_id: donor,
                context_owner_slot: 1,
                priority_donated: false,
                custody_active: false,
                target: IpcDonationTarget::AwaitingReceiver,
            },
            receiver_slot: None,
            funding: None,
        };

        // Insert 12 reservations and verify O(1) lookup.
        for donor in 1..=12_u64 {
            assert!(ledger.insert(make_res(donor)));
        }
        for donor in 1..=12_u64 {
            assert!(ledger.find_reservation(donor).is_some());
        }
        // Promote even donors to replies in both namespaces.
        for donor in (2..=12_u64).step_by(2) {
            let idx = ledger.find_reservation(donor).unwrap();
            let ns = if donor % 4 == 0 {
                DonationNamespace::PagerFault
            } else {
                DonationNamespace::IpcReply
            };
            let _ = ledger.promote_reservation(idx, 100 + donor, Some(ns));
            assert_eq!(ledger.find_reservation(donor), None);
            assert_eq!(ledger.find_reply(100 + donor, ns), Some(idx));
        }
        // Grow capacity and verify rebuild preserves every reservation and reply.
        ledger.ensure_capacity(64);
        for donor in (1..=11_u64).step_by(2) {
            assert!(ledger.find_reservation(donor).is_some());
        }
        for donor in (2..=12_u64).step_by(2) {
            let ns = if donor % 4 == 0 {
                DonationNamespace::PagerFault
            } else {
                DonationNamespace::IpcReply
            };
            assert!(ledger.find_reply(100 + donor, ns).is_some());
        }
        // Swap-remove from the front repeatedly to exercise backward-shift + repoint.
        while ledger.len > 0 {
            let removed = ledger.remove(0);
            if removed.donation.reply == 0 {
                assert_eq!(ledger.find_reservation(removed.donation.donor_task_id), None);
            } else {
                assert_eq!(
                    ledger.find_reply(removed.donation.reply, removed.donation.namespace),
                    None
                );
            }
            for idx in 0..ledger.len {
                let e = ledger.entries[idx].unwrap();
                if e.donation.reply == 0 {
                    assert_eq!(ledger.find_reservation(e.donation.donor_task_id), Some(idx));
                } else {
                    assert_eq!(
                        ledger.find_reply(e.donation.reply, e.donation.namespace),
                        Some(idx)
                    );
                }
            }
        }
    }
}

/// Pre-sizes the donation ledger vector when `TaskTable` grows in unmasked context.
pub(in crate::multitask) fn ensure_capacity(slots: usize) {
    let mut ledger = DONATION_LEDGER.lock();
    ledger.ensure_capacity(slots);
}
