//! Scheduler-owned CPU-affinity state and migration admission.
//!
//! - **Owner:** scheduler task slots own effective per-thread masks; Windows
//!   process masks are mirrored across every live task in the exact process.
//! - **Boundary:** policy services admit identity and requested masks before
//!   these methods perform the allocation-free scheduler commit.
//! - **Lifecycle:** fork/clone inherits, exec preserves, retirement clears, and
//!   a mask excluding a running CPU creates one mandatory reschedule edge.
//! - **Concurrency:** the global scheduler raw lock serializes every mask
//!   snapshot and mutation with dispatch and task publication.
//! - **Failure:** empty/offline masks and foreign targets return bounded
//!   errors; dispatch outside the committed mask is an invariant panic.
//! - **Forbidden:** no best-effort widening, raw APIC bit, partial Windows
//!   process update, or user return while migration remains pending.
//! - **Evidence:** `task-affinity-lifecycle`.

use super::*;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AffinityError {
    InvalidMask,
    MissingTask,
    PermissionDenied,
    WrongAbi,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AffinityCommit {
    pub previous_mask: u64,
    pub reschedule_required: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ProcessAffinitySnapshot {
    pub process_mask: u64,
    pub system_mask: u64,
}

impl Scheduler {
    fn admitted_mask(stored_mask: u64, online_mask: u64) -> Option<u64> {
        let effective = stored_mask & online_mask;
        (online_mask != 0 && effective != 0).then_some(effective)
    }

    fn validate_requested_mask(
        requested_mask: u64,
        online_mask: u64,
        container_mask: u64,
    ) -> Result<u64, AffinityError> {
        (online_mask != 0
            && requested_mask != 0
            && requested_mask & !online_mask == 0
            && requested_mask & !container_mask == 0)
            .then_some(requested_mask)
            .ok_or(AffinityError::InvalidMask)
    }

    fn migration_required(&self, slot: usize, requested_mask: u64) -> bool {
        #[cfg(not(test))]
        let owner_cpu = runqueue::owner(slot).cpu;
        #[cfg(test)]
        let owner_cpu = super::super::cpu_local::task_running_cpu(slot)
            // Boxed scheduler tests have no global CPU-slot publication. In
            // production, a scheduler guard's scratch `current_task` is the
            // exact calling CPU slot loaded by `cpu_local::scheduler_mut`.
            .or_else(|| {
                (slot == self.current_task_slot())
                    .then(nucleus_core::util::lockdep::current_cpu_index)
            });
        owner_cpu.is_some_and(|cpu| requested_mask & (1_u64 << cpu) == 0)
    }

    fn resolve_current_process_task(
        &self,
        target_task_id: u64,
        required_abi: UserAbi,
    ) -> Result<usize, AffinityError> {
        let current = self.contexts[self.current_task_slot()].ok_or(AffinityError::MissingTask)?;
        if !current.user_mode || current.user_abi != Some(required_abi) {
            return Err(AffinityError::WrongAbi);
        }
        let target_slot = if target_task_id == 0 {
            self.current_task_slot()
        } else {
            self.find_user_task_slot(target_task_id)
                .ok_or(AffinityError::MissingTask)?
        };
        let target = self.contexts[target_slot].ok_or(AffinityError::MissingTask)?;
        if target.user_abi != Some(required_abi) {
            return Err(AffinityError::WrongAbi);
        }
        if target.process_handle != current.process_handle {
            return Err(AffinityError::PermissionDenied);
        }
        Ok(target_slot)
    }

    pub(in crate::multitask) fn linux_task_affinity(
        &self,
        target_task_id: u64,
        online_mask: u64,
    ) -> Result<u64, AffinityError> {
        let slot = self.resolve_current_process_task(target_task_id, UserAbi::Linux)?;
        Self::admitted_mask(self.task_affinity_masks[slot], online_mask)
            .ok_or(AffinityError::InvalidMask)
    }

    pub(in crate::multitask) fn set_linux_task_affinity(
        &mut self,
        target_task_id: u64,
        requested_mask: u64,
        online_mask: u64,
    ) -> Result<AffinityCommit, AffinityError> {
        let slot = self.resolve_current_process_task(target_task_id, UserAbi::Linux)?;
        let requested = Self::validate_requested_mask(requested_mask, online_mask, online_mask)?;
        let previous_mask = Self::admitted_mask(self.task_affinity_masks[slot], online_mask)
            .ok_or(AffinityError::InvalidMask)?;
        let reschedule_required = self.migration_required(slot, requested);
        self.task_affinity_masks[slot] = requested;
        self.affinity_migration_pending[slot] = reschedule_required;
        #[cfg(not(test))]
        if reschedule_required {
            self.rehome_runqueue_slot(slot);
        }
        Ok(AffinityCommit {
            previous_mask,
            reschedule_required,
        })
    }

    pub(in crate::multitask) fn windows_process_affinity(
        &self,
        online_mask: u64,
    ) -> Result<ProcessAffinitySnapshot, AffinityError> {
        let slot = self.resolve_current_process_task(0, UserAbi::Windows)?;
        let process_mask = Self::admitted_mask(self.process_affinity_masks[slot], online_mask)
            .ok_or(AffinityError::InvalidMask)?;
        Ok(ProcessAffinitySnapshot {
            process_mask,
            system_mask: online_mask,
        })
    }

    pub(in crate::multitask) fn set_windows_process_affinity(
        &mut self,
        requested_mask: u64,
        online_mask: u64,
    ) -> Result<AffinityCommit, AffinityError> {
        let current_slot = self.resolve_current_process_task(0, UserAbi::Windows)?;
        let process_handle = self.contexts[current_slot]
            .and_then(|context| context.process_handle)
            .ok_or(AffinityError::MissingTask)?;
        let requested = Self::validate_requested_mask(requested_mask, online_mask, online_mask)?;
        let previous_mask =
            Self::admitted_mask(self.process_affinity_masks[current_slot], online_mask)
                .ok_or(AffinityError::InvalidMask)?;

        let mut reschedule_required = false;
        let mut matched = 0usize;
        for slot in FIRST_DYNAMIC_TASK_SLOT..MAX_TASK {
            let Some(context) = self.contexts[slot] else {
                continue;
            };
            if context.process_handle != Some(process_handle) {
                continue;
            }
            assert_eq!(
                context.user_abi,
                Some(UserAbi::Windows),
                "scheduler invariant: one process mixes Linux and Windows task ABI"
            );
            matched += 1;
            let migrate = self.migration_required(slot, requested);
            self.process_affinity_masks[slot] = requested;
            self.task_affinity_masks[slot] = requested;
            self.affinity_migration_pending[slot] = migrate;
            #[cfg(not(test))]
            if migrate {
                self.rehome_runqueue_slot(slot);
            }
            reschedule_required |= migrate;
        }
        assert_ne!(
            matched, 0,
            "scheduler invariant: current Windows process has no live task"
        );
        Ok(AffinityCommit {
            previous_mask,
            reschedule_required,
        })
    }

    pub(in crate::multitask) fn set_windows_current_thread_affinity(
        &mut self,
        requested_mask: u64,
        online_mask: u64,
    ) -> Result<AffinityCommit, AffinityError> {
        let slot = self.resolve_current_process_task(0, UserAbi::Windows)?;
        let process_mask = Self::admitted_mask(self.process_affinity_masks[slot], online_mask)
            .ok_or(AffinityError::InvalidMask)?;
        let requested = Self::validate_requested_mask(requested_mask, online_mask, process_mask)?;
        let previous_mask = Self::admitted_mask(self.task_affinity_masks[slot], online_mask)
            .ok_or(AffinityError::InvalidMask)?;
        let reschedule_required = self.migration_required(slot, requested);
        self.task_affinity_masks[slot] = requested;
        self.affinity_migration_pending[slot] = reschedule_required;
        #[cfg(not(test))]
        if reschedule_required {
            self.rehome_runqueue_slot(slot);
        }
        Ok(AffinityCommit {
            previous_mask,
            reschedule_required,
        })
    }

    pub(super) fn initialize_slot_affinity(
        &mut self,
        slot: usize,
        task_mask: u64,
        process_mask: u64,
    ) {
        assert!(
            slot < MAX_TASK && task_mask != 0 && process_mask != 0,
            "scheduler invariant: invalid initial affinity publication"
        );
        assert!(
            task_mask & !process_mask == 0,
            "scheduler invariant: initial task affinity escapes process mask"
        );
        self.task_affinity_masks[slot] = task_mask;
        self.process_affinity_masks[slot] = process_mask;
        self.affinity_migration_pending[slot] = false;
    }

    pub(super) fn inherited_process_affinity(&self, parent_process_id: Option<u64>) -> u64 {
        let Some(parent_process_id) = parent_process_id else {
            return UNRESTRICTED_CPU_MASK;
        };
        self.contexts
            .iter()
            .enumerate()
            .find_map(|(slot, context)| {
                context
                    .filter(|context| context.process_id == Some(parent_process_id))
                    .map(|_| self.process_affinity_masks[slot])
            })
            .unwrap_or(UNRESTRICTED_CPU_MASK)
    }

    pub(super) fn current_affinity_for_child_thread(&self) -> (u64, u64) {
        (
            self.task_affinity_masks[self.current_task_slot()],
            self.process_affinity_masks[self.current_task_slot()],
        )
    }

    pub(super) fn exec_affinity_snapshot(&self, slot: usize) -> (u64, u64, bool) {
        let task_mask = self.task_affinity_masks[slot];
        let process_mask = self.process_affinity_masks[slot];
        assert!(
            task_mask != 0 && process_mask != 0 && task_mask & !process_mask == 0,
            "scheduler invariant: exec target has invalid affinity state"
        );
        (
            task_mask,
            process_mask,
            self.affinity_migration_pending[slot],
        )
    }

    pub(super) fn assert_exec_affinity_preserved(&self, slot: usize, expected: (u64, u64, bool)) {
        assert_eq!(
            self.exec_affinity_snapshot(slot),
            expected,
            "scheduler invariant: exec changed task/process affinity authority"
        );
    }

    pub(super) fn assert_current_task_affinity_allows_dispatch(&self) {
        let logical_cpu = nucleus_core::util::lockdep::current_cpu_index();
        let bit = 1_u64
            .checked_shl(u32::try_from(logical_cpu).expect("logical CPU index overflow"))
            .expect("logical CPU index exceeds affinity mask");
        assert!(
            self.task_affinity_masks[self.current_task_slot()] & bit != 0,
            "scheduler invariant: task {} dispatched on excluded logical CPU {} mask={:#x} migration_pending={}",
            self.starts[self.current_task_slot()]
                .map(|start| start.id)
                .unwrap_or(0),
            logical_cpu,
            self.task_affinity_masks[self.current_task_slot()],
            self.affinity_migration_pending[self.current_task_slot()],
        );
    }
}

#[cfg(test)]
mod tests {
    use super::super::tests::{boxed_scheduler, test_process, test_user_context};
    use super::*;

    fn install_task(
        scheduler: &mut Scheduler,
        slot: usize,
        task_id: u64,
        process_handle: process_table::ProcessHandle,
        abi: UserAbi,
        task_mask: u64,
        process_mask: u64,
    ) {
        let mut context = test_user_context(process_handle);
        context.user_abi = Some(abi);
        scheduler.contexts[slot] = Some(context);
        scheduler.starts[slot] = Some(TaskStart {
            entry: |_| {},
            id: task_id,
        });
        scheduler.initialize_slot_affinity(slot, task_mask, process_mask);
    }

    #[test]
    fn task_affinity_snapshot_is_exact_and_online_bounded() {
        assert_eq!(Scheduler::admitted_mask(u64::MAX, 0b1111), Some(0b1111));
        assert_eq!(Scheduler::admitted_mask(0b1010, 0b1111), Some(0b1010));
        assert_eq!(Scheduler::admitted_mask(0b10000, 0b1111), None);
    }

    #[test]
    fn linux_thread_affinity_commits_exact_mask_and_previous_value() {
        // Affinity tests attach real processes to the one global process
        // table, so they must hold the same exclusive test isolation as every
        // other scheduler test or a concurrent test observes a foreign process.
        let _process_table = process_table::tests::isolate_process_table();
        let mut scheduler = boxed_scheduler();
        let process = test_process(0xaff1_0001);
        let slot = FIRST_DYNAMIC_TASK_SLOT;
        install_task(
            &mut scheduler,
            slot,
            0xaff1_1001,
            process,
            UserAbi::Linux,
            0b1111,
            0b1111,
        );
        scheduler.current_task = slot;

        assert_eq!(scheduler.linux_task_affinity(0, 0b1111), Ok(0b1111));
        let commit = scheduler
            .set_linux_task_affinity(0, 0b1010, 0b1111)
            .expect("valid Linux affinity commit");
        assert_eq!(commit.previous_mask, 0b1111);
        assert_eq!(scheduler.linux_task_affinity(0, 0b1111), Ok(0b1010));
    }

    #[test]
    fn invalid_affinity_changes_leave_all_authority_unchanged() {
        // Affinity tests attach real processes to the one global process
        // table, so they must hold the same exclusive test isolation as every
        // other scheduler test or a concurrent test observes a foreign process.
        let _process_table = process_table::tests::isolate_process_table();
        let mut scheduler = boxed_scheduler();
        let process = test_process(0xaff1_0002);
        let slot = FIRST_DYNAMIC_TASK_SLOT;
        install_task(
            &mut scheduler,
            slot,
            0xaff1_1002,
            process,
            UserAbi::Linux,
            0b0011,
            0b1111,
        );
        scheduler.current_task = slot;
        for invalid in [0, 0b1_0000] {
            assert_eq!(
                scheduler.set_linux_task_affinity(0, invalid, 0b1111),
                Err(AffinityError::InvalidMask)
            );
            assert_eq!(scheduler.task_affinity_masks[slot], 0b0011);
        }
        assert!(Scheduler::validate_requested_mask(0b1000, 0b1111, 0b0111).is_err());
        // Isolate the online-mask condition from the container-mask one: a
        // request for an offline CPU must be rejected even when the
        // container would otherwise have admitted it.
        assert!(Scheduler::validate_requested_mask(0b1_0000, 0b1111, 0b1_1111).is_err());
    }

    #[test]
    fn excluded_running_cpu_requires_remote_reschedule() {
        // Affinity tests attach real processes to the one global process
        // table, so they must hold the same exclusive test isolation as every
        // other scheduler test or a concurrent test observes a foreign process.
        let _process_table = process_table::tests::isolate_process_table();
        let mut scheduler = boxed_scheduler();
        let process = test_process(0xaff1_0006);
        let slot = FIRST_DYNAMIC_TASK_SLOT;
        install_task(
            &mut scheduler,
            slot,
            0xaff1_1007,
            process,
            UserAbi::Linux,
            0b0011,
            0b0011,
        );
        scheduler.current_task = slot;

        let commit = scheduler
            .set_linux_task_affinity(0, 0b0010, 0b0011)
            .expect("affinity commit excluding the running CPU");
        assert_eq!(commit.previous_mask, 0b0011);
        assert!(commit.reschedule_required);
        assert!(scheduler.affinity_migration_pending[slot]);
        assert!(!scheduler.context_is_schedulable(
            slot,
            scheduler.contexts[slot].expect("installed task context")
        ));
    }

    #[test]
    fn child_task_inherits_effective_parent_affinity() {
        // Affinity tests attach real processes to the one global process
        // table, so they must hold the same exclusive test isolation as every
        // other scheduler test or a concurrent test observes a foreign process.
        let _process_table = process_table::tests::isolate_process_table();
        let mut scheduler = boxed_scheduler();
        let process_id = 0xaff1_0003;
        let process = test_process(process_id);
        let slot = FIRST_DYNAMIC_TASK_SLOT;
        install_task(
            &mut scheduler,
            slot,
            0xaff1_1003,
            process,
            UserAbi::Linux,
            0b0101,
            0b0111,
        );
        scheduler.current_task = slot;
        assert_eq!(
            scheduler.current_affinity_for_child_thread(),
            (0b0101, 0b0111)
        );
        assert_eq!(
            scheduler.inherited_process_affinity(Some(process_id)),
            0b0111
        );
    }

    #[test]
    fn exec_preserves_task_and_process_affinity() {
        // Affinity tests attach real processes to the one global process
        // table, so they must hold the same exclusive test isolation as every
        // other scheduler test or a concurrent test observes a foreign process.
        let _process_table = process_table::tests::isolate_process_table();
        let mut scheduler = boxed_scheduler();
        let process = test_process(0xaff1_0007);
        let slot = FIRST_DYNAMIC_TASK_SLOT;
        install_task(
            &mut scheduler,
            slot,
            0xaff1_1008,
            process,
            UserAbi::Linux,
            0b0011,
            0b0111,
        );
        scheduler.current_task = slot;
        scheduler.affinity_migration_pending[slot] = true;
        let before = scheduler.exec_affinity_snapshot(slot);

        // These are representative exec resets. Affinity is intentionally
        // absent from the reset set and the production exec paths enforce the
        // same snapshot after committing the replacement image.
        scheduler.retired[slot] = false;
        scheduler.exec_target_quiesced[slot] = false;
        scheduler.retire_reasons[slot] = None;
        scheduler.assert_exec_affinity_preserved(slot, before);
    }

    #[test]
    fn windows_process_affinity_updates_every_live_thread_atomically() {
        // Affinity tests attach real processes to the one global process
        // table, so they must hold the same exclusive test isolation as every
        // other scheduler test or a concurrent test observes a foreign process.
        let _process_table = process_table::tests::isolate_process_table();
        let mut scheduler = boxed_scheduler();
        let process = test_process(0xaff1_0004);
        let first = FIRST_DYNAMIC_TASK_SLOT;
        let second = first + 1;
        install_task(
            &mut scheduler,
            first,
            0xaff1_1004,
            process,
            UserAbi::Windows,
            0b1111,
            0b1111,
        );
        install_task(
            &mut scheduler,
            second,
            0xaff1_1005,
            process,
            UserAbi::Windows,
            0b0011,
            0b1111,
        );
        scheduler.current_task = first;
        let commit = scheduler
            .set_windows_process_affinity(0b0101, 0b1111)
            .expect("valid Windows process affinity commit");
        assert_eq!(commit.previous_mask, 0b1111);
        for slot in [first, second] {
            assert_eq!(scheduler.process_affinity_masks[slot], 0b0101);
            assert_eq!(scheduler.task_affinity_masks[slot], 0b0101);
        }
    }

    #[test]
    fn windows_thread_affinity_returns_previous_and_rejects_process_escape() {
        // Affinity tests attach real processes to the one global process
        // table, so they must hold the same exclusive test isolation as every
        // other scheduler test or a concurrent test observes a foreign process.
        let _process_table = process_table::tests::isolate_process_table();
        let mut scheduler = boxed_scheduler();
        let process = test_process(0xaff1_0005);
        let slot = FIRST_DYNAMIC_TASK_SLOT;
        install_task(
            &mut scheduler,
            slot,
            0xaff1_1006,
            process,
            UserAbi::Windows,
            0b0011,
            0b0111,
        );
        scheduler.current_task = slot;
        let commit = scheduler
            .set_windows_current_thread_affinity(0b0010, 0b1111)
            .expect("valid Windows thread affinity commit");
        assert_eq!(commit.previous_mask, 0b0011);
        assert_eq!(scheduler.task_affinity_masks[slot], 0b0010);
        assert_eq!(
            scheduler.set_windows_current_thread_affinity(0b1000, 0b1111),
            Err(AffinityError::InvalidMask)
        );
        assert_eq!(scheduler.task_affinity_masks[slot], 0b0010);
    }
}
