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

use core::sync::atomic::{AtomicU8, Ordering};

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

pub(super) fn reset() {
    let mut ledger = DONATION_LEDGER.lock();
    *ledger = DonationLedger::new();
    for inherited in &INHERITED_SYSTEM {
        inherited.store(0, Ordering::Release);
    }
}

pub(super) fn reserve(donor_task_id: u64) -> bool {
    let mut ledger = DONATION_LEDGER.lock();
    if ledger.find_reservation(donor_task_id).is_some() {
        return false;
    }
    ledger.insert(LedgerEntry {
        donation: IpcPriorityDonation {
            reply: 0,
            donor_task_id,
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
    entry.receiver_slot = Some(receiver_slot);
    increment_receiver(receiver_slot);
    true
}

pub(super) fn upsert(
    reply: u64,
    donor_task_id: u64,
    receiver_task_id: u64,
    receiver_slot: usize,
) -> bool {
    if reply == 0 || donor_task_id == receiver_task_id || receiver_slot >= MAX_TASK {
        return false;
    }
    let mut ledger = DONATION_LEDGER.lock();
    if let Some(index) = ledger.find_reply(reply) {
        let entry = ledger.entries[index]
            .as_mut()
            .expect("scheduler donation reply disappeared");
        if let Some(previous_slot) = entry.receiver_slot.replace(receiver_slot) {
            decrement_receiver(previous_slot);
        }
        entry.donation.donor_task_id = donor_task_id;
        entry.donation.target = IpcDonationTarget::BoundWorker(receiver_task_id);
        increment_receiver(receiver_slot);
        return true;
    }
    if !ledger.insert(LedgerEntry {
        donation: IpcPriorityDonation {
            reply,
            donor_task_id,
            target: IpcDonationTarget::BoundWorker(receiver_task_id),
        },
        receiver_slot: Some(receiver_slot),
    }) {
        return false;
    }
    increment_receiver(receiver_slot);
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
    release_entry(ledger.remove(index));
    true
}

pub(super) fn release_task(task_id: u64) {
    let mut ledger = DONATION_LEDGER.lock();
    let mut index = 0;
    while index < ledger.len {
        let entry = ledger.entries[index]
            .expect("scheduler donation ledger prefix contains an empty entry");
        if entry.donation.donor_task_id == task_id
            || matches!(entry.donation.target, IpcDonationTarget::BoundWorker(target) if target == task_id)
        {
            release_entry(ledger.remove(index));
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

fn release_entry(entry: LedgerEntry) {
    if let Some(receiver_slot) = entry.receiver_slot {
        decrement_receiver(receiver_slot);
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use super::{
        attach, bind_reserved, inherited_system, release_reply, release_task, reserve, reset,
        upsert,
    };

    static TEST_GUARD: Mutex<()> = Mutex::new(());

    #[test]
    fn bound_reply_inheritance_is_exactly_once_and_rebinds_without_leak() {
        let _guard = TEST_GUARD
            .lock()
            .expect("donation ledger test lock poisoned");
        reset();

        assert!(reserve(41));
        assert!(!reserve(41), "a donor may reserve only one live reply edge");
        assert!(bind_reserved(700, 41, 52, 5));
        assert!(inherited_system(5));
        assert!(!inherited_system(6));

        assert!(upsert(700, 41, 53, 6));
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

        assert!(reserve(61));
        assert!(attach(701, 61));
        assert!(upsert(701, 61, 62, 7));
        assert!(inherited_system(7));
        release_task(62);
        assert!(!inherited_system(7));
        assert!(!release_reply(701));
    }
}
