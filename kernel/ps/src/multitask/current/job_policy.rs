//! Current-subject qualitative ceiling; never positive broker authority.
//!
//! - Owner: the scheduler installs Current; the process table owns membership.
//! - Boundary: exact published process/MM identity selects an immutable JobId.
//! - Lifecycle: missing exec/retirement publication refuses without recovery.
//! - Concurrency: local IRQ masking stabilizes Current, not remote lifecycle;
//!   exact membership is revalidated around the generation-bound cache check.
//! - Failure: unadmitted, preemption-disabled, absent or stale subjects refuse.
//! - Forbidden: no PID scan, state borrow, allocation or table/registry lock.
//! - Evidence: native binding refusal and job-policy-publication witnesses;
//!   opt-in boot probe checks the actual installed Current adapter.
use super::{
    current_cpu_task_slot_admitted, owned_current_user_binding,
    published_current_identity, process_table,
};
use x86_64::instructions::interrupts;

/// No PID lookup, state borrow, allocation or table/registry lock. Missing
/// publication (including exec transitions) refuses rather than falling back
/// to the lifecycle table or interpreting absent membership as the root job.
pub fn current_user_job_policy_allows(operation: u16) -> bool {
    if nucleus_core::util::lockdep::preemption_disabled() || !current_cpu_task_slot_admitted() {
        return false;
    }
    interrupts::without_interrupts(|| {
        let binding = match published_current_identity() {
            Some(identity) => identity.user_binding(),
            None => owned_current_user_binding(),
        };
        let Some((_, _, handle, _)) = binding else { return false };
        policy_for_binding(handle, operation)
    })
}

pub(in crate::multitask) fn policy_for_binding(
    handle: process_table::ProcessHandle, operation: u16,
) -> bool {
    let Some(job) = process_table::published_process_job(handle) else { return false };
    if !process_table::jobs::job_policy_allows(job, operation) {
        return false;
    }
    // Remote lifecycle changes are not excluded by local IRQ masking.
    // Revalidate the process incarnation around the cached policy decision.
    process_table::published_process_job(handle) == Some(job)
}
