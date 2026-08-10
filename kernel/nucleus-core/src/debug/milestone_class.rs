//! How a milestone's loss is classified, and what that costs it.
//!
//! The acceptance harness reads some milestone frames as evidence and treats
//! their absence as a failure, while the per-dispatch measurements are
//! deliberately one-shot. Keeping that policy in one place is what lets the
//! emission path stay a single flow with one decision in it.

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
        | "smp-qualification-complete" => MilestoneOutputClass::QualificationCritical,
        _ if name.starts_with("kernel-scheduler-") => MilestoneOutputClass::Measurement,
        _ if name.starts_with("smp-")
            || name.starts_with("product-")
            || name.starts_with("sched-activation-")
            || name == "dvm-block-first-completion"
            || name == "dvm-block-transport-revoked"
            || name == "task-context-corrupted"
            || name == "linux-user-fault"
            || name == "linux-thread-clone-rejected"
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
