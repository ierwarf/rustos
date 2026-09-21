//! Stable diagnostics and pure saved-context validation.
//!
//! The scheduler supplies an immutable frame view from its catalog; the
//! validator itself has no `Scheduler` dependency. That is the boundary a
//! future CPU-local dispatcher will use after it has acquired equivalent
//! published stack metadata.

use core::mem;

use super::*;

/// All scheduler-owned facts needed to validate one saved context.
///
/// This is a read view, not an ownership grant. Its caller still establishes
/// that the saved frame is stable before dereferencing it.
#[derive(Clone, Copy, Debug)]
pub(super) struct PublishedFrameView {
    pub(super) user_mode_task: bool,
    pub(super) kernel_process_task: bool,
    pub(super) kernel_stack_base: u64,
    pub(super) kernel_stack_top: u64,
    pub(super) alternate_stack_base: u64,
    pub(super) alternate_stack_top: u64,
    pub(super) stack_canary_intact: bool,
    pub(super) scheduler_storage_base: usize,
    pub(super) scheduler_storage_end: usize,
}

impl PublishedFrameView {
    fn context_stack_contains(self, start: usize, end: usize) -> bool {
        stack_range_contains(self.kernel_stack_base, self.kernel_stack_top, start, end)
            || stack_range_contains(
                self.alternate_stack_base,
                self.alternate_stack_top,
                start,
                end,
            )
    }

    fn scheduler_storage_contains(self, addr: usize) -> bool {
        if addr >= self.scheduler_storage_base && addr < self.scheduler_storage_end {
            return true;
        }

        let virt_offset = crate::memory::paging::KERNEL_VIRT_OFFSET as usize;
        if self.scheduler_storage_base >= virt_offset {
            let low_base = self.scheduler_storage_base - virt_offset;
            let low_end = self.scheduler_storage_end - virt_offset;
            return addr >= low_base && addr < low_end;
        }

        false
    }
}

fn stack_range_contains(base: u64, top: u64, start: usize, end: usize) -> bool {
    if base == 0 || top <= base || end < start {
        return false;
    }
    start >= base as usize && end <= top as usize
}

fn is_canonical_address(addr: u64) -> bool {
    let upper = addr >> 48;
    if ((addr >> 47) & 1) == 0 {
        upper == 0
    } else {
        upper == 0xFFFF
    }
}

fn saved_context_ref(saved_rsp: usize) -> Option<&'static SavedContext> {
    if saved_rsp == 0 || (saved_rsp & (mem::align_of::<SavedContext>() - 1)) != 0 {
        return None;
    }

    // The caller validates complete frame bounds before dereferencing this
    // pointer. Keeping dereference here makes every validator entry share one
    // explicit unsafe boundary.
    Some(unsafe { &*(saved_rsp as *const SavedContext) })
}

/// Validates a stable saved context using only the supplied frame view.
///
/// Error strings are the scheduler's diagnostic ABI; preserve them exactly.
pub(super) fn validate_published_saved_context(
    slot: usize,
    saved_rsp: usize,
    view: PublishedFrameView,
) -> Result<(), &'static str> {
    if saved_rsp == 0 || (saved_rsp & (mem::align_of::<SavedContext>() - 1)) != 0 {
        return Err("saved context pointer is outside the task stack");
    }
    if slot >= MAX_TASK {
        return Err("saved context pointer is outside the task stack");
    }
    let Some(frame_end) = saved_rsp.checked_add(SAVED_CONTEXT_BYTES) else {
        return Err("saved context pointer is outside the task stack");
    };
    if !view.context_stack_contains(saved_rsp, frame_end) {
        return Err("saved context pointer is outside the task stack");
    }
    if slot != ROOT_TASK_SLOT && !view.stack_canary_intact {
        return Err("kernel stack guard was corrupted");
    }

    let saved = saved_context_ref(saved_rsp).ok_or("saved context pointer is invalid")?;
    if (saved.rflags & RFLAGS_RESERVED_BIT_1) == 0 {
        return Err("saved rflags lost the reserved bit");
    }

    let user_cs = crate::arch::gdt::user_code_selector().0 as u64;
    let user_ss = crate::arch::gdt::user_data_selector().0 as u64;
    let kernel_cs = crate::arch::gdt::kernel_code_selector().0 as u64;
    let kernel_ss = crate::arch::gdt::kernel_data_selector().0 as u64;

    if saved.cs == user_cs {
        if !view.user_mode_task {
            return Err("kernel task cannot return directly to user mode");
        }
        if saved.ss != user_ss {
            return Err("user return frame carries an unexpected stack selector");
        }
        if !is_canonical_address(saved.rip)
            || !is_canonical_address(saved.rsp)
            || saved.rip >= crate::memory::paging::USER_SPACE_END_EXCLUSIVE
            || saved.rsp < crate::memory::paging::USER_SPACE_BASE
            || saved.rsp >= crate::memory::paging::USER_SPACE_END_EXCLUSIVE
        {
            return Err("user return frame points outside user space");
        }
        return Ok(());
    }

    if saved.cs != kernel_cs {
        return Err("saved code selector does not match any supported return mode");
    }
    if !is_canonical_address(saved.rip) {
        return Err("kernel return RIP is not canonical");
    }
    if saved.rip >= crate::memory::paging::USER_SPACE_BASE
        && saved.rip < crate::memory::paging::USER_SPACE_END_EXCLUSIVE
        && !view.kernel_process_task
    {
        return Err("kernel return RIP points into user space");
    }
    if view.scheduler_storage_contains(saved.rip as usize) {
        return Err("kernel return RIP points into scheduler storage");
    }

    let kernel_interrupt_frame = saved.rsp == 1 && saved.ss == 0;
    let initial_kernel_frame =
        saved.ss == kernel_ss && is_canonical_address(saved.rsp) && saved.rsp != 0;
    if !kernel_interrupt_frame && !initial_kernel_frame {
        return Err("kernel return frame has an invalid stack layout");
    }
    if initial_kernel_frame && !view.context_stack_contains(saved.rsp as usize, saved.rsp as usize)
    {
        return Err("kernel return RSP does not belong to the task stack");
    }

    Ok(())
}

/// Stable, allocation-free codes for the only failures emitted by
/// `validate_published_saved_context`. Keep this exhaustive: adding a new
/// validation branch without a code is an internal scheduler-contract change,
/// not an ordinary logging change.
pub(super) fn context_validation_reason_code(reason: &'static str) -> u8 {
    match reason {
        "saved context pointer is outside the task stack" => 1,
        "kernel stack guard was corrupted" => 2,
        "saved context pointer is invalid" => 3,
        "saved rflags lost the reserved bit" => 4,
        "kernel task cannot return directly to user mode" => 5,
        "user return frame carries an unexpected stack selector" => 6,
        "user return frame points outside user space" => 7,
        "saved code selector does not match any supported return mode" => 8,
        "kernel return RIP is not canonical" => 9,
        "kernel return RIP points into user space" => 10,
        "kernel return RIP points into scheduler storage" => 11,
        "kernel return frame has an invalid stack layout" => 12,
        "kernel return RSP does not belong to the task stack" => 13,
        _ => panic!(
            "scheduler context validation emitted an unregistered diagnostic reason: {reason}"
        ),
    }
}

impl Scheduler {
    pub(super) fn retire_invalid_ready_tasks(&mut self) {
        let current_cpu = nucleus_core::util::lockdep::current_cpu_index();
        for slot in runqueue::local_runnable_slots(current_cpu) {
            if slot == ROOT_TASK_SLOT {
                continue;
            }
            // Only a published runnable frame is immutable scheduler state.
            // The current task's saved frame is the live kernel stack and may
            // be overwritten until interrupt/syscall entry finishes saving
            // it. Blocked, suspended, and retired slots are validated at their
            // own transition; a foreign running slot is never inspected.
            if !published_frame_is_stable(
                slot,
                self.current_task_slot(),
                super::super::task_slot_is_running(slot),
            ) {
                continue;
            }
            let Some(context) = self.contexts[slot] else {
                continue;
            };
            if !should_validate_published_ready_frame(
                slot,
                self.current_task_slot(),
                self.retired[slot],
                self.start_suspended[slot],
                self.slot_is_runnable(slot),
                self.slot_blocked(slot),
            ) {
                continue;
            }
            let saved_rsp = self.slot_saved_rsp(slot);
            if let Err(reason) = self.validate_saved_context(slot, context.user_mode, saved_rsp) {
                self.log_invalid_context(slot, saved_rsp, reason, "ready-scan");
                self.retire_slot_due_to_invalid_context(slot, saved_rsp, reason);
            }
        }
    }

    pub(super) fn periodic_ready_validation_due(&mut self) -> bool {
        // One acquisition, not two. This runs on every dispatch, and the read
        // and the write were separate acquisitions of the same CPU-private
        // policy lock; the turn counter is the only state either touched.
        let mut policy = self.current_dispatch_policy_mut();
        let turn = policy.ready_validation_turn.saturating_add(1);
        let due = turn >= READY_VALIDATION_INTERVAL_TURNS;
        policy.ready_validation_turn = if due { 0 } else { turn };
        due
    }
}

#[cfg(test)]
mod tests {
    use super::context_validation_reason_code;

    #[test]
    fn context_validation_failure_codes_are_stable_and_exhaustive() {
        assert_eq!(
            context_validation_reason_code("saved context pointer is outside the task stack"),
            1
        );
        assert_eq!(
            context_validation_reason_code("kernel stack guard was corrupted"),
            2
        );
        assert_eq!(
            context_validation_reason_code("saved context pointer is invalid"),
            3
        );
        assert_eq!(
            context_validation_reason_code("saved rflags lost the reserved bit"),
            4
        );
        assert_eq!(
            context_validation_reason_code("kernel task cannot return directly to user mode"),
            5
        );
        assert_eq!(
            context_validation_reason_code(
                "user return frame carries an unexpected stack selector"
            ),
            6
        );
        assert_eq!(
            context_validation_reason_code("user return frame points outside user space"),
            7
        );
        assert_eq!(
            context_validation_reason_code(
                "saved code selector does not match any supported return mode"
            ),
            8
        );
        assert_eq!(
            context_validation_reason_code("kernel return RIP is not canonical"),
            9
        );
        assert_eq!(
            context_validation_reason_code("kernel return RIP points into user space"),
            10
        );
        assert_eq!(
            context_validation_reason_code("kernel return RIP points into scheduler storage"),
            11
        );
        assert_eq!(
            context_validation_reason_code("kernel return frame has an invalid stack layout"),
            12
        );
        assert_eq!(
            context_validation_reason_code("kernel return RSP does not belong to the task stack"),
            13
        );
    }
}
