//! Stable diagnostics for saved-context validation failures.
//!
//! The scheduler owns frame validation. This small table owns only the
//! allocation-free reason-code ABI consumed by SMP qualification logs.

/// Stable, allocation-free codes for the only failures emitted by
/// `Scheduler::validate_saved_context`. Keep this exhaustive: adding a new
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

use super::*;

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
                context.ready,
                context.blocked,
            ) {
                continue;
            }
            if let Err(reason) =
                self.validate_saved_context(slot, context.user_mode, context.saved_rsp)
            {
                self.log_invalid_context(slot, context.saved_rsp, reason, "ready-scan");
                self.retire_slot_due_to_invalid_context(slot, context.saved_rsp, reason);
            }
        }
    }

    pub(super) fn periodic_ready_validation_due(&mut self) -> bool {
        let turn = self
            .current_dispatch_policy()
            .ready_validation_turn
            .saturating_add(1);
        if turn >= READY_VALIDATION_INTERVAL_TURNS {
            self.current_dispatch_policy_mut().ready_validation_turn = 0;
            true
        } else {
            self.current_dispatch_policy_mut().ready_validation_turn = turn;
            false
        }
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
