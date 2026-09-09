//! Architectural state restoration after a scheduler ownership transfer.

use x86_64::instructions::segmentation::{DS, ES, FS, GS, Segment};
use x86_64::registers::model_specific::FsBase;
use x86_64::structures::gdt::SegmentSelector;
use x86_64::{PhysAddr, VirtAddr};

use super::{Scheduler, SchedulerDispatch, SchedulerPhase, TaskContext};

impl Scheduler {
    fn install_current_task_architecture(
        &mut self,
        current_slot: usize,
        current: TaskContext,
        return_to_user: bool,
        restore_address_space: bool,
        arch_marker: &mut u64,
    ) {
        if restore_address_space {
            crate::memory::paging::load_address_space_phys(PhysAddr::new(
                self.slot_address_space_root(current_slot),
            ));
        }
        let (_, kernel_stack_top) = self.slot_kernel_stack_bounds(current_slot);
        self.mark_phase(SchedulerPhase::ArchAddressSpace, arch_marker);
        if kernel_stack_top != 0 {
            assert_eq!(
                kernel_stack_top & 0xF,
                0,
                "scheduler selected a kernel stack top that violates the x86_64 SysV ABI"
            );
            crate::arch::gdt::set_privilege_stack(kernel_stack_top);
            crate::user::syscall::set_kernel_stack_top(kernel_stack_top);
        }

        let fs_base = self.slot_tls_fs_base(current_slot);
        let user_gs_base = current
            .windows_thread_state
            .map(|state| state.teb_address)
            .unwrap_or(0);
        let data_selector = if return_to_user {
            SegmentSelector(0)
        } else {
            crate::arch::gdt::kernel_data_selector()
        };
        let data_selectors_match = DS::get_reg() == data_selector
            && ES::get_reg() == data_selector
            && FS::get_reg() == data_selector
            && GS::get_reg() == data_selector;
        if !data_selectors_match {
            unsafe {
                DS::set_reg(data_selector);
                ES::set_reg(data_selector);
                FS::set_reg(data_selector);
                GS::set_reg(data_selector);
            }
        }
        FsBase::write(VirtAddr::new(fs_base));
        crate::user::syscall::prepare_for_context_return(return_to_user, user_gs_base);
        self.mark_phase(SchedulerPhase::ArchSegments, arch_marker);
    }

    /// Refreshes the architectural user-return contract for a live exception
    /// continuation after it resumes from a blocking scheduler handoff.
    /// The continuation is already executing, so no catalog saved frame is
    /// revalidated or consumed here.
    pub(in crate::multitask) fn refresh_current_user_return_architecture(&mut self) {
        let current_slot = self.current_task_slot();
        let current = self.contexts[current_slot]
            .expect("pager resume selected a missing current task context");
        assert!(
            current.user_mode,
            "pager resume architecture refresh requires a user-owned kernel continuation"
        );
        let mut arch_marker = Self::phase_chain_start();
        self.install_current_task_architecture(current_slot, current, true, true, &mut arch_marker);
    }

    pub(in crate::multitask) fn prepare_current_task_execution(&mut self) {
        self.prepare_current_task_execution_with_address_space_restore(true);
    }

    fn prepare_current_task_execution_with_address_space_restore(
        &mut self,
        restore_address_space: bool,
    ) {
        let mut arch_marker = Self::phase_chain_start();
        let current_slot = self.current_task_slot();
        let current =
            self.contexts[current_slot].expect("scheduler selected a missing task context");
        self.assert_current_task_affinity_allows_dispatch();
        let (task_mask, process_mask, _) = self.slot_affinity_snapshot(current_slot);
        self.replace_slot_affinity(current_slot, task_mask, process_mask, false);
        let return_to_user = self.context_returns_to_user(current_slot);
        self.validate_saved_context(
            current_slot,
            current.user_mode,
            self.slot_saved_rsp(current_slot),
        )
        .expect("scheduler selected an invalid task context");
        self.mark_phase(SchedulerPhase::ArchValidate, &mut arch_marker);
        self.install_current_task_architecture(
            current_slot,
            current,
            return_to_user,
            restore_address_space,
            &mut arch_marker,
        );
    }

    /// Restore task-specific architectural state only across a real task
    /// switch. Same-task scheduler turns retain the already-active CR3, TSS,
    /// syscall stack, segment state, and FS/GS bases. A same-address-space
    /// thread switch restores the per-thread state without replaying CR3. The
    /// SIMD image remains restored by every IRQ leaf because compiler-generated
    /// kernel code may use vector registers after the save boundary.
    pub(in crate::multitask) fn prepare_dispatched_task_execution(
        &mut self,
        dispatch: SchedulerDispatch,
    ) {
        let mut phase_marker = Self::phase_chain_start();
        assert_eq!(
            self.current_task_slot(),
            dispatch.next_slot,
            "scheduler invariant: architecture restore token does not name current task"
        );
        if !dispatch.requires_architectural_restore() {
            assert_eq!(
                dispatch.previous_slot, dispatch.next_slot,
                "scheduler invariant: same-task restore token changed slot identity"
            );
            self.mark_phase(SchedulerPhase::ArchRestore, &mut phase_marker);
            return;
        }
        let previous_root = self.slot_address_space_root(dispatch.previous_slot);
        let next_root = self.slot_address_space_root(dispatch.next_slot);
        let restore_address_space =
            dispatch.requires_address_space_restore(previous_root, next_root);
        self.prepare_current_task_execution_with_address_space_restore(restore_address_space);
        self.mark_phase(SchedulerPhase::ArchRestore, &mut phase_marker);
    }
}
