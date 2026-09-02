//! Bounded reply-edge custody outside the scheduler task catalog.
//!
//! - **Owner:** this ledger owns one pending or live IPC donation edge per
//!   donor task; the scheduler owns only task identity resolution.
//! - **Lifecycle:** reserve -> attach/bind -> release. A bound edge increments
//!   exactly one receiver-slot inheritance count and terminal release removes
//!   it before forgetting the reply identity.
//! - **Concurrency:** `SchedulerDonation` serializes edge mutation. Dispatch
//!   reads only an Acquire receiver-slot counter and never scans this ledger.
//! - **Failure:** duplicate reservation, stale reply, capacity exhaustion, and
//!   unmatched terminal release fail closed without changing a receiver class.
//! - **Evidence:** `ipc-priority-inheritance` and the focused ledger tests.

use core::sync::atomic::{AtomicU8, AtomicU16, AtomicU64, Ordering};

use nucleus_core::util::lockdep::{LockClass, TrackedSpinLock};

use super::{
    MAX_TASK,
    ipc_donation::{DonationNamespace, IpcDonationTarget, IpcPriorityDonation},
};

#[derive(Clone, Copy)]
struct LedgerEntry {
    donation: IpcPriorityDonation,
    receiver_slot: Option<usize>,
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

static DONATION_LEDGER: TrackedSpinLock<DonationLedger, { LockClass::SchedulerDonation as u8 }> =
    TrackedSpinLock::new(DonationLedger::new());
static INHERITED_SYSTEM: [AtomicU8; MAX_TASK] = [const { AtomicU8::new(0) }; MAX_TASK];
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
}

pub(super) fn reserve(
    donor_task_id: u64,
    context_owner_task_id: u64,
    context_owner_slot: usize,
    priority_donated: bool,
) -> bool {
    if context_owner_slot >= MAX_TASK {
        return false;
    }
    let mut ledger = DONATION_LEDGER.lock();
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

pub(super) fn bind_reserved(
    reply: u64,
    namespace: DonationNamespace,
    donor_task_id: u64,
    receiver_task_id: u64,
    receiver_slot: usize,
) -> bool {
    if reply == 0 || donor_task_id == receiver_task_id || receiver_slot >= MAX_TASK {
        return false;
    }
    let mut ledger = DONATION_LEDGER.lock();
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

pub(super) fn upsert(
    reply: u64,
    namespace: DonationNamespace,
    donor_task_id: u64,
    receiver_task_id: u64,
    receiver_slot: usize,
    context_owner_task_id: u64,
    context_owner_slot: usize,
    priority_donated: bool,
) -> bool {
    if reply == 0
        || donor_task_id == receiver_task_id
        || receiver_slot >= MAX_TASK
        || context_owner_slot >= MAX_TASK
    {
        return false;
    }
    let mut ledger = DONATION_LEDGER.lock();
    if let Some(index) = ledger.find_reply(reply, namespace) {
        let previous = ledger.entries[index].expect("scheduler donation reply disappeared");
        let entry = ledger.entries[index]
            .as_mut()
            .expect("scheduler donation reply disappeared");
        entry.receiver_slot = Some(receiver_slot);
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
    let previous = INHERITED_SYSTEM[slot].fetch_add(1, Ordering::Release);
    assert!(
        previous != u8::MAX,
        "scheduler donation receiver count overflow"
    );
}

fn decrement_receiver(slot: usize) {
    let previous = INHERITED_SYSTEM[slot].fetch_sub(1, Ordering::Release);
    assert!(previous != 0, "scheduler donation receiver count underflow");
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

#[cfg(test)]
mod tests {
    use super::DonationNamespace;

    static TEST_GUARD: Mutex<()> = Mutex::new(());

    /// A pager fault token and an IPC reply handle can be the same number.
    ///
    /// Reply handles are `(generation << 16) | (index + 1)` and fault tokens
    /// are `(generation << 8) | slot`, so the smallest reply handle `0x1_0001`
    /// is also the fault token for slot 1 at generation 256 - and slot 1 is
    /// reused on almost every fault. When the ledger was keyed by the number
    /// alone, the second binding found the first entry and the reply's
    /// scheduling-context settlement panicked with "cancelled reply returned
    /// stale scheduling-context custody". The key is the pair.
    #[test]
    fn a_fault_token_never_aliases_an_equal_reply_handle() {
        let _guard = TEST_GUARD
            .lock()
            .expect("donation ledger test lock poisoned");
        reset();
        const COLLIDING_KEY: u64 = (1 << 16) | 1;

        assert!(reserve(41, 41, 4, true));
        assert!(bind_reserved(
            COLLIDING_KEY,
            DonationNamespace::IpcReply,
            41,
            52,
            5
        ));
        assert!(reserve(42, 42, 6, true));
        assert!(bind_reserved(
            COLLIDING_KEY,
            DonationNamespace::PagerFault,
            42,
            53,
            7
        ));

        // Each namespace releases exactly its own edge, and neither can settle
        // or consume the other's.
        assert!(release_reply(COLLIDING_KEY, DonationNamespace::PagerFault));
        assert!(!release_reply(COLLIDING_KEY, DonationNamespace::PagerFault));
        assert!(release_reply(COLLIDING_KEY, DonationNamespace::IpcReply));
        assert!(!release_reply(COLLIDING_KEY, DonationNamespace::IpcReply));
    }

    /// The lock-free per-worker charge publication is part of the same key.
    /// Releasing an equal numeric key in another namespace must not clear the
    /// worker's newer custody stamp.
    #[test]
    fn colliding_namespace_cannot_clear_worker_charge_publication() {
        let _guard = TEST_GUARD
            .lock()
            .expect("donation ledger test lock poisoned");
        reset();
        const COLLIDING_KEY: u64 = (1 << 16) | 1;
        const WORKER_SLOT: usize = 9;

        assert!(reserve(61, 61, 6, true));
        assert!(bind_reserved(
            COLLIDING_KEY,
            DonationNamespace::IpcReply,
            61,
            70,
            WORKER_SLOT,
        ));
        assert!(reserve(62, 62, 7, true));
        assert!(bind_reserved(
            COLLIDING_KEY,
            DonationNamespace::PagerFault,
            62,
            70,
            WORKER_SLOT,
        ));
        assert_eq!(
            borrowed_context_charge_token(WORKER_SLOT),
            Some((7, COLLIDING_KEY))
        );

        assert!(release_reply(COLLIDING_KEY, DonationNamespace::IpcReply));
        assert_eq!(
            borrowed_context_charge_token(WORKER_SLOT),
            Some((7, COLLIDING_KEY))
        );
        assert!(release_reply(COLLIDING_KEY, DonationNamespace::PagerFault));
        assert_eq!(borrowed_context_charge_token(WORKER_SLOT), None);
    }
    use core::sync::atomic::Ordering;
    use std::sync::Mutex;

    use super::{
        attach, bind_reserved, borrowed_context_charge_token, borrowed_context_owner_slot,
        inherited_system, release_reply, release_task, reserve, reset, upsert,
    };

    #[test]
    fn bound_reply_inheritance_is_exactly_once_and_rebinds_without_leak() {
        let _guard = TEST_GUARD
            .lock()
            .expect("donation ledger test lock poisoned");
        reset();

        assert!(reserve(41, 41, 4, true));
        assert!(
            !reserve(41, 41, 4, true),
            "a donor may reserve only one live reply edge"
        );
        assert!(bind_reserved(700, DonationNamespace::IpcReply, 41, 52, 5));
        assert!(!bind_reserved(700, DonationNamespace::IpcReply, 42, 53, 6));
        assert!(inherited_system(5));
        assert!(!inherited_system(6));

        assert!(
            bind_reserved(700, DonationNamespace::IpcReply, 41, 53, 6),
            "the sender must accept a reply the receiver already bound"
        );
        assert!(
            inherited_system(5),
            "a stale sender waiter replaced the actual receiver"
        );
        assert!(!inherited_system(6));

        assert!(upsert(
            700,
            DonationNamespace::IpcReply,
            41,
            53,
            6,
            41,
            4,
            true
        ));
        assert!(
            !inherited_system(5),
            "rebind retained the old receiver boost"
        );
        assert!(inherited_system(6));
        assert!(release_reply(700, DonationNamespace::IpcReply));
        assert!(!inherited_system(6));
        assert!(
            !release_reply(700, DonationNamespace::IpcReply),
            "terminal reply release must be idempotent"
        );
    }

    #[test]
    fn attached_reply_releases_when_either_endpoint_task_retires() {
        let _guard = TEST_GUARD
            .lock()
            .expect("donation ledger test lock poisoned");
        reset();

        assert!(reserve(61, 61, 6, true));
        assert!(attach(701, DonationNamespace::IpcReply, 61));
        assert!(
            !attach(701, DonationNamespace::IpcReply, 62),
            "another donor claimed the reply edge"
        );
        assert!(
            attach(701, DonationNamespace::IpcReply, 61),
            "exact attach must be idempotent"
        );
        assert!(bind_reserved(701, DonationNamespace::IpcReply, 61, 62, 7));
        assert!(inherited_system(7));
        release_task(62);
        assert!(!inherited_system(7));
        assert!(!release_reply(701, DonationNamespace::IpcReply));
    }

    #[test]
    fn ordinary_nested_calls_borrow_one_root_context_without_system_promotion() {
        let _guard = TEST_GUARD
            .lock()
            .expect("donation ledger test lock poisoned");
        reset();

        assert!(reserve(71, 71, 4, false));
        assert!(bind_reserved(710, DonationNamespace::IpcReply, 71, 72, 5));
        assert_eq!(borrowed_context_owner_slot(5), Some(4));
        assert!(!inherited_system(5));

        assert!(reserve(72, 71, 4, false));
        assert!(bind_reserved(711, DonationNamespace::IpcReply, 72, 73, 6));
        assert_eq!(borrowed_context_owner_slot(6), Some(4));
        assert!(!inherited_system(6));

        assert!(release_reply(710, DonationNamespace::IpcReply));
        assert_eq!(borrowed_context_owner_slot(5), None);
        assert_eq!(borrowed_context_owner_slot(6), None);
        assert!(
            release_reply(711, DonationNamespace::IpcReply),
            "revoked descendant reply must retain one terminal identity"
        );
        assert!(!release_reply(711, DonationNamespace::IpcReply));
    }

    #[test]
    fn root_reply_revokes_every_nested_priority_boost_exactly_once() {
        let _guard = TEST_GUARD
            .lock()
            .expect("donation ledger test lock poisoned");
        reset();

        assert!(reserve(91, 91, 4, true));
        assert!(bind_reserved(910, DonationNamespace::IpcReply, 91, 92, 5));
        assert!(reserve(92, 91, 4, true));
        assert!(bind_reserved(911, DonationNamespace::IpcReply, 92, 93, 6));
        assert!(inherited_system(5));
        assert!(inherited_system(6));

        assert!(release_reply(910, DonationNamespace::IpcReply));
        assert!(!inherited_system(5));
        assert!(!inherited_system(6));
        assert!(release_reply(911, DonationNamespace::IpcReply));
        assert!(!release_reply(911, DonationNamespace::IpcReply));
    }

    #[test]
    fn multithreaded_server_charge_tokens_restore_the_previous_live_reply() {
        let _guard = TEST_GUARD
            .lock()
            .expect("donation ledger test lock poisoned");
        reset();

        assert!(reserve(81, 81, 4, false));
        assert!(bind_reserved(810, DonationNamespace::IpcReply, 81, 90, 9));
        assert_eq!(borrowed_context_owner_slot(9), Some(4));
        assert_eq!(borrowed_context_charge_token(9), Some((4, 810)));

        assert!(reserve(82, 82, 6, false));
        assert!(bind_reserved(811, DonationNamespace::IpcReply, 82, 90, 9));
        assert_eq!(borrowed_context_owner_slot(9), Some(6));
        assert_eq!(borrowed_context_charge_token(9), Some((6, 811)));

        assert!(release_reply(810, DonationNamespace::IpcReply));
        assert_eq!(borrowed_context_owner_slot(9), Some(6));
        assert_eq!(borrowed_context_charge_token(9), Some((6, 811)));
        assert!(release_reply(811, DonationNamespace::IpcReply));
        assert_eq!(borrowed_context_owner_slot(9), None);
        assert_eq!(borrowed_context_charge_token(9), None);
        assert_eq!(super::BORROWED_CONTEXT_REPLY[9].load(Ordering::Acquire), 0);
    }
}
