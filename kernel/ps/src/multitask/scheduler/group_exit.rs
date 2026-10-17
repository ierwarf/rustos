//! Owner: independent consumption of durable process group exit requests.
//! Boundary: original census/process pin, exact member backing loan -> terminal producer.
//! Lifecycle: claim -> effects -> original suspension or exact desired-stop ACK.
//! Concurrency: creators drain before collection; no raw leaf spans observation or stop effects.
//! Failure: competing terminal/exec admission hands back the SAME census, never a fresh lookup.
//! Forbidden: Scheduler authority, copied PID as lifetime, absence as ACK, or yielding with loans.
//! Evidence: actual census/pin/terminal witnesses; IRQ/topology/external consumer ACKs remain models.
use super::*;
use crate::multitask::current_identity as identity;
use crate::multitask::process_placement::exit_membership::{self, ExitMembership, Reason};
use nucleus_core::util::lockdep::{DispatchPreemption, SchedulerObservation as Observation};

/// Borrows the original census as well as retaining the exact physical member.
/// Its copied context is attribution only, never an execution/stop permission.
struct MemberLoan<'census> {
    census: &'census ExitMembership,
    pin: identity::execution::BindingPin,
    context: TaskContext,
}
impl<'census> MemberLoan<'census> {
    fn capture(census: &'census ExitMembership, slot: usize, id: u64) -> Option<Self> {
        assert!(census.members().contains(&(slot, id)), "foreign group member");
        let pin = identity::execution::pin_donation(slot, id)?;
        let context = stack_resources::context::read(pin.slot())?;
        assert!(context.user_mode && context.process_handle == Some(census.handle()),
            "group exit lost exact pinned process member");
        Some(Self { census, pin, context })
    }
    fn observe(&self, reason: Reason) {
        let Reason::Exited { slot, task_id, process_id, logical_cpu } = reason else { return; };
        let member = self.pin.slot();
        if member == slot { return; }
        let ready_since_ticks = runqueue::wait::ready_since_ticks(member);
        let blocked_since_ticks = runqueue::wait::blocked_since_ticks(member);
        let lifecycle = identity::lifecycle(member);
        let state_flags = (u64::from(ready_since_ticks != 0) * Observation::STATE_READY)
            | (u64::from(runqueue::wait::blocked(member)) * Observation::STATE_BLOCKED)
            | (u64::from(runqueue::wait::armed(member)) * Observation::STATE_WAKE_ARMED)
            | (u64::from(lifecycle.suspended) * Observation::STATE_SUSPENDED)
            | (u64::from(lifecycle.job_stopped) * Observation::STATE_STOPPED)
            | (u64::from(lifecycle.retired) * Observation::STATE_RETIRED);
        nucleus_core::util::lockdep::record_scheduler_observation(logical_cpu, Observation {
            kind: nucleus_core::util::lockdep::SchedulerObservationKind::ExitSnapshot,
            subject_task: self.pin.id(), subject_pid: self.context.process_id.unwrap_or(0),
            subject_slot: member, peer_task: task_id, peer_pid: process_id, peer_slot: slot,
            elapsed_ms: 0, state_flags, ready_since_ticks, blocked_since_ticks,
        });
    }
    fn stop(self, reason: TaskRetireReason) -> Option<retirement_cleanup::producer::Producer> {
        assert!(!self.census.member_stopped((self.pin.slot(), self.pin.id())),
            "group member stopped before original loan transfer");
        // Original loan survives terminal admission. Drop it before borrower
        // drain; the original producer now excludes every new binding/reset.
        let producer = terminal_quarantine::begin(self.pin.slot(), Some(self.pin.id()), reason, false);
        drop(self);
        producer
    }
}

fn done(census: &ExitMembership, member: (usize, u64)) -> bool {
    census.member_stopped(member)
        || crate::multitask::current_identity::terminal_retired_matches(member.0, member.1)
}

/// The caller's original scope is retained through census handback/ACK and all
/// terminal effects. Current slot is an ordering observation only; the actual
/// terminal engine requires the physical bank for an own-Current retirement.
pub(in crate::multitask) fn service(scope: &DispatchPreemption<'_>) -> bool {
    assert!(scope.is_masked(), "group effects require original masked CPU scope");
    #[cfg(not(test))]
    assert_eq!(scope.cpu(), nucleus_core::util::lockdep::current_cpu_index());
    assert_eq!(nucleus_core::util::lockdep::held_spin_lock_depth(), 0,
        "group worker entered with a raw leaf");
    let Some((mut census, reason)) = exit_membership::claim_next() else { return false; };
    let current = crate::multitask::cpu_local::published_current_slot(scope.cpu());
    let retire_reason = match reason {
        Reason::Exited { .. } => TaskRetireReason::Exited,
        Reason::Terminated { requested_by_pid } => TaskRetireReason::Terminated { requested_by_pid },
    };
    if !census.effects_started() { census.begin_effects(); }
    // Two bounded passes: non-Current first, Current last. No UID is inferred
    // from requester/PID and no resumed census is recollected from live slots.
    for current_pass in [false, true] {
        for index in 0..census.members().len() {
            let member = census.members()[index];
            if (Some(member.0) == current) != current_pass || done(&census, member) { continue; }
            let Some(loan) = MemberLoan::capture(&census, member.0, member.1) else {
                if done(&census, member) { continue; }
                census.suspend();
                return false;
            };
            loan.observe(reason);
            let Some(producer) = loan.stop(retire_reason) else {
                if done(&census, member) { continue; }
                census.suspend();
                return false;
            };
            // SAFETY: borrowed ORIGINAL masked same-CPU scope remains live;
            // original producer, not a raw directory, owns source lifetime.
            unsafe { terminal_publication::guarded(producer, &runqueue_policy::online_affinity_mask); }
            assert!(done(&census, member), "group stop lost original desired withdrawal");
        }
    }
    census.finish();
    true
}

/// Non-yielding housekeeping also resumes original terminal producers whose
/// old callers cannot acquire a new executor. No external cleanup loan here.
pub(in crate::multitask) fn service_deferred(scope: &DispatchPreemption<'_>) -> bool {
    let completed = service(scope);
    // SAFETY: service validated this SAME borrowed masked CPU scope; no raw
    // leaf or member loan remains, and it is retained until the drain finishes.
    unsafe { terminal_publication::drain_guarded(&runqueue_policy::online_affinity_mask); }
    completed
}

#[cfg(test)]
mod tests;
