//! Owner: original same-CPU Current wait loan, not queue-word lifetime inference.
//! Boundary: caller retains IRQ exclusion and physical Current bank for the entire loan.
//! Lifecycle: exact backing capture -> arm/cancel/commit or pager rollback -> original drop.
//! Concurrency: Terminal50 closes new capture; original binding survives raced stop/wake.
//! Failure: missing physical bank refuses; terminal stop never re-arms or resurrects.
//! Forbidden: scalar Current, Running or a BindingPin as a fresh execution grant.
//! Evidence: actual bank/UID/backing and wait CAS; complete dispatch serializer remains separate.
use super::{BlockReason, runqueue, stack_resources, task_wait};
use crate::multitask::{cpu_local, current_identity as identity};

/// Non-transferable backing loan from an already executing physical Current.
/// This does not authorize execution, frame access, CPU return or reuse.
pub(in crate::multitask) struct CurrentWait {
    binding: identity::execution::BindingPin,
    cpu: usize,
    pager_token: core::cell::Cell<Option<u64>>,
    affinity: core::marker::PhantomData<*mut ()>,
}
impl CurrentWait {
    /// # Safety
    /// Caller masks this CPU's IRQs before capture and retains that original
    /// same-CPU scope and Current bank until this non-Send loan is dropped.
    pub(in crate::multitask) unsafe fn capture() -> Option<Self> {
        let cpu = nucleus_core::util::lockdep::current_cpu_index();
        let slot = cpu_local::current_cpu_task_slot()?;
        if slot == super::ROOT_TASK_SLOT || !stack_resources::cpu_leases::owns_current(cpu, slot) {
            return None;
        }
        let id = stack_resources::task_start::read(slot)?.id;
        // SAFETY: original caller IRQ/Current scope is retained; the bank was
        // authenticated before borrowing its UID/backing, not inferred from rq.
        let binding = unsafe { identity::execution::pin_current(cpu, slot, id) }?;
        let owner = runqueue::owner(slot);
        if owner.state != runqueue::RunOwnerState::Running || owner.cpu != Some(cpu) {
            return None;
        }
        let (kind, token) = runqueue::wait::reason(slot);
        Some(Self {
            binding,
            cpu,
            pager_token: core::cell::Cell::new(
                (kind == runqueue::wait::REASON_PAGER_FAULT).then_some(token),
            ),
            affinity: core::marker::PhantomData,
        })
    }
    pub(super) fn slot(&self) -> usize {
        let slot = self.binding.slot();
        assert_eq!(nucleus_core::util::lockdep::current_cpu_index(), self.cpu);
        assert_eq!(cpu_local::current_cpu_task_slot(), Some(slot));
        assert!(stack_resources::cpu_leases::owns_current(self.cpu, slot));
        // This SAME BindingPin retains UID/backing, and the physical Current
        // bank plus the caller's continuous masked scope excludes replacement.
        // TaskStart may change its entry for same-UID exec, never its UID until
        // both loans drain. Do not reacquire Resources53 to re-prove that fact.
        // Mutable terminal/wait state is still checked at each actual effect.
        slot
    }
    pub(super) fn id(&self) -> u64 {
        let _ = self.slot();
        self.binding.id()
    }
    /// Borrow this SAME executing allocation/MM loan, never resolve a donor ID.
    pub(super) fn original_pin(&self) -> &identity::execution::BindingPin {
        let _ = self.slot();
        &self.binding
    }
    pub(super) fn cpu(&self) -> usize {
        let _ = self.slot();
        self.cpu
    }
    pub(super) fn committed_pager(&self, token: u64) -> bool {
        let slot = self.slot();
        token != 0 && self.pager_token.get() == Some(token)
            && !runqueue::owner(slot).state.is_terminal()
            && runqueue::wait::blocked(slot)
            && !runqueue::wait::armed(slot)
            && runqueue::wait::reason(slot) == (runqueue::wait::REASON_PAGER_FAULT, token)
    }
    pub(super) fn matches_armed_reply(&self, reply: u64) -> bool {
        let slot = self.slot();
        runqueue::wait::armed(slot)
            && !runqueue::wait::blocked(slot)
            && runqueue::wait::reason(slot) == (runqueue::wait::REASON_ENDPOINT_REPLY, reply)
    }
    pub(in crate::multitask) fn arm(&self, reason: BlockReason) -> bool {
        let slot = self.slot();
        if runqueue::wait::blocked(slot) {
            return false;
        }
        let (kind, id) = task_wait::encoded_reason(reason);
        runqueue::wait::set_ready_since_ticks(slot, 0);
        let armed = runqueue::wait::try_publish_arm(slot, kind, id);
        if armed {
            self.pager_token
                .set((kind == runqueue::wait::REASON_PAGER_FAULT).then_some(id));
        }
        armed
    }
    pub(in crate::multitask) fn cancel(&self) -> bool {
        let slot = self.slot();
        if !runqueue::wait::armed(slot) || runqueue::wait::blocked(slot) {
            return false;
        }
        runqueue::wait::set_ready_since_ticks(slot, 0);
        runqueue::wait::clear_arm(slot);
        self.pager_token.set(None);
        true
    }
    pub(in crate::multitask) fn commit(&self) -> bool {
        let slot = self.slot();
        match runqueue::wait::commit(slot) {
            runqueue::WaitCommitOutcome::Committed => {
                runqueue::wait::set_ready_since_ticks(slot, 0);
                runqueue::wait::set_blocked_since_ticks(slot, crate::arch::rtc::ticks());
                true
            }
            runqueue::WaitCommitOutcome::WakeWon => {
                runqueue::wait::set_ready_since_ticks(slot, 0);
                false
            }
            // The same original bank/backing is still retained: terminal or
            // an already committed wait refuses, not a catalog fallback grant.
            runqueue::WaitCommitOutcome::InvalidOwner => false,
        }
    }
    pub(in crate::multitask) fn restore_pager(&self, token: u64) -> bool {
        if token == 0 || self.pager_token.get() != Some(token) {
            return false;
        }
        let slot = self.slot();
        let owner = runqueue::owner(slot);
        if owner.state.is_terminal() {
            // Cancellation won while the original loan remained live. Do not
            // wake a terminal task, or recapture a vanished Current identity.
            return true;
        }
        let (kind, id) = runqueue::wait::reason(slot);
        if kind == runqueue::wait::REASON_PAGER_FAULT && id == token {
            runqueue::wake_wait(slot);
            return true;
        }
        owner.runnable
    }
}

/// # Safety
/// Original masked same-CPU IRQ/Current scope lasts through this operation.
pub(in crate::multitask) unsafe fn arm_current_wait(reason: BlockReason) -> Option<bool> {
    // SAFETY: loan is consumed entirely inside the caller's original scope.
    unsafe { CurrentWait::capture() }.map(|wait| wait.arm(reason))
}
/// # Safety
/// Original masked same-CPU IRQ/Current scope lasts through this operation.
pub(in crate::multitask) unsafe fn cancel_current_wait() -> Option<bool> {
    // SAFETY: loan is consumed entirely inside the caller's original scope.
    unsafe { CurrentWait::capture() }.map(|wait| wait.cancel())
}
/// # Safety
/// Original masked same-CPU IRQ/Current scope lasts through this operation;
/// caller drops every loan before entering a scheduling trap.
pub(in crate::multitask) unsafe fn commit_current_wait() -> Option<bool> {
    // SAFETY: original loan is released before caller's possible trap.
    unsafe { CurrentWait::capture() }.map(|wait| wait.commit())
}

#[cfg(test)]
pub(in crate::multitask::scheduler) mod tests;
