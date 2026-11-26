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
use core::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering, fence};

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
/// True while some slot may be non-idle. Set after the slot word, cleared only by
/// `settle`, which then re-scans, so a reader that sees it clear linearizes before the
/// reserve that raced it.
static PENDING: AtomicBool = AtomicBool::new(false);

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
    // ORDERING: the slot word is published before the hint, so a settle that
    // clears the hint and re-scans cannot miss it.
    PENDING.store(true, Ordering::Release);
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
/// would have produced up to now.
pub(super) fn settle(ledger: &mut DonationLedger) {
    // ORDERING: Acquire checks whether any lazy edge was published since last settle.
    if !PENDING.load(Ordering::Acquire) {
        return;
    }
    PENDING.store(false, Ordering::Relaxed);
    // ORDERING: SeqCst separates the hint clear from the slot scan (Dekker with `reserve`).
    fence(Ordering::SeqCst);
    let mut busy = false;
    for slot in task_table::allocated_slots() {
        let Some(edge) = edge_for(slot) else { continue; };
        for _ in 0..4 {
            match edge.state.load(Ordering::Acquire) {
                IDLE => break,
                RESERVED => {
                    if edge
                        .state
                        .compare_exchange(RESERVED, SETTLING, Ordering::AcqRel, Ordering::Relaxed)
                        .is_err()
                    {
                        continue;
                    }
                    let id = edge.donor_id.load(Ordering::Relaxed);
                    let reserved = reserve_locked(ledger, id, id, slot, false, Some(own_basis(slot, id)));
                    debug_assert!(reserved, "lazy reservation lost its ledger entry");
                    edge.state.store(IDLE, Ordering::Release);
                    break;
                }
                BOUND => {
                    if edge
                        .state
                        .compare_exchange(BOUND, SETTLING, Ordering::AcqRel, Ordering::Relaxed)
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
                            target: IpcDonationTarget::BoundWorker(edge.receiver_id.load(Ordering::Relaxed)),
                        },
                        receiver_slot: Some(receiver_slot),
                        funding: Some(own_basis(slot, id)),
                    });
                    debug_assert!(inserted, "lazy binding lost its ledger entry");
                    edge.state.store(IDLE, Ordering::Release);
                    break;
                }
                _ => {
                    busy = true;
                    break;
                }
            }
        }
        if edge.state.load(Ordering::Relaxed) != IDLE {
            busy = true;
        }
    }
    if busy {
        PENDING.store(true, Ordering::Release);
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
    // ORDERING: Release clears pending hint after resetting all allocated edges.
    PENDING.store(false, Ordering::Release);
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
}
