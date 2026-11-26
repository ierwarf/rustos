//! Owner: kill-safe kernel consumer admission and read-only kernel revocation fences.
//! Boundary: exact Current capture precedes Terminal50 -> Identity51 -> Resources53 executor join.
//! Lifecycle: captured publication generations complete only at real kernel effect ACK; reuse persists.
//! Concurrency: no terminal leaf crosses external work; fences retain no caller across yield.
//! Failure: admitted producer, user/prefix/foreign Current refuse; absence never acknowledges a fence.
//! Forbidden: root-slot-only assumptions, user-held cleanup loans, and replacement global serializers.
//! Evidence: actual row/identity/resource/ACK witnesses; physical scheduler return remains separate.
use super::*;
pub(in crate::multitask) use crate::multitask::current_identity::execution::kernel_executor::Executor;
use alloc::vec::Vec;

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
    let row = try_cell(slot)?.state.lock();
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

/// Kernel rows that were pending or claimed when the fence was captured, with
/// the generation each must complete. Sparse: only those rows are recorded.
pub(in crate::multitask) struct Fence {
    generations: Vec<(usize, u64)>,
}

#[cfg(test)]
mod tests;
impl Fence {
    /// Storage for a fence, allocated by the caller before it masks interrupts
    /// so `fill` never allocates. One entry per live slot plus headroom.
    pub(in crate::multitask) fn with_room() -> Self {
        Self { generations: Vec::with_capacity(crate::multitask::task_table::allocated_slot_bound().saturating_add(64)) }
    }
    #[cfg(test)]
    pub(in crate::multitask) fn capture() -> Self {
        let mut fence = Self::with_room();
        fence.fill();
        fence
    }
    /// Records every pending/claimed kernel row. Never allocates.
    pub(in crate::multitask) fn fill(&mut self) {
        self.generations.clear();
        // A Pending/Claimed kernel row is never idle, so the busy index covers it;
        // a publisher holding its leaf now is ordered after this capture.
        for slot in busy_slots() {
            let row = cell(slot).state.lock();
            if matches!(
                row.kernel,
                KernelState::Pending(_) | KernelState::Claimed(_)
            ) {
                self.generations.push((slot, row.generation));
            }
        }
    }
    pub(in crate::multitask) fn complete(&self) -> bool {
        self.generations.iter().all(|&(slot, generation)| {
            generation == 0 || cell(slot).state.lock().kernel_done_generation >= generation
        })
    }
}
