//! Lazily published own-funded IPC donation edges.
//!
//! - **Owner:** this module for the *publication* of an ordinary-class, self-funded
//!   register-IPC edge; the ledger stays the only authority for every other reader.
//! - **Lifecycle:** `reserve` (call side) -> `bind` (lane call) -> `release` (lane reply),
//!   all lock-free, or `settle` at the first ledger operation of any kind, which
//!   inserts exactly the entries `reserve_own` and `bind_own_funding` would have
//!   inserted and leaves the state idle.
//! - **Concurrency:** one slot per donor. Lane and call side move a slot with CAS;
//!   `settle` runs under the ledger leaf, so a lock-free step and a settle exclude each
//!   other through the slot word, and the loser takes the locked path.
//! - **Failure:** any refusal changes nothing and the caller continues on the eager
//!   path, which settles first; no edge is ever dropped.
//! - **Forbidden:** a class or priority edge (never lazy), a receiver that already
//!   carries a borrowed context, reading or writing the ledger without `settle`.
//! - **Evidence:** `tests::lazy_edge_settles_to_the_eager_entries`.
use crate::multitask::task_table;
use super::{
    publish_charge_token, reserve_locked, namespace_tag,
    DonationLedger, DonationNamespace, FundingBasis, IpcDonationTarget, IpcPriorityDonation,
    LedgerEntry,
};
use core::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

const IDLE: u64 = 0;
const RESERVED: u64 = 1;
const BOUND: u64 = 2;
/// Held only by `settle` under the ledger leaf; lock-free steps treat it as a loss.
const SETTLING: u64 = 3;

pub(in crate::multitask) struct Edge {
    state: AtomicU64,
    donor_id: AtomicU64,
    reply: AtomicU64,
    receiver_slot: AtomicUsize,
    receiver_id: AtomicU64,
}
impl Edge {
    pub(in crate::multitask) const fn new() -> Self {
        Self {
            state: AtomicU64::new(IDLE),
            donor_id: AtomicU64::new(0),
            reply: AtomicU64::new(0),
            receiver_slot: AtomicUsize::new(0),
            receiver_id: AtomicU64::new(0),
        }
    }
}

#[inline]
fn edge_for(slot: usize) -> Option<&'static Edge> {
    task_table::try_get(slot).map(|task| &*task.lazy_edge)
}
/// 3-level lock-free radix-64 dirty-donor bitmap covering all `MAX_TASK_SLOTS`
/// (262,144 slots = 4096 L1 words = 64 L0 words = 1 root word).
///
/// Writers (`reserve`, or `settle` re-marking a busy slot) set bits bottom-up
/// with SeqCst (`L1 -> L0 -> ROOT`); `settle` drains top-down with SeqCst
/// swaps (`ROOT -> L0 -> L1`). By the total SeqCst order, a concurrent writer
/// either sets `L1` before `settle`'s `L1.swap(0)` (so `settle` visits it) or
/// sets `L0`/`ROOT` after `settle`'s swaps (leaving the path set for the next
/// `settle`). `settle` therefore visits only `O(dirty)` slots under the global
/// IRQ-off `DONATION_LEDGER` lock.
const L1_WORDS: usize = task_table::MAX_TASK_SLOTS / 64;
const L0_WORDS: usize = L1_WORDS / 64;

static DIRTY_ROOT: AtomicU64 = AtomicU64::new(0);
static DIRTY_L0: [AtomicU64; L0_WORDS] = [const { AtomicU64::new(0) }; L0_WORDS];
static DIRTY_L1: [AtomicU64; L1_WORDS] = [const { AtomicU64::new(0) }; L1_WORDS];

#[inline]
fn mark_dirty(slot: usize) {
    debug_assert!(slot < task_table::MAX_TASK_SLOTS);
    let l1_idx = slot >> 6;
    let l1_bit = 1_u64 << (slot & 63);
    let l0_idx = l1_idx >> 6;
    let l0_bit = 1_u64 << (l1_idx & 63);
    let root_bit = 1_u64 << l0_idx;
    // ORDERING: SeqCst bottom-up pairs with top-down SeqCst drain in `settle`.
    DIRTY_L1[l1_idx].fetch_or(l1_bit, Ordering::SeqCst);
    DIRTY_L0[l0_idx].fetch_or(l0_bit, Ordering::SeqCst);
    DIRTY_ROOT.fetch_or(root_bit, Ordering::SeqCst);
}

fn own_basis(slot: usize, id: u64) -> FundingBasis {
    FundingBasis { donor_slot: slot, donor_id: id, root_slot: slot, root_id: id, source: None }
}

/// Reserve the donor's single edge without touching the ledger. `false` leaves
/// the decision to `reserve_own`.
#[cfg(any(rustos_boot_image, test))]
pub(in super::super) fn reserve(slot: usize, id: u64) -> bool {
    let Some(edge) = edge_for(slot) else { return false };
    if edge.state.load(Ordering::Relaxed) != IDLE {
        return false;
    }
    edge.donor_id.store(id, Ordering::Relaxed);
    if edge.state.compare_exchange(IDLE, RESERVED, Ordering::AcqRel, Ordering::Relaxed).is_err() {
        return false;
    }
    // ORDERING: the slot word is published before the dirty path so `settle`
    // draining the path always observes `RESERVED` (or a later state).
    mark_dirty(slot);
    true
}

/// Bind the donor's lazily reserved edge to `reply` and the receiver. `None`: not
/// lazily reserved or lost a race; the caller binds through the ledger.
pub(in super::super) fn bind(
    donor_slot: usize,
    donor_id: u64,
    reply: u64,
    receiver_slot: usize,
    receiver_id: u64,
) -> Option<bool> {
    let edge = edge_for(donor_slot)?;
    if edge.state.load(Ordering::Acquire) != RESERVED
        || edge.donor_id.load(Ordering::Relaxed) != donor_id
        || task_table::try_get(receiver_slot).is_none()
        || super::borrowed_context_charge_token(receiver_slot).is_some()
    {
        return None;
    }
    edge.reply.store(reply, Ordering::Relaxed);
    edge.receiver_slot.store(receiver_slot, Ordering::Relaxed);
    edge.receiver_id.store(receiver_id, Ordering::Relaxed);
    // The token is published with the bind, as the ledger bind publishes it under
    // its leaf, and withdrawn if the slot was settled meanwhile.
    publish_charge_token(receiver_slot, donor_slot, reply, DonationNamespace::IpcReply);
    if edge.state.compare_exchange(RESERVED, BOUND, Ordering::AcqRel, Ordering::Relaxed).is_err() {
        clear_token(receiver_slot, donor_slot, reply);
        return None;
    }
    Some(true)
}

/// Release the donor's bound edge for `reply`. `false`: not lazily bound (or already
/// settled); the caller releases through the ledger.
pub(in super::super) fn release(donor_slot: usize, donor_id: u64, reply: u64) -> bool {
    let Some(edge) = edge_for(donor_slot) else { return false };
    if edge.state.load(Ordering::Acquire) != BOUND
        || edge.reply.load(Ordering::Relaxed) != reply
        || edge.donor_id.load(Ordering::Relaxed) != donor_id
    {
        return false;
    }
    let receiver_slot = edge.receiver_slot.load(Ordering::Relaxed);
    if edge.state.compare_exchange(BOUND, IDLE, Ordering::AcqRel, Ordering::Relaxed).is_err() {
        return false;
    }
    clear_token(receiver_slot, donor_slot, reply);
    true
}

/// Withdraw the receiver's borrowed-context words if they still name this edge, the
/// no-replacement case of `restore_context_owner_after_release` (a lazy bind requires
/// the receiver to carry no other context, and any other writer settles us first).
fn clear_token(receiver_slot: usize, owner_slot: usize, reply: u64) {
    let encoded = u32::try_from(owner_slot).expect("context owner slot exceeds u32");
    let Some(receiver) = task_table::try_get(receiver_slot) else {
        return;
    };
    if receiver.borrowed_ctx_owner.load(Ordering::Relaxed) == encoded
        && receiver.borrowed_ctx_namespace.load(Ordering::Relaxed)
            == namespace_tag(DonationNamespace::IpcReply)
        && receiver.borrowed_ctx_reply.load(Ordering::Relaxed) == reply
    {
        receiver
            .borrowed_ctx_owner
            .store(task_table::NO_BORROWED_OWNER, Ordering::Relaxed);
        receiver.borrowed_ctx_namespace.store(0, Ordering::Relaxed);
        // ORDERING: Release clears borrowed-context reply word after owner/namespace clear.
        receiver.borrowed_ctx_reply.store(0, Ordering::Release);
    }
}

/// Make every lazy edge a ledger entry. Called with the ledger leaf held by every
/// ledger operation, so the operation sees exactly the entries the eager protocol
/// would have produced up to now. Visits only dirty donor slots (`O(dirty)`).
pub(super) fn settle(ledger: &mut DonationLedger) {
    // ORDERING: Acquire fast-path check avoids locked RMW when no lazy edges exist.
    if DIRTY_ROOT.load(Ordering::Acquire) == 0 {
        return;
    }
    // ORDERING: SeqCst top-down drain pairs with bottom-up SeqCst `mark_dirty`.
    let mut root = DIRTY_ROOT.swap(0, Ordering::SeqCst);
    while root != 0 {
        let l0_idx = root.trailing_zeros() as usize;
        root &= root - 1;
        let mut l0 = DIRTY_L0[l0_idx].swap(0, Ordering::SeqCst);
        while l0 != 0 {
            let l1_off = l0.trailing_zeros() as usize;
            l0 &= l0 - 1;
            let l1_idx = l0_idx * 64 + l1_off;
            let mut l1 = DIRTY_L1[l1_idx].swap(0, Ordering::SeqCst);
            while l1 != 0 {
                let bit = l1.trailing_zeros() as usize;
                l1 &= l1 - 1;
                let slot = l1_idx * 64 + bit;
                let Some(edge) = edge_for(slot) else { continue; };
                for _ in 0..4 {
                    match edge.state.load(Ordering::Acquire) {
                        IDLE => break,
                        RESERVED => {
                            if edge
                                .state
                                .compare_exchange(
                                    RESERVED,
                                    SETTLING,
                                    Ordering::AcqRel,
                                    Ordering::Relaxed,
                                )
                                .is_err()
                            {
                                continue;
                            }
                            let id = edge.donor_id.load(Ordering::Relaxed);
                            let reserved = reserve_locked(
                                ledger,
                                id,
                                id,
                                slot,
                                false,
                                Some(own_basis(slot, id)),
                            );
                            debug_assert!(reserved, "lazy reservation lost its ledger entry");
                            edge.state.store(IDLE, Ordering::Release);
                            break;
                        }
                        BOUND => {
                            if edge
                                .state
                                .compare_exchange(
                                    BOUND,
                                    SETTLING,
                                    Ordering::AcqRel,
                                    Ordering::Relaxed,
                                )
                                .is_err()
                            {
                                continue;
                            }
                            let id = edge.donor_id.load(Ordering::Relaxed);
                            let receiver_slot = edge.receiver_slot.load(Ordering::Relaxed);
                            let inserted = ledger.insert(LedgerEntry {
                                donation: IpcPriorityDonation {
                                    reply: edge.reply.load(Ordering::Relaxed),
                                    namespace: DonationNamespace::IpcReply,
                                    donor_task_id: id,
                                    context_owner_task_id: id,
                                    context_owner_slot: slot,
                                    priority_donated: false,
                                    custody_active: true,
                                    target: IpcDonationTarget::BoundWorker(
                                        edge.receiver_id.load(Ordering::Relaxed),
                                    ),
                                },
                                receiver_slot: Some(receiver_slot),
                                funding: Some(own_basis(slot, id)),
                            });
                            debug_assert!(inserted, "lazy binding lost its ledger entry");
                            edge.state.store(IDLE, Ordering::Release);
                            break;
                        }
                        _ => break,
                    }
                }
                if edge.state.load(Ordering::Relaxed) != IDLE {
                    mark_dirty(slot);
                }
            }
        }
    }
}

/// Forget every lazy edge (ledger reset).
pub(super) fn reset() {
    for slot in task_table::allocated_slots() {
        if let Some(edge) = edge_for(slot) {
            // ORDERING: Release resets edge state before re-enabling ledger tests.
            edge.state.store(IDLE, Ordering::Release);
        }
    }
    let mut root = DIRTY_ROOT.swap(0, Ordering::SeqCst);
    while root != 0 {
        let l0_idx = root.trailing_zeros() as usize;
        root &= root - 1;
        let mut l0 = DIRTY_L0[l0_idx].swap(0, Ordering::SeqCst);
        while l0 != 0 {
            let l1_off = l0.trailing_zeros() as usize;
            l0 &= l0 - 1;
            DIRTY_L1[l0_idx * 64 + l1_off].store(0, Ordering::Release);
        }
    }
}

/// Whether `slot` currently holds a lazy edge (native witnesses).
#[cfg(test)]
pub(in super::super) fn state(slot: usize) -> u64 {
    // ORDERING: Acquire pairs with edge publication/settle Release stores.
    edge_for(slot).map_or(IDLE, |edge| edge.state.load(Ordering::Acquire))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slot_words_are_distinct() {
        assert_eq!([IDLE, RESERVED, BOUND, SETTLING], [0, 1, 2, 3]);
    }

    #[test]
    fn dirty_donor_bitmap_tracks_sparse_high_and_low_slots_without_full_scan() {
        let _cpu = crate::multitask::cpu_local::test_publication_lock();
        let _queue = crate::multitask::scheduler::runqueue::test_serial_guard();
        reset();
        assert_eq!(DIRTY_ROOT.load(Ordering::Acquire), 0);

        let slots = [1_usize, 63, 64, 4095, 4096, 65535, task_table::MAX_TASK_SLOTS - 1];
        for &s in &slots {
            mark_dirty(s);
        }
        assert_ne!(DIRTY_ROOT.load(Ordering::Acquire), 0);

        let mut drained = alloc::vec::Vec::new();
        let mut root = DIRTY_ROOT.swap(0, Ordering::SeqCst);
        while root != 0 {
            let l0_idx = root.trailing_zeros() as usize;
            root &= root - 1;
            let mut l0 = DIRTY_L0[l0_idx].swap(0, Ordering::SeqCst);
            while l0 != 0 {
                let l1_off = l0.trailing_zeros() as usize;
                l0 &= l0 - 1;
                let l1_idx = l0_idx * 64 + l1_off;
                let mut l1 = DIRTY_L1[l1_idx].swap(0, Ordering::SeqCst);
                while l1 != 0 {
                    let bit = l1.trailing_zeros() as usize;
                    l1 &= l1 - 1;
                    drained.push(l1_idx * 64 + bit);
                }
            }
        }
        assert_eq!(drained, slots);
        assert_eq!(DIRTY_ROOT.load(Ordering::Acquire), 0);
    }
}
