// RING3-MIGRATION-REFERENCE START: DVM input wake substrate.
// Ring0 owns only task wait registration and wake delivery. It never queues,
// decodes, coalesces, or interprets DVM input records; inputd owns those
// policies and their lifecycle.
use alloc::vec::Vec;
use core::sync::atomic::{AtomicU64, Ordering};

use nucleus_core::util::lockdep::{LockClass, TrackedSpinLock};
#[cfg(all(rustos_boot_image, not(test)))]
use x86_64::instructions::interrupts;

static INPUT_WAITERS: TrackedSpinLock<Vec<u64>, { LockClass::InputWaiter as u8 }> =
    TrackedSpinLock::new(Vec::new());
// The capability-gated inputd ingestion worker must never compete with
// untrusted application waiters for a finite shared slot.
static INPUTD_INGESTION_WAITER: AtomicU64 = AtomicU64::new(0);

// Test-only: no production caller registers an ordinary (non-inputd) waiter any
// more; only the retire-time `disarm_input_waiter` and the wake sweep remain.
#[cfg(test)]
pub(crate) fn arm_input_waiter(task_id: u64) -> bool {
    if task_id == 0 {
        return false;
    }
    with_input_waiters(|waiters| {
        if !waiters.contains(&task_id) {
            waiters.push(task_id);
        }
        true
    })
}

pub(crate) fn disarm_input_waiter(task_id: u64) -> bool {
    with_input_waiters(|waiters| {
        let before = waiters.len();
        waiters.retain(|&id| id != task_id);
        waiters.len() != before
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
    loop {
        let mut batch = [0_u64; 64];
        let count = with_input_waiters(|waiters| {
            let mut count = 0;
            while count < batch.len() {
                let Some(task_id) = waiters.pop() else {
                    break;
                };
                batch[count] = task_id;
                count += 1;
            }
            count
        });
        if count == 0 {
            break;
        }
        for task_id in batch.iter().take(count).copied() {
            let _ = crate::multitask::wake_task(task_id);
        }
    }
    if inputd_waiter != 0 {
        let _ = crate::multitask::wake_task(inputd_waiter);
    }
}

fn with_input_waiters<R>(f: impl FnOnce(&mut Vec<u64>) -> R) -> R {
    // A dependent crate's test binary links this with `cfg(test)` false, so
    // guarding `cli`/`sti` on `cfg(test)` would run them in a host process.
    // `rustos_boot_image` is the fact that decides whether we own the CPU.
    #[cfg(any(not(rustos_boot_image), test))]
    {
        f(&mut INPUT_WAITERS.lock())
    }

    #[cfg(all(rustos_boot_image, not(test)))]
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
    fn input_waiter_queue_scales_past_legacy_slot_limits() {
        for i in 0..1500_u64 {
            assert!(arm_input_waiter(10_000 + i));
        }
        for i in 0..1500_u64 {
            assert!(disarm_input_waiter(10_000 + i));
        }
    }
}
// RING3-MIGRATION-REFERENCE END: DVM input wake substrate.
