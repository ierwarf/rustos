//! Owner: exact suspended activation transaction, independent of Scheduler directory.
//! Boundary: kernel mechanism only; caller service/registry retains activation policy.
//! Lifecycle: capture pinned Cohort -> frame preflight -> authority consumption ->
//! effects -> one admission Release -> slow unseal -> deferred notification.
//! Concurrency: per-slot Identity51 joins reservation/frame/affinity owners; own-CPU
//! preemption/IRQ unit spans copies, not any raw leaf across the caller callback.
//! Failure: refusal does not consume authority; post-effect abandonment quarantines.
//! Forbidden: unqualified facade, callback under resource/policy leaves, global serializer.
//! Evidence: native owner/component tests, production source/compiler routing;
//! neither bindings nor COMPLETE claim CR3/TSS/RSP acceptance or whole process exit.
use super::MAX_ATOMIC_ACTIVATION_HANDOFFS as CAP;
pub(super) mod publication;
use crate::multitask::current_identity::cohort::Cohort;

/// Called only while holding this CPU's Policy42 leaf. Empty-queue polling is
/// merely a latency hint; competing disjoint cohorts can fill it before here.
pub(super) fn enqueue_priority(
    policy: &mut super::dispatch_policy::CpuDispatchPolicy,
    record: super::handoff_queue::FirstTurnHandoff,
) -> bool {
    let Some(next) = policy.atomic_activation_handoff_remaining.checked_add(1) else {
        return false;
    };
    if !matches!(
        policy.atomic_activation_pick_hints.enqueue(record),
        Ok(true)
    ) {
        return false;
    }
    policy.atomic_activation_handoff_remaining = next;
    true
}

pub(super) struct Activation {
    members: [(usize, u64); CAP],
    len: usize,
    cohort: Cohort,
}
impl Activation {
    pub(super) fn capture(members: &[(usize, u64)]) -> Option<Self> {
        if members.is_empty() || members.len() > CAP {
            return None;
        }
        if !distinct_nonzero_ids(members.iter().map(|&(_, id)| id)) {
            return None;
        }
        let cohort = Cohort::begin_activation(members)?;
        let mut captured = [(0, 0); CAP];
        captured[..members.len()].copy_from_slice(members);
        Some(Self {
            members: captured,
            len: members.len(),
            cohort,
        })
    }
    #[cfg(test)]
    pub(super) fn commit_owned(
        self,
        turn: &nucleus_core::util::lockdep::DispatchPreemption,
        eligible_cpus: impl Fn(usize) -> u64,
        commit_authority: impl FnOnce(),
    ) -> bool {
        publication::commit(self, turn, eligible_cpus, || Some(commit_authority()))
    }
}
// Bounded input preflight, NOT binding or execution authority. Both entrances
// share this validator; Cohort still independently owns participant custody.
fn distinct_nonzero_ids(ids: impl Iterator<Item = u64>) -> bool {
    let mut prefix = [0; CAP];
    for (i, id) in ids.enumerate() {
        if i == CAP || id == 0 || prefix[..i].contains(&id) {
            return false;
        }
        prefix[i] = id;
    }
    true
}
pub(in crate::multitask) fn activate<G, F>(ids: &[u64], commit_authority: F) -> bool
where
    F: FnOnce() -> Option<G>,
{
    // Bound the public ID list before indexing the fixed original-member array.
    if ids.len() > CAP { return false; }
    assert!(
        crate::multitask::cpu_local::scheduler_initialized(),
        "activation preceded complete initial image"
    );
    let mut turn = nucleus_core::util::lockdep::DispatchPreemption::enter();
    turn.mask_irqs();
    if ids.is_empty() || !distinct_nonzero_ids(ids.iter().copied()) {
        return false;
    }
    let mut members = [(0, 0); CAP];
    for (i, &id) in ids.iter().enumerate() {
        // The directory accelerator is not authority: scan exact published
        // user identities, then retain the original physical binding and member.
        let Some(slot) = (super::FIRST_DYNAMIC_TASK_SLOT..super::MAX_TASK).find(|&slot| {
            crate::multitask::current_identity::read(slot)
                .is_some_and(|v| v.user_mode && v.task_id == Some(id))
        }) else {
            return false;
        };
        members[i] = (slot, id);
    }
    let Some(owner) = Activation::capture(&members[..ids.len()]) else {
        return false;
    };
    let activated = publication::commit(
        owner,
        &turn,
        super::runqueue_policy::online_affinity_mask,
        commit_authority,
    );
    crate::multitask::irq::flush_deferred_target_reschedules();
    activated
}
#[cfg(test)]
mod tests;
