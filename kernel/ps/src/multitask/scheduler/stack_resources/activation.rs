//! Exact resource-context MM activation captured before claim or architecture.
//! No leaf crosses allocation, process/table work, hardware or queue acquisition.
use super::*;
use crate::multitask::process_table::{MmActivationPin, MmExecutionPin};

pub(in crate::multitask) struct ActivationCredential {
    slot: usize,
    task_id: u64,
    mm: MmActivationPin,
    owner: crate::multitask::scheduler::runqueue::RunOwnerSnapshot,
}
impl ActivationCredential {
    pub(in crate::multitask) fn slot(&self) -> usize {
        self.slot
    }
    /// Borrow the original claim's activation; never clone/activate after stop.
    pub(in crate::multitask) fn root_for(&self, slot: usize, id: u64) -> Option<u64> {
        (self.slot == slot && self.task_id == id && self.is_current()).then(|| self.mm.root())
    }
    pub(in crate::multitask) fn same(&self, other: &Self) -> bool {
        self.slot == other.slot && self.task_id == other.task_id && self.mm.same(&other.mm)
    }
    pub(in crate::multitask) fn matches_identity(
        &self,
        slot: usize,
        task_id: u64,
        mm: &MmExecutionPin,
    ) -> bool {
        self.slot == slot && self.task_id == task_id && self.mm.matches(mm)
    }
    pub(in crate::multitask::scheduler) fn bind_claimed_owner(
        &mut self,
        owner: crate::multitask::scheduler::runqueue::RunOwnerSnapshot,
    ) {
        self.owner = owner;
    }
    pub(in crate::multitask) fn is_current(&self) -> bool {
        RESOURCES[self.slot].with(|resources| {
            if crate::multitask::scheduler::runqueue::owner(self.slot) != self.owner {
                return false;
            }
            resources
                .start
                .is_some_and(|start| start.id == self.task_id)
                && resources
                    .mm_pin
                    .as_ref()
                    .is_some_and(|pin| self.mm.matches(pin))
        })
    }
}
/// Called while the same resource leaf excludes readers/writers and owns context.
pub(super) fn capture(
    resources: &Resources,
    slot: usize,
    cpu: usize,
) -> Option<Option<ActivationCredential>> {
    #[cfg(test)]
    if resources.mm_pin.is_none() {
        return Some(None);
    }
    let Some(context) = resources.context else {
        #[cfg(test)]
        return Some(None); // pure queue fixtures have no physical MM context
        #[cfg(not(test))]
        return None;
    };
    match context.process_handle {
        Some(handle) => {
            let start = resources.start?;
            let mm: &MmExecutionPin = resources.mm_pin.as_ref()?;
            if !mm.matches(handle, mm.root()) || !context.scheduling_context.is_bound_to(start.id) {
                return None;
            }
            if !crate::multitask::cpu_local::can_retain_mm_identity(cpu, slot, start.id, mm) {
                return None;
            }
            Some(Some(ActivationCredential {
                slot,
                task_id: start.id,
                mm: mm.try_activate()?,
                owner: crate::multitask::scheduler::runqueue::owner(slot),
            }))
        }
        None => (!context.user_mode && resources.mm_pin.is_none()).then_some(None),
    }
}

/// The exec receipt has installed next-generation resource MM custody after
/// root/CR3 checks. Transfer the already-captured staged activation, not a fresh
/// grant sampled after hardware work. Validation drops no grant under a leaf.
pub(in crate::multitask::scheduler) fn bind_exec(
    slot: usize,
    task_id: u64,
    mm: MmActivationPin,
) -> Option<ActivationCredential> {
    let owner = RESOURCES.get(slot)?.with(|r| {
        let context = r.context?;
        if r.closing
            || !r.frame_writing
            || !context.scheduling_context.is_bound_to(task_id)
            || !r.start.is_some_and(|start| start.id == task_id)
            || !r.mm_pin.as_ref().is_some_and(|pin| mm.matches(pin))
        {
            return None;
        }
        Some(crate::multitask::scheduler::runqueue::owner(slot))
    })?;
    Some(ActivationCredential {
        slot,
        task_id,
        mm,
        owner,
    })
}
