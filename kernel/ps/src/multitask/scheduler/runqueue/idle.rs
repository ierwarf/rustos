//! Owner: generation-reset boot idle classification and fixed AP placement.
//! Boundary: classification is not a Running/frame/CPU execution grant.
//! Lifecycle: publish before admission, clear before slot lifetime reuse.
//! Concurrency: Release/Acquire tag; exact owner/generation still admits use.
//! Failure: untagged slots have no AP mapping; invalid boot mapping fails stop.
//! Forbidden: no catalog scan or idle tag may synthesize execution custody.
//! Evidence: native tag/root/clear witness and production bootstrap checks.

use core::sync::atomic::{AtomicBool, Ordering};

use super::MAX_TASK;

static IDLE_SLOT: [AtomicBool; MAX_TASK] = [const { AtomicBool::new(false) }; MAX_TASK];

/// Publishes immutable-for-one-slot-lifetime idle classification before a
/// local picker can observe the task.
pub(in crate::multitask) fn set(slot: usize, idle: bool) {
    // ORDERING: publish lifetime classification before runnable admission.
    IDLE_SLOT
        .get(slot)
        .expect("scheduler idle slot exceeds capacity")
        .store(idle, Ordering::Release);
}

pub(in crate::multitask) fn is_idle(slot: usize) -> bool {
    // ORDERING: observe admission/reset classification, never execution permission.
    IDLE_SLOT
        .get(slot)
        .is_some_and(|idle| idle.load(Ordering::Acquire))
}

/// The admitted tag authenticates fixed boot placement, not a scan of a
/// mutable catalog. Root classification is separate from AP CPU ownership.
#[cfg(test)]
pub(in crate::multitask) fn cpu_for_slot(slot: usize) -> u8 {
    use super::super::{MAX_TASK, NO_IDLE_CPU, ROOT_TASK_SLOT};
    if slot == ROOT_TASK_SLOT || !is_idle(slot) {
        return NO_IDLE_CPU;
    }
    let cpu = MAX_TASK
        .checked_sub(slot)
        .filter(|cpu| *cpu > 0 && *cpu < nucleus_core::util::lockdep::MAX_TRACKED_CPUS)
        .expect("idle publication has no exact boot CPU slot");
    u8::try_from(cpu).expect("idle CPU exceeds tag capacity")
}
#[cfg(test)]
#[test]
fn admitted_tag_binds_fixed_ap_cpu_and_clear_removes_mapping_without_turning_root_into_ap() {
    use super::super::{MAX_TASK, NO_IDLE_CPU, ROOT_TASK_SLOT};
    let _fixture = crate::multitask::scheduler::tests::boxed_scheduler();
    clear(ROOT_TASK_SLOT);
    clear(MAX_TASK - 1);
    assert_eq!(cpu_for_slot(MAX_TASK - 1), NO_IDLE_CPU);
    set(MAX_TASK - 1, true);
    assert_eq!(cpu_for_slot(MAX_TASK - 1), 1);
    set(ROOT_TASK_SLOT, true);
    assert!(is_idle(ROOT_TASK_SLOT));
    assert_eq!(cpu_for_slot(ROOT_TASK_SLOT), NO_IDLE_CPU);
    clear(MAX_TASK - 1);
    assert_eq!(cpu_for_slot(MAX_TASK - 1), NO_IDLE_CPU);
    clear(ROOT_TASK_SLOT);
    assert!(!is_idle(ROOT_TASK_SLOT));
}

pub(super) fn clear(slot: usize) {
    // ORDERING: classification removal precedes detached slot reuse.
    IDLE_SLOT
        .get(slot)
        .expect("scheduler idle slot exceeds capacity")
        .store(false, Ordering::Release);
}

pub(super) fn reset_before_publication() {
    for idle in &IDLE_SLOT {
        // ORDERING: boot reset completes before any runnable publication.
        idle.store(false, Ordering::Release);
    }
}
