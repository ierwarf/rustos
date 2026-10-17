//! Captured placement permission, separate from desired affinity and hardware ACK.
use super::*;
use crate::multitask::scheduler::{runqueue, stack_resources};
use crate::multitask::{cpu_local, current_identity};
use core::marker::PhantomData;

#[must_use]
pub(in crate::multitask) struct PlacementGrant {
    slot: usize,
    task_id: Option<u64>,
    cpu: usize,
    version: u64,
    owner: Option<runqueue::RunOwnerSnapshot>,
    affinity: PhantomData<*mut ()>,
}
impl PlacementGrant {
    pub(super) fn new(slot: usize, task_id: Option<u64>, cpu: usize, version: u64) -> Self {
        Self {
            slot,
            task_id,
            cpu,
            version,
            owner: None,
            affinity: PhantomData,
        }
    }
    pub(in crate::multitask) fn slot(&self) -> usize {
        self.slot
    }
    #[cfg(test)]
    pub(in crate::multitask) fn is_fixture(&self) -> bool {
        self.task_id.is_none()
            || !current_identity::execution::banks::owns_execution(self.cpu, self.slot)
    }
    pub(in crate::multitask) fn bind_claimed(&mut self, owner: runqueue::RunOwnerSnapshot) {
        assert!(
            self.owner.is_none()
                && owner.state == RunOwnerState::Running
                && owner.cpu == Some(self.cpu),
            "placement grant lacks exact successful Running claim"
        );
        self.owner = Some(owner);
    }
    pub(in crate::multitask) fn same(&self, other: &Self) -> bool {
        self.slot == other.slot
            && self.task_id == other.task_id
            && self.cpu == other.cpu
            && self.owner == other.owner
    }
    pub(in crate::multitask) fn is_current(&self) -> bool {
        self.owner
            .is_some_and(|owner| runqueue::owner(self.slot) == owner)
            && self.task_id.is_some_and(|id| {
                stack_resources::task_start::read(self.slot).is_some_and(|start| start.id == id)
            })
            && current_identity::execution::banks::owns_execution(self.cpu, self.slot)
    }

    /// Borrowed installed permission may authorize a same-Current architecture
    /// scope, never a fresh Running claim. Caller continuously owns this CPU/IRQ.
    pub(in crate::multitask) fn copy_current_scope(&self) -> Option<Self> {
        (self.is_current() && current_identity::execution::banks::owns_current(self.cpu, self.slot))
            .then(|| Self {
                slot: self.slot,
                task_id: self.task_id,
                cpu: self.cpu,
                version: self.version,
                owner: self.owner,
                affinity: PhantomData,
            })
    }
    pub(in crate::multitask::scheduler) fn rebind_exec(
        &mut self,
        writer: &stack_resources::frame_access::FrameWriter,
    ) {
        writer.exec_stack_top(self.slot, self.task_id.expect("exec placement lacks UID"));
        let owner = runqueue::owner(self.slot);
        assert!(
            self.owner.is_some()
                && owner.state == RunOwnerState::Running
                && owner.cpu == Some(self.cpu)
                && current_identity::execution::banks::owns_execution(self.cpu, self.slot),
            "exec placement lost Current custody"
        );
        self.owner = Some(owner);
    }

    /// A newer desired mask cannot revoke this admitted return, nor may its
    /// migration intent be acknowledged by this older captured version.
    pub(in crate::multitask) fn acknowledge(&self) {
        let affinity = &AFFINITIES[self.slot];
        let binding = affinity.binding.lock();
        if *binding != self.task_id || affinity.version.load(Ordering::Relaxed) != self.version {
            return;
        }
        if affinity.migration_pending.load(Ordering::Relaxed) {
            let task = affinity.task_mask.load(Ordering::Relaxed);
            let process = affinity.process_mask.load(Ordering::Relaxed);
            store(affinity, task, process, false);
        }
    }
}

pub(in crate::multitask) fn capture_architecture(slot: usize, cpu: usize) -> bool {
    if cpu_local::has_placement(cpu, slot) {
        return true;
    }
    let Some(grant) = capture_current(slot, cpu) else {
        return false;
    };
    cpu_local::retain_placement(cpu, grant);
    true
}
/// Caller owns a continuous own-CPU IRQ/image scope and existing execution bank.
pub(in crate::multitask) fn capture_current(slot: usize, cpu: usize) -> Option<PlacementGrant> {
    #[cfg(rustos_boot_image)]
    assert!(
        !x86_64::instructions::interrupts::are_enabled()
            && nucleus_core::util::lockdep::current_cpu_index() == cpu,
        "placement capture lacks continuous own-CPU IRQ custody"
    );
    if let Some(installed) = cpu_local::current_placement_scope(cpu, slot)? {
        return Some(installed);
    }
    if !current_identity::execution::banks::owns_execution(cpu, slot) {
        return None;
    }
    let start = stack_resources::task_start::read(slot)?;
    let owner = runqueue::owner(slot);
    if owner.state != RunOwnerState::Running || owner.cpu != Some(cpu) {
        return None;
    }
    let ((), mut grant) = with_placement_admission(slot, Some(start.id), cpu, || ())?;
    grant.bind_claimed(owner);
    grant.is_current().then_some(grant)
}
