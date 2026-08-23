//! Bounded public scheduling-context values shared across kernel-ps modules.

/// Kernel-internal form of a rootd-authored temporal authority. Creation is
/// restricted to the compat broker after it consumes a kernel-sealed grant.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SchedulingContextAdmission {
    pub budget_ns: u64,
    pub period_ns: u64,
    pub refill_capacity: u8,
    pub cpu_mask: u64,
    pub criticality: u8,
    pub domain: u64,
    pub policy_epoch: u64,
    pub timeout_endpoint_cap: u64,
}

/// Read-only accounting evidence for the exact scheduling context currently
/// consumed by the caller, including its last bounded timeout disposition.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SchedulingContextRuntimeSnapshot {
    pub executing_task_id: u64,
    pub context_owner_task_id: u64,
    pub context_identity_slot: u64,
    pub context_identity_generation: u64,
    pub domain: u64,
    pub policy_epoch: u64,
    pub budget_ns: u64,
    pub period_ns: u64,
    pub context_available_ns: u64,
    pub context_pending_refill_ns: u64,
    pub context_next_eligible_ns: u64,
    pub context_consumed_ns: u64,
    pub context_exhaustion_count: u64,
    pub context_refill_count: u64,
    pub context_overflow_merge_count: u64,
    pub timeout_fault_count: u64,
    pub timeout_fault_consumed_ns: u64,
    pub timeout_fault_budget_ns: u64,
    pub timeout_fault_period_ns: u64,
    pub timeout_fault_reply: u64,
    pub timeout_endpoint_cap: u64,
    pub timeout_fault_action: u64,
    pub domain_available_ns: u64,
    pub domain_pending_refill_ns: u64,
    pub domain_next_eligible_ns: u64,
    pub domain_consumed_ns: u64,
    pub domain_exhaustion_count: u64,
    pub domain_refill_count: u64,
    pub domain_overflow_merge_count: u64,
}
