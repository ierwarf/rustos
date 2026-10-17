//! Owner: exact caller BindingPin and single-target physical Cohort.
//! Boundary: non-yielding Linux query/set and Windows thread-mask mechanisms.
//! Lifecycle: original caller/process/ABI and target UID qualify before masks through queue ACK.
//! Concurrency: IRQ-excluded Current; pending target cohort excludes group writers and source CAS.
//! Failure: untouched refusal releases pins; effects retain existing cohort poison custody.
//! Forbidden: directory fallback, copied identity as a lifetime grant or affinity as a blocked wake.
//! Evidence: shared physical Ticket/binding/MM/queue witnesses; no cold or hardware equivalence.
use super::{AffinityCommit, AffinityError, MAX_TASK, runqueue, stack_resources};
use crate::multitask::{current_identity as identity, process_table::ProcessHandle};
use crate::user::abi::UserAbi;
use core::marker::PhantomData;

struct Grant {
    _caller: identity::execution::BindingPin,
    cohort: identity::cohort::Cohort,
    slot: usize,
    id: u64,
    _cpu_affinity: PhantomData<*mut ()>,
}
impl Grant {
    fn capture(
        caller: (usize, u64, ProcessHandle, UserAbi),
        target_id: u64,
        abi: UserAbi,
    ) -> Result<Self, AffinityError> {
        let (caller_slot, caller_id, handle, caller_abi) = caller;
        if caller_abi != abi {
            return Err(AffinityError::WrongAbi);
        }
        let caller_pin = identity::execution::pin_creation(caller_slot, caller_id, handle, abi)
            .ok_or(AffinityError::MissingTask)?;
        let caller_context =
            stack_resources::context::read(caller_slot).ok_or(AffinityError::MissingTask)?;
        if stack_resources::task_start::read(caller_slot).map(|start| start.id) != Some(caller_id)
            || !caller_context.user_mode
            || caller_context.user_abi != Some(abi)
            || caller_context.process_handle != Some(handle)
        {
            return Err(AffinityError::MissingTask);
        }
        let id = if target_id == 0 { caller_id } else { target_id };
        let slot = if id == caller_id {
            caller_slot
        } else {
            // Copies discover a candidate only. The exact physical cohort below
            // joins descriptor UID/resource binding before any context/mask use.
            (0..MAX_TASK)
                .find(|&slot| {
                    stack_resources::task_start::read(slot).is_some_and(|start| start.id == id)
                        && stack_resources::context::read(slot)
                            .is_some_and(|context| context.user_mode)
                })
                .ok_or(AffinityError::MissingTask)?
        };
        let cohort =
            identity::cohort::Cohort::begin(&[(slot, id)]).ok_or(AffinityError::MissingTask)?;
        let context = stack_resources::context::read(slot).ok_or(AffinityError::MissingTask)?;
        if stack_resources::task_start::read(slot).map(|start| start.id) != Some(id)
            || !context.user_mode
        {
            return Err(AffinityError::MissingTask);
        }
        if context.user_abi != Some(abi) {
            return Err(AffinityError::WrongAbi);
        }
        if context.process_handle != Some(handle) {
            return Err(AffinityError::PermissionDenied);
        }
        Ok(Self {
            _caller: caller_pin,
            cohort,
            slot,
            id,
            _cpu_affinity: PhantomData,
        })
    }

    fn current(target: u64, abi: UserAbi) -> Result<Self, AffinityError> {
        let slot = crate::multitask::cpu_local::current_cpu_task_slot()
            .ok_or(AffinityError::MissingTask)?;
        let (id, actual_abi, handle, _) =
            stack_resources::context::current_user_binding().ok_or(AffinityError::MissingTask)?;
        Self::capture((slot, id, handle, actual_abi), target, abi)
    }

    fn mask(&self, online: u64) -> Result<u64, AffinityError> {
        let (mask, _, _) = runqueue::affinity_payload::affinity_snapshot(self.slot);
        let admitted = mask & online;
        (admitted != 0)
            .then_some(admitted)
            .ok_or(AffinityError::InvalidMask)
    }

    fn finish_with(
        mut self,
        requested: u64,
        online: u64,
        rehome: impl FnOnce(usize, u64, &identity::cohort::Cohort),
    ) -> Result<AffinityCommit, AffinityError> {
        let (_, process_mask, _) = runqueue::affinity_payload::affinity_snapshot(self.slot);
        if requested == 0 || requested & !online != 0 || requested & !process_mask != 0 {
            return Err(AffinityError::InvalidMask);
        }
        let previous_mask = self.mask(online)?;
        self.cohort.begin_effects();
        // Ordinary source transfer/dispatch CAS share Identity/cohort admission;
        // none can change this owner to an excluded CPU across the mask store.
        let migrate = runqueue::owner(self.slot)
            .cpu
            .is_some_and(|cpu| requested & (1u64 << cpu) == 0);
        runqueue::affinity_payload::set_affinity_owned(
            self.slot, self.id, requested, process_mask, migrate, &self.cohort
        );
        if migrate {
            rehome(self.slot, self.id, &self.cohort);
        }
        self.cohort.complete(); // queue effects return before admission/unseal
        Ok(AffinityCommit {
            previous_mask,
            reschedule_required: migrate,
        })
    }
}

pub(in crate::multitask) fn linux_mask(target: u64, online: u64) -> Result<u64, AffinityError> {
    Grant::current(target, UserAbi::Linux)?.mask(online)
}
pub(in crate::multitask) fn set(
    target: u64,
    requested: u64,
    online: u64,
    abi: UserAbi,
) -> Result<AffinityCommit, AffinityError> {
    let grant = Grant::current(target, abi)?;
    grant.finish_with(requested, online, |slot, id, cohort| {
        #[cfg(not(test))]
        super::runqueue_policy::rehome_committed_affinity(slot, id, cohort);
        // No native HAL/IPI claim, but identical owner-qualified queue protocol.
        #[cfg(test)]
        let _ = super::runqueue_policy::rehome_owned_on_mask(
            slot,
            requested & online,
            runqueue::weight::value(slot),
            cohort,
            id,
        );
    })
}

#[cfg(test)]
pub(super) mod tests;
