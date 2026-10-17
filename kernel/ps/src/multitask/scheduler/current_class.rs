//! Owner: irreversible self-demotion under original physical Current/CPU custody.
//! Boundary: masked CPU -> Current bank -> Terminal50/Identity51/Resources53 loan.
//! Lifecycle: candidate -> original bank binding -> user check -> monotonic weight cap.
//! Concurrency: no raw leaf spans class work; original bank and loan exclude reuse.
//! Failure: unmasked/foreign CPU, missing bank/binding or kernel context refuse.
//! Forbidden: scalar Current as lifetime grant, base promotion or donation revocation.
//! Evidence: actual born Current resources; native CPU/IRQ entry remains modeled.
use super::{runqueue, stack_resources, LOAD_WEIGHT_MASK, NICE_0_LOAD};
use crate::multitask::{cpu_local, current_identity as identity};
use nucleus_core::util::lockdep::DispatchPreemption;

pub(in crate::multitask) fn demote() -> bool {
    let mut scope = DispatchPreemption::enter();
    scope.mask_irqs();
    demote_with(&scope)
}

pub(super) fn demote_with(scope: &DispatchPreemption<'_>) -> bool {
    if !scope.is_masked() || scope.cpu() != nucleus_core::util::lockdep::current_cpu_index() {
        return false;
    }
    let Some(slot) = cpu_local::current_cpu_task_slot() else {
        return false;
    };
    if !stack_resources::cpu_leases::owns_current(scope.cpu(), slot) {
        return false;
    }
    let Some(start) = stack_resources::task_start::read(slot) else {
        return false;
    };
    // SAFETY: this SAME masked CPU and original physical Current bank outlive
    // the loan and weight effect. The scalar slot/start are candidates only.
    let Some(pin) = (unsafe { identity::execution::pin_current(scope.cpu(), slot, start.id) })
    else {
        return false;
    };
    let Some(context) = stack_resources::context::read(pin.slot()) else {
        return false;
    };
    if !context.user_mode {
        return false;
    }
    // Irreversible base-class removal without increasing a low fair weight.
    // Reply-scoped effective class/funding remains owned by the original ledger.
    let weight = runqueue::weight::value(pin.slot());
    runqueue::weight::replace(pin.slot(), (weight & LOAD_WEIGHT_MASK).min(NICE_0_LOAD));
    true
}

#[cfg(test)]
mod tests;
