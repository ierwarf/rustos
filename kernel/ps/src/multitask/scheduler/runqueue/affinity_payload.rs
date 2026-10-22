//! Per-slot affinity and locality payloads outside the scheduler catalog.
//!
//! One ranked leaf owns exact task binding, replacement and Running admission.
//! Copied seqlock snapshots remain observations, never an execution grant.

use core::sync::atomic::{AtomicBool, AtomicU8, AtomicU64, Ordering, fence};

use super::{MAX_TASK, RunOwnerState, owner};
use nucleus_core::util::lockdep::LockClass;
use crate::multitask::irq_leaf::IrqLeaf;

const UNRESTRICTED_MASK: u64 = u64::MAX;
const NO_LAST_CPU: u8 = u8::MAX;

struct SlotAffinity {
    binding: IrqLeaf<Option<u64>, { LockClass::TaskAffinityAdmission as u8 }>,
    version: AtomicU64,
    initialized: AtomicBool,
    task_mask: AtomicU64,
    process_mask: AtomicU64,
    migration_pending: AtomicBool,
}

impl SlotAffinity {
    const fn new() -> Self {
        Self {
            binding: IrqLeaf::new(None),
            version: AtomicU64::new(0),
            initialized: AtomicBool::new(false),
            task_mask: AtomicU64::new(UNRESTRICTED_MASK),
            process_mask: AtomicU64::new(UNRESTRICTED_MASK),
            migration_pending: AtomicBool::new(false),
        }
    }
}

static AFFINITIES: [SlotAffinity; MAX_TASK] = [const { SlotAffinity::new() }; MAX_TASK];
static LAST_CPU: [AtomicU8; MAX_TASK] = [const { AtomicU8::new(NO_LAST_CPU) }; MAX_TASK];

pub(crate) fn reset_before_publication() {
    for (affinity, last_cpu) in AFFINITIES.iter().zip(LAST_CPU.iter()) {
        let mut binding = affinity.binding.lock();
        *binding = None;
        affinity.version.store(0, Ordering::Release);
        affinity.initialized.store(false, Ordering::Relaxed);
        affinity
            .task_mask
            .store(UNRESTRICTED_MASK, Ordering::Release);
        affinity
            .process_mask
            .store(UNRESTRICTED_MASK, Ordering::Release);
        affinity.migration_pending.store(false, Ordering::Release);
        last_cpu.store(NO_LAST_CPU, Ordering::Release);
    }
}

/// Hidden construction retains the SAME Ticket through exact Identity51 and
/// Affinity54 publication. No native fixture exemption may authorize a birth.
pub(in crate::multitask::scheduler) fn initialize_reserved(
    ticket: &super::super::stack_resources::admission::Ticket,
    task_mask: u64,
    process_mask: u64,
) -> bool {
    assert!(task_mask != 0 && process_mask != 0 && task_mask & !process_mask == 0,
        "scheduler affinity payload is invalid");
    let slot = ticket.slot();
    if !ticket.is_committing() || owner(slot).state != RunOwnerState::Dormant {
        return false;
    }
    crate::multitask::current_identity::with_exact_publication(slot, ticket.task_id(), || {
        let affinity = &AFFINITIES[slot];
        let mut binding = affinity.binding.lock();
        if binding.is_some() { return false; }
        *binding = Some(ticket.task_id());
        affinity.initialized.store(true, Ordering::Relaxed);
        store(affinity, task_mask, process_mask, false);
        LAST_CPU[slot].store(NO_LAST_CPU, Ordering::Release);
        true
    }) == Some(true)
}

// Unallocated scalar fixtures are never used by production constructors.
#[cfg(test)]
pub(crate) fn initialize_affinity(slot: usize, task_mask: u64, process_mask: u64) {
    assert_eq!(
        owner(slot).state,
        RunOwnerState::Dormant,
        "scheduler affinity initialized after owner publication"
    );
    set_affinity(slot, task_mask, process_mask, false);
    LAST_CPU[slot].store(NO_LAST_CPU, Ordering::Release);
}

pub(crate) fn reset_affinity(slot: usize) {
    let affinity = &AFFINITIES[slot];
    let mut binding = affinity.binding.lock();
    assert_eq!(
        crate::multitask::current_identity::execution::count(slot),
        0,
        "affinity reset raced captured execution"
    );
    assert!(
        !crate::multitask::current_identity::exec_intent::busy(slot),
        "affinity reset discards cold exec intent"
    );
    affinity.initialized.store(true, Ordering::Relaxed);
    store(affinity, UNRESTRICTED_MASK, UNRESTRICTED_MASK, false);
    *binding = None;
    LAST_CPU[slot].store(NO_LAST_CPU, Ordering::Release);
}

#[cfg(test)]
pub(crate) fn set_affinity(
    slot: usize,
    task_mask: u64,
    process_mask: u64,
    migration_pending: bool,
) {
    assert!(
        try_set_affinity(slot, task_mask, process_mask, migration_pending),
        "affinity replacement lacks cold cohort authority"
    );
}
#[cfg(test)]
pub(crate) fn try_set_affinity(
    slot: usize,
    task_mask: u64,
    process_mask: u64,
    migration_pending: bool,
) -> bool {
    replace_affinity(slot, task_mask, process_mask, migration_pending, None)
}
pub(in crate::multitask::scheduler) fn set_affinity_owned(
    slot: usize,
    id: u64,
    task_mask: u64,
    process_mask: u64,
    migration_pending: bool,
    cohort: &crate::multitask::current_identity::cohort::Cohort,
) {
    assert!(
        replace_affinity(
            slot,
            task_mask,
            process_mask,
            migration_pending,
            Some((cohort, id)),
        ),
        "affinity replacement lacks cold cohort authority"
    );
}
fn replace_affinity(
    slot: usize,
    task_mask: u64,
    process_mask: u64,
    migration_pending: bool,
    owner: Option<(&crate::multitask::current_identity::cohort::Cohort, u64)>,
) -> bool {
    assert!(
        task_mask != 0 && process_mask != 0 && task_mask & !process_mask == 0,
        "scheduler affinity payload is invalid"
    );
    let affinity = AFFINITIES
        .get(slot)
        .expect("scheduler affinity slot exceeds capacity");
    let descriptor = super::super::stack_resources::task_start::read(slot).map(|start| start.id);
    #[cfg(test)]
    let descriptor = descriptor
        .or_else(|| crate::multitask::current_identity::read(slot).and_then(|view| view.task_id));
    #[cfg(not(test))]
    assert!(
        descriptor.is_some(),
        "affinity replacement lacks exact descriptor"
    );
    let task_id = match owner {
        Some((_, id)) => {
            #[cfg(not(test))]
            assert_eq!(descriptor, Some(id), "owned affinity lost descriptor UID");
            #[cfg(test)]
            assert!(descriptor.is_none() || descriptor == Some(id));
            Some(id)
        }
        None => descriptor,
    };
    crate::multitask::current_identity::with_affinity_replacement(
        slot,
        task_id,
        owner.map(|(cohort, _)| cohort),
        || {
            let mut binding = affinity.binding.lock();
            if let Some(bound) = *binding {
                assert_eq!(
                    task_id,
                    Some(bound),
                    "affinity rebound without drained reset"
                );
            }
            *binding = task_id;
            // ORDERING: initialization joins the exact Identity51/cold owner before versioned store.
            affinity.initialized.store(true, Ordering::Relaxed);
            store(affinity, task_mask, process_mask, migration_pending);
        },
    )
    .is_some()
}

fn store(affinity: &SlotAffinity, task_mask: u64, process_mask: u64, migration_pending: bool) {
    let version = affinity.version.load(Ordering::Relaxed);
    assert_eq!(
        version & 1,
        0,
        "scheduler affinity replacement has an odd writer version"
    );
    let committed = version
        .checked_add(2)
        .expect("affinity publication epoch exhausted");
    affinity.version.store(version + 1, Ordering::Relaxed);
    fence(Ordering::Release);
    affinity.task_mask.store(task_mask, Ordering::Relaxed);
    affinity.process_mask.store(process_mask, Ordering::Relaxed);
    affinity
        .migration_pending
        .store(migration_pending, Ordering::Relaxed);
    affinity.version.store(committed, Ordering::Release);
}

/// Identity51 -> Affinity54 joins pending cohort exclusion and the current
/// target mask to a scalar queue-owner CAS. Owned affinity keeps its pins;
/// ordinary cold callers retain their existing enclosing lifetime authority.
/// No queue, process, allocation or notification work may enter `claim`.
pub(in crate::multitask::scheduler) fn with_rehome_admission<R>(
    slot: usize,
    cpu: usize,
    owner: Option<(&crate::multitask::current_identity::cohort::Cohort, u64)>,
    claim: impl FnOnce() -> R,
) -> Option<R> {
    let id = super::super::stack_resources::task_start::read(slot).map(|start| start.id);
    #[cfg(test)]
    let id =
        id.or_else(|| crate::multitask::current_identity::read(slot).and_then(|view| view.task_id));
    if owner.is_some_and(|(_, expected)| id != Some(expected)) {
        return None;
    }
    let (result, _grant) = crate::multitask::current_identity::with_affinity_replacement(
        slot,
        id,
        owner.map(|(cohort, _)| cohort),
        || with_placement_admission(slot, id, cpu, claim),
    )??;
    Some(result)
}

/// Resource53 → Affinity54 normally joins only an exact owner-word CAS.
/// ORIGINAL IPC loans instead enter via Identity51 and may nest the bounded
/// FIFO56 -> Wait58 scalar CAS/clear/record join. No resource/frame/budget,
/// process/identity, allocation, destruction or notification work enters here.
pub(in crate::multitask::scheduler) fn with_placement_admission<R>(
    slot: usize,
    task_id: Option<u64>,
    cpu: usize,
    claim: impl FnOnce() -> R,
) -> Option<(R, PlacementGrant)> {
    let affinity = AFFINITIES.get(slot)?;
    let binding = affinity.binding.lock();
    let exact_uid = task_id.is_some() && *binding == task_id;
    // Unbound native queue/resource fixtures are not production admission.
    #[cfg(test)]
    let fixture_unbound =
        !affinity.initialized.load(Ordering::Relaxed) || (binding.is_none() && task_id.is_none());
    #[cfg(not(test))]
    let fixture_unbound = false;
    if !exact_uid && !fixture_unbound {
        return None;
    }
    let bit = 1_u64.checked_shl(u32::try_from(cpu).ok()?)?;
    let task_mask = affinity.task_mask.load(Ordering::Relaxed);
    let process_mask = affinity.process_mask.load(Ordering::Relaxed);
    // Pending migration denies retained old-CPU fallback, not a fresh claim
    // on an included destination CPU; architecture installation ACKs it.
    if task_mask & bit == 0 || process_mask & bit == 0 {
        return None;
    }
    let grant = PlacementGrant::new(slot, task_id, cpu, affinity.version.load(Ordering::Relaxed));
    Some((claim(), grant))
}

mod placement;
pub(in crate::multitask) use placement::{PlacementGrant, capture_current};
#[cfg(test)]
pub(in crate::multitask) use placement::{capture_architecture};

#[cfg(test)]
fn with_admission<R>(
    slot: usize,
    task_id: Option<u64>,
    cpu: usize,
    claim: impl FnOnce() -> R,
) -> Option<R> {
    with_placement_admission(slot, task_id, cpu, claim).map(|(outcome, _grant)| outcome)
}
#[cfg(test)]
mod admission_tests;

/// Returns a coherent affinity commit. An overlapping writer is a fail-closed
/// empty record; scheduler-serialized callers never observe that fallback.
#[inline]
pub(crate) fn affinity_snapshot(slot: usize) -> (u64, u64, bool) {
    let affinity = AFFINITIES
        .get(slot)
        .expect("scheduler affinity slot exceeds capacity");
    let version = affinity.version.load(Ordering::Acquire);
    if version & 1 != 0 {
        return (0, 0, true);
    }
    let task_mask = affinity.task_mask.load(Ordering::Relaxed);
    let process_mask = affinity.process_mask.load(Ordering::Relaxed);
    let migration_pending = affinity.migration_pending.load(Ordering::Relaxed);
    fence(Ordering::Acquire);
    if affinity.version.load(Ordering::Relaxed) != version {
        return (0, 0, true);
    }
    (task_mask, process_mask, migration_pending)
}

#[inline]
pub(crate) fn record_last_cpu(slot: usize, cpu: u8) {
    LAST_CPU
        .get(slot)
        .expect("scheduler locality slot exceeds capacity")
        .store(cpu, Ordering::Release);
}

#[inline]
pub(crate) fn last_cpu(slot: usize) -> u8 {
    LAST_CPU
        .get(slot)
        .expect("scheduler locality slot exceeds capacity")
        .load(Ordering::Acquire)
}
