//! How a milestone's loss is classified, and what that costs it.
//!
//! The acceptance harness reads some milestone frames as evidence and treats
//! their absence as a failure, while the per-dispatch measurements are
//! deliberately one-shot. Keeping that policy in one place is what lets the
//! emission path stay a single flow with one decision in it.
//!
//! - Owner: the debug sink owns bounded output attempts and deferred custody.
//! - Boundary: only exact acceptance names/families require sink delivery;
//!   classification grants no process or broker authority.
//! - Lifecycle: required records retain custody when immediate output contends.
//! - Concurrency: the serialized sink/deferred owner allocates output sequence;
//!   this pure selector neither waits nor acquires a lock.
//! - Failure: bounded queue exhaustion remains explicit loss, not proof success.
//! - Forbidden: no broad policy-marker prefix or one-shot scheduler promotion.
//! - Evidence: native exact-class tests and strict JobPolicy host acceptance;
//!   models do not prove physical output, queue capacity or arbitrary progress.

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum MilestoneOutputClass {
    BestEffort,
    Measurement,
    Required,
    QualificationCritical,
}

impl MilestoneOutputClass {
    /// Whether losing this milestone would invalidate acceptance evidence.
    pub(super) const fn must_reach_sink(self) -> bool {
        matches!(self, Self::Required | Self::QualificationCritical)
    }

    pub(super) const fn output_attempts(self) -> usize {
        match self {
            Self::Required | Self::QualificationCritical => {
                super::REQUIRED_MILESTONE_OUTPUT_ATTEMPTS
            }
            Self::BestEffort | Self::Measurement => 1,
        }
    }
}

pub(super) fn milestone_output_class(name: &str) -> MilestoneOutputClass {
    match name {
        "smp-qualification-ready"
        | "smp-qualification-start"
        | "smp-qualification-finish"
        | "smp-qualification-complete"
        | "hecs-registers"
        | "hecs-rsp-arrivals" => MilestoneOutputClass::QualificationCritical,
        _ if name.starts_with("kernel-scheduler-") => MilestoneOutputClass::Measurement,
        _ if name.starts_with("smp-")
            || name.starts_with("product-")
            || name.starts_with("sched-activation-")
            || name == "dvm-block-first-completion"
            || name == "dvm-block-transport-revoked"
            || name == "task-context-corrupted"
            || name == "linux-user-fault"
            || name == "linux-user-page-fault-address"
            || name == "pager-user-fault-vma-rejected"
            || name == "pager-user-anon-fault-refused"
            || name == "linux-thread-clone-rejected"
            || name == "job-policy-current-cache-checked"
            || name.starts_with("ipc-donation-")
            || name.starts_with("dvm-input-") =>
        {
            MilestoneOutputClass::Required
        }
        _ => MilestoneOutputClass::BestEffort,
    }
}

pub(super) fn milestone_loss_snapshot(
    output_class: MilestoneOutputClass,
    milestones_dropped: u64,
    discarded_bytes: u64,
    qualification_milestones_dropped: u64,
    qualification_discarded_bytes: u64,
) -> (u64, u64) {
    match output_class {
        MilestoneOutputClass::QualificationCritical => (
            qualification_milestones_dropped,
            qualification_discarded_bytes,
        ),
        _ => (milestones_dropped, discarded_bytes),
    }
}

/// Milestones emitted once per process lifecycle step (spawn, exec prepare/map/
/// commit, activation, termination). A debugcon frame is a synchronous VM exit
/// (about 3.5 ms for one milestone line), so a fork or exec loop that wrote all
/// of them would measure the console. They always reach the bounded in-memory
/// ring; only a sample reaches the wire. Failure evidence is deliberately not in
/// this family.
pub(super) fn per_process_lifecycle(name: &str) -> bool {
    matches!(
        name,
        "spawn-user-process"
            | "spawn-user-process-state"
            | "linux-process-termination"
            | "retired-task-ipc-cancelled"
            | "pager-anon-reservation"
            | "proc-prepare-published"
            | "proc-map-file-batch"
            | "proc-linux-runtime-published"
            | "proc-commit-address-space-begin"
            | "proc-commit-address-space-done"
            | "proc-commit-prepare-done"
            | "proc-activate-batch-member"
            | "proc-activate-batch-committed"
    ) || name.starts_with("lifecycle-")
}

/// Boot is fully visible; later occurrences are sampled at powers of two so the
/// stream still shows that the family is alive and how much of it there was.
pub(super) const LIFECYCLE_EARLY_EMISSIONS: u64 = 256;

pub(super) const fn lifecycle_sample_admits(occurrence: u64) -> bool {
    occurrence <= LIFECYCLE_EARLY_EMISSIONS || occurrence.is_power_of_two()
}
