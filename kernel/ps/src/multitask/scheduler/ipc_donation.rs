//! Reply-owned IPC priority donation.
//!
//! - **Owner:** one live reply capability owns at most one donation record;
//!   `SchedulerDonation` owns the bounded ledger while the scheduler resolves
//!   task identity and dispatch reads the receiver's derived floor.
//! - **Boundary:** the compat IPC boundary supplies exact reply, donor, and
//!   receiver identities. A process id is admitted only long enough to select
//!   one concrete eligible worker; it never becomes the donation target.
//! - **Lifecycle:** reserve on call admission, bind to the exact receiving
//!   worker at receive commit, then revoke on the first terminal event
//!   (reply, cancel, donor exit, worker exit, or process teardown).
//! - **Concurrency:** `SchedulerDonation` serializes reply-edge mutation;
//!   dispatch observes only its receiver-slot inheritance counter. The
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
    /// Root scheduling-context owner propagated across nested passive-server
    /// calls. The immediate donor remains the reply wake owner.
    pub(super) context_owner_task_id: u64,
    pub(super) context_owner_slot: usize,
    pub(super) priority_donated: bool,
    pub(super) custody_active: bool,
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
                        && self.slot_is_runnable(*slot)
                        && self.handoff_slot_ready(*slot)
                })
            })
            .min_by_key(|slot| {
                self.contexts[*slot]
                    .map(|_| (self.slot_vruntime(*slot), *slot))
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
        #[cfg(not(test))]
        let target_cpu = self.slot_dispatch_cpu(slot);
        let _ = self.enqueue_synchronous_handoff_slot(slot);
        #[cfg(not(test))]
        super::super::irq::request_target_reschedule(target_cpu);
        Some(receiver_task_id)
    }

    /// The caller's scheduling class and its donation reservation, in one
    /// scheduler entry.
    ///
    /// Only a System caller reserves, so the class decides whether the
    /// reservation is attempted at all -- which is exactly why the two used to
    /// be separate calls, and exactly why one entry can do both from a single
    /// slot lookup. See [`super::IpcCallAdmission`].
    pub(in crate::multitask) fn reserve_ipc_call_donation(
        &mut self,
        donor_task_id: u64,
    ) -> super::IpcCallAdmission {
        let Some(slot) = self.find_task_slot(donor_task_id) else {
            return super::IpcCallAdmission {
                system_class: false,
                donation_reserved: false,
                scheduling_context: None,
                scheduling_context_owner_task_id: None,
            };
        };
        let system_class = !self.retired[slot] && self.slot_class(slot) == Some(SchedClass::System);
        let context_owner_slot = self.effective_scheduling_context_owner_slot(slot);
        let context_owner_task_id = self.starts[context_owner_slot].map(|start| start.id);
        let scheduling_context = self.contexts[context_owner_slot]
            .map(|context| context.scheduling_context)
            .zip(context_owner_task_id)
            .filter(|(context, owner_task_id)| context.is_bound_to(*owner_task_id))
            .map(|(context, _)| context.identity());
        let donation_reserved = scheduling_context.is_some()
            && context_owner_task_id.is_some()
            && self.reserve_ipc_priority_with_context(
                donor_task_id,
                context_owner_task_id.expect("live context owner disappeared"),
                context_owner_slot,
                system_class,
            );
        super::IpcCallAdmission {
            system_class,
            donation_reserved,
            scheduling_context,
            scheduling_context_owner_task_id: context_owner_task_id,
        }
    }

    pub(super) fn effective_scheduling_context_owner_slot(&self, slot: usize) -> usize {
        #[cfg(not(test))]
        {
            super::donation_ledger::borrowed_context_owner_slot(slot).unwrap_or(slot)
        }
        #[cfg(test)]
        {
            let task_id = self.starts[slot].map(|start| start.id);
            self.ipc_priority_donations[..self.ipc_priority_donation_len]
                .iter()
                .flatten()
                .find_map(|entry| {
                    (entry.custody_active
                        && matches!(entry.target, IpcDonationTarget::BoundWorker(target)
                        if Some(target) == task_id))
                    .then_some(entry.context_owner_slot)
                })
                .unwrap_or(slot)
        }
    }

    pub(super) fn effective_scheduling_context_charge_token(&self, slot: usize) -> (usize, u64) {
        #[cfg(not(test))]
        {
            super::donation_ledger::borrowed_context_charge_token(slot).unwrap_or((slot, 0))
        }
        #[cfg(test)]
        {
            let task_id = self.starts[slot].map(|start| start.id);
            self.ipc_priority_donations[..self.ipc_priority_donation_len]
                .iter()
                .rev()
                .flatten()
                .find_map(|entry| {
                    (entry.custody_active
                        && matches!(entry.target, IpcDonationTarget::BoundWorker(target)
                        if Some(target) == task_id))
                    .then_some((entry.context_owner_slot, entry.reply))
                })
                .unwrap_or((slot, 0))
        }
    }

    /// Reserve bounded donation capacity before the IPC runtime publishes a
    /// reply capability or removes a receive waiter. The caller task is the
    /// temporary unique identity until the reply and worker are both known.
    pub(in crate::multitask) fn reserve_ipc_priority(&mut self, donor_task_id: u64) -> bool {
        let Some(donor_slot) = self.find_task_slot(donor_task_id) else {
            return false;
        };
        let context_owner_slot = self.effective_scheduling_context_owner_slot(donor_slot);
        let Some(context_owner_task_id) = self.starts[context_owner_slot].map(|start| start.id)
        else {
            return false;
        };
        self.reserve_ipc_priority_with_context(
            donor_task_id,
            context_owner_task_id,
            context_owner_slot,
            true,
        )
    }

    fn reserve_ipc_priority_with_context(
        &mut self,
        donor_task_id: u64,
        context_owner_task_id: u64,
        context_owner_slot: usize,
        priority_donated: bool,
    ) -> bool {
        #[cfg(not(test))]
        {
            let Some(donor_slot) = self.find_task_slot(donor_task_id) else {
                return false;
            };
            return !self.retired[donor_slot]
                && super::donation_ledger::reserve(
                    donor_task_id,
                    context_owner_task_id,
                    context_owner_slot,
                    priority_donated,
                );
        }
        #[cfg(test)]
        {
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
            self.ipc_priority_donations[self.ipc_priority_donation_len] =
                Some(IpcPriorityDonation {
                    reply: 0,
                    donor_task_id,
                    context_owner_task_id,
                    context_owner_slot,
                    priority_donated,
                    custody_active: false,
                    target: IpcDonationTarget::AwaitingReceiver,
                });
            self.ipc_priority_donation_len += 1;
            true
        }
    }

    pub(in crate::multitask) fn cancel_ipc_priority_reservation(
        &mut self,
        donor_task_id: u64,
    ) -> bool {
        #[cfg(not(test))]
        {
            return super::donation_ledger::cancel_reservation(donor_task_id);
        }
        #[cfg(test)]
        {
            let Some(index) = self.ipc_priority_donations[..self.ipc_priority_donation_len]
                .iter()
                .position(|entry| {
                    entry.is_some_and(|entry| {
                        entry.reply == 0 && entry.donor_task_id == donor_task_id
                    })
                })
            else {
                return false;
            };
            self.remove_ipc_priority_donation(index);
            true
        }
    }

    /// Transfers a pre-enqueue reservation to the exact reply when no worker
    /// was waiting at publication time. The eventual `IPC_RECV` owns binding
    /// that reply to its concrete receiver; timeout/cancel can meanwhile
    /// revoke it by reply identity without retaining process-wide authority.
    pub(in crate::multitask) fn attach_reserved_ipc_priority(
        &mut self,
        reply: u64,
        donor_task_id: u64,
    ) -> bool {
        #[cfg(not(test))]
        {
            return super::donation_ledger::attach(reply, donor_task_id);
        }
        #[cfg(test)]
        {
            if reply == 0 {
                return false;
            }
            let Some(entry) = self.ipc_priority_donations[..self.ipc_priority_donation_len]
                .iter_mut()
                .find(|entry| {
                    entry.is_some_and(|entry| {
                        entry.reply == 0 && entry.donor_task_id == donor_task_id
                    })
                })
            else {
                return false;
            };
            let mut reservation = entry.expect("located IPC reservation disappeared");
            reservation.reply = reply;
            reservation.custody_active = false;
            *entry = Some(reservation);
            true
        }
    }

    pub(in crate::multitask) fn bind_reserved_ipc_priority(
        &mut self,
        reply: u64,
        donor_task_id: u64,
        receiver_task_id: u64,
    ) -> bool {
        #[cfg(not(test))]
        {
            if reply == 0 || donor_task_id == receiver_task_id {
                return false;
            }
            let Some(receiver_slot) = self.find_task_slot(receiver_task_id) else {
                return false;
            };
            if self.retired[receiver_slot] {
                return false;
            }
            return super::donation_ledger::bind_reserved(
                reply,
                donor_task_id,
                receiver_task_id,
                receiver_slot,
            );
        }
        #[cfg(test)]
        {
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
                    entry.is_some_and(|entry| {
                        entry.reply == 0 && entry.donor_task_id == donor_task_id
                    })
                })
            else {
                return false;
            };
            let mut reservation = entry.expect("located IPC reservation disappeared");
            reservation.reply = reply;
            reservation.target = IpcDonationTarget::BoundWorker(receiver_task_id);
            reservation.custody_active = true;
            *entry = Some(reservation);
            true
        }
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
        let Some(donor_slot) = self.find_task_slot(donor_task_id) else {
            return false;
        };
        let context_owner_slot = self.effective_scheduling_context_owner_slot(donor_slot);
        let Some(context_owner_task_id) = self.starts[context_owner_slot].map(|start| start.id)
        else {
            return false;
        };
        let priority_donated = self.slot_class(donor_slot) == Some(SchedClass::System);
        #[cfg(not(test))]
        {
            let Some(receiver_slot) = self.find_task_slot(receiver_task_id) else {
                return false;
            };
            if self.retired[receiver_slot] {
                return false;
            }
            return super::donation_ledger::upsert(
                reply,
                donor_task_id,
                receiver_task_id,
                receiver_slot,
                context_owner_task_id,
                context_owner_slot,
                priority_donated,
            );
        }
        #[cfg(test)]
        {
            if let Some(entry) = self.ipc_priority_donations[..self.ipc_priority_donation_len]
                .iter_mut()
                .flatten()
                .find(|entry| entry.reply == reply)
            {
                entry.donor_task_id = donor_task_id;
                entry.target = IpcDonationTarget::BoundWorker(receiver_task_id);
                entry.custody_active = true;
                return true;
            }
            if !self.ipc_priority_donation_capacity_available() {
                // The caller must fail admission before blocking. Silently losing
                // donation here would violate the reply's scheduling contract.
                return false;
            }
            self.ipc_priority_donations[self.ipc_priority_donation_len] =
                Some(IpcPriorityDonation {
                    reply,
                    donor_task_id,
                    context_owner_task_id,
                    context_owner_slot,
                    priority_donated,
                    custody_active: true,
                    target: IpcDonationTarget::BoundWorker(receiver_task_id),
                });
            self.ipc_priority_donation_len += 1;
            true
        }
    }

    #[cfg(test)]
    pub(super) fn ipc_priority_donation_capacity_available(&self) -> bool {
        self.ipc_priority_donation_len < MAX_TASK
    }

    /// Revokes the donation associated with a completed or cancelled reply
    /// capability.  This is deliberately idempotent so reply/error/timeout
    /// races cannot leave an inherited System class behind.
    #[cfg(test)]
    pub(in crate::multitask) fn release_ipc_priority(&mut self, reply: u64) -> bool {
        let Some(index) = self.ipc_priority_donations[..self.ipc_priority_donation_len]
            .iter()
            .position(|entry| entry.is_some_and(|entry| entry.reply == reply))
        else {
            return false;
        };
        let removed = self.remove_ipc_priority_donation(index);
        if removed.donor_task_id == removed.context_owner_task_id {
            for entry in self.ipc_priority_donations[..self.ipc_priority_donation_len]
                .iter_mut()
                .flatten()
            {
                if entry.context_owner_slot == removed.context_owner_slot {
                    entry.custody_active = false;
                }
            }
        }
        true
    }

    pub(super) fn release_ipc_priorities_for_task(&mut self, task_id: u64) {
        #[cfg(not(test))]
        {
            super::donation_ledger::release_task(task_id);
            return;
        }
        #[cfg(test)]
        {
            let mut index = 0;
            while index < self.ipc_priority_donation_len {
                if self.ipc_priority_donations[index].is_some_and(|entry| {
                entry.donor_task_id == task_id
                    || entry.context_owner_task_id == task_id
                    || matches!(entry.target, IpcDonationTarget::BoundWorker(target) if target == task_id)
            }) {
                self.remove_ipc_priority_donation(index);
            } else {
                index += 1;
            }
            }
        }
    }

    pub(in crate::multitask) fn release_ipc_priorities_for_process(&mut self, process_id: u64) {
        #[cfg(not(test))]
        {
            for slot in 0..MAX_TASK {
                if self.contexts[slot].is_some_and(|context| context.process_id == Some(process_id))
                    && let Some(start) = self.starts[slot]
                {
                    super::donation_ledger::release_task(start.id);
                }
            }
            return;
        }
        #[cfg(test)]
        {
            let mut index = 0;
            while index < self.ipc_priority_donation_len {
                let Some(entry) = self.ipc_priority_donations[index] else {
                    panic!("scheduler donation prefix contains an empty entry");
                };
                if self.task_belongs_to_process(entry.donor_task_id, process_id)
                    || self.task_belongs_to_process(entry.context_owner_task_id, process_id)
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
    }

    #[cfg(test)]
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

    #[cfg(test)]
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
        if !self.slot_is_runnable(target_slot) || !self.context_is_schedulable(target_slot, target)
        {
            return;
        }
        if self.contexts[self.current_task_slot()].is_none() {
            return;
        }
        if !self.is_fair_candidate_slot(target_slot)
            || !self.is_fair_candidate_slot(self.current_task_slot())
        {
            return;
        }
        let caller_floor = self
            .slot_vruntime(self.current_task_slot())
            .saturating_sub(IPC_DONATION_BONUS_NS);
        let class_floor = self
            .slot_class(target_slot)
            .map(|class| {
                self.min_ready_vruntime_in_class(class)
                    .saturating_sub(IPC_DONATION_BONUS_NS)
            })
            .unwrap_or(caller_floor);
        let donated_floor = caller_floor.min(class_floor);
        if self.contexts[target_slot].is_some() {
            self.lower_slot_vruntime_ceiling(target_slot, donated_floor);
        }
    }
}
