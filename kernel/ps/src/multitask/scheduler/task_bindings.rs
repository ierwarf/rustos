//! Current-task identity views needed by user ABI and scheduler-owned waits.
//!
//! These are catalog snapshots, distinct from the per-slot execution payload.

use super::{ConsoleSessionHandle, ProcessHandle, Scheduler, UserAbi};

impl Scheduler {
    pub(in crate::multitask) fn current_user_process_binding(
        &self,
    ) -> Option<(u64, UserAbi, ProcessHandle, ConsoleSessionHandle)> {
        let slot = self.current_task_slot();
        let context = self.contexts[slot]?;
        if !context.user_mode {
            return None;
        }

        let thread_id = self.starts[slot].map(|start| start.id)?;
        let abi = context.user_abi?;
        let process_handle = context.process_handle?;
        Some((thread_id, abi, process_handle, context.console_session))
    }

    /// Snapshot the immutable identity needed by scheduler-owned wait keys.
    ///
    /// Futex admission runs before a task has installed timeout recovery
    /// authority, so it cannot spin behind unrelated process-state mutation.
    pub(in crate::multitask) fn current_user_wait_binding(&self) -> Option<(u64, UserAbi, u64)> {
        let slot = self.current_task_slot();
        let context = self.contexts[slot]?;
        if !context.user_mode {
            return None;
        }
        let thread_id = self.starts[slot].map(|start| start.id)?;
        Some((
            thread_id,
            context.user_abi?,
            self.slot_address_space_root(slot),
        ))
    }
}
