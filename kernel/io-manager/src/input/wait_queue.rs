// RING3-MIGRATION-REFERENCE START: DVM input wake substrate.
// Ring0 owns only task wait registration and wake delivery. It never queues,
// decodes, coalesces, or interprets DVM input records; inputd owns those
// policies and their lifecycle.
use core::sync::atomic::{AtomicU64, Ordering};

use nucleus_core::util::lockdep::{LockClass, TrackedSpinLock};
#[cfg(not(test))]
use x86_64::instructions::interrupts;

const INPUT_WAITERS_CAPACITY: usize = crate::multitask::MAX_SCHEDULER_TASKS;

static INPUT_WAITERS: TrackedSpinLock<
    [Option<u64>; INPUT_WAITERS_CAPACITY],
    { LockClass::InputWaiter as u8 },
> = TrackedSpinLock::new([None; INPUT_WAITERS_CAPACITY]);
// The capability-gated inputd ingestion worker must never compete with
// untrusted application waiters for a finite shared slot.
static INPUTD_INGESTION_WAITER: AtomicU64 = AtomicU64::new(0);

pub(crate) fn arm_input_waiter(task_id: u64) -> bool {
    with_input_waiters(|waiters| {
        if waiters.contains(&Some(task_id)) {
            return true;
        }
        for waiter in waiters.iter_mut() {
            if waiter.is_none() {
                *waiter = Some(task_id);
                return true;
            }
        }
        false
    })
}

pub(crate) fn disarm_input_waiter(task_id: u64) -> bool {
    with_input_waiters(|waiters| {
        let mut removed = false;
        for waiter in waiters.iter_mut() {
            if *waiter == Some(task_id) {
                *waiter = None;
                removed = true;
            }
        }
        removed
    })
}

/// Reserve the one DVM-ingestion wake slot for inputd. A dead predecessor is
/// reclaimed; a live different task is a split-brain service violation.
pub(crate) fn arm_inputd_ingestion_waiter(task_id: u64) -> bool {
    if task_id == 0 {
        return false;
    }
    loop {
        let current = INPUTD_INGESTION_WAITER.load(Ordering::Acquire);
        if current == task_id {
            return true;
        }
        if current != 0 && crate::multitask::is_user_task_alive(current) {
            return false;
        }
        if INPUTD_INGESTION_WAITER
            .compare_exchange(current, task_id, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
        {
            return true;
        }
    }
}

pub(crate) fn disarm_inputd_ingestion_waiter(task_id: u64) -> bool {
    if task_id != 0 {
        return INPUTD_INGESTION_WAITER
            .compare_exchange(task_id, 0, Ordering::AcqRel, Ordering::Acquire)
            .is_ok();
    }
    false
}

/// Wake-only leaf used by MSI-X and transport revoke. No shared bytes are
/// inspected and no device policy runs in interrupt context.
pub(crate) fn wake_input_waiters() {
    let inputd_waiter = INPUTD_INGESTION_WAITER.swap(0, Ordering::AcqRel);
    let mut task_ids = [0_u64; INPUT_WAITERS_CAPACITY];
    let count = with_input_waiters(|waiters| {
        let mut count = 0;
        for waiter in waiters.iter_mut() {
            if let Some(task_id) = waiter.take() {
                task_ids[count] = task_id;
                count += 1;
            }
        }
        count
    });
    for task_id in task_ids.iter().take(count).copied() {
        let _ = crate::multitask::wake_task(task_id);
    }
    if inputd_waiter != 0 {
        let _ = crate::multitask::wake_task(inputd_waiter);
    }
}

fn with_input_waiters<R>(f: impl FnOnce(&mut [Option<u64>; INPUT_WAITERS_CAPACITY]) -> R) -> R {
    #[cfg(test)]
    {
        f(&mut INPUT_WAITERS.lock())
    }

    #[cfg(not(test))]
    {
        interrupts::without_interrupts(|| f(&mut INPUT_WAITERS.lock()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retired_task_disarm_releases_both_input_wait_classes() {
        let task_id = u64::MAX - 601;
        assert!(arm_input_waiter(task_id));
        assert!(arm_inputd_ingestion_waiter(task_id));
        assert!(disarm_input_waiter(task_id));
        assert!(disarm_inputd_ingestion_waiter(task_id));
        assert!(!disarm_input_waiter(task_id));
        assert!(!disarm_inputd_ingestion_waiter(task_id));
    }

    #[test]
    #[allow(
        clippy::assertions_on_constants,
        reason = "the mutation witness must compile a reduced capacity and fail at runtime"
    )]
    fn input_waiter_capacity_covers_every_scheduler_task() {
        assert!(INPUT_WAITERS_CAPACITY >= crate::multitask::MAX_SCHEDULER_TASKS);
    }
}
// RING3-MIGRATION-REFERENCE END: DVM input wake substrate.
