//! Owner: original exec intent/frozen membership and exact sibling terminal producers.
//! Boundary: captured UIDs are observations; only original producers authorize stop/detach.
//! Lifecycle: sealed target -> immutable census -> stop -> borrower drain -> detach publication.
//! Concurrency: caller retains original IRQ/preemption and dispatch serializer; no new lock.
//! Failure: busy producers/election or old execution borrowers keep the barrier pending.
//! Forbidden: retired bit, missing descriptor or copied UID as physical quiescence/reuse ACK.
//! Evidence: actual root/thread/terminal owners; guarded read/return and hardware remain distinct.
use super::*;
use crate::multitask::{
    cpu_local, current_identity as identity, process_placement::exec_membership::ExecMembership,
};

/// One independently owned retry. The original intent/census remain outside;
/// the CPU owner is released BEFORE cancellation, external ACK or scheduling.
pub(in crate::multitask) fn retry(
    intent: &identity::exec_intent::Intent,
    census: &ExecMembership,
) -> Option<bool> {
    let mut preemption = nucleus_core::util::lockdep::DispatchPreemption::enter();
    preemption.mask_irqs();
    let result = quiesce(&preemption, intent, census, &runqueue_policy::online_affinity_mask);
    // Actual queue/request publication may defer an IPI while pinned. No leaf
    // or directory lock survives here; emit before restoring this CPU's IF.
    #[cfg(not(test))]
    crate::multitask::irq::flush_deferred_target_reschedules();
    result
}

/// Borrow the ORIGINAL non-migratable masked CPU owner for one retry only.
/// Intent and census survive scheduling between retries; this CPU loan does not.
/// No directory, scratch Current or new post-stop resource pin is acquired.
pub(in crate::multitask) fn quiesce(
    preemption: &nucleus_core::util::lockdep::DispatchPreemption<'_>,
    intent: &identity::exec_intent::Intent,
    census: &ExecMembership,
    eligible_cpus: &dyn Fn(usize) -> u64,
) -> Option<bool> {
    if !preemption.is_masked()
        || preemption.cpu() != nucleus_core::util::lockdep::current_cpu_index()
    {
        return None;
    }
    // SAFETY: this original borrowed owner excludes local IRQ/dispatch reentry
    // until the engine returns. The actual census fences physical reuse; the
    // same terminal producer closes fresh loans before draining old borrowers.
    unsafe { guarded(intent, census, eligible_cpus) }
}

/// # Safety
/// Caller retains same-CPU masked IRQ/preemption custody through all effects.
/// Intent owns the target image and the original census fences physical reuse.
/// Terminal's original seal/drain owns source lifetime; recipient BindingPin
/// and FrameReader own recipient lifetime and final queue-claim arbitration.
/// This compatibility entry does not grant execution from a scalar Current.
pub(in crate::multitask::scheduler) unsafe fn guarded(
    intent: &identity::exec_intent::Intent,
    census: &ExecMembership,
    eligible_cpus: &dyn Fn(usize) -> u64,
) -> Option<bool> {
    census.assert_matches(intent);
    let target = intent.slot();
    let handle = intent.reservation().process_handle();
    let current = cpu_local::current_cpu_task_slot()?;
    if !intent.mode_matches(target == current) {
        return None;
    }
    let target_context = stack_resources::context::read(target)?;
    if identity::lifecycle(target).retired
        || retirement_cleanup::deferred(target).is_some()
        || retirement_cleanup::pending(target)
        || retirement_cleanup::kernel_pending(target)
        || retirement_cleanup::reason(target).is_some()
        || !target_context.user_mode
        || target_context.process_handle != Some(handle)
        || stack_resources::task_start::read(target).map(|s| s.id) != Some(intent.task_id())
        || target_context.process_id != Some(intent.task_id())
    {
        return None;
    }
    // A scalar Current match is not permission to retire the caller's live
    // stack. Refuse before spending ANY sibling producer.
    if census
        .members()
        .iter()
        .any(|&(slot, _)| slot == current && slot != target)
    {
        return None;
    }
    let mut waiting = false;
    for &(slot, id) in census.members() {
        if stack_resources::task_start::read(slot).map(|s| s.id) != Some(id) {
            // The original physical census excludes reuse of a captured UID.
            // Absence alone cannot ACK an old retained stack/bank.
            if stack_resources::task_start::read(slot).is_none()
                && (cpu_local::task_execution_owner(slot).is_some()
                    || identity::execution::count(slot) != 0
                    || stack_resources::allocated(slot))
            {
                waiting = true;
                request_owner(slot);
            }
            continue;
        }
        let Some(context) = stack_resources::context::read(slot) else {
            waiting = true;
            request_owner(slot);
            continue;
        };
        let retired = identity::lifecycle(slot).retired;
        // Original detach can remove the handle, not the frozen UID. Such a
        // retired captured member must STILL drain its physical execution bank.
        if context.process_handle != Some(handle) && !(retired && context.process_handle.is_none())
        {
            continue;
        }
        if slot != target && (!retired || retirement_cleanup::deferred(slot).is_some()) {
            let Some(owner) =
                terminal_quarantine::begin(slot, Some(id), TaskRetireReason::Exited, true)
            else {
                waiting = true;
                request_owner(slot);
                continue;
            };
            // SAME non-Copy producer closes fresh admission BEFORE its borrower
            // check and retains detach payload through busy election and retry.
            // SAFETY: original enclosing masked CPU custody remains; producer
            // and census independently retain the exact source resources.
            unsafe {
                terminal_publication::guarded(owner, eligible_cpus);
            }
        }
        let executing = cpu_local::task_execution_owner(slot).is_some();
        if (slot != current && (executing || identity::execution::count(slot) != 0))
            || (slot != target && retirement_cleanup::deferred(slot).is_some())
        {
            waiting = true;
            request_owner(slot);
        }
    }
    Some(!waiting)
}

fn request_owner(slot: usize) {
    let target = cpu_local::task_execution_owner(slot)
        .map(|role| match role {
            cpu_local::TaskExecutionOwner::Current(cpu)
            | cpu_local::TaskExecutionOwner::Transition(cpu) => cpu,
        })
        .or_else(|| runqueue::owner(slot).cpu);
    #[cfg(not(test))]
    if let Some(cpu) = target {
        crate::multitask::irq::request_target_reschedule(cpu);
    }
    #[cfg(test)]
    let _ = target;
}

#[cfg(test)]
pub(in crate::multitask::scheduler) mod tests;
