//! Terminal wake handoff for replies and pager faults.
//!
//! - **Owner:** kernel-ps owns the exact wake transition a completed reply or
//!   pager fault performs on its one blocked owner, and the opaque post-wake
//!   proof handed to the per-CPU synchronous-handoff owner.
//! - **Boundary:** the compat IPC and pager boundaries supply a reply handle
//!   or fault token plus the task identity that is claimed to be waiting on
//!   it. Both are untrusted until the slot's live identity and wait reason
//!   agree with them.
//! - **Lifecycle:** revoke the donation this key owns, wake the exact blocked
//!   owner once, then mint one runqueue proof for it. A key that no longer
//!   names a live wait mints nothing and wakes nothing.
//! - **Concurrency:** every transition here runs under the caller's scheduler
//!   acquisition; the returned proof reads the owner word exactly once.
//! - **Failure:** a stale key, a substituted slot identity, or an executing
//!   owner fails closed rather than waking an unrelated wait generation.
//! - **Forbidden:** no generic wake, no best-effort pick hint, and no proof
//!   minted for a task the wake did not itself transition.
//! - **Evidence:** `ipc-priority-inheritance`, `synchronous-ipc-handoff`, and
//!   `pager-fault-slot-lifecycle`.

use super::*;

impl Scheduler {
    /// Completes the scheduler half of a terminal IPC reply under one catalog
    /// acquisition.  The returned proof carries no lifecycle authority: it is
    /// only an exact post-wake runqueue snapshot for the external per-CPU
    /// synchronous-handoff owner.
    pub(in crate::multitask) fn complete_ipc_reply_wake_handoff(
        &mut self,
        reply: u64,
        task_id: u64,
    ) -> Option<ReplyWakeHandoff> {
        #[cfg(test)]
        let _ = self.release_ipc_priority(reply, ipc_donation::DonationNamespace::IpcReply);
        #[cfg(not(test))]
        let _ = reply;
        let slot = self.find_task_slot(task_id)?;
        if !self.wake_task_slot(slot) {
            return None;
        }
        self.reply_wake_handoff(slot, task_id)
    }

    /// Completes one exact pager-fault wait. The token check happens before
    /// wake ownership changes, so a delayed reply can never wake a task that
    /// has already moved on to another wait generation.
    pub(in crate::multitask) fn complete_pager_fault_wake_handoff(
        &mut self,
        fault_token: u64,
        task_id: u64,
    ) -> Option<ReplyWakeHandoff> {
        let slot = self.find_task_slot(task_id)?;
        if self.slot_block_reason(slot) != BlockReason::PagerFault(fault_token) {
            return None;
        }
        if !self.wake_task_slot(slot) {
            return None;
        }
        self.reply_wake_handoff(slot, task_id)
    }

    pub(in crate::multitask) fn complete_fast_ipc_reply_handoff(
        &mut self,
        reply: u64,
        caller_task_id: u64,
    ) -> FastIpcReplyHandoffOutcome {
        let Some(caller_slot) = self.find_task_slot(caller_task_id) else {
            return FastIpcReplyHandoffOutcome::Rejected;
        };
        self.complete_fast_ipc_reply_handoff_slot(reply, caller_slot)
    }

    fn complete_fast_ipc_reply_handoff_slot(
        &mut self,
        reply: u64,
        caller_slot: usize,
    ) -> FastIpcReplyHandoffOutcome {
        if self.retired[caller_slot]
            || self.start_suspended[caller_slot]
            || self.slot_wait_armed(caller_slot)
            || self.slot_block_reason(caller_slot) != BlockReason::EndpointReply(reply)
            || self.contexts[caller_slot].is_none()
            || !self.slot_blocked(caller_slot)
        {
            return FastIpcReplyHandoffOutcome::Rejected;
        }
        let current_cpu = Self::current_dispatch_cpu();
        if self.slot_dispatch_cpu(caller_slot) != current_cpu {
            return FastIpcReplyHandoffOutcome::Rejected;
        }
        #[cfg(not(test))]
        {
            if !self.context_is_dispatch_eligible(
                caller_slot,
                self.contexts[caller_slot].expect("validated fast IPC caller lost context"),
            ) {
                return FastIpcReplyHandoffOutcome::Rejected;
            }
        }
        #[cfg(not(test))]
        if !matches!(
            runqueue::publish_direct_handoff(caller_slot, current_cpu),
            runqueue::RemoteWakeOutcome::Published { .. }
        ) {
            return FastIpcReplyHandoffOutcome::Rejected;
        }
        let direct = self.enqueue_synchronous_handoff_slot(caller_slot);
        if !direct {
            #[cfg(not(test))]
            assert!(
                runqueue::materialize_direct_handoff(
                    caller_slot,
                    current_cpu,
                    self.slot_weight(caller_slot),
                ),
                "fast IPC reply fallback lost caller custody"
            );
        }
        #[cfg(test)]
        {
            let caller = self.contexts[caller_slot]
                .as_mut()
                .expect("validated fast IPC caller lost context");
            caller.block_reason = BlockReason::None;
            caller.test_ready = true;
        }
        self.set_slot_blocked(caller_slot, false);
        self.set_slot_blocked_since_ticks(caller_slot, 0);
        self.set_slot_ready_since_ticks(caller_slot, Self::ready_since_now_ticks());
        self.set_slot_block_reason(caller_slot, BlockReason::None);
        if direct {
            FastIpcReplyHandoffOutcome::Direct
        } else {
            FastIpcReplyHandoffOutcome::LocalFallback
        }
    }

    /// Validates returned scheduling-context custody, releases the reply-owned
    /// donation, and publishes the reverse fast handoff under one scheduler
    /// catalog acquisition. The established Scheduler -> DonationLedger order
    /// is already used by call admission; no reverse acquisition exists.
    pub(in crate::multitask) fn settle_and_complete_fast_ipc_reply_handoff(
        &mut self,
        reply: u64,
        caller_task_id: u64,
        context_owner_task_id: u64,
        scheduling_context: ObjectIdentity,
    ) -> Option<FastIpcReplyHandoffOutcome> {
        let context_owner_slot =
            self.scheduling_context_slot(context_owner_task_id, scheduling_context)?;
        let caller_slot = if context_owner_task_id == caller_task_id {
            context_owner_slot
        } else {
            self.find_task_slot(caller_task_id)?
        };
        let _ = release_reply_donation(reply, ipc_donation::DonationNamespace::IpcReply);
        Some(self.complete_fast_ipc_reply_handoff_slot(reply, caller_slot))
    }

    pub(super) fn reply_wake_handoff(&self, slot: usize, task_id: u64) -> Option<ReplyWakeHandoff> {
        self.reply_wake_handoff_from_owner(slot, task_id, runqueue::owner(slot))
    }

    /// Pure token-mint decision shared by the production owner-word read and
    /// host witnesses. Keeping this outside `cfg(not(test))` makes the exact
    /// identity/runnability/custody seam executable without weakening the
    /// unit-test isolation of the global runqueue backend.
    pub(super) fn reply_wake_handoff_from_owner(
        &self,
        slot: usize,
        task_id: u64,
        owner: runqueue::RunOwnerSnapshot,
    ) -> Option<ReplyWakeHandoff> {
        if self.starts[slot].is_none_or(|start| start.id != task_id) {
            return None;
        }
        ReplyWakeHandoff::from_owner(slot, task_id, owner)
    }
}
