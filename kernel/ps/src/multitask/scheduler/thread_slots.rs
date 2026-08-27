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
    scheduling_policy: Option<scheduling_context::SchedulingContextPolicy>,
    scheduling_domain_slot: Option<usize>,
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

        let root_phys = self.slot_address_space_root(self.current_task_slot());
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
                    weight: self.slot_weight(self.current_task_slot()),
                    scheduling_policy: current.scheduling_context.policy(),
                    scheduling_domain_slot: current.scheduling_context.domain_slot(),
                    vruntime_ns: self
                        .slot_vruntime(self.current_task_slot())
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
        let saved_rsp = self.init_user_task_context(slot, &bootstrap, user_cs, user_ss, rflags);
        let mut scheduling_context =
            scheduling_context::SchedulingContext::bind(slot, reservation.id);
        match (
            reservation.scheduling_policy,
            reservation.scheduling_domain_slot,
        ) {
            (Some(policy), Some(domain_slot)) => {
                if !scheduling_context.admit(policy, domain_slot) {
                    self.cancel_user_thread_slot(reservation);
                    return None;
                }
            }
            (None, None) => {}
            _ => {
                self.cancel_user_thread_slot(reservation);
                return None;
            }
        }
        self.contexts[slot] = Some(TaskContext {
            scheduling_context,
            #[cfg(test)]
            saved_rsp,
            #[cfg(test)]
            test_ready: false,
            #[cfg(test)]
            ready_since_ticks: 0,
            #[cfg(test)]
            blocked: true,
            #[cfg(test)]
            blocked_since_ticks: crate::arch::rtc::ticks(),
            #[cfg(test)]
            wake_armed: false,
            #[cfg(test)]
            block_reason: BlockReason::None,
            #[cfg(test)]
            weight: reservation.weight,
            #[cfg(test)]
            vruntime_ns: reservation.vruntime_ns,
            #[cfg(test)]
            exec_start_ticks: 0,
            #[cfg(test)]
            address_space_root: reservation.root_phys,
            #[cfg(test)]
            kernel_stack_base: kernel_stack_base as u64,
            #[cfg(test)]
            kernel_stack_top: kernel_stack_top as u64,
            #[cfg(test)]
            alternate_kernel_stack_base: 0,
            #[cfg(test)]
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
        self.initialize_slot_vruntime(slot, reservation.vruntime_ns);
        self.initialize_slot_exec_start_ticks(slot, 0);
        self.initialize_slot_weight(slot, reservation.weight);
        self.initialize_slot_address_space_root(slot, reservation.root_phys);
        self.initialize_slot_wait_state(slot);
        self.set_slot_blocked(slot, true);
        self.set_slot_blocked_since_ticks(slot, crate::arch::rtc::ticks());
        self.initialize_slot_saved_rsp(slot, saved_rsp);
        self.initialize_slot_kernel_stack_bounds(
            slot,
            kernel_stack_base as u64,
            kernel_stack_top as u64,
        );
        self.initialize_slot_alternate_kernel_stack_bounds(slot);
        self.initialize_slot_simd_state(slot);
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

#[cfg(test)]
mod tests {
    use super::super::FIRST_DYNAMIC_TASK_SLOT;
    use super::super::tests::boxed_scheduler;

    #[test]
    fn a_reserved_thread_slot_is_never_handed_to_a_process_allocation() {
        // `reserve_user_thread_slot` leaves `contexts[slot]` as `None` while it
        // holds the slot, so an allocation scan that only tests for an absent
        // context will take a slot a pending thread commit already owns. Both
        // then write the same stack: the process allocation zeroes it and
        // installs its frame, the thread commit installs its own context whose
        // `saved_rsp` points into the zeroed region, and activation later finds
        // a frame with correct bounds, an intact canary, and all-zero contents.
        //
        // That is the `handoffs.rs` activation panic that failed 8-vCPU runs
        // intermittently across three sessions.
        let mut scheduler = boxed_scheduler();
        let reserved = FIRST_DYNAMIC_TASK_SLOT;
        scheduler.thread_slot_reserved[reserved] = true;
        assert!(scheduler.contexts[reserved].is_none());

        // Every allocation scan must skip it while it stays reserved.
        assert!(scheduler.first_allocatable_slot() > reserved);
        scheduler.thread_slot_reserved[reserved] = false;
        assert_eq!(scheduler.first_allocatable_slot(), reserved);
    }
}
