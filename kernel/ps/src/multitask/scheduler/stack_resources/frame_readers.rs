//! Saved-header readers and the Local/Direct -> Running CAS share one leaf.
//! Allocation refs retain bytes; this separate admission excludes CPU claims.
use super::super::runqueue;
use super::{PublishedFrameView, RESOURCES, ResourceCell, Resources, StackPin};
use crate::multitask::cpu_local;
use core::{marker::PhantomData, ptr::NonNull, sync::atomic::Ordering};

pub(in crate::multitask::scheduler) struct FrameReader<'a> {
    _registration: ReaderRegistration,
    _pin: PhantomData<&'a StackPin>,
}

/// One admission owns both backing and reader exclusion. Field order releases
/// the reader before its allocation pin, including early return and unwind.
pub(in crate::multitask::scheduler) struct PinnedFrameReader {
    registration: ReaderRegistration,
    pin: StackPin,
    canary: Option<NonNull<u8>>,
}
impl PinnedFrameReader {
    /// Read the ORIGINAL allocated guard, never the publication's old bool or
    /// a slot re-resolution. Admission bounded this pointer; backing and reader
    /// exclusion retain it through this byte read outside the leaf.
    pub(in crate::multitask::scheduler) fn owned_canary_intact(&self) -> Option<bool> {
        let pointer = self.canary?;
        // SAFETY: admission checked this complete guard in the pinned Vec;
        // writers/claims cannot mutate it while this registration is retained.
        let guard = unsafe { core::slice::from_raw_parts(pointer.as_ptr(), super::super::TASK_STACK_GUARD_BYTES) };
        Some(guard.chunks_exact(core::mem::size_of::<u64>()).all(|word| {
            u64::from_ne_bytes(word.try_into().expect("exact canary word")) == super::super::STACK_CANARY_WORD
        }))
    }

    pub(in crate::multitask::scheduler) fn into_pin(self) -> StackPin {
        let Self { registration, pin, .. } = self;
        drop(registration);
        pin
    }
}

struct ReaderRegistration {
    slot: usize,
    _local: PhantomData<*mut ()>,
}

/// The caller must exclude IRQ/preemption on `cpu` throughout an executing or
/// retained-Current read. Nonexecuting readers need no directory claim guard.
/// All callers retain their exact epoch/identity/geometry checks separately.
pub(in crate::multitask::scheduler) unsafe fn acquire<'a>(
    pin: &'a StackPin,
    cpu: usize,
) -> Option<FrameReader<'a>> {
    let caller_irq_disabled = caller_irq_state(cpu)?;
    let cell = RESOURCES.get(pin.slot)?;
    cell.with(|resources| {
        if cell.pins.load(Ordering::Acquire) == 0 { return None; }
        let registration = register(cell, resources, pin.slot, cpu, caller_irq_disabled)?;
        Some(FrameReader { _registration: registration, _pin: PhantomData })
    })
}

/// Same CPU/IRQ contract as `acquire`. Identity/geometry pin admission and
/// reader registration share ONE leaf; no byte is accessed inside the leaf.
pub(in crate::multitask::scheduler) unsafe fn pin_and_acquire(
    slot: usize, task_id: u64, view: PublishedFrameView, cpu: usize,
) -> Option<PinnedFrameReader> {
    let caller_irq_disabled = caller_irq_state(cpu)?;
    let cell = RESOURCES.get(slot)?;
    let mut pin = None;
    let mut canary = None;
    let registration = cell.with(|resources| {
        if !cell.pin_locked(resources, task_id, view) { return None; }
        pin = Some(StackPin { slot });
        let registration = register(cell, resources, slot, cpu, caller_irq_disabled)?;
        canary = resources.stack.as_ref()
            .and_then(|storage| storage.get(..super::super::TASK_STACK_GUARD_BYTES))
            .and_then(|guard| NonNull::new(guard.as_ptr() as *mut u8));
        Some(registration)
    })?;
    // A refused registration releases its provisional pin AFTER the leaf ends.
    Some(PinnedFrameReader { registration, pin: pin?, canary })
}

fn caller_irq_state(cpu: usize) -> Option<bool> {
    #[cfg(not(test))]
    if nucleus_core::util::lockdep::current_cpu_index() != cpu { return None; }
    #[cfg(test)]
    let _ = cpu;
    #[cfg(rustos_boot_image)]
    return Some(!x86_64::instructions::interrupts::are_enabled());
    #[cfg(not(rustos_boot_image))]
    Some(true)
}

fn register(
    cell: &ResourceCell, resources: &Resources, slot: usize, cpu: usize,
    caller_irq_disabled: bool,
) -> Option<ReaderRegistration> {
    if resources.reservation.is_some()
        || resources.closing
        || resources.frame_writing
        || resources.binding.is_none()
        || resources.start.is_none()
        || resources.mm_pin.as_ref().is_some_and(|pin| !pin.is_open())
    {
        return None;
    }
    let owner = runqueue::owner(slot);
    let bank = super::cpu_leases::owns_current(cpu, slot)
        || super::cpu_leases::owns_prepared(cpu, slot);
    if (owner.state == runqueue::RunOwnerState::Running || bank) && !caller_irq_disabled {
        return None;
    }
    if owner.state == runqueue::RunOwnerState::Running {
        if owner.cpu != Some(cpu) || !bank { return None; }
    } else if !matches!(
        owner.state,
        runqueue::RunOwnerState::Local
            | runqueue::RunOwnerState::DirectHandoff
            | runqueue::RunOwnerState::Blocked
            | runqueue::RunOwnerState::RemoteQueued
    ) {
        return None;
    }
    if cpu_local::task_execution_owner(slot)
        .is_some_and(|owner| owner != cpu_local::TaskExecutionOwner::Current(cpu) || !bank)
    {
        return None;
    }
    // Claims perform their owner CAS under this same leaf. Once registered,
    // no Local/Direct claim can activate these bytes until this reader drops.
    // ORDERING: AcqRel; registration and zero-reader admission share the leaf.
    cell.readers.fetch_update(Ordering::AcqRel, Ordering::Acquire, |count| count.checked_add(1)).ok()?;
    Some(ReaderRegistration { slot, _local: PhantomData })
}

impl Drop for ReaderRegistration {
    fn drop(&mut self) {
        // ORDERING: Release publishes every byte this reader observed before a
        // claim's Acquire read of zero under the leaf can activate the frame.
        RESOURCES[self.slot]
            .readers
            .fetch_update(Ordering::Release, Ordering::Relaxed, |count| count.checked_sub(1))
            .expect("frame reader released twice");
    }
}

#[cfg(test)]
pub(in crate::multitask::scheduler) fn with_claim_exclusion<R>(
    slot: usize,
    claim: impl FnOnce() -> R,
) -> Option<R> {
    RESOURCES.get(slot)?.with(|resources| {
        if !claim_eligible(&RESOURCES[slot], resources, None) {
            return None;
        }
        Some(claim())
    })
}
fn claim_eligible(
    cell: &super::ResourceCell, resources: &super::Resources,
    original: Option<&PinnedFrameReader>,
) -> bool {
    // ORDERING: Acquire pairs with reader release. Registration needs this
    // same leaf. A consuming claim may retain ONLY its own original reader;
    // any independent observer still refuses without spending queue custody.
    let readers = cell.readers.load(Ordering::Acquire);
    let allowed = u32::from(original.is_some());
    if original.is_some_and(|reader| !core::ptr::eq(cell, &RESOURCES[reader.pin.slot])) {
        return false;
    }
    if resources.reservation.is_some() || readers != allowed || resources.frame_writing {
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
    slot: usize, cpu: usize, claim: impl FnOnce() -> R,
) -> Option<ClaimAttempt<R>> {
    with_claim_activation_for_reader(slot, cpu, None, claim)
}

/// The selected loan remains registered through the owner CAS. Only that
/// loan may be promoted; no reader-free gap permits a competing byte owner.
/// Transfer its SAME backing into Prepared only after all leaves end.
pub(in crate::multitask::scheduler) fn with_claim_activation_for_reader<R>(
    slot: usize, cpu: usize, original: Option<&PinnedFrameReader>,
    claim: impl FnOnce() -> R,
) -> Option<ClaimAttempt<R>> {
    let (outcome, permit) =
        crate::multitask::current_identity::execution::with_claim(slot, |identity| {
            RESOURCES.get(slot)?.with(|resources| {
                if !claim_eligible(&RESOURCES[slot], resources, original) {
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
        .map_or(0, |cell| cell.readers.load(Ordering::Acquire))
}

#[cfg(test)]
mod tests;
