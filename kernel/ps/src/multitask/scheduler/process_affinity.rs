//! Owner: actual Class55 Change and all-live Cohort physical binding pins.
//! Boundary: complete non-yielding Windows process affinity, not cold syscall lifetime.
//! Lifecycle: exact membership/terminal ACK capture through masks, queue effects and final unseal.
//! Concurrency: IRQ-excluded caller; source CAS joins Identity/cohort and target-mask admission.
//! Failure: untouched refusal rolls back; effects abandon to existing retained poison custody.
//! Forbidden: directory/substitute global loan, absence as drain or affinity as a blocked-peer wake.
//! Evidence: real Ticket/process/MM/binding/queue native witnesses; no HAL/CR3/RSP equivalence.
use super::{AffinityCommit, AffinityError, MAX_TASK, runqueue, stack_resources};
use crate::multitask::{current_identity, process_placement, process_table::ProcessHandle};
use crate::user::abi::UserAbi;

struct Grant {
    change: process_placement::Change,
    cohort: current_identity::cohort::Cohort,
    members: [(usize, u64); MAX_TASK],
    len: usize,
    requested: u64,
    previous: u64,
}

impl Grant {
    fn capture(
        handle: ProcessHandle,
        caller: (usize, u64),
        requested: u64,
        online: u64,
    ) -> Result<Self, AffinityError> {
        if online == 0 || requested == 0 || requested & !online != 0 {
            return Err(AffinityError::InvalidMask);
        }
        let change =
            process_placement::Change::begin(handle, caller).ok_or(AffinityError::MissingTask)?;
        let previous = change.previous() & online;
        if previous == 0 {
            return Err(AffinityError::InvalidMask);
        }
        let mut members = [(0, 0); MAX_TASK];
        let mut len = 0;
        // Only captured terminal ACKs exclude old members. Never slot absence,
        // a new UID or a desired-stop observation without the terminal ledger.
        for member in change.live_members() {
            members[len] = member;
            len += 1;
        }
        let cohort = current_identity::cohort::Cohort::begin(&members[..len])
            .ok_or(AffinityError::MissingTask)?;
        // Post-pin physical qualification: exec/reuse cannot replace this tuple
        // between these reads and the final masks + queue ACK.
        for &(slot, id) in &members[..len] {
            let start =
                stack_resources::task_start::read(slot).ok_or(AffinityError::MissingTask)?;
            let context = stack_resources::context::read(slot).ok_or(AffinityError::MissingTask)?;
            if start.id != id || !context.user_mode || context.process_handle != Some(handle) {
                return Err(AffinityError::MissingTask);
            }
            if context.user_abi != Some(UserAbi::Windows) {
                return Err(AffinityError::WrongAbi);
            }
        }
        Ok(Self {
            change,
            cohort,
            members,
            len,
            requested,
            previous,
        })
    }

    fn finish_with(
        mut self,
        mut rehome: impl FnMut(usize, u64, u64, &current_identity::cohort::Cohort),
    ) -> AffinityCommit {
        self.change.begin_effects();
        self.cohort.begin_effects();
        let mut reschedule_required = false;
        for &(slot, id) in &self.members[..self.len] {
            let migrate = runqueue::owner(slot)
                .cpu
                .is_some_and(|cpu| self.requested & (1u64 << cpu) == 0);
            runqueue::affinity_payload::set_affinity_owned(
                slot,
                id,
                self.requested,
                self.requested,
                migrate,
                &self.cohort,
            );
            if migrate {
                rehome(slot, id, self.requested, &self.cohort);
            }
            reschedule_required |= migrate;
        }
        // Queue effects precede admission. Class55 publishes the process mask
        // and cohort ACK together; lower-rank unseal/pin destruction is outside.
        self.change.publish(self.requested, &mut self.cohort);
        self.cohort.finish_published();
        self.change.finish();
        AffinityCommit {
            previous_mask: self.previous,
            reschedule_required,
        }
    }
}

/// Caller excludes IRQs for capture through ACK. Nothing here blocks or yields.
pub(in crate::multitask) fn set_current(
    requested: u64,
    online: u64,
) -> Result<AffinityCommit, AffinityError> {
    let slot =
        crate::multitask::cpu_local::current_cpu_task_slot().ok_or(AffinityError::MissingTask)?;
    let (id, abi, handle, _) =
        stack_resources::context::current_user_binding().ok_or(AffinityError::MissingTask)?;
    if abi != UserAbi::Windows {
        return Err(AffinityError::WrongAbi);
    }
    let grant = Grant::capture(handle, (slot, id), requested, online)?;
    Ok(grant.finish_with(|slot, id, requested, cohort| {
        #[cfg(not(test))]
        {
            let _ = requested;
            super::runqueue_policy::rehome_committed_affinity(slot, id, cohort);
        }
        // Native has no admitted HAL topology/IPI. It still uses the identical
        // physical queue owner transitions; hardware notification is not modeled.
        #[cfg(test)]
        let _ = super::runqueue_policy::rehome_owned_on_mask(
            slot,
            requested & online,
            runqueue::weight::value(slot),
            cohort,
            id,
        );
    }))
}

#[cfg(test)]
pub(super) mod tests;
