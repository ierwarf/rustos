use super::*;

#[cfg(test)]
static TEST_CLAIM_CAS_RACE_SLOT: core::sync::atomic::AtomicUsize =
    core::sync::atomic::AtomicUsize::new(usize::MAX);

#[cfg(test)]
pub(super) fn force_claim_cas_race(slot: usize) {
    TEST_CLAIM_CAS_RACE_SLOT.store(slot, Ordering::SeqCst);
}

#[cfg(test)]
pub(super) fn reset_claim_cas_race() {
    TEST_CLAIM_CAS_RACE_SLOT.store(usize::MAX, Ordering::SeqCst);
}

#[cfg(test)]
static TEST_ADMISSION_SLOT: core::sync::atomic::AtomicUsize =
    core::sync::atomic::AtomicUsize::new(usize::MAX);
#[cfg(test)]
static TEST_ADMISSION_EFFECT: core::sync::atomic::AtomicUsize =
    core::sync::atomic::AtomicUsize::new(0);

#[cfg(test)]
pub(in crate::multitask::scheduler) fn probe_next_claim_admission(slot: usize) {
    TEST_ADMISSION_EFFECT.store(0, Ordering::SeqCst);
    TEST_ADMISSION_SLOT.store(slot, Ordering::SeqCst);
}
#[cfg(test)]
pub(in crate::multitask::scheduler) fn take_claim_admission_probe() -> usize {
    TEST_ADMISSION_SLOT.store(usize::MAX, Ordering::SeqCst);
    TEST_ADMISSION_EFFECT.swap(0, Ordering::SeqCst)
}
#[cfg(test)]
fn observe_claim_admission(slot: usize, funding: Option<&super::super::scheduling_runtime::FundingPin>) {
    if TEST_ADMISSION_SLOT.compare_exchange(slot, usize::MAX, Ordering::SeqCst, Ordering::SeqCst).is_ok() {
        let (context, domain) = funding.map(|pin| pin.test_admission_exclusion()).unwrap_or((false, false));
        let class = super::super::donation_ledger::test_class_excluded();
        TEST_ADMISSION_EFFECT.store(usize::from(context) | (usize::from(domain) << 1)
            | (usize::from(class) << 2), Ordering::SeqCst);
    }
}

pub(in crate::multitask::scheduler) fn claim_dispatch(
    slot: usize,
    cpu: usize,
    weight: u32,
) -> bool {
    claim_dispatch_with_owner_generation(slot, cpu, weight, None)
}

/// A local picker must claim the exact validated owner generation. Catalog
/// and exact-handoff claims retain their existing owner-specific admission.
pub(in crate::multitask::scheduler) fn claim_dispatch_with_owner_generation(
    slot: usize,
    cpu: usize,
    weight: u32,
    expected_generation: Option<u64>,
) -> bool {
    claim_dispatch_inner(slot, cpu, weight, expected_generation, None, None)
}

pub(in crate::multitask::scheduler) fn claim_validated_dispatch(
    frame: &super::super::dispatch_claim::DispatchFrame<'_>, weight: u32,
    funding: Option<&super::super::scheduling_runtime::FundingPin>,
) -> bool {
    claim_dispatch_inner(frame.slot(), frame.cpu(), weight, Some(frame.generation()), Some(frame), funding)
}

fn admitted_claim<T>(
    frame: Option<&super::super::dispatch_claim::DispatchFrame<'_>>,
    funding: Option<&super::super::scheduling_runtime::FundingPin>,
    claim: impl FnOnce() -> Option<T>,
) -> Option<T> {
    match frame {
        Some(frame) => frame.with_admission(funding, claim),
        None => claim(),
    }
}

fn claim_dispatch_inner(
    slot: usize, cpu: usize, weight: u32, expected_generation: Option<u64>,
    frame: Option<&super::super::dispatch_claim::DispatchFrame<'_>>,
    funding: Option<&super::super::scheduling_runtime::FundingPin>,
) -> bool {
    validate_cpu(cpu);
    let owner = owner(slot);
    if expected_generation.is_some_and(|generation| generation != owner.generation) {
        return false;
    }
    if owner.state == RunOwnerState::DirectHandoff {
        if owner.cpu != Some(cpu) || !owner.runnable {
            return false;
        }
        let Some((claim, activation, execution, mut placement)) =
            admitted_claim(frame, funding, || {
                super::super::stack_resources::frame_readers::with_claim_activation(slot, cpu, || {
                    #[cfg(test)]
                    observe_claim_admission(slot, funding);
                    if frame.is_some_and(|frame| !frame.is_current()) { return Err(owner); }
                    OWNER_WORDS[slot]
                        .compare_exchange(owner, owner.next(RunOwnerState::Running, Some(cpu)))
                })
            }).and_then(|attempt| attempt.into_claimed())
        else {
            return false;
        };
        if claim.is_err() {
            drop(activation);
            drop(execution);
            return false;
        }
        if let Some(mut activation) = activation {
            activation.bind_claimed_owner(owner.next(RunOwnerState::Running, Some(cpu)));
            crate::multitask::cpu_local::retain_mm_activation(cpu, activation);
        }
        if let Some(execution) = execution {
            crate::multitask::current_identity::execution::banks::prepare(cpu, execution);
        }
        placement.bind_claimed(owner.next(RunOwnerState::Running, Some(cpu)));
        crate::multitask::cpu_local::retain_placement(cpu, placement);
        return true;
    }
    if owner.state != RunOwnerState::Local || owner.cpu != Some(cpu) || !owner.runnable {
        return false;
    }
    let mut rq = RUN_QUEUES[cpu].inner.lock();
    if !rq.contains(slot) {
        // Retirement or rehome may have taken this queue between the owner
        // read and our lock acquisition. Only a generation-bound local claim
        // can abandon that stale observation; a still-Local owner without
        // membership is a broken queue invariant, never a normal fallback.
        let observed = super::owner(slot);
        if expected_generation.is_some()
            && (observed.state != RunOwnerState::Local || observed.cpu != Some(cpu))
        {
            return false;
        }
        panic!("scheduler owner says Local but rq omits slot={slot} cpu={cpu}");
    }
    #[cfg(test)]
    if TEST_CLAIM_CAS_RACE_SLOT
        .compare_exchange(slot, usize::MAX, Ordering::SeqCst, Ordering::SeqCst)
        .is_ok()
    {
        // A lock-free owner-word writer can change runnable after the queue
        // lock is acquired and before its exact owner CAS.
        set_runnable(slot, false);
    }
    let next = owner.next(RunOwnerState::Running, Some(cpu));
    // The owner CAS is the claim's linearization point. A lock-free writer
    // can still update runnable/wait metadata while the rq lock is held.
    // Do not remove and reinsert queue custody across a failed CAS: that
    // publishes a spurious membership epoch and may restore a stale slot.
    let Some(attempt) =
        admitted_claim(frame, funding, || {
            super::super::stack_resources::frame_readers::with_claim_activation(slot, cpu, || {
                #[cfg(test)]
                observe_claim_admission(slot, funding);
                if frame.is_some_and(|frame| !frame.is_current()) { return Err(owner); }
                OWNER_WORDS[slot].compare_exchange(owner, next)
            })
        })
    else {
        return false;
    };
    if !attempt.is_claimed() {
        drop(rq);
        drop(attempt); // all provisional loans leave EVERY raw leaf first
        return false;
    }
    let (claim, activation, execution, mut placement) = attempt.into_claimed().unwrap();
    if let Err(observed) = claim {
        drop(rq);
        drop(activation);
        drop(execution);
        if expected_generation.is_some() || observed.state.is_terminal() {
            return false;
        }
        panic!("scheduler dispatch lost owner race observed={observed:?}");
    }
    // A reader can briefly see Running with old published membership; its
    // frame/owner qualification fails closed until this removal is published.
    rq.remove(slot, weight);
    RUN_QUEUES[cpu].publish_load(&rq);
    drop(rq);
    if let Some(mut activation) = activation {
        activation.bind_claimed_owner(next);
        crate::multitask::cpu_local::retain_mm_activation(cpu, activation);
    }
    if let Some(execution) = execution {
        crate::multitask::current_identity::execution::banks::prepare(cpu, execution);
    }
    placement.bind_claimed(next);
    crate::multitask::cpu_local::retain_placement(cpu, placement);
    true
}
