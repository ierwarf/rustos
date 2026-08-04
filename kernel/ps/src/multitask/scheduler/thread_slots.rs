//! Transactional user-thread scheduler-slot reservation.
//!
//! Clone allocates stack/process membership first, publishes no runnable task
//! until all faultable userspace copyout has committed, and can cancel the
//! exact reservation without leaving a partially visible scheduler slot.

use super::*;

pub(in crate::multitask) struct UserThreadSlotReservation {
    pub(in crate::multitask) slot: usize,
    pub(in crate::multitask) id: u64,
    pub(in crate::multitask) process_handle: ProcessHandle,
    pub(in crate::multitask) process_id: u64,
    root_phys: u64,
    inherited_task_mask: u64,
    inherited_process_mask: u64,
    weight: u32,
    vruntime_ns: u64,
}

impl Scheduler {
    pub(in crate::multitask) fn reserve_user_thread_slot(
        &mut self,
        id: u64,
    ) -> Option<UserThreadSlotReservation> {
        let current = self.contexts[self.current_task_slot()]?;
        let (inherited_task_mask, inherited_process_mask) =
            self.current_affinity_for_child_thread();
        if !current.user_mode {
            return None;
        }

        let root_phys = current.address_space_root;
        let process_handle = current.process_handle?;
        let process_id = current.process_id?;
        for slot in FIRST_DYNAMIC_TASK_SLOT..MAX_TASK {
            if self.contexts[slot].is_none() && !self.thread_slot_reserved[slot] {
                self.reset_stack_storage(slot)?;
                if process_table::attach_task(process_handle).is_none() {
                    self.release_stack_storage(slot);
                    return None;
                }
                self.thread_slot_reserved[slot] = true;
                return Some(UserThreadSlotReservation {
                    slot,
                    id,
                    process_handle,
                    process_id,
                    root_phys,
                    inherited_task_mask,
                    inherited_process_mask,
                    weight: current.weight,
                    vruntime_ns: current
                        .vruntime_ns
                        .saturating_add(SCHED_NEW_TASK_VRUNTIME_PENALTY_NS),
                });
            }
        }
        None
    }

    #[cfg_attr(not(rustos_debug_print_enabled), allow(unused_variables))]
    pub(in crate::multitask) fn commit_user_thread_slot(
        &mut self,
        reservation: UserThreadSlotReservation,
        bootstrap: UserTaskBootstrap,
        user_cs: u64,
        user_ss: u64,
        rflags: u64,
    ) -> Option<(usize, u32)> {
        let slot = reservation.slot;
        if !self
            .thread_slot_reserved
            .get(slot)
            .copied()
            .unwrap_or(false)
            || self.contexts[slot].is_some()
            || process_table::is_process_exiting(reservation.process_id) != Some(false)
        {
            self.cancel_user_thread_slot(reservation);
            return None;
        }
        let (kernel_stack_base, kernel_stack_top) = self.stack_bounds(slot);
        self.contexts[slot] = Some(TaskContext {
            saved_rsp: self.init_user_task_context(slot, &bootstrap, user_cs, user_ss, rflags),
            ready: false,
            ready_since_ticks: 0,
            blocked: true,
            blocked_since_ticks: crate::arch::rtc::ticks(),
            wake_armed: false,
            weight: reservation.weight,
            vruntime_ns: reservation.vruntime_ns,
            exec_start_ticks: 0,
            address_space_root: reservation.root_phys,
            kernel_stack_base: kernel_stack_base as u64,
            kernel_stack_top: kernel_stack_top as u64,
            alternate_kernel_stack_base: 0,
            alternate_kernel_stack_top: 0,
            user_mode: true,
            user_abi: Some(bootstrap.abi),
            console_session: bootstrap.console_session,
            process_handle: Some(reservation.process_handle),
            process_id: Some(reservation.process_id),
            user_stack: bootstrap.user_stack,
            windows_thread_state: bootstrap.windows_thread_state.map(|mut state| {
                state.thread_id = reservation.id;
                state
            }),
        });
        self.simd_states[slot] = SimdState::new();
        self.syscall_user_simd_active[slot] = false;
        self.start_suspended[slot] = true;
        self.starts[slot] = Some(TaskStart {
            entry: super::super::noop_task_entry,
            id: reservation.id,
        });
        self.publish_slot_identity(slot);
        self.install_linux_thread_state(
            slot,
            bootstrap.linux_thread_state.map(|_| reservation.id),
            bootstrap.linux_thread_state,
        );
        self.initialize_slot_affinity(
            slot,
            reservation.inherited_task_mask,
            reservation.inherited_process_mask,
        );
        self.thread_slot_reserved[slot] = false;
        #[cfg(not(test))]
        self.admit_runqueue_slot(slot, false);
        Some((slot, reservation.weight))
    }

    pub(in crate::multitask) fn cancel_user_thread_slot(
        &mut self,
        reservation: UserThreadSlotReservation,
    ) {
        let slot = reservation.slot;
        if self
            .thread_slot_reserved
            .get(slot)
            .copied()
            .unwrap_or(false)
        {
            self.thread_slot_reserved[slot] = false;
            let _ = process_table::detach_task(reservation.process_handle);
            self.release_stack_storage(slot);
        }
    }
}
