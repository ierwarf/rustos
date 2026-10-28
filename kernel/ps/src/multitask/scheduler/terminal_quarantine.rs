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
    if !withdraw(&mut owner) {
        owner.park_unwithdrawn();
        return;
    }
    owner.defer();
}

/// Stop every fresh identity/execution/Cohort admission BEFORE borrower drain.
/// Retain the same non-Copy original producer across election without an
/// unlocked defer/reclaim gap; no other claimant can spend this stop.
///
/// `Err` returns the untouched producer: another CPU's dispatch claim of this
/// task is in flight and withdrawing now would retire the owner word under it.
pub(super) fn retain_for_finalization(mut owner: Producer) -> Result<Producer, Producer> {
    if !withdraw(&mut owner) {
        return Err(owner);
    }
    Ok(owner.retain_stopped())
}

/// Returns false, having changed nothing, while a remote dispatch claim of the
/// task is in flight. The run-owner retirement is first and is itself the check
/// (see `runqueue::retire_unless_claim_in_flight`), because a check made before
/// it could be overtaken by the claim.
fn withdraw(owner: &mut Producer) -> bool {
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
    if runqueue::retire_unless_claim_in_flight(slot, runqueue::weight::value(slot))
        == runqueue::RetireOutcome::ClaimInFlight
    {
        return false;
    }
    owner.begin_effects();
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
    true
}

#[cfg(test)]
mod tests;
