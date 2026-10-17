//! Owner: exact terminal producer from reservation through deferred stop publication.
//! Boundary: Terminal50 -> Identity51 -> Resources53, never a scheduler catalog.
//! Lifecycle: claim original UID/nonce -> retain binding -> withdraw -> defer original reason.
//! Concurrency: no raw leaf spans queue or notification work; producer seal excludes reuse.
//! Failure: untouched refusal rolls back, executor wait retains the original stop request.
//! Forbidden: copied slot authority, root retirement, reason replacement or fresh execution grant.
//! Evidence: actual root-born owner witnesses; native external completion is not hardware ACK.
use super::{TaskRetireReason, retirement_cleanup::producer::Producer, runqueue, stack_resources};
use crate::multitask::current_identity as identity;

pub(super) fn claim(slot: usize, id: Option<u64>) -> Option<Producer> {
    let owner = Producer::claim(slot, id)?;
    if !stack_resources::terminal_binding_matches(slot, owner.task_id()) {
        // Drop restores the original deferred/executor-wait phase; no new
        // reason, detached reference or hardware quiescence is synthesized.
        return None;
    }
    Some(owner)
}

pub(super) fn begin(
    slot: usize,
    id: Option<u64>,
    reason: TaskRetireReason,
    detach: bool,
) -> Option<Producer> {
    if let Some(owner) = claim(slot, id) {
        return Some(owner);
    }
    let owner = Producer::begin(slot, id, reason, detach)?;
    if !stack_resources::terminal_binding_matches(slot, owner.task_id()) {
        owner.wait_for_executor();
        return None;
    }
    Some(owner)
}

pub(super) fn request(slot: usize, id: u64, reason: TaskRetireReason) {
    assert_ne!(
        slot,
        super::ROOT_TASK_SLOT,
        "scheduler root kernel task cannot be quarantined"
    );
    if stack_resources::task_start::read(slot).map(|s| s.id) != Some(id) {
        return;
    }
    if let Some(owner) = begin(slot, Some(id), reason, false) {
        publish(owner);
    }
}

pub(super) fn publish(mut owner: Producer) {
    withdraw(&mut owner);
    owner.defer();
}

/// Stop every fresh identity/execution/Cohort admission BEFORE borrower drain.
/// Retain the same non-Copy original producer across election without an
/// unlocked defer/reclaim gap; no other claimant can spend this stop.
pub(super) fn retain_for_finalization(mut owner: Producer) -> Producer {
    withdraw(&mut owner);
    owner.retain_stopped()
}

fn withdraw(owner: &mut Producer) {
    let slot = owner.slot();
    assert_ne!(
        slot,
        super::ROOT_TASK_SLOT,
        "scheduler root kernel task cannot be quarantined"
    );
    let id = owner
        .task_id()
        .expect("terminal quarantine lacks exact UID");
    assert_eq!(
        stack_resources::task_start::read(slot).map(|s| s.id),
        Some(id),
        "terminal quarantine lost exact binding"
    );
    assert!(
        stack_resources::terminal_binding_matches(slot, Some(id)),
        "terminal quarantine lost original resource custody"
    );
    let context =
        stack_resources::context::read(slot).expect("terminal quarantine lost retained context");
    owner.begin_effects();
    runqueue::retire(slot, runqueue::weight::value(slot));
    assert!(
        identity::update_live_lifecycle(slot, id, identity::LifecycleGate::Retired, true),
        "terminal quarantine lost exact lifecycle authority"
    );
    // Withdraw before the deferred reason becomes visible. The descriptor is
    // retained, not detached, and no terminal consumer has received its ACK.
    runqueue::wait::set_ready_since_ticks(slot, 0);
    runqueue::wait::set_blocked(slot, false);
    runqueue::wait::set_blocked_since_ticks(slot, 0);
    runqueue::wait::set_armed(slot, false);
    runqueue::wait::set_reason(slot, 0, 0);
    assert_eq!(
        stack_resources::context::read(slot).map(|c| c.process_handle),
        Some(context.process_handle),
        "terminal stop detached process before publication"
    );
}

#[cfg(test)]
mod tests;
