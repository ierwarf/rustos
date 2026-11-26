//! Owner: generation-reset boot idle classification and fixed AP placement.
//! Boundary: classification is not a Running/frame/CPU execution grant.
//! Lifecycle: publish before admission, clear before slot lifetime reuse.
//! Concurrency: Release/Acquire tag; exact owner/generation still admits use.
//! Failure: untagged slots have no AP mapping; invalid boot mapping fails stop.
//! Forbidden: no catalog scan or idle tag may synthesize execution custody.
//! Evidence: native tag/root/clear witness and production bootstrap checks.

use core::sync::atomic::Ordering;

use crate::multitask::task_table;

/// Publishes immutable-for-one-slot-lifetime idle classification before a
/// local picker can observe the task.
pub(in crate::multitask) fn set(slot: usize, idle: bool) {
    // ORDERING: publish lifetime classification before runnable admission.
    task_table::get(slot).idle.store(idle, Ordering::Release);
}

pub(in crate::multitask) fn is_idle(slot: usize) -> bool {
    // ORDERING: observe admission/reset classification, never execution permission.
    task_table::try_get(slot)
        .is_some_and(|task| task.idle.load(Ordering::Acquire))
}

/// The admitted tag authenticates fixed boot placement, not a scan of a
/// mutable catalog. Root classification is separate from AP CPU ownership.
#[cfg(test)]
pub(in crate::multitask) fn cpu_for_slot(slot: usize) -> u8 {
    use super::super::{NO_IDLE_CPU, ROOT_TASK_SLOT};
    if slot == ROOT_TASK_SLOT || !is_idle(slot) {
        return NO_IDLE_CPU;
    }
    assert!(
        slot > ROOT_TASK_SLOT && slot < nucleus_core::util::lockdep::MAX_TRACKED_CPUS,
        "idle publication has no exact boot CPU slot"
    );
    u8::try_from(slot).expect("idle CPU exceeds tag capacity")
}
#[cfg(test)]
#[test]
fn admitted_tag_binds_fixed_ap_cpu_and_clear_removes_mapping_without_turning_root_into_ap() {
    use super::super::{NO_IDLE_CPU, ROOT_TASK_SLOT};
    let _fixture = crate::multitask::scheduler::tests::boxed_scheduler();
    clear(ROOT_TASK_SLOT);
    clear(1);
    assert_eq!(cpu_for_slot(1), NO_IDLE_CPU);
    set(1, true);
    assert_eq!(cpu_for_slot(1), 1);
    set(ROOT_TASK_SLOT, true);
    assert!(is_idle(ROOT_TASK_SLOT));
    assert_eq!(cpu_for_slot(ROOT_TASK_SLOT), NO_IDLE_CPU);
    clear(1);
    assert_eq!(cpu_for_slot(1), NO_IDLE_CPU);
    clear(ROOT_TASK_SLOT);
    assert!(!is_idle(ROOT_TASK_SLOT));
}

pub(super) fn clear(slot: usize) {
    // ORDERING: classification removal precedes detached slot reuse.
    task_table::get(slot).idle.store(false, Ordering::Release);
}

pub(super) fn reset_before_publication() {
    for slot in task_table::allocated_slots() {
        // ORDERING: boot reset completes before any runnable publication.
        task_table::get(slot).idle.store(false, Ordering::Release);
    }
}
