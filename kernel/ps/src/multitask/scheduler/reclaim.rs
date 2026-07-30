//! Deferred retirement side effects outside the global scheduler raw owner.
//!
//! - **Owner:** `kernel-ps` owns exact task retirement and deferred teardown.
//! - **Boundary:** scheduler-protected metadata becomes fixed cleanup tokens;
//!   IPC, timer, process-table, allocator, and diagnostic owners run later.
//! - **Lifecycle:** retire and revoke scheduling authority, detach one token,
//!   complete external revocation, acknowledge runtime cleanup, then reclaim.
//! - **Concurrency:** no token is completed while preemption is disabled or
//!   the scheduler raw owner is live; wakeups reacquire it only afterward.
//! - **Failure:** duplicate or skipped cleanup ownership panics or keeps the
//!   retired slot quarantined rather than recycling live authority.
//! - **Forbidden:** allocator release, cross-subsystem locks, output, or IPC
//!   descriptor destruction while the scheduler raw owner is held.
//! - **Evidence:** `cross-cpu-task-retirement`, `endpoint-lifecycle`, and
//!   `process-address-space-lifecycle`.

use alloc::vec::Vec;

use crate::debug;
use crate::multitask::process_table::{self, ProcessHandle};

use super::TaskRetireReason;

#[derive(Clone, Copy)]
pub(in crate::multitask) struct RetirementSideEffect {
    task_id: Option<u64>,
    terminal_process_id: Option<u64>,
    detach_process_handle: Option<ProcessHandle>,
}

impl RetirementSideEffect {
    pub(super) const fn new(task_id: Option<u64>, terminal_process_id: Option<u64>) -> Self {
        Self {
            task_id,
            terminal_process_id,
            detach_process_handle: None,
        }
    }

    pub(super) fn defer_process_detach(&mut self, process_handle: ProcessHandle) {
        assert!(
            self.detach_process_handle.replace(process_handle).is_none(),
            "retirement side effect received duplicate process detach ownership"
        );
    }

    pub(in crate::multitask) fn complete(self, mut wake_task: impl FnMut(u64)) {
        assert!(
            !nucleus_core::util::lockdep::preemption_disabled(),
            "scheduler retirement side effects require released raw ownership"
        );
        if let Some(task_id) = self.task_id {
            kernel_hal::api::arch::rtc::disarm_sleep_waiter(task_id);
            kernel_ipc_runtime::api::remove_endpoint_waiters_for_task(task_id);
            let cancelled =
                kernel_ipc_runtime::api::cancel_endpoint_calls_for_task(task_id, |discarded| {
                    crate::user::handles::drop_ipc_transfer_descriptors(discarded);
                });
            if cancelled != 0 {
                debug::record_milestone(
                    debug::LogCategory::Sched,
                    "retired-task-ipc-cancelled",
                    task_id,
                    cancelled as u64,
                );
            }
            let wake_set = kernel_ipc_runtime::api::fail_endpoints_owned_by_task(
                task_id,
                kernel_ipc_runtime::api::IpcError::PeerClosed,
            );
            wake_tasks(wake_set.callers(), &mut wake_task);
            wake_tasks(wake_set.receivers(), &mut wake_task);
        }
        if let Some(process_id) = self.terminal_process_id {
            let wake_set = kernel_ipc_runtime::api::fail_endpoints_owned_by_process(
                process_id,
                kernel_ipc_runtime::api::IpcError::PeerClosed,
            );
            wake_tasks(wake_set.callers(), &mut wake_task);
            wake_tasks(wake_set.receivers(), &mut wake_task);
        }
        if let Some(process_handle) = self.detach_process_handle {
            let _ = process_table::detach_task(process_handle);
        }
    }
}

fn wake_tasks(task_ids: &[u64], wake_task: &mut impl FnMut(u64)) {
    for task_id in task_ids {
        wake_task(*task_id);
    }
}

/// Ownership detached under the scheduler lock and destroyed only after that
/// lock and local interrupt exclusion have been released.
pub(in crate::multitask) struct RetiredSlotReclaim {
    process_handle: Option<ProcessHandle>,
    stack: Option<Vec<u8>>,
    task_id: u64,
    slot: usize,
    user_mode: bool,
    stack_base: u64,
    stack_top: u64,
    reason: Option<TaskRetireReason>,
}

impl RetiredSlotReclaim {
    #[allow(clippy::too_many_arguments)]
    pub(super) fn new(
        process_handle: Option<ProcessHandle>,
        stack: Option<Vec<u8>>,
        task_id: u64,
        slot: usize,
        user_mode: bool,
        stack_base: u64,
        stack_top: u64,
        reason: Option<TaskRetireReason>,
    ) -> Self {
        Self {
            process_handle,
            stack,
            task_id,
            slot,
            user_mode,
            stack_base,
            stack_top,
            reason,
        }
    }

    pub(in crate::multitask) fn complete(self) {
        match self.reason {
            Some(TaskRetireReason::UserFault {
                vector,
                error_code,
                cr2,
                rip,
            }) => debug::warn!(
                sched,
                "reaped user task pid={} slot={} vector={} error={:?} cr2={:#x} rip={:#x}",
                self.task_id,
                self.slot,
                vector,
                error_code,
                cr2,
                rip,
            ),
            Some(TaskRetireReason::CorruptedContext { saved_rsp, reason }) => {
                debug::record_milestone(
                    debug::LogCategory::Sched,
                    "task-context-corrupted",
                    self.task_id,
                    saved_rsp as u64,
                );
                debug::warn!(
                    sched,
                    "reaped corrupted task pid={} slot={} user_mode={} saved_rsp={:#x} stack=[{:#x}, {:#x}) reason={}",
                    self.task_id,
                    self.slot,
                    self.user_mode,
                    saved_rsp,
                    self.stack_base,
                    self.stack_top,
                    reason,
                );
            }
            Some(TaskRetireReason::Terminated { requested_by_pid }) => {
                let _ = requested_by_pid;
                debug::debug!(
                    sched,
                    "reaped terminated task pid={} slot={} user_mode={} requested_by={:?}",
                    self.task_id,
                    self.slot,
                    self.user_mode,
                    requested_by_pid,
                );
            }
            Some(TaskRetireReason::Exited) => debug::debug!(
                sched,
                "reaped exited task pid={} slot={} user_mode={}",
                self.task_id,
                self.slot,
                self.user_mode,
            ),
            None => {}
        }
        if let Some(handle) = self.process_handle {
            let _ = process_table::detach_task(handle);
        }
        // LIFECYCLE: a task stack may release allocator/page ownership. The
        // token is completed outside the scheduler raw owner so a remote CPU
        // is never forced to spin behind reclamation.
        drop(self.stack);
    }
}
