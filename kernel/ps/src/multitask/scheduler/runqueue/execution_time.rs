//! CPU-local execution-time baseline publication.

use core::sync::atomic::Ordering;

use crate::multitask::task_table;

pub(super) fn clear(slot: usize) {
    set(slot, 0);
    if let Some(task) = task_table::try_get(slot) {
        task.settled_cpu_ns.store(0, Ordering::Release);
    }
}

/// Adds one settled execution interval to the slot's CPU time.
///
/// Only the execution owner settles an interval, so there is one writer per
/// slot; the reader is that same task, so a plain add is exact.
pub(in crate::multitask) fn charge(slot: usize, elapsed_ns: u64) {
    if let Some(task) = task_table::try_get(slot) {
        task.settled_cpu_ns.fetch_add(elapsed_ns, Ordering::AcqRel);
    }
}

/// Settled CPU time plus the part of the open interval measured on the same
/// CPU-local clock the interval itself is charged with.
pub(in crate::multitask) fn consumed(slot: usize, now_local_ns: Option<u64>) -> u64 {
    let Some(task) = task_table::try_get(slot) else {
        return 0;
    };
    let settled = task.settled_cpu_ns.load(Ordering::Acquire);
    let start = task.exec_start_local_ns.load(Ordering::Acquire);
    let open = now_local_ns
        .filter(|now| start != 0 && *now >= start)
        .map_or(0, |now| now - start);
    settled.saturating_add(open)
}

pub(in crate::multitask) fn set(slot: usize, value: u64) {
    task_table::get(slot)
        .exec_start_local_ns
        .store(value, Ordering::Release);
}

pub(super) fn reset_before_publication() {
    for slot in task_table::allocated_slots() {
        let task = task_table::get(slot);
        task.exec_start_local_ns.store(0, Ordering::Release);
        task.settled_cpu_ns.store(0, Ordering::Release);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn consumed_is_settled_time_plus_the_open_interval_and_clear_resets_it() {
        let slot = 41;
        clear(slot);
        assert_eq!(consumed(slot, Some(500)), 0);

        charge(slot, 300);
        charge(slot, 200);
        assert_eq!(consumed(slot, None), 500, "no open interval without a baseline");

        set(slot, 1_000);
        assert_eq!(consumed(slot, Some(1_250)), 750, "open interval is added");
        assert_eq!(consumed(slot, Some(900)), 500, "a clock behind the baseline adds nothing");
        assert_eq!(consumed(slot, None), 500);

        clear(slot);
        assert_eq!(consumed(slot, Some(2_000)), 0, "slot reuse starts from zero");
    }
}
