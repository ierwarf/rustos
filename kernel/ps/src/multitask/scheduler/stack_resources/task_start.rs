//! Immutable task ID and kernel entry ownership, co-located with allocation
//! custody. A CPU/selected-frame reference also prevents descriptor reset.
//! Cold construction/exec still uses the enclosing directory transaction;
//! these leaf operations never hold a lock across construction or entry calls.

use super::super::TaskStart;
use super::RESOURCES;
use core::sync::atomic::Ordering;

/// Negative native fault injection only: original physical/census loans remain
/// retained. A corrupted copied descriptor is NOT a replacement UID or grant.
#[cfg(test)]
pub(in crate::multitask::scheduler) fn test_mismatched_start<R>(
    slot: usize,
    id: u64,
    f: impl FnOnce() -> R,
) -> R {
    struct Restore(usize, TaskStart);
    impl Drop for Restore {
        fn drop(&mut self) {
            RESOURCES[self.0].with(|resources| resources.start = Some(self.1));
        }
    }
    let original = RESOURCES[slot].with(|resources| {
        let original = resources.start.expect("negative probe needs original start");
        resources.start = Some(TaskStart { id, ..original });
        original
    });
    let _restore = Restore(slot, original);
    // No resource leaf crosses the probe or its terminal/queue checks.
    f()
}

pub(in crate::multitask::scheduler) fn read(slot: usize) -> Option<TaskStart> {
    RESOURCES.get(slot)?.with(|resources| resources.start)
}

pub(in crate::multitask::scheduler) fn publish(slot: usize, start: TaskStart) {
    publish_with_reservation(slot, start, None)
}
pub(super) fn publish_with_reservation(
    slot: usize,
    start: TaskStart,
    reservation: Option<super::admission::Reservation>,
) {
    RESOURCES[slot].with(|resources| {
        assert_eq!(
            resources.reservation, reservation,
            "foreign slot descriptor installation"
        );
        assert!(
            !resources.closing,
            "closing task descriptor cannot be republished"
        );
        assert!(
            resources.start.is_none_or(|old| old.id == start.id),
            "task descriptor identity changed without reset"
        );
        assert!(
            resources.binding.is_none_or(|(id, _)| id == start.id),
            "task descriptor differs from allocation identity"
        );
        assert!(
            resources
                .context
                .is_none_or(|context| context.scheduling_context.is_bound_to(start.id)),
            "task descriptor differs from context identity"
        );
        // Same-identity exec may replace the entry, never the task identity or
        // any outstanding allocation reference. Readers receive owned copies.
        resources.start = Some(start);
    });
}

/// Running entry lookup is IRQ-excluded by the trampoline. Allocation-bank
/// Current custody prevents descriptor reset while its copied entry is used;
/// missing publication never falls back to the old catalog.
pub(in crate::multitask) fn current_entry() -> Option<TaskStart> {
    let cpu = nucleus_core::util::lockdep::current_cpu_index();
    let slot = crate::multitask::cpu_local::current_cpu_task_slot()?;
    let current = super::cpu_leases::owns_current(cpu, slot);
    let identity = crate::multitask::current_identity::read_dispatch(slot)?;
    qualified_running_start(
        read(slot),
        identity.task_id,
        identity.lifecycle_dispatchable(),
        current,
    )
}

fn qualified_running_start(
    start: Option<TaskStart>,
    task_id: Option<u64>,
    dispatchable: bool,
    current: bool,
) -> Option<TaskStart> {
    start.filter(|start| current && dispatchable && Some(start.id) == task_id)
}

pub(in crate::multitask::scheduler) fn clear(slot: usize) {
    RESOURCES[slot].with(|resources| {
        assert!(
            !crate::multitask::current_identity::exec_intent::busy(slot),
            "descriptor reset with cold exec intent custody"
        );
        assert_eq!(
            crate::multitask::current_identity::execution::count(slot),
            0,
            "descriptor reset with captured lifecycle execution custody"
        );
        assert!(
            !resources.frame_writing,
            "task descriptor reset with frame writer custody"
        );
        assert_eq!(
            RESOURCES[slot].pins.load(Ordering::Acquire),
            0,
            "task descriptor reset with execution/frame custody"
        );
        assert!(
            resources.start.is_none() || resources.closing,
            "live task descriptor reset without closing"
        );
        assert!(
            !crate::multitask::process_placement::reclaim::busy(&super::Membership { resources, slot }),
            "descriptor reset with frozen exec census custody"
        );
        resources.start = None;
    });
}

#[cfg(test)]
mod tests;
