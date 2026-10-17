//! Saved-header readers and the Local/Direct -> Running CAS share one leaf.
//! Allocation refs retain bytes; this separate admission excludes CPU claims.
use super::super::runqueue;
use super::{RESOURCES, StackPin};
use crate::multitask::cpu_local;
use core::{marker::PhantomData, sync::atomic::Ordering};

pub(in crate::multitask::scheduler) struct FrameReader<'a> {
    pin: &'a StackPin,
    _local: PhantomData<*mut ()>,
}

/// The caller must exclude IRQ/preemption on `cpu` throughout an executing or
/// retained-Current read. Nonexecuting readers need no directory claim guard.
/// All callers retain their exact epoch/identity/geometry checks separately.
pub(in crate::multitask::scheduler) unsafe fn acquire<'a>(
    pin: &'a StackPin,
    cpu: usize,
) -> Option<FrameReader<'a>> {
    #[cfg(not(test))]
    if nucleus_core::util::lockdep::current_cpu_index() != cpu {
        return None;
    }
    #[cfg(rustos_boot_image)]
    let caller_irq_disabled = !x86_64::instructions::interrupts::are_enabled();
    let cell = RESOURCES.get(pin.slot)?;
    cell.with(|resources| {
        if resources.reservation.is_some()
            || resources.closing
            || resources.frame_writing
            || resources.binding.is_none()
            || resources.start.is_none()
            || cell.pins.load(Ordering::Acquire) == 0
            || resources.mm_pin.as_ref().is_some_and(|pin| !pin.is_open())
        {
            return None;
        }
        let owner = runqueue::owner(pin.slot);
        let bank = super::cpu_leases::owns_current(cpu, pin.slot)
            || super::cpu_leases::owns_prepared(cpu, pin.slot);
        #[cfg(rustos_boot_image)]
        if (owner.state == runqueue::RunOwnerState::Running || bank) && !caller_irq_disabled {
            return None;
        }
        if owner.state == runqueue::RunOwnerState::Running {
            if owner.cpu != Some(cpu) || !bank {
                return None;
            }
        } else if !matches!(
            owner.state,
            runqueue::RunOwnerState::Local
                | runqueue::RunOwnerState::DirectHandoff
                | runqueue::RunOwnerState::Blocked
                | runqueue::RunOwnerState::RemoteQueued
        ) {
            return None;
        }
        if cpu_local::task_execution_owner(pin.slot)
            .is_some_and(|owner| owner != cpu_local::TaskExecutionOwner::Current(cpu) || !bank)
        {
            return None;
        }
        // Claims perform their owner CAS under this same leaf. Once registered,
        // no Local/Direct claim can activate these bytes until this reader drops.
        resources.frame_readers = resources.frame_readers.checked_add(1)?;
        Some(FrameReader {
            pin,
            _local: PhantomData,
        })
    })
}

impl Drop for FrameReader<'_> {
    fn drop(&mut self) {
        RESOURCES[self.pin.slot].with(|resources| {
            resources.frame_readers = resources
                .frame_readers
                .checked_sub(1)
                .expect("frame reader released twice");
        });
    }
}

#[cfg(test)]
pub(in crate::multitask::scheduler) fn with_claim_exclusion<R>(
    slot: usize,
    claim: impl FnOnce() -> R,
) -> Option<R> {
    RESOURCES.get(slot)?.with(|resources| {
        if !claim_eligible(resources) {
            return None;
        }
        Some(claim())
    })
}
fn claim_eligible(resources: &super::Resources) -> bool {
    if resources.reservation.is_some() || resources.frame_readers != 0 || resources.frame_writing {
        return false;
    }
    #[cfg(not(test))]
    if resources.closing || resources.mm_pin.as_ref().is_some_and(|pin| !pin.is_open()) {
        return false;
    }
    true
}
type Claimed<R> = (
    R,
    Option<super::activation::ActivationCredential>,
    Option<crate::multitask::current_identity::execution::ExecutionPermit>,
    super::super::runqueue::affinity_payload::PlacementGrant,
);

/// Provisional refusal loans must escape EVERY enclosing admission leaf before
/// Drop. In particular a final funding/class join is lower-ranked than these
/// resources but still must not span MM cancellation or reaper notification.
pub(in crate::multitask::scheduler) enum ClaimAttempt<R> {
    Claimed(Claimed<R>),
    Refused {
        activation: Option<super::activation::ActivationCredential>,
        permit: Option<crate::multitask::current_identity::execution::ExecutionPermit>,
    },
}
impl<R> ClaimAttempt<R> {
    pub(in crate::multitask::scheduler) fn is_claimed(&self) -> bool {
        matches!(self, Self::Claimed(_))
    }
    pub(in crate::multitask::scheduler) fn into_claimed(self) -> Option<Claimed<R>> {
        match self {
            Self::Claimed(claimed) => Some(claimed),
            Self::Refused { activation, permit } => {
                drop(activation);
                drop(permit);
                None
            }
        }
    }
}

pub(in crate::multitask::scheduler) fn with_claim_activation<R>(
    slot: usize,
    cpu: usize,
    claim: impl FnOnce() -> R,
) -> Option<ClaimAttempt<R>> {
    let (outcome, permit) =
        crate::multitask::current_identity::execution::with_claim(slot, |identity| {
            RESOURCES.get(slot)?.with(|resources| {
                if !claim_eligible(resources) {
                    return None;
                }
                if let Some(identity) = identity {
                    if resources
                        .start
                        .is_none_or(|start| Some(start.id) != identity.task_id)
                    {
                        return None;
                    }
                    if let Some(context) = resources.context {
                        if context.user_mode != identity.user_mode
                            || context.process_handle != identity.process_handle
                            || context.process_id != identity.process_id
                            || !context.scheduling_context.is_bound_to(identity.task_id?)
                        {
                            return None;
                        }
                    } else {
                        #[cfg(not(test))]
                        return None;
                    }
                }
                let activation = super::activation::capture(resources, slot, cpu)?;
                let claimed = super::super::runqueue::affinity_payload::with_placement_admission(
                    slot,
                    resources.start.map(|start| start.id),
                    cpu,
                    claim,
                );
                Some((claimed, activation))
            })
        })?;
    match outcome {
        Some((Some((claim, placement)), activation)) => {
            Some(ClaimAttempt::Claimed((claim, activation, permit, placement)))
        }
        Some((None, activation)) => Some(ClaimAttempt::Refused { activation, permit }),
        None => Some(ClaimAttempt::Refused { activation: None, permit }),
    }
}

/// Native observation of the real resource counter, never a read/claim grant.
#[cfg(test)]
pub(in crate::multitask::scheduler) fn test_count(slot: usize) -> u32 {
    RESOURCES
        .get(slot)
        .map_or(0, |cell| cell.with(|r| r.frame_readers))
}

#[cfg(test)]
mod tests;
