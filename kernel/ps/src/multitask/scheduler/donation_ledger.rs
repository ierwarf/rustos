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
    ipc_donation::{IpcDonationTarget, IpcPriorityDonation},
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

    fn find_reply(&self, reply: u64) -> Option<usize> {
        self.entries[..self.len]
            .iter()
            .position(|entry| entry.is_some_and(|entry| entry.donation.reply == reply))
    }
}

static DONATION_LEDGER: TrackedSpinLock<DonationLedger, { LockClass::SchedulerDonation as u8 }> =
    TrackedSpinLock::new(DonationLedger::new());
static INHERITED_SYSTEM: [AtomicU8; MAX_TASK] = [const { AtomicU8::new(0) }; MAX_TASK];
const NO_BORROWED_CONTEXT: u16 = u16::MAX;
static BORROWED_CONTEXT_OWNER: [AtomicU16; MAX_TASK] =
    [const { AtomicU16::new(NO_BORROWED_CONTEXT) }; MAX_TASK];
static BORROWED_CONTEXT_REPLY: [AtomicU64; MAX_TASK] = [const { AtomicU64::new(0) }; MAX_TASK];

pub(super) fn reset() {
    let mut ledger = DONATION_LEDGER.lock();
    *ledger = DonationLedger::new();
    for inherited in &INHERITED_SYSTEM {
        inherited.store(0, Ordering::Release);
    }
    for owner in &BORROWED_CONTEXT_OWNER {
        owner.store(NO_BORROWED_CONTEXT, Ordering::Release);
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

pub(super) fn attach(reply: u64, donor_task_id: u64) -> bool {
    if reply == 0 {
        return false;
    }
    let mut ledger = DONATION_LEDGER.lock();
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
    donor_task_id: u64,
    receiver_task_id: u64,
    receiver_slot: usize,
) -> bool {
    if reply == 0 || donor_task_id == receiver_task_id || receiver_slot >= MAX_TASK {
        return false;
    }
    let mut ledger = DONATION_LEDGER.lock();
    let Some(index) = ledger.find_reservation(donor_task_id) else {
        return false;
    };
    let entry = ledger.entries[index]
        .as_mut()
        .expect("scheduler donation reservation disappeared");
    entry.donation.reply = reply;
    entry.donation.target = IpcDonationTarget::BoundWorker(receiver_task_id);
    entry.donation.custody_active = true;
    entry.receiver_slot = Some(receiver_slot);
    publish_charge_token(
        receiver_slot,
        entry.donation.context_owner_slot,
        entry.donation.reply,
    );
    if entry.donation.priority_donated {
        increment_receiver(receiver_slot);
    }
    true
}

pub(super) fn upsert(
    reply: u64,
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
    if let Some(index) = ledger.find_reply(reply) {
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
            );
            if previous.donation.priority_donated {
                decrement_receiver(previous_slot);
            }
        }
        publish_charge_token(receiver_slot, context_owner_slot, reply);
        if priority_donated {
            increment_receiver(receiver_slot);
        }
        return true;
    }
    if !ledger.insert(LedgerEntry {
        donation: IpcPriorityDonation {
            reply,
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
    publish_charge_token(receiver_slot, context_owner_slot, reply);
    if priority_donated {
        increment_receiver(receiver_slot);
    }
    true
}

pub(super) fn release_reply(reply: u64) -> bool {
    if reply == 0 {
        return false;
    }
    let mut ledger = DONATION_LEDGER.lock();
    let Some(index) = ledger.find_reply(reply) else {
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
        );
        if entry.donation.priority_donated {
            decrement_receiver(receiver_slot);
        }
    }
}

fn publish_charge_token(receiver_slot: usize, context_owner_slot: usize, reply: u64) {
    assert!(
        reply != 0,
        "borrowed scheduling context requires a live reply"
    );
    let encoded = u16::try_from(context_owner_slot).expect("context owner slot exceeds u16");
    BORROWED_CONTEXT_OWNER[receiver_slot].store(encoded, Ordering::Relaxed);
    BORROWED_CONTEXT_REPLY[receiver_slot].store(reply, Ordering::Release);
}

fn restore_context_owner_after_release(
    ledger: &DonationLedger,
    receiver_slot: usize,
    context_owner_slot: usize,
    reply: u64,
) {
    let encoded = u16::try_from(context_owner_slot).expect("context owner slot exceeds u16");
    let replacement = ledger.entries[..ledger.len]
        .iter()
        .rev()
        .flatten()
        .find(|entry| entry.donation.custody_active && entry.receiver_slot == Some(receiver_slot))
        .map(|entry| (entry.donation.context_owner_slot, entry.donation.reply));
    if BORROWED_CONTEXT_OWNER[receiver_slot].load(Ordering::Relaxed) != encoded
        || BORROWED_CONTEXT_REPLY[receiver_slot].load(Ordering::Relaxed) != reply
    {
        return;
    }
    if let Some((owner_slot, reply)) = replacement {
        publish_charge_token(receiver_slot, owner_slot, reply);
    } else {
        BORROWED_CONTEXT_OWNER[receiver_slot].store(NO_BORROWED_CONTEXT, Ordering::Relaxed);
        BORROWED_CONTEXT_REPLY[receiver_slot].store(0, Ordering::Release);
    }
}

#[cfg(test)]
mod tests {
    use core::sync::atomic::Ordering;
    use std::sync::Mutex;

    use super::{
        attach, bind_reserved, borrowed_context_charge_token, borrowed_context_owner_slot,
        inherited_system, release_reply, release_task, reserve, reset, upsert,
    };

    static TEST_GUARD: Mutex<()> = Mutex::new(());

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
        assert!(bind_reserved(700, 41, 52, 5));
        assert!(inherited_system(5));
        assert!(!inherited_system(6));

        assert!(upsert(700, 41, 53, 6, 41, 4, true));
        assert!(
            !inherited_system(5),
            "rebind retained the old receiver boost"
        );
        assert!(inherited_system(6));
        assert!(release_reply(700));
        assert!(!inherited_system(6));
        assert!(
            !release_reply(700),
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
        assert!(attach(701, 61));
        assert!(upsert(701, 61, 62, 7, 61, 6, true));
        assert!(inherited_system(7));
        release_task(62);
        assert!(!inherited_system(7));
        assert!(!release_reply(701));
    }

    #[test]
    fn ordinary_nested_calls_borrow_one_root_context_without_system_promotion() {
        let _guard = TEST_GUARD
            .lock()
            .expect("donation ledger test lock poisoned");
        reset();

        assert!(reserve(71, 71, 4, false));
        assert!(bind_reserved(710, 71, 72, 5));
        assert_eq!(borrowed_context_owner_slot(5), Some(4));
        assert!(!inherited_system(5));

        assert!(reserve(72, 71, 4, false));
        assert!(bind_reserved(711, 72, 73, 6));
        assert_eq!(borrowed_context_owner_slot(6), Some(4));
        assert!(!inherited_system(6));

        assert!(release_reply(710));
        assert_eq!(borrowed_context_owner_slot(5), None);
        assert_eq!(borrowed_context_owner_slot(6), None);
        assert!(
            release_reply(711),
            "revoked descendant reply must retain one terminal identity"
        );
        assert!(!release_reply(711));
    }

    #[test]
    fn root_reply_revokes_every_nested_priority_boost_exactly_once() {
        let _guard = TEST_GUARD
            .lock()
            .expect("donation ledger test lock poisoned");
        reset();

        assert!(reserve(91, 91, 4, true));
        assert!(bind_reserved(910, 91, 92, 5));
        assert!(reserve(92, 91, 4, true));
        assert!(bind_reserved(911, 92, 93, 6));
        assert!(inherited_system(5));
        assert!(inherited_system(6));

        assert!(release_reply(910));
        assert!(!inherited_system(5));
        assert!(!inherited_system(6));
        assert!(release_reply(911));
        assert!(!release_reply(911));
    }

    #[test]
    fn multithreaded_server_charge_tokens_restore_the_previous_live_reply() {
        let _guard = TEST_GUARD
            .lock()
            .expect("donation ledger test lock poisoned");
        reset();

        assert!(reserve(81, 81, 4, false));
        assert!(bind_reserved(810, 81, 90, 9));
        assert_eq!(borrowed_context_owner_slot(9), Some(4));
        assert_eq!(borrowed_context_charge_token(9), Some((4, 810)));

        assert!(reserve(82, 82, 6, false));
        assert!(bind_reserved(811, 82, 90, 9));
        assert_eq!(borrowed_context_owner_slot(9), Some(6));
        assert_eq!(borrowed_context_charge_token(9), Some((6, 811)));

        assert!(release_reply(810));
        assert_eq!(borrowed_context_owner_slot(9), Some(6));
        assert_eq!(borrowed_context_charge_token(9), Some((6, 811)));
        assert!(release_reply(811));
        assert_eq!(borrowed_context_owner_slot(9), None);
        assert_eq!(borrowed_context_charge_token(9), None);
        assert_eq!(super::BORROWED_CONTEXT_REPLY[9].load(Ordering::Acquire), 0);
    }
}
