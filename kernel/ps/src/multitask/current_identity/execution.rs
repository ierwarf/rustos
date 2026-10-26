//! Captured execution admission, distinct from mutable lifecycle intent.
//! Identity/lifecycle qualification and Running CAS share class51 -> class53.
//! Last release is atomic; identity clear/reuse waits for all CPU bank holders.
use super::*;
use crate::multitask::scheduler;
use core::sync::atomic::AtomicU32;
pub(in crate::multitask) mod kernel_executor;
static REFS: [AtomicU32; MAX_SCHEDULER_TASKS] = [const { AtomicU32::new(0) }; MAX_SCHEDULER_TASKS];
#[derive(Debug)]
pub(in crate::multitask) struct ExecutionPermit {
    slot: usize,
}
impl ExecutionPermit {
    pub(in crate::multitask) fn slot(&self) -> usize {
        self.slot
    }
    pub(in crate::multitask) fn park(self) -> usize {
        let encoded = self.slot + 1;
        core::mem::forget(self);
        encoded
    }
    /// Only an atomic bank that consumed a parked unit may reconstruct it.
    pub(in crate::multitask) fn take(encoded: usize) -> Option<Self> {
        (encoded != 0).then(|| Self { slot: encoded - 1 })
    }
}
impl Drop for ExecutionPermit {
    fn drop(&mut self) {
        let previous = REFS[self.slot]
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |count| {
                count.checked_sub(1)
            })
            .expect("execution permit released twice");
        if previous == 1 {
            scheduler::retirement_cleanup::mark_reap_pending();
        }
    }
}
pub(in crate::multitask) fn count(slot: usize) -> u32 {
    // A resident Blocked task parks its permit in the scheduler's cell. That
    // unit is not execution: observers asking whether the slot is quiescent
    // must not count it, and anyone about to mutate evicts it first.
    REFS.get(slot).map_or(u32::MAX, |cell| {
        cell.load(Ordering::Acquire)
            .saturating_sub(scheduler::resident_permit_units(slot))
    })
}
/// UID lifetime only, never CPU execution/architecture authority.
pub(in crate::multitask) struct BindingPin {
    permit: ExecutionPermit,
    id: u64,
}
impl BindingPin {
    pub(in crate::multitask) fn slot(&self) -> usize {
        self.permit.slot
    }
    pub(in crate::multitask) fn id(&self) -> u64 {
        self.id
    }
    /// A second donation-class reference to the slot this pin already holds.
    ///
    /// `pin_donation` takes three nested leaves to prove the identity, then adds
    /// one reference. A caller that already owns a pin on the same identity
    /// (the executing sender of a synchronous call) needs only the reference:
    /// the held pin keeps the count above zero, so the identity and its backing
    /// cannot be cleared or reused while this one is taken. The admission gates
    /// `pin_donation` checks under the writer (retired, cold exec intent) are
    /// read from their lock-free publications and refuse identically.
    pub(in crate::multitask) fn duplicate_donation(&self) -> Option<BindingPin> {
        let slot = self.slot();
        let published = super::read_dispatch(slot)?;
        if published.task_id != Some(self.id)
            || published.retired()
            || super::exec_intent::busy(slot)
        {
            return None;
        }
        acquire(slot).map(|permit| BindingPin {
            permit,
            id: self.id,
        })
    }
}
/// One more reference to a task whose identity the caller already keeps alive
/// through a parked or banked execution unit (the count cannot reach zero, so
/// nothing can clear or reuse the slot while this is taken). The admission
/// gates it checks are the lock-free publications `duplicate_donation` reads.
pub(in crate::multitask) fn pin_held(slot: usize, id: u64) -> Option<BindingPin> {
    if !qualified(slot, id) {
        return None;
    }
    acquire(slot).map(|permit| BindingPin { permit, id })
}

/// The admission half of [`pin_held`] without taking the permit: `slot` still
/// publishes `id`, is not retired and has no exec intent. For a caller whose own
/// Current bank already holds the task (nothing can retire or reuse the slot
/// while it executes on this masked CPU), a second permit buys nothing.
pub(in crate::multitask) fn qualified(slot: usize, id: u64) -> bool {
    super::read_task_dispatch(slot)
        .is_some_and(|published| published.task_id == Some(id) && !published.retired)
        && !super::exec_intent::busy(slot)
}
pub(in crate::multitask) fn pin_creation(
    slot: usize,
    id: u64,
    handle: crate::multitask::process_table::ProcessHandle,
    abi: crate::user::abi::UserAbi,
) -> Option<BindingPin> {
    with_writer(slot, |writer| {
        let identity = writer.identity?;
        if writer.lifecycle.retired
            || writer.lifecycle.job_stopped
            || writer.exec_intent.is_some()
            || !super::cohort::admission_ready(slot)
            || !identity.user_mode
            || identity.abi != Some(abi)
            || identity.process_handle != Some(handle)
        {
            return None;
        }
        pin_binding(slot, writer, id)
    })?
}

/// Wake token/backing only, never a continuation or fresh execution grant.
/// Terminal50 excludes new borrowers before Identity51 -> Resources53 capture.
pub(in crate::multitask) fn pin_wake(slot: usize, id: u64) -> Option<BindingPin> {
    crate::multitask::scheduler::retirement_cleanup::with_wake_binding(slot, || {
        with_writer(slot, |writer| {
            if writer.lifecycle.retired || writer.lifecycle.suspended || writer.exec_intent.is_some() {
                return None;
            }
            pin_binding(slot, writer, id)
        })?
    })?
}

/// Donation metadata/backing loan only. Suspended tasks may retain reply custody;
/// original terminal admission excludes late edges and physical reuse. No frame,
/// execution, MM activation or funding interval authority is acquired here.
pub(in crate::multitask) fn pin_donation(slot: usize, id: u64) -> Option<BindingPin> {
    crate::multitask::scheduler::retirement_cleanup::with_wake_binding(slot, || {
        with_writer(slot, |writer| {
            if writer.lifecycle.retired || writer.exec_intent.is_some() {
                return None;
            }
            pin_binding(slot, writer, id)
        })?
    })?
}

/// Retains the exact backing of an already executing physical Current bank.
/// Terminal50 excludes new loans before Identity51 -> Resources53 capture.
/// This is not a fresh execution grant or reschedulable CPU turn.
/// # Safety
/// Caller retains original masked same-CPU IRQ scope and that Current bank for
/// the full returned loan; scalar Current and Running alone are insufficient.
pub(in crate::multitask) unsafe fn pin_current(cpu: usize, slot: usize, id: u64) -> Option<BindingPin> {
    if !crate::multitask::scheduler::stack_resources::cpu_leases::owns_current(cpu, slot) {
        return None;
    }
    // The executing bank's own permit keeps the count above zero, so the
    // identity and its backing cannot be cleared or reused while this extra
    // reference is taken; the lifecycle gates are read from their lock-free
    // publication, as `duplicate_donation` does. Terminal50 still closes new
    // loans, through its one cleanup leaf; the identity and resource leaves a
    // fresh wake needs prove nothing further for a task this CPU is running.
    crate::multitask::scheduler::retirement_cleanup::with_wake_binding(slot, || {
        let published = super::read_dispatch(slot)?;
        if published.task_id != Some(id) || published.retired() || published.suspended() {
            return None;
        }
        acquire(slot).map(|permit| BindingPin { permit, id })
    })?
}

/// Native negative witness: retains backing, not post-retirement dispatch
/// permission or a transferable continuation. Never sufficient for a live drain.
#[cfg(test)]
pub(in crate::multitask) fn pin_waiter(slot: usize, id: u64) -> Option<BindingPin> {
    with_writer(slot, |writer| pin_binding(slot, writer, id))?
}

pub(super) fn pin_binding(slot: usize, writer: &IdentityWriter, id: u64) -> Option<BindingPin> {
    if writer.bound_task_id != Some(id)
        || writer.identity.and_then(|identity| identity.task_id) != Some(id)
    {
        return None;
    }
    crate::multitask::scheduler::stack_resources::with_cold_binding(slot, id, || {
        acquire(slot).map(|permit| BindingPin { permit, id })
    })?
}

#[cfg(test)]
pub(super) fn recover_binding_pin_for_test(slot: usize) {
    assert!(count(slot) > 0);
    drop(ExecutionPermit { slot });
}

fn acquire(slot: usize) -> Option<ExecutionPermit> {
    REFS.get(slot)?
        .fetch_update(Ordering::AcqRel, Ordering::Acquire, |count| {
            count.checked_add(1)
        })
        .ok()?;
    Some(ExecutionPermit { slot })
}
fn eligible(writer: &IdentityWriter) -> Option<TaskIdentity> {
    if !writer.lifecycle.dispatchable() {
        return None;
    }
    let identity = writer.identity?;
    identity.task_id?;
    if identity.process_handle.is_some_and(crate::multitask::process_placement::root_membership::pending)
    {
        return None;
    }
    if identity.user_mode && (identity.user_binding().is_none() || identity.process_id.is_none()) {
        return None;
    }
    Some(identity)
}
/// No external work under the writer: callback may only join resource admission
/// and the exact owner CAS. Provisional permits escape before failed-result Drop.
pub(in crate::multitask) fn with_claim<R>(
    slot: usize,
    claim: impl FnOnce(Option<TaskIdentity>) -> R,
) -> Option<(R, Option<ExecutionPermit>)> {
    with_writer(slot, |writer| {
        if !super::cohort::admission_ready(slot) {
            return None;
        }
        #[cfg(test)]
        if writer.identity.is_none() {
            // Pure queue projection must not spend an actual reserved producer
            // prefix or the real root Slot-ACK/membership-ACK gap.
            if !crate::multitask::scheduler::stack_resources::fixture_claim_allowed(slot) {
                return None;
            }
            return Some((claim(None), None));
        } // pure queue fixtures
        let identity = eligible(writer)?;
        let permit = acquire(slot)?;
        Some((claim(Some(identity)), Some(permit)))
    })?
}
pub(in crate::multitask) fn initial(slot: usize, task_id: u64) -> Option<ExecutionPermit> {
    with_writer(slot, |writer| {
        if !super::cohort::admission_ready(slot) {
            return None;
        }
        let identity = eligible(writer)?;
        if identity.task_id != Some(task_id) {
            return None;
        }
        crate::multitask::scheduler::stack_resources::with_initial_execution(slot, || {
            acquire(slot)
        })?
    })?
}
pub(in crate::multitask) fn binding(slot: usize) -> Option<u64> {
    with_writer(slot, |writer| writer.bound_task_id).flatten()
}
pub(in crate::multitask) mod banks;
#[cfg(test)]
mod tests;
