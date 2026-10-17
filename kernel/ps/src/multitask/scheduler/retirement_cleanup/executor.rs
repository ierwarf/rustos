//! Owner: kill-safe kernel consumer admission and read-only kernel revocation fences.
//! Boundary: exact Current capture precedes Terminal50 -> Identity51 -> Resources53 executor join.
//! Lifecycle: captured publication generations complete only at real kernel effect ACK; reuse persists.
//! Concurrency: no terminal leaf crosses external work; Copy fences retain no caller across yield.
//! Failure: admitted producer, user/prefix/foreign Current refuse; absence never acknowledges a fence.
//! Forbidden: root-slot-only assumptions, user-held cleanup loans, and replacement global serializers.
//! Evidence: actual row/identity/resource/ACK witnesses; physical scheduler return remains separate.
use super::*;
pub(in crate::multitask) use crate::multitask::current_identity::execution::kernel_executor::Executor;

pub(in crate::multitask) fn capture_current() -> Option<Executor> {
    let mut preemption = nucleus_core::util::lockdep::DispatchPreemption::enter();
    preemption.mask_irqs();
    if !crate::multitask::cpu_local::current_cpu_task_slot_admitted() {
        return None;
    }
    let slot = crate::multitask::cpu_local::current_cpu_task_slot()?;
    let id = super::super::stack_resources::task_start::read(slot)?.id;
    capture_binding(slot, id)
}

pub(in crate::multitask::scheduler) fn capture_binding(slot: usize, id: u64) -> Option<Executor> {
    let row = CELLS.get(slot)?.state.lock();
    if row
        .producer
        .is_some_and(|r| !producer::Producer::waiting_executor(r))
        || row.producing
        || row.reset_seal.is_some()
    {
        return None;
    }
    // Hold the actual producer-reservation leaf until the kernel stop exclusion
    // is installed. A producer admitted just before this scope cannot be bypassed.
    Executor::capture_binding(slot, id, row.deferred.is_some())
}

#[derive(Clone, Copy)]
pub(in crate::multitask) struct Fence {
    generations: [u64; MAX_TASK],
}

#[cfg(test)]
mod tests;
impl Fence {
    pub(in crate::multitask) fn capture() -> Self {
        let mut generations = [0; MAX_TASK];
        for slot in FIRST_DYNAMIC_TASK_SLOT..MAX_TASK {
            let row = CELLS[slot].state.lock();
            if matches!(
                row.kernel,
                KernelState::Pending(_) | KernelState::Claimed(_)
            ) {
                generations[slot] = row.generation;
            }
        }
        Self { generations }
    }
    pub(in crate::multitask) fn complete(&self) -> bool {
        self.generations
            .iter()
            .enumerate()
            .all(|(slot, &generation)| {
                generation == 0 || CELLS[slot].state.lock().kernel_done_generation >= generation
            })
    }
}
