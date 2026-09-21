//! Mutable scheduling-context budget state, keyed by exact context identity.
//!
//! Domain cells additionally carry a monotonic slot-lifetime generation. A
//! domain number and policy epoch are reusable metadata; neither may identify
//! mutable budget authority after the catalog slot has been recycled.
//!
//! The catalog retains identity, policy, and domain membership. Runtime budget
//! mutation is isolated here so a reused task slot cannot inherit a prior
//! context's refills or timeout state.

use core::sync::atomic::{AtomicU32, Ordering};

use kernel_object::api::identity::ObjectIdentity;
use nucleus_core::util::lockdep::{LockClass, TrackedSpinLock};

use super::super::current_identity::PagerChargeStamp;
use super::MAX_TASK;
use super::scheduling_context::MAX_SCHEDULING_DOMAINS;
use super::scheduling_context::{
    BudgetCounterScope, BudgetSnapshot, BudgetState, ChargeOutcome, SchedulingContextPolicy,
};

#[derive(Clone, Copy)]
struct ContextBudgetRuntime {
    live_identity: Option<ObjectIdentity>,
    policy: Option<SchedulingContextPolicy>,
    domain_slot: Option<u8>,
    domain_generation: Option<u32>,
    budget: Option<BudgetState>,
}

impl ContextBudgetRuntime {
    const fn empty() -> Self {
        Self {
            live_identity: None,
            policy: None,
            domain_slot: None,
            domain_generation: None,
            budget: None,
        }
    }
}

type ContextBudgetLock =
    TrackedSpinLock<ContextBudgetRuntime, { LockClass::SchedulerContextBudget as u8 }>;

static CONTEXT_BUDGETS: [ContextBudgetLock; MAX_TASK] =
    [const { ContextBudgetLock::new(ContextBudgetRuntime::empty()) }; MAX_TASK];

fn cell(slot: usize) -> Option<&'static ContextBudgetLock> {
    CONTEXT_BUDGETS.get(slot)
}

pub(super) fn initialize(
    slot: usize,
    identity: ObjectIdentity,
    policy: SchedulingContextPolicy,
    domain_slot: u8,
    domain_generation: u32,
) -> bool {
    let Some(budget) = BudgetState::admitted(policy, BudgetCounterScope::Context) else {
        return false;
    };
    let Some(cell) = cell(slot) else {
        return false;
    };
    let mut runtime = cell.lock();
    // An occupied cell is live authority, even if its catalog counterpart was
    // lost during an aborted admission. Never overwrite it: a caller must
    // release the exact identity first, so stale/reused slots fail closed.
    if runtime.live_identity.is_some() {
        return false;
    }
    *runtime = ContextBudgetRuntime {
        live_identity: Some(identity),
        policy: Some(policy),
        domain_slot: Some(domain_slot),
        domain_generation: Some(domain_generation),
        budget: Some(budget),
    };
    true
}

pub(super) fn release(slot: usize, identity: ObjectIdentity) {
    let Some(cell) = cell(slot) else {
        return;
    };
    let mut runtime = cell.lock();
    if runtime.live_identity == Some(identity) {
        *runtime = ContextBudgetRuntime::empty();
    }
}

fn budget_for(
    slot: usize,
    identity: ObjectIdentity,
    policy: SchedulingContextPolicy,
) -> Option<
    nucleus_core::util::lockdep::TrackedSpinGuard<
        'static,
        ContextBudgetRuntime,
        { LockClass::SchedulerContextBudget as u8 },
    >,
> {
    let cell = cell(slot)?;
    let runtime = cell.lock();
    (runtime.live_identity == Some(identity) && runtime.policy == Some(policy)).then_some(runtime)
}

pub(super) fn is_eligible(
    slot: usize,
    identity: ObjectIdentity,
    policy: SchedulingContextPolicy,
    now_ns: u64,
) -> bool {
    budget_for(slot, identity, policy)
        .and_then(|runtime| runtime.budget)
        .is_some_and(|budget| budget.is_eligible(now_ns))
}

pub(super) fn prepare_dispatch(
    slot: usize,
    identity: ObjectIdentity,
    policy: SchedulingContextPolicy,
    now_ns: u64,
) -> bool {
    let Some(mut runtime) = budget_for(slot, identity, policy) else {
        return false;
    };
    runtime
        .budget
        .as_mut()
        .is_some_and(|budget| budget.replenish(now_ns) && budget.available())
}

pub(super) fn record_timeout_fault(
    slot: usize,
    identity: ObjectIdentity,
    policy: SchedulingContextPolicy,
    reply: u64,
) -> bool {
    let Some(mut runtime) = budget_for(slot, identity, policy) else {
        return false;
    };
    let Some(budget) = runtime.budget.as_mut() else {
        return false;
    };
    budget.record_timeout_fault(reply);
    true
}

pub(super) fn runtime_snapshot(
    slot: usize,
    identity: ObjectIdentity,
    policy: SchedulingContextPolicy,
) -> Option<BudgetSnapshot> {
    budget_for(slot, identity, policy)?.budget?.snapshot()
}

#[derive(Clone, Copy)]
struct DomainBudgetRuntime {
    generation: u32,
    policy: Option<SchedulingContextPolicy>,
    budget: Option<BudgetState>,
}

impl DomainBudgetRuntime {
    const fn empty() -> Self {
        Self {
            generation: 0,
            policy: None,
            budget: None,
        }
    }
}

type DomainBudgetLock =
    TrackedSpinLock<DomainBudgetRuntime, { LockClass::SchedulerDomainBudget as u8 }>;

static DOMAIN_BUDGETS: [DomainBudgetLock; MAX_SCHEDULING_DOMAINS] =
    [const { DomainBudgetLock::new(DomainBudgetRuntime::empty()) }; MAX_SCHEDULING_DOMAINS];
// This counter deliberately survives runtime release/replacement. A stale
// snapshot must never become valid merely because the same domain policy was
// admitted into the same fixed cell again.
static DOMAIN_GENERATIONS: [AtomicU32; MAX_SCHEDULING_DOMAINS] =
    [const { AtomicU32::new(0) }; MAX_SCHEDULING_DOMAINS];

#[cfg(test)]
static TEST_GUARD: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[cfg(test)]
pub(super) fn test_serial_guard() -> std::sync::MutexGuard<'static, ()> {
    TEST_GUARD
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Test fixtures construct independent catalogs over shared fixed runtime
/// cells. Clear the cells under `test_serial_guard` but intentionally retain
/// the generation counters, so a test cannot hide a slot-reuse ABA mistake.
#[cfg(test)]
pub(super) fn reset_for_test() {
    for cell in &CONTEXT_BUDGETS {
        *cell.lock() = ContextBudgetRuntime::empty();
    }
    for cell in &DOMAIN_BUDGETS {
        *cell.lock() = DomainBudgetRuntime::empty();
    }
}

fn domain_cell(slot: usize) -> Option<&'static DomainBudgetLock> {
    DOMAIN_BUDGETS.get(slot)
}

fn next_domain_generation(slot: usize) -> u32 {
    let generation = DOMAIN_GENERATIONS
        .get(slot)
        .expect("scheduling domain generation slot exceeds capacity")
        .fetch_add(1, Ordering::AcqRel)
        .wrapping_add(1);
    assert_ne!(generation, 0, "scheduling domain generation exhausted");
    generation
}

pub(super) fn initialize_domain(slot: usize, policy: SchedulingContextPolicy) -> Option<u32> {
    let Some(budget) = BudgetState::admitted(policy, BudgetCounterScope::Domain) else {
        return None;
    };
    let Some(cell) = domain_cell(slot) else {
        return None;
    };
    let mut runtime = cell.lock();
    // As with context cells, admission may only fill an empty cell. A catalog
    // write must happen *after* this succeeds, so a failed runtime admission
    // cannot leave metadata that points at absent mutable authority.
    if runtime.policy.is_some() {
        return None;
    }
    let generation = next_domain_generation(slot);
    *runtime = DomainBudgetRuntime {
        generation,
        policy: Some(policy),
        budget: Some(budget),
    };
    Some(generation)
}

/// Replaces an unused catalog domain only after a fully admitted replacement
/// budget exists. The scheduler serializes this cold path and verifies there
/// are no live members before invoking it.
pub(super) fn replace_domain(
    slot: usize,
    expected: SchedulingContextPolicy,
    expected_generation: u32,
    replacement: SchedulingContextPolicy,
) -> Option<u32> {
    let Some(budget) = BudgetState::admitted(replacement, BudgetCounterScope::Domain) else {
        return None;
    };
    let Some(cell) = domain_cell(slot) else {
        return None;
    };
    let mut runtime = cell.lock();
    if runtime.policy != Some(expected) || runtime.generation != expected_generation {
        return None;
    }
    let generation = next_domain_generation(slot);
    *runtime = DomainBudgetRuntime {
        generation,
        policy: Some(replacement),
        budget: Some(budget),
    };
    Some(generation)
}

pub(super) fn domain_matches(
    slot: usize,
    generation: u32,
    policy: SchedulingContextPolicy,
) -> bool {
    domain_cell(slot)
        .map(|cell| {
            let runtime = cell.lock();
            runtime.generation == generation && runtime.policy == Some(policy)
        })
        .unwrap_or(false)
}

fn domain_budget_for(
    slot: usize,
    generation: u32,
    policy: SchedulingContextPolicy,
) -> Option<
    nucleus_core::util::lockdep::TrackedSpinGuard<
        'static,
        DomainBudgetRuntime,
        { LockClass::SchedulerDomainBudget as u8 },
    >,
> {
    let runtime = domain_cell(slot)?.lock();
    (runtime.generation == generation && runtime.policy == Some(policy)).then_some(runtime)
}

pub(super) fn domain_is_eligible(
    slot: usize,
    generation: u32,
    policy: SchedulingContextPolicy,
    now_ns: u64,
) -> bool {
    domain_budget_for(slot, generation, policy)
        .and_then(|runtime| runtime.budget)
        .is_some_and(|budget| budget.is_eligible(now_ns))
}

/// Validates a lock-free identity stamp against the exact live context and
/// domain budget cells. The lock order is Context -> Domain, matching charge
/// and dispatch preparation; any reused generation or policy publication
/// fails closed.
pub(super) fn published_stamp_is_eligible(stamp: PagerChargeStamp, now_ns: u64) -> bool {
    let Some(context_slot) = stamp
        .context_slot
        .checked_sub(1)
        .and_then(|slot| usize::try_from(slot).ok())
    else {
        return false;
    };
    let Some(context_cell) = cell(context_slot) else {
        return false;
    };
    let context = context_cell.lock();
    let Some(identity) = context.live_identity else {
        return false;
    };
    let Some(policy) = context.policy else {
        return false;
    };
    let Some(domain_slot) = context.domain_slot else {
        return false;
    };
    let Some(domain_generation) = context.domain_generation else {
        return false;
    };
    if identity.slot() != stamp.context_slot
        || identity.generation() != stamp.context_generation
        || policy.domain != stamp.scheduling_domain
        || policy.policy_epoch != stamp.policy_epoch
        || policy.period_ns != stamp.period_ns
        || u64::from(domain_generation) != stamp.domain_generation
        || !context
            .budget
            .is_some_and(|budget| budget.is_eligible(now_ns))
    {
        return false;
    }
    let Some(domain_cell) = domain_cell(usize::from(domain_slot)) else {
        return false;
    };
    let domain = domain_cell.lock();
    domain.generation == domain_generation
        && domain.policy == Some(policy)
        && domain
            .budget
            .is_some_and(|budget| budget.is_eligible(now_ns))
}

pub(super) fn prepare_domain_dispatch(
    slot: usize,
    generation: u32,
    policy: SchedulingContextPolicy,
    now_ns: u64,
) -> bool {
    let Some(mut runtime) = domain_budget_for(slot, generation, policy) else {
        return false;
    };
    runtime
        .budget
        .as_mut()
        .is_some_and(|budget| budget.replenish(now_ns) && budget.available())
}

pub(super) fn charge_domain_runtime(
    slot: usize,
    generation: u32,
    policy: SchedulingContextPolicy,
    now_ns: u64,
    elapsed_ns: u64,
) -> Option<ChargeOutcome> {
    let mut runtime = domain_budget_for(slot, generation, policy)?;
    runtime.budget.as_mut()?.charge(now_ns, elapsed_ns)
}

/// Charges a scheduling context and its shared domain as one transaction.
///
/// Lock order is mandatory: ContextBudget then DomainBudget. Both exact keys
/// are checked and both charges are preflighted while the pair is held, so a
/// stale domain generation or a bounded-refill failure cannot debit only one
/// side of temporal accounting.
pub(super) fn charge_pair(
    context_slot: usize,
    identity: ObjectIdentity,
    policy: SchedulingContextPolicy,
    domain_slot: usize,
    domain_generation: u32,
    now_ns: u64,
    elapsed_ns: u64,
) -> Option<(ChargeOutcome, ChargeOutcome)> {
    let mut context = budget_for(context_slot, identity, policy)?;
    let context_budget = context.budget.as_mut()?;
    let mut domain = domain_budget_for(domain_slot, domain_generation, policy)?;
    let domain_budget = domain.budget.as_mut()?;
    if !context_budget.can_charge(now_ns, elapsed_ns)
        || !domain_budget.can_charge(now_ns, elapsed_ns)
    {
        return None;
    }
    let context_outcome = context_budget
        .charge(now_ns, elapsed_ns)
        .expect("preflighted context budget charge refused");
    let domain_outcome = domain_budget
        .charge(now_ns, elapsed_ns)
        .expect("preflighted domain budget charge refused");
    Some((context_outcome, domain_outcome))
}

pub(super) fn domain_snapshot(
    slot: usize,
    generation: u32,
    policy: SchedulingContextPolicy,
) -> Option<BudgetSnapshot> {
    domain_budget_for(slot, generation, policy)?
        .budget?
        .snapshot()
}

pub(super) fn domain_dispatch_refusal(
    slot: usize,
    generation: u32,
    policy: SchedulingContextPolicy,
    now_ns: u64,
) -> super::scheduling_context::DomainRefusalCause {
    let Some(runtime) = domain_budget_for(slot, generation, policy) else {
        return super::scheduling_context::DomainRefusalCause::Policy;
    };
    let Some(budget) = runtime.budget else {
        return super::scheduling_context::DomainRefusalCause::ConservationRefused;
    };
    if budget.available() {
        return super::scheduling_context::DomainRefusalCause::RefillRefused;
    }
    match budget.next_eligible_ns() {
        None => super::scheduling_context::DomainRefusalCause::EmptyNoRefill,
        Some(eligible_ns) if eligible_ns > now_ns => {
            super::scheduling_context::DomainRefusalCause::EmptyRefillPending
        }
        Some(_) => super::scheduling_context::DomainRefusalCause::ConservationRefused,
    }
}
