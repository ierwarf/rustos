//! Owner: kernel stack high-water telemetry for retired tasks.
//! Boundary: reads only the owned, already detached stack allocation in the
//! reaper; no resource leaf, scheduler guard or live task stack is touched.
//! Lifecycle: a stack is zero-filled at creation and never reused, so the first
//! non-zero byte above the guard is the deepest the task ever wrote.
//! Concurrency: lock-free; peaks are independent `fetch_max` words.
//! Failure: a missing allocation or an overflowed guard records nothing; the
//! guard canary remains the overflow authority.
//! Forbidden: using a measured peak as a runtime limit. It only sizes
//! `TASK_STACK_SIZE` at build time (see `docs/ai/chrome-gap-mm-threads.md` M7).
//! Evidence: native tests over synthetic stacks; peaks come from retired tasks
//! only, so a thread that is still running is not represented.

#[cfg(not(rustos_boot_image))]
use super::TASK_STACK_GUARD_BYTES;
#[cfg(test)]
use super::TASK_STACK_SIZE;
use core::sync::atomic::{AtomicU32, AtomicU64, Ordering};

#[cfg(test)]
const PAGE: usize = 4096;
/// A new peak is reported only when it grows by this much, which bounds the
/// records one class can produce (`TASK_STACK_SIZE / PEAK_STEP`).
const PEAK_STEP: u64 = 4096;
/// Total records per boot; a debugcon frame is a synchronous VM exit.
const RECORD_BUDGET: u32 = 64;

static PEAK_USER: AtomicU64 = AtomicU64::new(0);
static PEAK_KERNEL: AtomicU64 = AtomicU64::new(0);
static RETIRED: AtomicU32 = AtomicU32::new(0);
static RECORDED: AtomicU32 = AtomicU32::new(0);

/// Bytes of `stack` ever written, measured from the top. `None` when the whole
/// region above the guard is still zero (the task never ran) or `stack` is too
/// small to have a guard.
#[cfg(test)]
pub(super) fn used_bytes(stack: &[u8]) -> Option<usize> {
    kernel_mm::api::kernel_stack::used_bytes_in_slice(stack, super::TASK_STACK_GUARD_BYTES)
}

/// Records one retired task's stack depth. Called with the detached allocation
/// before it is destroyed. Scans only committed pages on hardware stacks.
pub(super) fn record_retired(
    id: u64,
    user_mode: bool,
    stack: Option<&super::stack_resources::KernelStack>,
) {
    let count = RETIRED.fetch_add(1, Ordering::Relaxed).wrapping_add(1);
    #[cfg(rustos_boot_image)]
    let used = stack.and_then(|s| s.used_bytes());
    #[cfg(not(rustos_boot_image))]
    let used = stack.and_then(|s| s.host_used_bytes(TASK_STACK_GUARD_BYTES));
    let Some(used) = used else {
        return;
    };
    let used = used as u64;
    let peak = if user_mode { &PEAK_USER } else { &PEAK_KERNEL };
    let previous = peak.fetch_max(used, Ordering::Relaxed);
    if used < previous.saturating_add(PEAK_STEP) {
        return;
    }
    // ORDERING: Relaxed; a saturating telemetry budget, as `clone_stage_failed`.
    let within_budget = RECORDED
        .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |recorded| {
            (recorded < RECORD_BUDGET).then_some(recorded + 1)
        })
        .is_ok();
    if within_budget {
        nucleus_core::debug::record_milestone(
            nucleus_core::debug::LogCategory::Sched,
            "kernel-stack-highwater",
            (u64::from(user_mode) << 63) | used,
            (id << 32) | u64::from(count),
        );
    }
}

/// `TASK_STACK_SIZE` minus the largest measured use of either class: how much
/// of the stack no retired task has ever touched.
#[cfg(test)]
pub(super) fn untouched_headroom() -> u64 {
    let peak = PEAK_USER
        .load(Ordering::Relaxed)
        .max(PEAK_KERNEL.load(Ordering::Relaxed));
    (TASK_STACK_SIZE as u64).saturating_sub(peak)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stack_with_write_at(offset_from_top: usize) -> alloc::vec::Vec<u8> {
        let mut stack = alloc::vec![0_u8; TASK_STACK_SIZE];
        let len = stack.len();
        stack[len - offset_from_top] = 0xa5;
        stack
    }

    #[test]
    fn deepest_write_names_the_used_bytes() {
        assert_eq!(used_bytes(&stack_with_write_at(1)), Some(1));
        assert_eq!(used_bytes(&stack_with_write_at(PAGE)), Some(PAGE));
        assert_eq!(used_bytes(&stack_with_write_at(PAGE + 1)), Some(PAGE + 1));
        assert_eq!(
            used_bytes(&stack_with_write_at(TASK_STACK_SIZE - TASK_STACK_GUARD_BYTES)),
            Some(TASK_STACK_SIZE - TASK_STACK_GUARD_BYTES)
        );
    }

    #[test]
    fn the_guard_canary_is_not_stack_use() {
        let mut stack = alloc::vec![0_u8; TASK_STACK_SIZE];
        stack[..TASK_STACK_GUARD_BYTES].fill(0x44);
        assert_eq!(used_bytes(&stack), None);
        let len = stack.len();
        stack[len - 64] = 1;
        assert_eq!(used_bytes(&stack), Some(64));
    }

    #[test]
    fn short_or_untouched_stacks_record_nothing() {
        assert_eq!(used_bytes(&[]), None);
        assert_eq!(used_bytes(&alloc::vec![0_u8; TASK_STACK_SIZE]), None);
    }

    #[test]
    fn a_peak_only_reports_when_it_grows_by_a_step() {
        let before = RECORDED.load(Ordering::Relaxed);
        let stack = super::super::stack_resources::KernelStack::from_host_vec(
            stack_with_write_at(TASK_STACK_SIZE - TASK_STACK_GUARD_BYTES),
        );
        record_retired(1, true, Some(&stack));
        let after_first = RECORDED.load(Ordering::Relaxed);
        record_retired(2, true, Some(&stack));
        assert!(after_first <= before.saturating_add(1));
        assert_eq!(RECORDED.load(Ordering::Relaxed), after_first);
        assert!(untouched_headroom() <= TASK_STACK_GUARD_BYTES as u64);
    }
}
