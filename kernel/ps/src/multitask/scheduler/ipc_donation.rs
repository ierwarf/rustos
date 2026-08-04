//! Reply-owned IPC priority donation.
//!
//! - **Owner:** one live reply capability owns at most one donation record;
//!   the scheduler owns the fixed-capacity table and the derived class floor.
//! - **Boundary:** the compat IPC boundary supplies exact reply, donor, and
//!   receiver identities. A process id is admitted only long enough to select
//!   one concrete eligible worker; it never becomes the donation target.
//! - **Lifecycle:** reserve on call admission, bind to the exact receiving
//!   worker at receive commit, then revoke on the first terminal event
//!   (reply, cancel, donor exit, worker exit, or process teardown).
//! - **Concurrency:** the scheduler raw lock serializes the table; the
//!   selected worker's direct handoff is enqueued on that worker's own CPU
//!   dispatch policy, never on a system-wide hint.
//! - **Failure:** exhausted capacity fails admission closed instead of
//!   silently dropping the inheritance edge, and an unknown or retired
//!   identity is rejected rather than repaired.
//! - **Forbidden:** no process-wide or permanent boost, no donation without a
//!   live reply capability, and no target rebinding outside the bind
//!   transaction.
//! - **Evidence:** `ipc-priority-inheritance`.

use super::*;

/// A bounded priority-inheritance edge for one synchronous IPC reply
/// capability.  The edge lasts only while that reply capability is live: the
/// caller donates its effective scheduling class to the receiver, and the
/// class therefore propagates through a nested synchronous call chain.
///
/// Keeping this in the scheduler rather than in the IPC object store avoids a
/// dependency from `kernel-ipc-runtime` back into `kernel-ps`.  Its lifetime
/// is still tied to the reply capability by the compat IPC boundary.
#[derive(Clone, Copy)]
pub(super) enum IpcDonationTarget {
    AwaitingReceiver,
    BoundWorker(u64),
}

#[derive(Clone, Copy)]
pub(super) struct IpcPriorityDonation {
    pub(super) reply: u64,
    pub(super) donor_task_id: u64,
    /// Donation is never process-wide. A process-owned endpoint must select a
    /// concrete eligible receiver before a System-class call is admitted.
    pub(super) target: IpcDonationTarget,
}

impl Scheduler {
    pub(super) fn eligible_process_worker_slot(&self, process_id: u64) -> Option<usize> {
        (0..MAX_TASK)
            .filter(|slot| {
                self.contexts[*slot].is_some_and(|context| {
                    context.process_id == Some(process_id)
                        && context.ready
                        && self.handoff_slot_ready(*slot)
                })
            })
            .min_by_key(|slot| {
                self.contexts[*slot]
                    .map(|context| (context.vruntime_ns, *slot))
                    .unwrap_or((u64::MAX, *slot))
            })
    }

    /// Atomically selects one concrete process worker, reserves the bounded
    /// reply donation, and publishes its direct handoff. A System-class caller
    /// must use this transaction instead of temporarily boosting a process.
    pub(in crate::multitask) fn bind_ipc_priority_to_process_worker(
        &mut self,
        reply: u64,
        donor_task_id: u64,
        receiver_process_id: u64,
    ) -> Option<u64> {
        let slot = self.eligible_process_worker_slot(receiver_process_id)?;
        let receiver_task_id = self.starts[slot]?.id;
        if !self.bind_reserved_ipc_priority(reply, donor_task_id, receiver_task_id) {
            return None;
        }
        self.apply_ipc_donation(slot);
        let target_cpu = self.slot_dispatch_cpu(slot);
        self.cpu_dispatch[target_cpu]
            .lock()
            .sync_pick_hints
            .enqueue(slot)
            .expect("scheduler synchronous process handoff queue overflow");
        #[cfg(not(test))]
        super::super::irq::request_target_reschedule(target_cpu);
        Some(receiver_task_id)
    }

    /// Reserve bounded donation capacity before the IPC runtime publishes a
    /// reply capability or removes a receive waiter. The caller task is the
    /// temporary unique identity until the reply and worker are both known.
    pub(in crate::multitask) fn reserve_ipc_priority(&mut self, donor_task_id: u64) -> bool {
        let Some(donor_slot) = self.find_task_slot(donor_task_id) else {
            return false;
        };
        if self.retired[donor_slot]
            || self.ipc_priority_donations[..self.ipc_priority_donation_len]
                .iter()
                .flatten()
                .any(|entry| entry.reply == 0 && entry.donor_task_id == donor_task_id)
        {
            return false;
        }
        if !self.ipc_priority_donation_capacity_available() {
            return false;
        }
        self.ipc_priority_donations[self.ipc_priority_donation_len] = Some(IpcPriorityDonation {
            reply: 0,
            donor_task_id,
            target: IpcDonationTarget::AwaitingReceiver,
        });
        self.ipc_priority_donation_len += 1;
        true
    }

    pub(in crate::multitask) fn cancel_ipc_priority_reservation(&mut self, donor_task_id: u64) -> bool {
        let Some(index) = self.ipc_priority_donations[..self.ipc_priority_donation_len]
            .iter()
            .position(|entry| {
                entry.is_some_and(|entry| entry.reply == 0 && entry.donor_task_id == donor_task_id)
            })
        else {
            return false;
        };
        self.remove_ipc_priority_donation(index);
        true
    }

    /// Transfers a pre-enqueue reservation to the exact reply when no worker
    /// was waiting at publication time. The eventual `IPC_RECV` owns binding
    /// that reply to its concrete receiver; timeout/cancel can meanwhile
    /// revoke it by reply identity without retaining process-wide authority.
    pub(in crate::multitask) fn attach_reserved_ipc_priority(&mut self, reply: u64, donor_task_id: u64) -> bool {
        if reply == 0 {
            return false;
        }
        let Some(entry) = self.ipc_priority_donations[..self.ipc_priority_donation_len]
            .iter_mut()
            .find(|entry| {
                entry.is_some_and(|entry| entry.reply == 0 && entry.donor_task_id == donor_task_id)
            })
        else {
            return false;
        };
        *entry = Some(IpcPriorityDonation {
            reply,
            donor_task_id,
            target: IpcDonationTarget::AwaitingReceiver,
        });
        true
    }

    pub(in crate::multitask) fn bind_reserved_ipc_priority(
        &mut self,
        reply: u64,
        donor_task_id: u64,
        receiver_task_id: u64,
    ) -> bool {
        if reply == 0 || donor_task_id == receiver_task_id {
            return false;
        }
        let Some(receiver_slot) = self.find_task_slot(receiver_task_id) else {
            return false;
        };
        if self.retired[receiver_slot] {
            return false;
        }
        let Some(entry) = self.ipc_priority_donations[..self.ipc_priority_donation_len]
            .iter_mut()
            .find(|entry| {
                entry.is_some_and(|entry| entry.reply == 0 && entry.donor_task_id == donor_task_id)
            })
        else {
            return false;
        };
        *entry = Some(IpcPriorityDonation {
            reply,
            donor_task_id,
            target: IpcDonationTarget::BoundWorker(receiver_task_id),
        });
        true
    }

    /// Makes `receiver_task_id` inherit the effective strict scheduling class
    /// of `donor_task_id` until `reply` is completed or cancelled.  Repeating
    /// this for a reply updates the receiver because a process-owned endpoint
    /// may hand a queued request to a different worker than the one initially
    /// woken by the sender.
    pub(in crate::multitask) fn inherit_ipc_priority(
        &mut self,
        reply: u64,
        donor_task_id: u64,
        receiver_task_id: u64,
    ) -> bool {
        if reply == 0 || donor_task_id == receiver_task_id {
            return false;
        }
        let Some(donor_slot) = self.find_task_slot(donor_task_id) else {
            return false;
        };
        let Some(receiver_slot) = self.find_task_slot(receiver_task_id) else {
            return false;
        };
        if self.retired[donor_slot] || self.retired[receiver_slot] {
            return false;
        }

        if self.slot_class(donor_slot) != Some(SchedClass::System) {
            return true;
        }

        if self.bind_reserved_ipc_priority(reply, donor_task_id, receiver_task_id) {
            return true;
        }

        self.upsert_ipc_priority_donation(reply, donor_task_id, receiver_task_id)
    }

    pub(super) fn upsert_ipc_priority_donation(
        &mut self,
        reply: u64,
        donor_task_id: u64,
        receiver_task_id: u64,
    ) -> bool {
        if let Some(entry) = self.ipc_priority_donations[..self.ipc_priority_donation_len]
            .iter_mut()
            .flatten()
            .find(|entry| entry.reply == reply)
        {
            entry.donor_task_id = donor_task_id;
            entry.target = IpcDonationTarget::BoundWorker(receiver_task_id);
            return true;
        }
        if !self.ipc_priority_donation_capacity_available() {
            // The caller must fail admission before blocking. Silently losing
            // donation here would violate the reply's scheduling contract.
            return false;
        }
        self.ipc_priority_donations[self.ipc_priority_donation_len] = Some(IpcPriorityDonation {
            reply,
            donor_task_id,
            target: IpcDonationTarget::BoundWorker(receiver_task_id),
        });
        self.ipc_priority_donation_len += 1;
        true
    }

    pub(super) fn ipc_priority_donation_capacity_available(&self) -> bool {
        self.ipc_priority_donation_len < MAX_TASK
    }

    /// Revokes the donation associated with a completed or cancelled reply
    /// capability.  This is deliberately idempotent so reply/error/timeout
    /// races cannot leave an inherited System class behind.
    pub(in crate::multitask) fn release_ipc_priority(&mut self, reply: u64) -> bool {
        let mut released = false;
        let mut index = 0;
        while index < self.ipc_priority_donation_len {
            if self.ipc_priority_donations[index].is_some_and(|entry| entry.reply == reply) {
                self.remove_ipc_priority_donation(index);
                released = true;
            } else {
                index += 1;
            }
        }
        released
    }

    pub(super) fn release_ipc_priorities_for_task(&mut self, task_id: u64) {
        let mut index = 0;
        while index < self.ipc_priority_donation_len {
            if self.ipc_priority_donations[index].is_some_and(|entry| {
                entry.donor_task_id == task_id
                    || matches!(entry.target, IpcDonationTarget::BoundWorker(target) if target == task_id)
            }) {
                self.remove_ipc_priority_donation(index);
            } else {
                index += 1;
            }
        }
    }

    pub(in crate::multitask) fn release_ipc_priorities_for_process(&mut self, process_id: u64) {
        let mut index = 0;
        while index < self.ipc_priority_donation_len {
            let Some(entry) = self.ipc_priority_donations[index] else {
                panic!("scheduler donation prefix contains an empty entry");
            };
            if self.task_belongs_to_process(entry.donor_task_id, process_id)
                || matches!(
                    entry.target,
                    IpcDonationTarget::BoundWorker(task_id)
                        if self.task_belongs_to_process(task_id, process_id)
                )
            {
                self.remove_ipc_priority_donation(index);
            } else {
                index += 1;
            }
        }
    }

    pub(super) fn remove_ipc_priority_donation(&mut self, index: usize) -> IpcPriorityDonation {
        assert!(index < self.ipc_priority_donation_len);
        self.ipc_priority_donation_len -= 1;
        let removed = self.ipc_priority_donations[index]
            .take()
            .expect("scheduler donation prefix contains an empty entry");
        if index != self.ipc_priority_donation_len {
            self.ipc_priority_donations[index] =
                self.ipc_priority_donations[self.ipc_priority_donation_len].take();
        }
        removed
    }

    pub(super) fn task_belongs_to_process(&self, task_id: u64, process_id: u64) -> bool {
        self.find_task_slot(task_id).is_some_and(|slot| {
            self.contexts[slot].is_some_and(|context| context.process_id == Some(process_id))
        })
    }

    pub(super) fn apply_ipc_donation(&mut self, target_slot: usize) {
        if target_slot == self.current_task_slot() || target_slot >= MAX_TASK {
            return;
        }
        let Some(target) = self.contexts[target_slot] else {
            return;
        };
        if !target.ready || !self.context_is_schedulable(target_slot, target) {
            return;
        }
        let Some(current) = self.contexts[self.current_task_slot()] else {
            return;
        };
        if !self.is_fair_candidate_slot(target_slot)
            || !self.is_fair_candidate_slot(self.current_task_slot())
        {
            return;
        }
        let caller_floor = current.vruntime_ns.saturating_sub(IPC_DONATION_BONUS_NS);
        let class_floor = self
            .slot_class(target_slot)
            .map(|class| {
                self.min_ready_vruntime_in_class(class)
                    .saturating_sub(IPC_DONATION_BONUS_NS)
            })
            .unwrap_or(caller_floor);
        let donated_floor = caller_floor.min(class_floor);
        if let Some(target) = self.contexts[target_slot].as_mut() {
            target.vruntime_ns = target.vruntime_ns.min(donated_floor);
        }
    }
}
