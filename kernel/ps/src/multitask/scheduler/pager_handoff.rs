//! Fixed-slot page-fault handoff from exception ingress to pagerd.

use super::*;

/// Result of waking a pagerd worker after the faulting task has committed its
/// exact fixed-slot wait. This path deliberately owns no generic endpoint or
/// reply object; its only scheduler input is the pager-fault token and an
/// already registered pagerd task identity.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PagerFaultHandoffOutcome {
    DirectSameCpu,
    DirectCrossCpu,
    WakeOnly,
    SenderMismatch,
    ReceiverMismatch,
    EligibilityUnavailable,
    DirectCustodyUnavailable,
    OrderingUnavailable,
}

impl Scheduler {
    /// Wakes a pagerd worker after exception ingress has committed the current
    /// task's `PagerFault(token)` wait. Unlike generic IPC this is safe to
    /// invoke from the page-fault exception path: every input is a fixed owner
    /// word or a pre-registered task slot, and no endpoint, reply, allocation,
    /// or process-state registry is consulted.
    pub(in crate::multitask) fn handoff_pager_fault_to_waiter(
        &mut self,
        token: u64,
        receiver_task_id: u64,
    ) -> PagerFaultHandoffOutcome {
        let sender_slot = self.current_task_slot();
        let sender_matches = token != 0
            && !self.retired[sender_slot]
            && !self.start_suspended[sender_slot]
            && self.contexts[sender_slot].is_some()
            && self.slot_blocked(sender_slot)
            && self.slot_block_reason(sender_slot) == BlockReason::PagerFault(token);
        if !sender_matches {
            return PagerFaultHandoffOutcome::SenderMismatch;
        }
        let Some(receiver_slot) = self.find_task_slot(receiver_task_id) else {
            return PagerFaultHandoffOutcome::ReceiverMismatch;
        };
        if receiver_slot == sender_slot
            || self.retired[receiver_slot]
            || self.start_suspended[receiver_slot]
            || self.contexts[receiver_slot].is_none()
        {
            return PagerFaultHandoffOutcome::ReceiverMismatch;
        }

        // A pagerd worker can publish its fixed waiter just before committing
        // its wait. If exception ingress wins that tiny race, an ordinary wake
        // clears the arm; the worker's commit observes WakeWon and immediately
        // drains the already-blocked fault slot. No request is lost.
        if !self.slot_wait_armed(receiver_slot)
            || self.slot_block_reason(receiver_slot) != BlockReason::PagerService
            || !self.slot_blocked(receiver_slot)
        {
            return self
                .wake_task_slot(receiver_slot)
                .then_some(PagerFaultHandoffOutcome::WakeOnly)
                .unwrap_or(PagerFaultHandoffOutcome::ReceiverMismatch);
        }

        let current_cpu = Self::current_dispatch_cpu();
        let target_cpu = self.slot_dispatch_cpu(receiver_slot);
        #[cfg(not(test))]
        {
            if self
                .context_dispatch_ineligibility_on_cpu(
                    receiver_slot,
                    self.contexts[receiver_slot].expect("validated pagerd waiter lost context"),
                    target_cpu,
                )
                .is_some()
            {
                return PagerFaultHandoffOutcome::EligibilityUnavailable;
            }
        }

        if target_cpu == current_cpu {
            #[cfg(not(test))]
            if !matches!(
                runqueue::publish_direct_handoff(receiver_slot, current_cpu),
                runqueue::RemoteWakeOutcome::Published { .. }
            ) {
                return PagerFaultHandoffOutcome::DirectCustodyUnavailable;
            }
            if !self.enqueue_synchronous_handoff_slot(receiver_slot) {
                #[cfg(not(test))]
                assert!(
                    runqueue::rollback_direct_handoff(receiver_slot, current_cpu),
                    "pager fault ordering rejection lost direct receiver custody"
                );
                return PagerFaultHandoffOutcome::OrderingUnavailable;
            }
        } else {
            #[cfg(not(test))]
            if !matches!(
                runqueue::publish_remote_wake(
                    receiver_slot,
                    target_cpu,
                    self.slot_weight(receiver_slot),
                ),
                runqueue::RemoteWakeOutcome::Published { .. }
            ) {
                return PagerFaultHandoffOutcome::DirectCustodyUnavailable;
            }
            assert!(
                self.enqueue_synchronous_handoff_slot(receiver_slot),
                "cross-CPU pager fault RunTransfer lost bounded ordering custody"
            );
            #[cfg(not(test))]
            super::super::irq::request_target_reschedule(target_cpu);
        }

        #[cfg(test)]
        {
            let receiver = self.contexts[receiver_slot]
                .as_mut()
                .expect("validated pagerd waiter lost context");
            receiver.block_reason = BlockReason::None;
            receiver.test_ready = true;
        }
        self.set_slot_blocked(receiver_slot, false);
        self.set_slot_blocked_since_ticks(receiver_slot, 0);
        self.set_slot_ready_since_ticks(receiver_slot, Self::ready_since_now_ticks());
        self.set_slot_wait_armed(receiver_slot, false);
        self.set_slot_block_reason(receiver_slot, BlockReason::None);
        // Exception ingress cannot take the donation ledger lock. Once the
        // exact worker is runnable, transfer a bounded one-shot vruntime floor
        // so it executes promptly; the waiter syscall binds the durable
        // scheduling-context donation before returning to userspace.
        self.apply_blocked_pager_donation(sender_slot, receiver_slot);
        if target_cpu == current_cpu {
            PagerFaultHandoffOutcome::DirectSameCpu
        } else {
            PagerFaultHandoffOutcome::DirectCrossCpu
        }
    }
}
