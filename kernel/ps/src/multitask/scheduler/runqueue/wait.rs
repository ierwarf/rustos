//! Per-slot wait payload, separate from lifecycle catalog identity.
//!
//! The block state, ready/block timing, arm, and exact wait identity belong to
//! the owner-generation-bound execution slot rather than to the lifecycle
//! catalog. Admission writes this payload before owner publication; terminal
//! release erases it before reuse.
//!
//! The arm and the exact reason kind share **one** word. A racing wake clears
//! the arm, and the commit that follows refuses to sleep because of it, so the
//! two fields have to change together: with separate words a wake landing
//! between an arm's reason store and its arm store would leave an armed slot
//! with no reason, which is a lost wake rather than a lost hint. Holding the
//! catalog guard used to supply that atomicity; the packed word supplies it
//! directly, which is what lets the arm run without the guard at all.
//! Exact payload/arm publication additionally uses a per-slot checked epoch
//! leaf. Original typed wake holds a non-Copy pending phase (not that leaf)
//! through frame/queue publication; new arms refuse until that phase releases.
//! Boundary: exact Binding loan -> Wait58 payload/arm leaf -> queue after unlock.
//! Concurrency: checked epoch and pending phase arbitrate all exact payload reads.
//! Failure: terminal/foreign/exhausted admission refuses without payload mutation.
//! Forbidden: RunOwner generation as wait lifetime, raw leaves through frame/work.
//! Evidence: actual typed wake/arm CAS, physical resource and queue/FIFO witnesses.

use core::sync::atomic::{AtomicU64, Ordering};

use super::{MAX_TASK, RunOwnerState, owner};

mod epoch;
pub(in crate::multitask::scheduler) use epoch::TypedWake;

pub(in crate::multitask) const REASON_NONE: u8 = 0;
pub(in crate::multitask) const REASON_GENERIC: u8 = 1;
pub(in crate::multitask) const REASON_ENDPOINT_RECEIVE: u8 = 2;
pub(in crate::multitask) const REASON_ENDPOINT_REPLY: u8 = 3;
pub(in crate::multitask) const REASON_PAGER_FAULT: u8 = 4;
pub(in crate::multitask) const REASON_PAGER_SERVICE: u8 = 5;

static READY_SINCE_TICKS: [AtomicU64; MAX_TASK] = [const { AtomicU64::new(0) }; MAX_TASK];
static BLOCKED_SINCE_TICKS: [AtomicU64; MAX_TASK] = [const { AtomicU64::new(0) }; MAX_TASK];
static REASON_ID: [AtomicU64; MAX_TASK] = [const { AtomicU64::new(0) }; MAX_TASK];

pub(in crate::multitask) fn reset_before_publication() {
    for slot in 0..MAX_TASK {
        clear(slot);
    }
}

pub(in crate::multitask) fn initialize(slot: usize) {
    assert_eq!(
        owner(slot).state,
        RunOwnerState::Dormant,
        "scheduler wait payload initialized after owner publication"
    );
    clear(slot);
}

pub(in crate::multitask) fn clear(slot: usize) {
    epoch::with(slot, |state| {
        assert!(
            state.can_arm(),
            "wait reset passed original typed wake loan"
        );
        assert!(state.advance(), "scheduler wait reset epoch exhausted");
        // ORDERING: withdraw all scalar payload before slot reuse publication.
        REASON_ID[slot].store(0, Ordering::Release);
        BLOCKED_SINCE_TICKS[slot].store(0, Ordering::Release);
        READY_SINCE_TICKS[slot].store(0, Ordering::Release);
        super::clear_wait_arm(slot);
    });
}

#[inline]
pub(in crate::multitask) fn blocked(slot: usize) -> bool {
    super::wait_blocked(owner(slot))
}

#[inline]
pub(in crate::multitask) fn set_blocked(slot: usize, blocked: bool) {
    if blocked {
        super::set_runnable(slot, false);
    } else {
        super::wake_wait(slot);
    }
}

#[inline]
pub(in crate::multitask) fn ready_since_ticks(slot: usize) -> u64 {
    // ORDERING: pair with producer timing publication; timing grants no custody.
    READY_SINCE_TICKS[slot].load(Ordering::Acquire)
}

#[inline]
pub(in crate::multitask) fn set_ready_since_ticks(slot: usize, ticks: u64) {
    // ORDERING: publish advisory ready timing after admitted producer effects.
    READY_SINCE_TICKS[slot].store(ticks, Ordering::Release);
}

#[inline]
pub(in crate::multitask) fn blocked_since_ticks(slot: usize) -> u64 {
    // ORDERING: pair with wait-arm timing publication, never identity admission.
    BLOCKED_SINCE_TICKS[slot].load(Ordering::Acquire)
}

#[inline]
pub(in crate::multitask) fn set_blocked_since_ticks(slot: usize, ticks: u64) {
    // ORDERING: publish wait timing before packed owner/arm CAS.
    BLOCKED_SINCE_TICKS[slot].store(ticks, Ordering::Release);
}

#[inline]
pub(in crate::multitask) fn armed(slot: usize) -> bool {
    owner(slot).wait_armed
}

#[inline]
pub(in crate::multitask) fn set_armed(slot: usize, armed: bool) {
    epoch::with(slot, |state| {
        assert!(
            !armed || state.can_arm(),
            "legacy wait arm passed pending typed wake"
        );
        assert!(state.advance(), "scheduler wait epoch exhausted");
        super::set_wait_armed(slot, armed);
    });
}

#[inline]
pub(in crate::multitask) fn reason(slot: usize) -> (u8, u64) {
    epoch::with(slot, |_| reason_locked(slot))
}
fn reason_locked(slot: usize) -> (u8, u64) {
    let kind = owner(slot).wait_reason_kind;
    // ORDERING: Acquire, ordered after the packed load, observes the identity
    // its publisher stored before publishing the kind.
    let id = REASON_ID[slot].load(Ordering::Acquire);
    (kind, id)
}

fn validate_reason(kind: u8, id: u64) {
    assert!(
        matches!(
            kind,
            REASON_NONE
                | REASON_GENERIC
                | REASON_ENDPOINT_RECEIVE
                | REASON_ENDPOINT_REPLY
                | REASON_PAGER_FAULT
                | REASON_PAGER_SERVICE
        ),
        "scheduler wait payload has invalid reason kind"
    );
    assert!(
        !matches!(
            kind,
            REASON_ENDPOINT_RECEIVE | REASON_ENDPOINT_REPLY | REASON_PAGER_FAULT
        ) || id != 0,
        "scheduler typed wait reason has zero identity"
    );
}

pub(in crate::multitask) fn set_reason(slot: usize, kind: u8, id: u64) {
    validate_reason(kind, id);
    epoch::with(slot, |state| {
        assert!(
            kind == REASON_NONE || state.can_arm(),
            "legacy wait reason passed pending typed wake"
        );
        assert!(state.advance(), "scheduler wait epoch exhausted");
        // ORDERING: same leaf owns ID before packed reason publication.
        REASON_ID[slot].store(id, Ordering::Release);
        super::set_wait_reason(slot, kind);
    });
}

/// Publishes an arm and its exact reason in one store.
///
/// The identity is stored first, so any reader that observes the published
/// kind also observes the identity that belongs to it.
pub(in crate::multitask) fn publish_arm(slot: usize, kind: u8, id: u64) {
    assert!(
        try_publish_arm(slot, kind, id),
        "scheduler wait arm published without running ownership"
    );
}

/// A terminal stop may win after the original Current loan is captured.
/// Refuse without asserting or re-arming a terminal owner. The payload identity
/// is meaningful only with its successfully published reason kind.
pub(in crate::multitask) fn try_publish_arm(slot: usize, kind: u8, id: u64) -> bool {
    validate_reason(kind, id);
    epoch::with(slot, |state| {
        if !state.can_arm() || !state.advance() {
            return false;
        }
        // ORDERING: same epoch leaf owns payload through the arm/reason CAS.
        // A pending typed wake cannot be overtaken by any later arm.
        let previous_id = REASON_ID[slot].swap(id, Ordering::AcqRel);
        if super::publish_wait_arm(slot, kind) {
            return true;
        }
        // ORDERING: refused Running admission must not rewrite a blocked wait.
        // Every exact payload reader borrows this same leaf, so rollback is
        // invisible; terminal/return CAS never treats the scalar ID as custody.
        REASON_ID[slot].store(previous_id, Ordering::Release);
        false
    })
}

/// Withdraws an arm and its reason in one store.
///
/// The reason identity is deliberately left behind: it is meaningless without
/// a kind, every reader decodes the kind first, and the next arm overwrites it
/// before publishing a kind again. Clearing it would add a second store to the
/// one transition that has to stay indivisible against a racing wake.
pub(in crate::multitask) fn clear_arm(slot: usize) {
    epoch::with(slot, |state| {
        assert!(state.advance(), "scheduler wait cancel epoch exhausted");
        super::clear_wait_arm(slot);
    });
}

/// Shared untyped wake CAS also invalidates captured typed admission.
pub(super) fn wake(slot: usize) {
    epoch::with(slot, |state| {
        assert!(state.advance(), "scheduler wait wake epoch exhausted");
        super::wake_wait_locked(slot);
    });
}

fn clear_wake_locked(slot: usize) {
    super::wake_wait_locked(slot);
    // ORDERING: all scalar payload withdrawal remains inside this SAME leaf.
    BLOCKED_SINCE_TICKS[slot].store(0, Ordering::Release);
    REASON_ID[slot].store(0, Ordering::Release);
}
pub(in crate::multitask::scheduler) fn clear_wake(slot: usize) {
    epoch::with(slot, |state| {
        assert!(state.advance(), "scheduler wait wake epoch exhausted");
        clear_wake_locked(slot);
    });
}

pub(in crate::multitask) fn commit(slot: usize) -> super::WaitCommitOutcome {
    super::commit_wait(slot)
}
