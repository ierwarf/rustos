use super::*;

/// Allows a user helper to surrender its inherited System-class admission and
/// elevated permanent fair share.
///
/// There is intentionally no symmetric promotion syscall: executable launch
/// policy admits the initial class, and synchronous IPC inherits a class only
/// for the lifetime of its exact reply capability.  A caller can therefore
/// reduce contention but cannot manufacture privileged scheduling priority or
/// increase a pre-existing low fair weight.
pub(super) fn syscall_linux_rustos_sched_demote_self() -> u64 {
    if multitask::demote_current_user_task_to_user_class() {
        0
    } else {
        linux_errno(LINUX_EPERM)
    }
}

pub(super) fn syscall_sched_context_snapshot(out_ptr: u64) -> u64 {
    if out_ptr == 0 {
        return linux_errno(LINUX_EFAULT);
    }
    let Some(snapshot) = multitask::current_scheduling_context_runtime_snapshot() else {
        return linux_errno(LINUX_EAGAIN);
    };
    let wire = rustos_user_abi::syscall::RustosSchedulingContextSnapshot {
        abi_version: rustos_user_abi::syscall::SCHEDULING_CONTEXT_SNAPSHOT_ABI_VERSION,
        reserved0: 0,
        flags: 0,
        executing_task_id: snapshot.executing_task_id,
        context_owner_task_id: snapshot.context_owner_task_id,
        context_identity_slot: snapshot.context_identity_slot,
        context_identity_generation: snapshot.context_identity_generation,
        domain: snapshot.domain,
        policy_epoch: snapshot.policy_epoch,
        budget_ns: snapshot.budget_ns,
        period_ns: snapshot.period_ns,
        context_available_ns: snapshot.context_available_ns,
        context_pending_refill_ns: snapshot.context_pending_refill_ns,
        context_next_eligible_ns: snapshot.context_next_eligible_ns,
        context_consumed_ns: snapshot.context_consumed_ns,
        context_exhaustion_count: snapshot.context_exhaustion_count,
        context_refill_count: snapshot.context_refill_count,
        context_overflow_merge_count: snapshot.context_overflow_merge_count,
        timeout_fault_count: snapshot.timeout_fault_count,
        timeout_fault_consumed_ns: snapshot.timeout_fault_consumed_ns,
        timeout_fault_budget_ns: snapshot.timeout_fault_budget_ns,
        timeout_fault_period_ns: snapshot.timeout_fault_period_ns,
        timeout_fault_reply: snapshot.timeout_fault_reply,
        timeout_endpoint_cap: snapshot.timeout_endpoint_cap,
        timeout_fault_action: snapshot.timeout_fault_action,
        domain_available_ns: snapshot.domain_available_ns,
        domain_pending_refill_ns: snapshot.domain_pending_refill_ns,
        domain_next_eligible_ns: snapshot.domain_next_eligible_ns,
        domain_consumed_ns: snapshot.domain_consumed_ns,
        domain_exhaustion_count: snapshot.domain_exhaustion_count,
        domain_refill_count: snapshot.domain_refill_count,
        domain_overflow_merge_count: snapshot.domain_overflow_merge_count,
    };
    match usermem::write_current_user_struct(out_ptr, &wire) {
        Ok(()) => 0,
        Err(err) => linux_errno(address_space_error_to_linux_errno(err)),
    }
}
