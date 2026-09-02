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

/// Which key space a donation ledger entry belongs to.
///
/// The ledger is keyed by a bare `u64`, and two subsystems mint keys into it:
/// generic IPC reply handles, `(generation << 16) | (index + 1)`, and pager
/// fault tokens, `(generation << 8) | slot`. Both are generational packings
/// over disjoint object tables, but their *numbers* overlap - the smallest
/// reply handle `0x1_0001` is also the fault token for slot 1 at generation
/// 256, and slot 1 is reused constantly. Without a namespace an aliased
/// lookup settles another subsystem's donation, which surfaces as
/// "cancelled reply returned stale scheduling-context custody". The key is
/// therefore the pair, never the number alone.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(in crate::multitask) enum DonationNamespace {
    IpcReply,
    PagerFault,
}

#[derive(Clone, Copy)]
pub(super) struct IpcPriorityDonation {
    pub(super) reply: u64,
    pub(super) namespace: DonationNamespace,
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
    /// Whether a monotonic task still owns live scheduling-context authority.
    /// A retired task deliberately keeps its slot/start identity until bounded
    /// cleanup completes, so slot presence alone cannot classify terminal IPC
    /// custody as corruption.
    pub(in crate::multitask) fn scheduling_context_owner_is_live(&self, task_id: u64) -> bool {
        let Some(slot) = self.find_task_slot(task_id) else {
            return false;
        };
        self.contexts[slot].is_some_and(|context| {
            !self.retired[slot] && context.scheduling_context.is_bound_to(task_id)
        })
    }

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
        if !self.bind_reserved_ipc_priority(
            reply,
            DonationNamespace::IpcReply,
            donor_task_id,
            receiver_task_id,
        ) {
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
        // A syscall call admission is issued by the currently executing donor.
        // Validate that CPU-local slot first; keep the bounded scan only for
        // non-syscall/test callers so the fast path does not rediscover its
        // own already-published identity in the global task table.
        let current_slot = self.current_task_slot();
        let slot = if !self.retired[current_slot]
            && self.contexts[current_slot].is_some()
            && self.starts[current_slot].is_some_and(|start| start.id == donor_task_id)
        {
            Some(current_slot)
        } else {
            self.find_task_slot(donor_task_id)
        };
        let Some(slot) = slot else {
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
                    namespace: DonationNamespace::IpcReply,
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
        namespace: DonationNamespace,
        donor_task_id: u64,
    ) -> bool {
        #[cfg(not(test))]
        {
            return super::donation_ledger::attach(reply, namespace, donor_task_id);
        }
        #[cfg(test)]
        {
            if reply == 0 {
                return false;
            }
            if let Some(existing) = self.ipc_priority_donations[..self.ipc_priority_donation_len]
                .iter()
                .flatten()
                .find(|entry| entry.reply == reply && entry.namespace == namespace)
            {
                return existing.donor_task_id == donor_task_id;
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
        namespace: DonationNamespace,
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
                namespace,
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
            if let Some(existing) = self.ipc_priority_donations[..self.ipc_priority_donation_len]
                .iter_mut()
                .flatten()
                .find(|entry| entry.reply == reply && entry.namespace == namespace)
            {
                // Match the production ledger's cross-CPU receive race: an
                // already-bound exact reply is successful admission, but the
                // stale sender waiter must not replace its worker.
                if existing.donor_task_id != donor_task_id {
                    return false;
                }
                if existing.custody_active {
                    return matches!(existing.target, IpcDonationTarget::BoundWorker(_));
                }
                existing.target = IpcDonationTarget::BoundWorker(receiver_task_id);
                existing.custody_active = true;
                return true;
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

        if self.bind_reserved_ipc_priority(
            reply,
            DonationNamespace::IpcReply,
            donor_task_id,
            receiver_task_id,
        ) {
            return true;
        }

        self.upsert_ipc_priority_donation(
            reply,
            DonationNamespace::IpcReply,
            donor_task_id,
            receiver_task_id,
        )
    }

    /// Binds a dispatched pager fault directly to the fault owner's effective
    /// scheduling context. Fault entry cannot reserve the donation ledger;
    /// the fixed fault slot is the prior admission proof for this upsert.
    pub(in crate::multitask) fn inherit_pager_fault_priority(
        &mut self,
        fault_token: u64,
        donor_task_id: u64,
        receiver_task_id: u64,
    ) -> bool {
        if fault_token == 0 || donor_task_id == receiver_task_id {
            return false;
        }
        self.upsert_ipc_priority_donation(
            fault_token,
            DonationNamespace::PagerFault,
            donor_task_id,
            receiver_task_id,
        )
    }

    pub(super) fn upsert_ipc_priority_donation(
        &mut self,
        reply: u64,
        namespace: DonationNamespace,
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
                namespace,
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
                .find(|entry| entry.reply == reply && entry.namespace == namespace)
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
                    namespace,
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
    pub(in crate::multitask) fn release_ipc_priority(
        &mut self,
        reply: u64,
        namespace: DonationNamespace,
    ) -> bool {
        let Some(index) = self.ipc_priority_donations[..self.ipc_priority_donation_len]
            .iter()
            .position(|entry| {
                entry.is_some_and(|entry| entry.reply == reply && entry.namespace == namespace)
            })
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

    /// Applies the one-shot latency floor for a pager fault after the donor has
    /// committed its typed blocked state.
    ///
    /// Unlike generic IPC donation, the donor is intentionally not runnable:
    /// that is the safety proof that pagerd cannot be boosted on behalf of an
    /// unrelated running task. Durable class and budget custody is still bound
    /// by the ordinary donation ledger before pagerd returns to userspace.
    pub(super) fn apply_blocked_pager_donation(&mut self, donor_slot: usize, target_slot: usize) {
        if donor_slot == target_slot || donor_slot >= MAX_TASK || target_slot >= MAX_TASK {
            return;
        }
        if self.contexts[donor_slot].is_none() {
            return;
        }
        if self.contexts[target_slot].is_none() {
            return;
        }
        if !self.slot_blocked(donor_slot)
            || !matches!(
                self.slot_block_reason(donor_slot),
                super::BlockReason::PagerFault(_)
            )
            || !self.slot_is_runnable(target_slot)
            || self.slot_class(donor_slot) == Some(SchedClass::Idle)
            || !self.is_fair_candidate_slot(target_slot)
        {
            return;
        }
        let donor_floor = self
            .slot_vruntime(donor_slot)
            .saturating_sub(IPC_DONATION_BONUS_NS);
        let class_floor = self
            .slot_class(target_slot)
            .map(|class| {
                self.min_ready_vruntime_in_class(class)
                    .saturating_sub(IPC_DONATION_BONUS_NS)
            })
            .unwrap_or(donor_floor);
        self.lower_slot_vruntime_ceiling(target_slot, donor_floor.min(class_floor));
    }
}

/// Reserves one synchronous-call donation for the task this CPU is running,
/// without the task catalog.
///
/// Every input this admission needs is already published per slot: the current
/// slot and its derived idle class come from CPU-local publication, the
/// trusted System-class bit from the per-slot fair-share weight, the borrowed
/// context owner and the inherited-System count from the donation ledger's own
/// atomics, and the scheduling-context identity is a pure function of the
/// owning slot and the monotonic task bound to it. The ledger serializes the
/// edge behind its own bounded lock, so the exclusive catalog guard added one
/// acquisition per synchronous call and excluded nothing.
///
/// `None` means the catalog guard must answer: the donor is not this CPU's
/// published current task, the record was caught mid-update, the slot has
/// reached terminal run ownership, or a borrowed context owner could not be
/// resolved from publication alone.
pub(in crate::multitask) fn reserve_current_call_donation(
    donor_task_id: u64,
) -> Option<super::IpcCallAdmission> {
    let logical_cpu = nucleus_core::util::lockdep::current_cpu_index();
    let slot = super::super::cpu_local::current_cpu_task_slot()?;
    // The root task's Idle class depends on catalog `root_idle`, which is not
    // published per slot. It issues no synchronous service calls in steady
    // state, so it keeps the authoritative path rather than a second rule.
    if slot == super::ROOT_TASK_SLOT {
        return None;
    }
    if super::current_identity::read(slot)?.task_id != Some(donor_task_id) {
        return None;
    }
    // Terminal run ownership is what `retired` means here: `runqueue::retire`
    // drives the owner word terminal in the same guarded transaction that sets
    // the catalog flag, and it runs first.
    if super::runqueue::owner(slot).state.is_terminal() {
        return None;
    }

    // `slot_class` upgrades a User base class to System through the ledger and
    // never upgrades an Idle one, which is exactly this expression.
    let system_class = !super::super::cpu_local::current_cpu_task_is_idle(logical_cpu)
        && (super::runqueue::weight::value(slot) & SYSTEM_CLASS_WEIGHT_FLAG != 0
            || super::donation_ledger::inherited_system(slot));

    let context_owner_slot =
        super::donation_ledger::borrowed_context_owner_slot(slot).unwrap_or(slot);
    let context_owner_task_id = if context_owner_slot == slot {
        donor_task_id
    } else {
        super::current_identity::read(context_owner_slot)?.task_id?
    };
    let scheduling_context = scheduling_context::SchedulingContext::derived_identity(
        context_owner_slot,
        context_owner_task_id,
    );
    let donation_reserved = scheduling_context.is_some()
        && super::donation_ledger::reserve(
            donor_task_id,
            context_owner_task_id,
            context_owner_slot,
            system_class,
        );
    Some(super::IpcCallAdmission {
        system_class,
        donation_reserved,
        scheduling_context,
        scheduling_context_owner_task_id: Some(context_owner_task_id),
    })
}

/// Binds a reserved donor edge to the receiver this CPU is running, without
/// the task catalog.
///
/// This is the common receive commit: the server that just took a request is
/// the current task, and the call admission that preceded it already reserved
/// the donor edge, so the bind needs only the ledger's own lock plus two
/// published identities. `None` means the guard must answer -- the receiver is
/// not this CPU's published current task, either party has reached terminal
/// run ownership, or no reservation existed and the edge has to be created
/// through the authoritative upsert path.
pub(in crate::multitask) fn bind_current_receiver_call_donation(
    reply: u64,
    donor_task_id: u64,
    receiver_task_id: u64,
) -> Option<bool> {
    // `bind_reserved` is the authority on a zero reply and on a self-loop and
    // rejects both without mutating the ledger, so this reader does not repeat
    // either test: a second copy would be one more place to keep in agreement.
    let receiver_slot = super::super::cpu_local::current_cpu_task_slot()?;
    if super::current_identity::read(receiver_slot)?.task_id != Some(receiver_task_id)
        || super::runqueue::owner(receiver_slot).state.is_terminal()
    {
        return None;
    }
    let donor_slot = super::task_directory::live_slot(donor_task_id)?;
    if super::runqueue::owner(donor_slot).state.is_terminal() {
        return None;
    }
    // A false result is a missing reservation, not a rejection: the ledger
    // leaves its state unchanged and the upsert path under the guard owns
    // creating the edge from scratch.
    super::donation_ledger::bind_reserved(
        reply,
        DonationNamespace::IpcReply,
        donor_task_id,
        receiver_task_id,
        receiver_slot,
    )
    .then_some(true)
}

#[cfg(test)]
mod current_donation_tests {
    use super::*;
    use crate::io::session::ConsoleSessionHandle;
    use crate::multitask::cpu_local::{install_test_current_owner, test_publication_lock};
    use crate::multitask::current_identity::{self, TaskIdentity};

    const CPU: usize = 0;
    const DONOR_SLOT: usize = 11;
    const RECEIVER_SLOT: usize = 12;

    /// Holds every shared publication these witnesses install. Production
    /// ownership is CPU-local and cannot alias; host tests share one process,
    /// so the same guards the scheduler fixtures take are required here.
    struct Published {
        donor_task: u64,
        _serial: std::sync::MutexGuard<'static, ()>,
        _runqueue: std::sync::MutexGuard<'static, ()>,
        _cpu: crate::multitask::cpu_local::TestCpuPublicationRestore,
    }

    impl Drop for Published {
        fn drop(&mut self) {
            // The donation ledger is process-wide. Leave no reservation behind
            // even when an assertion aborted the body.
            let _ = super::super::donation_ledger::cancel_reservation(self.donor_task);
            current_identity::clear(DONOR_SLOT);
            current_identity::clear(RECEIVER_SLOT);
            super::super::runqueue::reset_before_publication();
        }
    }

    fn user_identity(task_id: u64) -> TaskIdentity {
        TaskIdentity {
            task_id: Some(task_id),
            user_mode: true,
            abi: Some(crate::user::abi::UserAbi::Linux),
            process_handle: None,
            process_id: Some(0x900),
            console_session: ConsoleSessionHandle::SYSTEM,
            pager_charge: None,
        }
    }

    /// Publishes a donor and a receiver in the exact shape a synchronous call
    /// admission observes: one running on this CPU, one blocked peer, both
    /// resolvable through the shared task directory.
    fn publish(donor_task: u64, receiver_task: u64, running: usize, weight: u32) -> Published {
        let serial = test_publication_lock();
        let runqueue = super::super::runqueue::test_serial_guard();
        super::super::runqueue::reset_before_publication();
        for (slot, task_id) in [(DONOR_SLOT, donor_task), (RECEIVER_SLOT, receiver_task)] {
            current_identity::clear(slot);
            current_identity::publish(slot, user_identity(task_id));
            super::super::task_directory::record(task_id, slot);
        }
        super::super::runqueue::weight::initialize(DONOR_SLOT, weight);
        super::super::runqueue::weight::initialize(RECEIVER_SLOT, NICE_0_LOAD);
        super::super::runqueue::admit_running(running, CPU);
        super::super::runqueue::admit_blocked(if running == DONOR_SLOT {
            RECEIVER_SLOT
        } else {
            DONOR_SLOT
        });
        let cpu = install_test_current_owner(CPU, running);
        Published {
            donor_task,
            _serial: serial,
            _runqueue: runqueue,
            _cpu: cpu,
        }
    }

    #[test]
    fn a_running_donor_is_admitted_from_publication_without_the_catalog() {
        let donor = 0x5101;
        let published = publish(donor, 0x5102, DONOR_SLOT, NICE_0_LOAD);
        let admission =
            reserve_current_call_donation(donor).expect("published donor needs no catalog");
        assert!(
            !admission.system_class,
            "a plain weight is not System class"
        );
        assert!(admission.donation_reserved);
        assert_eq!(
            admission.scheduling_context,
            scheduling_context::SchedulingContext::derived_identity(DONOR_SLOT, donor),
        );
        assert_eq!(admission.scheduling_context_owner_task_id, Some(donor));
        drop(published);
    }

    #[test]
    fn the_trusted_system_weight_bit_decides_the_admitted_class() {
        let donor = 0x5201;
        let published = publish(
            donor,
            0x5202,
            DONOR_SLOT,
            NICE_0_LOAD | SYSTEM_CLASS_WEIGHT_FLAG,
        );
        let admission =
            reserve_current_call_donation(donor).expect("published donor needs no catalog");
        assert!(admission.system_class);
        drop(published);
    }

    #[test]
    fn a_donor_that_is_not_this_cpus_current_task_defers_to_the_catalog() {
        let donor = 0x5301;
        let receiver = 0x5302;
        let published = publish(donor, receiver, DONOR_SLOT, NICE_0_LOAD);
        // The receiver is live and published, but it is not what this CPU is
        // running, so the unlocked reader must refuse rather than answer.
        assert!(reserve_current_call_donation(receiver).is_none());
        assert!(reserve_current_call_donation(donor + 0x1000).is_none());
        drop(published);
    }

    #[test]
    fn a_reserved_edge_binds_to_the_running_receiver_without_the_catalog() {
        let donor = 0x5401;
        let receiver = 0x5402;
        let published = publish(donor, receiver, RECEIVER_SLOT, NICE_0_LOAD);
        assert!(super::super::donation_ledger::reserve(
            donor, donor, DONOR_SLOT, false
        ));
        assert_eq!(
            bind_current_receiver_call_donation(0x77, donor, receiver),
            Some(true)
        );
        assert!(super::super::donation_ledger::release_reply(
            0x77,
            DonationNamespace::IpcReply
        ));
        drop(published);
    }

    /// The receiver a reserved edge binds to is decided by this CPU's own
    /// publication, never by the caller's claim. With a live reservation in
    /// place, that check is the only thing standing between a wrong receiver
    /// identity and a committed edge.
    #[test]
    fn a_claimed_receiver_that_is_not_the_published_one_is_never_bound() {
        let donor = 0x5601;
        let receiver = 0x5602;
        let impostor = 0x5603;
        let published = publish(donor, receiver, RECEIVER_SLOT, NICE_0_LOAD);
        assert!(super::super::donation_ledger::reserve(
            donor, donor, DONOR_SLOT, false
        ));
        assert_eq!(
            bind_current_receiver_call_donation(0x7b, donor, impostor),
            None,
            "an unpublished receiver claim must reach the catalog, not the ledger"
        );
        // The reservation is still unbound, which is what makes the refusal a
        // deferral rather than a silent consumption.
        assert!(super::super::donation_ledger::cancel_reservation(donor));
        drop(published);
    }

    #[test]
    fn a_missing_reservation_self_loop_or_foreign_receiver_defers_to_the_catalog() {
        let donor = 0x5501;
        let receiver = 0x5502;
        let published = publish(donor, receiver, RECEIVER_SLOT, NICE_0_LOAD);
        // No reservation exists, so creating the edge belongs to the
        // authoritative upsert path rather than to this reader.
        assert_eq!(
            bind_current_receiver_call_donation(0x78, donor, receiver),
            None
        );
        assert_eq!(
            bind_current_receiver_call_donation(0, donor, receiver),
            None
        );
        assert_eq!(
            bind_current_receiver_call_donation(0x79, receiver, receiver),
            None
        );
        // A receiver this CPU is not running is never bound from publication.
        assert_eq!(
            bind_current_receiver_call_donation(0x7a, receiver, donor),
            None
        );
        drop(published);
    }
}
