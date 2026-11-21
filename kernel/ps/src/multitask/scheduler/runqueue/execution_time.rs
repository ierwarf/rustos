//! CPU-local execution-time baseline publication.

use core::sync::atomic::{AtomicU64, Ordering};

use super::MAX_TASK;

static EXEC_START_LOCAL_NS: [AtomicU64; MAX_TASK] = [const { AtomicU64::new(0) }; MAX_TASK];

/// CPU time the slot's task has already been charged for settled execution
/// intervals. The interval still open is added by the reader, so a thread asking
/// about itself sees the time it is spending right now.
static SETTLED_CPU_NS: [AtomicU64; MAX_TASK] = [const { AtomicU64::new(0) }; MAX_TASK];

pub(super) fn clear(slot: usize) {
    set(slot, 0);
    if let Some(settled) = SETTLED_CPU_NS.get(slot) {
        settled.store(0, Ordering::Release);
    }
}

/// Adds one settled execution interval to the slot's CPU time.
///
/// Only the execution owner settles an interval, so there is one writer per
/// slot; the reader is that same task, so a plain add is exact.
pub(in crate::multitask) fn charge(slot: usize, elapsed_ns: u64) {
    if let Some(settled) = SETTLED_CPU_NS.get(slot) {
        settled.fetch_add(elapsed_ns, Ordering::AcqRel);
    }
}

/// Settled CPU time plus the part of the open interval measured on the same
/// CPU-local clock the interval itself is charged with.
pub(in crate::multitask) fn consumed(slot: usize, now_local_ns: Option<u64>) -> u64 {
    let settled = SETTLED_CPU_NS
        .get(slot)
        .map_or(0, |settled| settled.load(Ordering::Acquire));
    let start = EXEC_START_LOCAL_NS
        .get(slot)
        .map_or(0, |start| start.load(Ordering::Acquire));
    let open = now_local_ns
        .filter(|now| start != 0 && *now >= start)
        .map_or(0, |now| now - start);
    settled.saturating_add(open)
}

pub(in crate::multitask) fn set(slot: usize, value: u64) {
    EXEC_START_LOCAL_NS
        .get(slot)
        .expect("scheduler local execution baseline slot exceeds capacity")
        .store(value, Ordering::Release);
}

pub(super) fn reset_before_publication() {
    for baseline in &EXEC_START_LOCAL_NS {
        baseline.store(0, Ordering::Release);
    }
    for settled in &SETTLED_CPU_NS {
        settled.store(0, Ordering::Release);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn consumed_is_settled_time_plus_the_open_interval_and_clear_resets_it() {
        let slot = MAX_TASK - 1;
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
