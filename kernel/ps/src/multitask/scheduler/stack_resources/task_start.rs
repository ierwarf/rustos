//! Immutable task ID and kernel entry ownership, co-located with allocation
//! custody. A CPU/selected-frame reference also prevents descriptor reset.
//! Cold construction/exec still uses the enclosing directory transaction;
//! these leaf operations never hold a lock across construction or entry calls.

use super::super::{MAX_TASK, TaskStart};
use super::RESOURCES;
use core::sync::atomic::{AtomicU32, AtomicU64, AtomicUsize, Ordering, fence};

/// Lock-free mirror of `Resources::start`, one seqlock per slot.
///
/// The descriptor is two immutable words (entry, task id) written only under
/// the slot's resource leaf, yet `read` is a task-identity check taken dozens
/// of times per synchronous IPC round trip (SIMD image binding, wake binding,
/// every capture). Taking the leaf only to copy it was the largest single
/// source of `TaskStackResources` acquisitions. Writers are serialized by that
/// leaf with IRQs masked, so the sequence is never odd across a preemption and
/// a reader's retry is bounded by one store pair. An empty slot is entry 0.
struct StartMirror {
    seq: AtomicU32,
    id: AtomicU64,
    entry: AtomicUsize,
}
static START_MIRROR: [StartMirror; MAX_TASK] = [const {
    StartMirror {
        seq: AtomicU32::new(0),
        id: AtomicU64::new(0),
        entry: AtomicUsize::new(0),
    }
}; MAX_TASK];

/// Publishes `start` into the mirror.
///
/// The caller holds `slot`'s resource leaf, which is the sole writer exclusion.
pub(super) fn mirror_store(slot: usize, start: Option<TaskStart>) {
    let mirror = &START_MIRROR[slot];
    let sequence = mirror.seq.load(Ordering::Relaxed);
    mirror.seq.store(sequence.wrapping_add(1), Ordering::Relaxed);
    // ORDERING: the odd sequence is visible before either data word changes.
    fence(Ordering::Release);
    let (id, entry) = match start {
        Some(start) => (start.id, start.entry as usize),
        None => (0, 0),
    };
    mirror.id.store(id, Ordering::Relaxed);
    mirror.entry.store(entry, Ordering::Relaxed);
    // ORDERING: Release publishes both words before the even sequence.
    mirror.seq.store(sequence.wrapping_add(2), Ordering::Release);
}

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
            RESOURCES[self.0].with(|resources| {
                resources.start = Some(self.1);
                mirror_store(self.0, resources.start);
            });
        }
    }
    let original = RESOURCES[slot].with(|resources| {
        let original = resources.start.expect("negative probe needs original start");
        resources.start = Some(TaskStart { id, ..original });
        mirror_store(slot, resources.start);
        original
    });
    let _restore = Restore(slot, original);
    // No resource leaf crosses the probe or its terminal/queue checks.
    f()
}

pub(in crate::multitask::scheduler) fn read(slot: usize) -> Option<TaskStart> {
    let mirror = START_MIRROR.get(slot)?;
    loop {
        // ORDERING: Acquire pairs with the writer's closing Release store.
        let before = mirror.seq.load(Ordering::Acquire);
        if before & 1 != 0 {
            core::hint::spin_loop();
            continue;
        }
        let id = mirror.id.load(Ordering::Relaxed);
        let entry = mirror.entry.load(Ordering::Relaxed);
        // ORDERING: the data loads complete before the sequence is re-read.
        fence(Ordering::Acquire);
        if mirror.seq.load(Ordering::Relaxed) != before {
            continue;
        }
        return (entry != 0).then(|| TaskStart {
            // SAFETY: a nonzero entry word is only ever stored from a valid
            // `fn(u64)` by `mirror_store`.
            entry: unsafe { core::mem::transmute::<usize, fn(u64)>(entry) },
            id,
        });
    }
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
        mirror_store(slot, resources.start);
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
        mirror_store(slot, None);
    });
}

#[cfg(test)]
mod tests;
