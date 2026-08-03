//! Secondary-CPU idle admission and first scheduler entry.
//!
//! - **Owner:** the BSP creates one disjoint idle slot per admitted AP; the AP
//!   may enter only its own slot after the lifecycle reaches `SchedulerReady`.
//! - **Boundary:** HAL-owned bootstrap stacks remain AP idle stacks until a
//!   complete interrupt frame publishes a resumable continuation.
//! - **Lifecycle:** BSP publication precedes `SchedulerReady`; each AP proves
//!   its slot and stack before publishing `Online`.
//! - **Concurrency:** the global scheduler lock serializes idle-slot creation
//!   and each AP touches only its previously published current slot.
//! - **Failure:** identity, stack, lifecycle, or slot mismatches are fatal
//!   scheduler invariants.
//! - **Forbidden:** no shared idle slot, cross-CPU idle selection, or AP
//!   dispatch before its first complete interrupt continuation.
//! - **Evidence:** `cpu-online-lifecycle`, `scheduler-lifecycle`, and
//!   `smp-reschedule-ipi-lifecycle`.

use super::*;

fn candidate_has_foreign_execution_owner(
    candidate_slot: usize,
    current_slot: usize,
    current_cpu: usize,
    running_cpu: Option<usize>,
) -> bool {
    running_cpu.is_some_and(|owner_cpu| candidate_slot != current_slot || owner_cpu != current_cpu)
}

impl Scheduler {
    pub(super) fn defer_remote_retirement(
        &mut self,
        slot: usize,
        reason: TaskRetireReason,
    ) -> bool {
        if !remote_task_requires_quiescence(
            slot,
            self.current_task,
            super::super::task_slot_is_running(slot),
        ) {
            return false;
        }
        self.quarantine_slot_for_deferred_retirement(slot, reason);
        super::super::request_deferred_reschedule();
        true
    }

    pub(in crate::multitask) fn quiesce_current_exec_siblings(&mut self) -> Option<bool> {
        let current_slot = self.current_task;
        let process_handle = self.contexts[current_slot]?.process_handle?;
        let (siblings, count) =
            self.collect_live_process_sibling_slots(current_slot, process_handle);
        let mut waiting_for_remote = false;
        for slot in siblings.into_iter().take(count) {
            if remote_task_requires_quiescence(
                slot,
                current_slot,
                super::super::task_slot_is_running(slot),
            ) {
                self.quarantine_slot_for_deferred_retirement(slot, TaskRetireReason::Exited);
                waiting_for_remote = true;
                continue;
            }
            self.deferred_retire_reasons[slot] = None;
            self.retire_exec_sibling_slot(slot);
        }
        if waiting_for_remote {
            super::super::request_deferred_reschedule();
        }
        Some(!waiting_for_remote)
    }

    pub(in crate::multitask) fn process_handle_for_thread(
        &self,
        process_id: u64,
        thread_id: u64,
    ) -> Option<ProcessHandle> {
        let slot = self.find_linux_thread_slot(process_id, thread_id)?;
        self.contexts[slot]?.process_handle
    }

    /// Seal an externally targeted exec against dispatch on every CPU.
    ///
    /// Sibling threads use normal retirement because Linux exec destroys
    /// them. The target uses a distinct quiesce bit: normal retirement would
    /// detach its process handle and allow the old address space to disappear
    /// before the replacement state is committed.
    pub(in crate::multitask) fn quiesce_exec_target_and_siblings(
        &mut self,
        process_id: u64,
        thread_id: u64,
        process_handle: ProcessHandle,
    ) -> Option<bool> {
        let target_slot = self.find_linux_thread_slot(process_id, thread_id)?;
        if self.contexts[target_slot]?.process_handle != Some(process_handle) {
            return None;
        }
        self.exec_target_quiesced[target_slot] = true;

        let mut waiting_for_remote = remote_task_requires_quiescence(
            target_slot,
            self.current_task,
            super::super::task_slot_is_running(target_slot),
        );
        let (siblings, count) =
            self.collect_live_process_sibling_slots(target_slot, process_handle);
        for slot in siblings.into_iter().take(count) {
            if remote_task_requires_quiescence(
                slot,
                self.current_task,
                super::super::task_slot_is_running(slot),
            ) {
                self.quarantine_slot_for_deferred_retirement(slot, TaskRetireReason::Exited);
                waiting_for_remote = true;
                continue;
            }
            self.deferred_retire_reasons[slot] = None;
            self.retire_exec_sibling_slot(slot);
        }
        if waiting_for_remote {
            super::super::request_deferred_reschedule();
        }
        Some(!waiting_for_remote)
    }

    pub(in crate::multitask) fn cancel_exec_target_quiesce(
        &mut self,
        process_id: u64,
        thread_id: u64,
        process_handle: ProcessHandle,
    ) -> bool {
        let Some(slot) = self.find_linux_thread_slot(process_id, thread_id) else {
            return false;
        };
        if self.contexts[slot].and_then(|context| context.process_handle) != Some(process_handle) {
            return false;
        }
        self.exec_target_quiesced[slot] = false;
        true
    }

    pub(super) fn assert_exec_target_replacement_safe(&self, slot: usize) {
        assert!(
            self.exec_target_quiesced[slot],
            "scheduler invariant: target exec committed without quiesce admission"
        );
        assert!(
            slot == self.current_task || !super::super::task_slot_is_running(slot),
            "scheduler invariant: target exec replaced a remotely running task"
        );
    }

    pub(super) fn context_is_schedulable(&self, slot: usize, context: TaskContext) -> bool {
        let current_cpu = nucleus_core::util::lockdep::current_cpu_index();
        if candidate_has_foreign_execution_owner(
            slot,
            self.current_task,
            current_cpu,
            super::super::cpu_local::task_running_cpu(slot),
        ) {
            return false;
        }
        if slot == ROOT_TASK_SLOT && current_cpu != 0 {
            return false;
        }
        let idle_cpu = self.idle_cpu[slot];
        if idle_cpu != NO_IDLE_CPU && usize::from(idle_cpu) != current_cpu {
            return false;
        }
        let affinity_bit = 1_u64
            .checked_shl(u32::try_from(current_cpu).expect("logical CPU index overflow"))
            .expect("logical CPU index exceeds affinity mask");
        if self.task_affinity_masks[slot] & affinity_bit == 0 {
            return false;
        }
        !self.job_stopped[slot]
            && !self.exec_target_quiesced[slot]
            && self
                .context_validation_error(slot, context, context.saved_rsp)
                .is_none()
    }

    pub(super) fn idle_fallback_slot(&self) -> usize {
        let current_cpu = nucleus_core::util::lockdep::current_cpu_index();
        if current_cpu == 0 {
            return ROOT_TASK_SLOT;
        }
        self.idle_cpu
            .iter()
            .position(|owner| usize::from(*owner) == current_cpu)
            .unwrap_or_else(|| {
                panic!("scheduler invariant: logical CPU {current_cpu} has no idle slot")
            })
    }

    pub(in crate::multitask) fn initialize_secondary_idle(
        &mut self,
        logical_index: u8,
        entry: fn(u64),
        id: u64,
        raw_stack_base: u64,
        stack_top: u64,
    ) -> usize {
        assert!(
            self.initialized(),
            "secondary idle initialized before BSP scheduler"
        );
        assert!(
            logical_index != 0
                && usize::from(logical_index) < nucleus_core::util::lockdep::MAX_TRACKED_CPUS,
            "secondary idle has an invalid logical CPU"
        );
        assert!(
            stack_top > raw_stack_base
                && stack_top - raw_stack_base
                    > (TASK_STACK_GUARD_BYTES + SAVED_CONTEXT_BYTES) as u64,
            "secondary idle stack is too small"
        );
        let slot = MAX_TASK
            .checked_sub(usize::from(logical_index))
            .expect("secondary idle slot underflow");
        assert!(
            self.contexts[slot].is_none() && self.idle_cpu[slot] == NO_IDLE_CPU,
            "secondary idle slot is already owned"
        );

        let canary_words = TASK_STACK_GUARD_BYTES / mem::size_of::<u64>();
        let guard = raw_stack_base as *mut u64;
        for index in 0..canary_words {
            // SAFETY: kernel-hal assigned this disjoint static stack to the
            // exact AP, whose active RSP starts at the opposite end.
            unsafe {
                ptr::write(guard.add(index), STACK_CANARY_WORD);
            }
        }
        let now = crate::arch::rtc::ticks();
        self.contexts[slot] = Some(TaskContext {
            saved_rsp: 0,
            ready: false,
            ready_since_ticks: 0,
            blocked: false,
            blocked_since_ticks: 0,
            wake_armed: false,
            weight: NICE_0_LOAD,
            vruntime_ns: 0,
            exec_start_ticks: now,
            address_space_root: crate::memory::paging::kernel_root_phys().as_u64(),
            kernel_stack_base: raw_stack_base + TASK_STACK_GUARD_BYTES as u64,
            kernel_stack_top: stack_top,
            alternate_kernel_stack_base: 0,
            alternate_kernel_stack_top: 0,
            user_mode: false,
            user_abi: None,
            console_session: ConsoleSessionHandle::SYSTEM,
            process_handle: None,
            process_id: None,
            user_stack: None,
            windows_thread_state: None,
        });
        self.starts[slot] = Some(TaskStart { entry, id });
        self.idle_cpu[slot] = logical_index;
        let idle_mask = 1_u64 << logical_index;
        self.initialize_slot_affinity(slot, idle_mask, idle_mask);
        slot
    }

    pub(in crate::multitask) fn prepare_secondary_idle_execution(&self, logical_index: u8) {
        assert!(
            logical_index != 0
                && usize::from(logical_index) < nucleus_core::util::lockdep::MAX_TRACKED_CPUS,
            "secondary scheduler entry has an invalid logical CPU"
        );
        let expected_slot = MAX_TASK
            .checked_sub(usize::from(logical_index))
            .expect("secondary idle slot underflow");
        assert_eq!(
            self.current_task, expected_slot,
            "secondary CPU entered with another CPU's current task"
        );
        assert_eq!(
            self.idle_cpu[expected_slot], logical_index,
            "secondary CPU entered an idle task it does not own"
        );
        let context = self.contexts[expected_slot]
            .expect("secondary CPU entered without an admitted idle context");
        assert!(
            !context.user_mode && !context.ready && !context.blocked && context.saved_rsp == 0,
            "secondary CPU idle bootstrap state is inconsistent"
        );
        crate::memory::paging::load_address_space_phys(PhysAddr::new(context.address_space_root));
        crate::arch::gdt::set_privilege_stack(context.kernel_stack_top);
        crate::user::syscall::set_kernel_stack_top(context.kernel_stack_top);
        let task_id = self.starts[expected_slot]
            .map(|start| start.id)
            .expect("secondary idle task is missing identity");
        nucleus_core::util::lockdep::set_current_task_owner(
            task_id
                .checked_add(1)
                .expect("secondary idle task id exhausted lock owner token"),
        );
    }
}

const fn remote_task_requires_quiescence(
    slot: usize,
    current_slot: usize,
    task_is_running: bool,
) -> bool {
    slot != current_slot && task_is_running
}

#[cfg(test)]
mod tests {
    use super::{candidate_has_foreign_execution_owner, remote_task_requires_quiescence};

    #[test]
    fn remote_or_transition_owned_task_is_not_schedulable() {
        assert!(!candidate_has_foreign_execution_owner(7, 7, 1, Some(1)));
        assert!(candidate_has_foreign_execution_owner(7, 7, 1, Some(0)));
        assert!(candidate_has_foreign_execution_owner(7, 3, 1, Some(1)));
        assert!(candidate_has_foreign_execution_owner(7, 3, 1, Some(0)));
        assert!(!candidate_has_foreign_execution_owner(7, 3, 1, None));
    }

    #[test]
    fn remote_retirement_waits_only_for_another_cpus_running_slot() {
        assert!(remote_task_requires_quiescence(7, 3, true));
        assert!(!remote_task_requires_quiescence(3, 3, true));
        assert!(!remote_task_requires_quiescence(7, 3, false));
    }
}
