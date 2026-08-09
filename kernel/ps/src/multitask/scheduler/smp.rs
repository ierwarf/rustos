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
        let running_cpu = super::super::cpu_local::task_running_cpu(slot);
        if !remote_task_requires_quiescence(slot, self.current_task_slot(), running_cpu.is_some()) {
            return false;
        }
        self.quarantine_slot_for_deferred_retirement(slot, reason);
        #[cfg(not(test))]
        super::super::irq::request_target_reschedule(
            running_cpu.expect("remote retirement lost running CPU"),
        );
        #[cfg(test)]
        super::super::request_deferred_reschedule();
        true
    }

    pub(in crate::multitask) fn quiesce_current_exec_siblings(&mut self) -> Option<bool> {
        let current_slot = self.current_task_slot();
        let process_handle = self.contexts[current_slot]?.process_handle?;
        let (siblings, count) =
            self.collect_live_process_sibling_slots(current_slot, process_handle);
        let mut waiting_for_remote = false;
        for slot in siblings.into_iter().take(count) {
            let running_cpu = super::super::cpu_local::task_running_cpu(slot);
            if remote_task_requires_quiescence(slot, current_slot, running_cpu.is_some()) {
                self.quarantine_slot_for_deferred_retirement(slot, TaskRetireReason::Exited);
                #[cfg(not(test))]
                super::super::irq::request_target_reschedule(
                    running_cpu.expect("exec sibling lost running CPU"),
                );
                waiting_for_remote = true;
                continue;
            }
            self.deferred_retire_reasons[slot] = None;
            self.retire_exec_sibling_slot(slot);
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

        let target_running_cpu = super::super::cpu_local::task_running_cpu(target_slot);
        let mut waiting_for_remote = remote_task_requires_quiescence(
            target_slot,
            self.current_task_slot(),
            target_running_cpu.is_some(),
        );
        #[cfg(not(test))]
        if waiting_for_remote {
            super::super::irq::request_target_reschedule(
                target_running_cpu.expect("exec target lost running CPU"),
            );
        }
        let (siblings, count) =
            self.collect_live_process_sibling_slots(target_slot, process_handle);
        for slot in siblings.into_iter().take(count) {
            let running_cpu = super::super::cpu_local::task_running_cpu(slot);
            if remote_task_requires_quiescence(
                slot,
                self.current_task_slot(),
                running_cpu.is_some(),
            ) {
                self.quarantine_slot_for_deferred_retirement(slot, TaskRetireReason::Exited);
                #[cfg(not(test))]
                super::super::irq::request_target_reschedule(
                    running_cpu.expect("exec sibling lost running CPU"),
                );
                waiting_for_remote = true;
                continue;
            }
            self.deferred_retire_reasons[slot] = None;
            self.retire_exec_sibling_slot(slot);
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
            slot == self.current_task_slot() || !super::super::task_slot_is_running(slot),
            "scheduler invariant: target exec replaced a remotely running task"
        );
    }

    pub(super) fn context_is_schedulable(&self, slot: usize, context: TaskContext) -> bool {
        let current_cpu = nucleus_core::util::lockdep::current_cpu_index();
        #[cfg(not(test))]
        if !runqueue::is_local_dispatchable(slot, current_cpu) {
            return false;
        }
        if candidate_has_foreign_execution_owner(
            slot,
            self.current_task_slot(),
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

    /// Checks a queued continuation while an idle CPU considers moving it from
    /// an explicitly named foreign source. This is deliberately separate from
    /// [`Self::context_is_schedulable`]: the latter requires local dispatch
    /// custody, while this path must inspect `Local(source_cpu)` before the
    /// existing source-owner CAS transfers it through the target mailbox.
    pub(super) fn context_is_migratable_from_source(
        &self,
        slot: usize,
        context: TaskContext,
        source_cpu: usize,
        target_cpu: usize,
    ) -> bool {
        if source_cpu == target_cpu || !runqueue::is_local_dispatchable(slot, source_cpu) {
            return false;
        }
        if super::super::cpu_local::task_running_cpu(slot).is_some() {
            return false;
        }
        if slot == ROOT_TASK_SLOT || self.idle_cpu[slot] != NO_IDLE_CPU {
            return false;
        }
        let target_bit = 1_u64
            .checked_shl(u32::try_from(target_cpu).expect("logical CPU index overflow"))
            .expect("logical CPU index exceeds affinity mask");
        if self.task_affinity_masks[slot] & self.process_affinity_masks[slot] & target_bit == 0 {
            return false;
        }
        self.handoff_slot_ready(slot)
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
        self.publish_slot_identity(slot);
        self.idle_cpu[slot] = logical_index;
        let idle_mask = 1_u64 << logical_index;
        self.initialize_slot_affinity(slot, idle_mask, idle_mask);
        #[cfg(not(test))]
        runqueue::admit_running(slot, usize::from(logical_index));
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
            self.current_task_slot(),
            expected_slot,
            "secondary CPU entered with another CPU's current task"
        );
        assert_eq!(
            self.idle_cpu[expected_slot], logical_index,
            "secondary CPU entered an idle task it does not own"
        );
        let context = self.contexts[expected_slot]
            .expect("secondary CPU entered without an admitted idle context");
        assert!(
            !context.user_mode
                && !self.slot_is_runnable(expected_slot)
                && !context.blocked
                && context.saved_rsp == 0,
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
    use crate::memory::paging::ProcessAddressSpace;
    use crate::multitask::{UserTaskBootstrap, noop_task_entry, process_table};
    use crate::user::abi::UserAbi;

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

    #[test]
    fn source_migration_requires_exact_runnable_local_owner_and_target_affinity() {
        let _process_table = process_table::tests::isolate_process_table();
        let _cpu_publication = crate::multitask::cpu_local::test_publication_lock();
        let _runqueue_guard = super::super::runqueue::test_serial_guard();
        super::super::runqueue::reset_before_publication();
        let mut scheduler = super::super::tests::boxed_scheduler();
        let source_cpu = 1;
        let target_cpu = 2;
        let base = crate::memory::paging::USER_SPACE_BASE;
        let user_cs = crate::arch::gdt::user_code_selector().0 as u64;
        let user_ss = crate::arch::gdt::user_data_selector().0 as u64;
        let slot = scheduler
            .allocate_user_slot(
                0x7d01,
                ProcessAddressSpace::empty_for_tests(),
                UserTaskBootstrap::new(
                    UserAbi::Linux,
                    x86_64::VirtAddr::new(base + 0x10_000),
                    x86_64::VirtAddr::new(base + 0x11_000),
                ),
                None,
                crate::arch::pit::divisor_from_micros(100),
                user_cs,
                user_ss,
                super::super::RFLAGS_RESERVED_BIT_1,
                false,
                noop_task_entry,
            )
            .expect("source migration slot");
        let target_mask = (1_u64 << source_cpu) | (1_u64 << target_cpu);
        scheduler.initialize_slot_affinity(slot, target_mask, target_mask);
        super::super::runqueue::admit_blocked(slot);
        let context = scheduler.contexts[slot].expect("source migration context");
        super::super::runqueue::publish_local(slot, source_cpu, context.weight);

        assert!(scheduler.context_is_migratable_from_source(slot, context, source_cpu, target_cpu));
        super::super::runqueue::set_runnable(slot, false);
        assert!(
            !scheduler.context_is_migratable_from_source(slot, context, source_cpu, target_cpu),
            "Local runnable=false must not enter the idle-steal source set"
        );

        super::super::runqueue::set_runnable(slot, true);
        scheduler.initialize_slot_affinity(slot, 1_u64 << source_cpu, 1_u64 << source_cpu);
        assert!(
            !scheduler.context_is_migratable_from_source(slot, context, source_cpu, target_cpu),
            "a source candidate must carry target CPU affinity before migration"
        );

        scheduler.initialize_slot_affinity(slot, target_mask, target_mask);
        {
            let _transition_owner = crate::multitask::cpu_local::install_test_transition_owner(
                source_cpu,
                scheduler.current_task_slot(),
                slot,
            );
            assert!(
                !scheduler.context_is_migratable_from_source(slot, context, source_cpu, target_cpu),
                "an outgoing transition stack owner must never enter a migration source set"
            );
        }
        assert!(
            scheduler.context_is_migratable_from_source(slot, context, source_cpu, target_cpu),
            "dropping the exact execution owner must restore queued migration eligibility"
        );
        super::super::runqueue::reset_before_publication();
    }
}
