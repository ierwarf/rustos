//! Mutable scheduling-context budget state, keyed by exact context identity.
//!
//! Domain cells additionally carry a monotonic slot-lifetime generation. A
//! domain number and policy epoch are reusable metadata; neither may identify
//! mutable budget authority after the catalog slot has been recycled.
//!
//! A domain policy's `budget/period` is a per-CPU share, matching deadline
//! admission (`DEADLINE_UTILIZATION_LIMIT_PPM` of every admitted CPU). Each
//! domain therefore keeps one budget bank per logical CPU: an interval is
//! admitted and charged against the bank of the CPU that executes it. A single
//! shared bank capped a whole multi-task domain at one CPU's share no matter
//! how many CPUs were online, which starved user workloads at eight vCPUs.
//!
//! Owner: exact context/domain mutable budgets and retained domain members.
//! Boundary: policy copies are observations; this is not CPU/return custody.
//! Lifecycle: captures and initialized contexts block replacement through release;
//! closing stops fresh funding, not captured settlement or membership retention.
//! Concurrency: cold registry57 → context48 → domain49; hot pairs omit registry.
//! Failure: stale/exhausted/missing authority refuses; broken live custody stops.
//! Forbidden: allocation, hardware, resource leaves and callbacks inside leaves.
//! Evidence: native/source budget witnesses, not complete lifecycle/hardware proof.

use core::sync::atomic::{AtomicU32, Ordering};

use kernel_object::api::identity::ObjectIdentity;
use nucleus_core::util::lockdep::{LockClass, MAX_TRACKED_CPUS};
use crate::multitask::irq_leaf::IrqLeaf;

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
    active_intervals: u32,
    closing: bool,
}

impl ContextBudgetRuntime {
    const fn empty() -> Self {
        Self {
            live_identity: None,
            policy: None,
            domain_slot: None,
            domain_generation: None,
            budget: None,
            active_intervals: 0,
            closing: false,
        }
    }
}

type ContextBudgetLock =
    IrqLeaf<ContextBudgetRuntime, { LockClass::SchedulerContextBudget as u8 }>;

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
    super::domain_admission::with(|_turn| {
        let mut runtime = cell.lock();
        if runtime.live_identity.is_some() {
            return false;
        }
        if retain_domain_member(usize::from(domain_slot), domain_generation, policy).is_none() {
            return false;
        }
        *runtime = ContextBudgetRuntime {
            live_identity: Some(identity),
            policy: Some(policy),
            domain_slot: Some(domain_slot),
            domain_generation: Some(domain_generation),
            budget: Some(budget),
            active_intervals: 0,
            closing: false,
        };
        true
    })
}

pub(super) fn release(slot: usize, identity: ObjectIdentity) {
    let Some(cell) = cell(slot) else {
        return;
    };
    let mut runtime = cell.lock();
    if runtime.live_identity == Some(identity) {
        assert_eq!(
            runtime.active_intervals, 0,
            "runtime released with executing funding leases"
        );
        release_domain_member(
            usize::from(
                runtime
                    .domain_slot
                    .expect("context member lost domain slot"),
            ),
            runtime
                .domain_generation
                .expect("context member lost domain generation"),
            runtime.policy.expect("context member lost domain policy"),
        );
        *runtime = ContextBudgetRuntime::empty();
    }
}

fn budget_for(
    slot: usize,
    identity: ObjectIdentity,
    policy: SchedulingContextPolicy,
) -> Option<
    crate::multitask::irq_leaf::Guard<
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
        .filter(|runtime| !runtime.closing)
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
    if runtime.closing {
        return false;
    }
    runtime
        .budget
        .as_mut()
        .is_some_and(|budget| budget.replenish(now_ns) && budget.available())
}

#[cfg(test)]
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
    /// One bank per logical CPU; see the module comment.
    banks: Option<[BudgetState; MAX_TRACKED_CPUS]>,
    active_intervals: u32,
    // Captured cold grants and initialized context runtimes, not catalog reads.
    members: u32,
}

impl DomainBudgetRuntime {
    const fn empty() -> Self {
        Self {
            generation: 0,
            policy: None,
            banks: None,
            active_intervals: 0,
            members: 0,
        }
    }

    fn bank(&self, cpu: usize) -> Option<&BudgetState> {
        self.banks.as_ref()?.get(cpu)
    }

    fn bank_mut(&mut self, cpu: usize) -> Option<&mut BudgetState> {
        self.banks.as_mut()?.get_mut(cpu)
    }

    /// Whole-domain ledger: monotonic counters and credit summed over every
    /// CPU bank, earliest pending refill across banks.
    fn aggregate_snapshot(&self) -> Option<BudgetSnapshot> {
        let mut total = BudgetSnapshot::default();
        for bank in self.banks.as_ref()? {
            let snapshot = bank.snapshot()?;
            total.available_ns = total.available_ns.saturating_add(snapshot.available_ns);
            total.pending_refill_ns =
                total.pending_refill_ns.saturating_add(snapshot.pending_refill_ns);
            if snapshot.next_eligible_ns != 0
                && (total.next_eligible_ns == 0 || snapshot.next_eligible_ns < total.next_eligible_ns)
            {
                total.next_eligible_ns = snapshot.next_eligible_ns;
            }
            total.consumed_ns = total.consumed_ns.saturating_add(snapshot.consumed_ns);
            total.exhaustion_count =
                total.exhaustion_count.saturating_add(snapshot.exhaustion_count);
            total.refill_count = total.refill_count.saturating_add(snapshot.refill_count);
            total.overflow_merge_count =
                total.overflow_merge_count.saturating_add(snapshot.overflow_merge_count);
            total.timeout_fault_count =
                total.timeout_fault_count.saturating_add(snapshot.timeout_fault_count);
        }
        Some(total)
    }
}

fn admitted_domain_banks(policy: SchedulingContextPolicy) -> Option<[BudgetState; MAX_TRACKED_CPUS]> {
    let bank = BudgetState::admitted(policy, BudgetCounterScope::Domain)?;
    Some([bank; MAX_TRACKED_CPUS])
}

type DomainBudgetLock =
    IrqLeaf<DomainBudgetRuntime, { LockClass::SchedulerDomainBudget as u8 }>;

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

fn next_domain_generation(slot: usize) -> Option<u32> {
    // ORDERING: checked reservation never wraps or revives a retired generation.
    DOMAIN_GENERATIONS
        .get(slot)?
        .fetch_update(Ordering::AcqRel, Ordering::Acquire, |old| {
            old.checked_add(1)
        })
        .ok()?
        .checked_add(1)
}

#[cfg(test)]
pub(super) fn set_domain_generation_for_test(slot: usize, generation: u32) -> u32 {
    // ORDERING: isolated native exhaustion/recovery, never a production epoch grant.
    DOMAIN_GENERATIONS[slot].swap(generation, Ordering::AcqRel)
}

#[cfg(test)]
pub(super) fn initialize_domain(slot: usize, policy: SchedulingContextPolicy) -> Option<u32> {
    super::domain_admission::with(|turn| initialize_domain_owned(turn, slot, policy))
}
pub(super) fn initialize_domain_owned(
    _turn: &super::domain_admission::Turn,
    slot: usize,
    policy: SchedulingContextPolicy,
) -> Option<u32> {
    let banks = admitted_domain_banks(policy)?;
    let mut runtime = domain_cell(slot)?.lock();
    if runtime.policy.is_some() {
        return None;
    }
    let generation = next_domain_generation(slot)?;
    *runtime = DomainBudgetRuntime {
        generation,
        policy: Some(policy),
        banks: Some(banks),
        active_intervals: 0,
        members: 0,
    };
    Some(generation)
}

#[cfg(test)]
pub(super) fn replace_domain(
    slot: usize,
    expected: SchedulingContextPolicy,
    expected_generation: u32,
    replacement: SchedulingContextPolicy,
) -> Option<u32> {
    super::domain_admission::with(|turn| {
        replace_domain_owned(turn, slot, expected, expected_generation, replacement)
    })
}
pub(super) fn replace_domain_owned(
    _turn: &super::domain_admission::Turn,
    slot: usize,
    expected: SchedulingContextPolicy,
    expected_generation: u32,
    replacement: SchedulingContextPolicy,
) -> Option<u32> {
    let banks = admitted_domain_banks(replacement)?;
    let mut runtime = domain_cell(slot)?.lock();
    if runtime.policy != Some(expected)
        || runtime.generation != expected_generation
        || runtime.active_intervals != 0
        || runtime.members != 0
    {
        return None;
    }
    let generation = next_domain_generation(slot)?;
    *runtime = DomainBudgetRuntime {
        generation,
        policy: Some(replacement),
        banks: Some(banks),
        active_intervals: 0,
        members: 0,
    };
    Some(generation)
}

pub(super) fn retain_domain_member(
    slot: usize,
    generation: u32,
    policy: SchedulingContextPolicy,
) -> Option<()> {
    let mut domain = domain_cell(slot)?.lock();
    if generation == 0
        || domain.generation != generation
        || domain.policy != Some(policy)
        || domain.banks.is_none()
    {
        return None;
    }
    domain.members = domain.members.checked_add(1)?;
    Some(())
}
pub(super) fn release_domain_member(slot: usize, generation: u32, policy: SchedulingContextPolicy) {
    let mut domain = domain_cell(slot).expect("domain member lost slot").lock();
    assert!(
        domain.generation == generation && domain.policy == Some(policy),
        "domain member lost exact generation/policy"
    );
    domain.members = domain
        .members
        .checked_sub(1)
        .expect("domain membership underflow");
}

/// Copied descriptor from the sole production policy/generation/budget cell.
/// Metadata is not an eligibility grant. Cold allocation/utilization scans still
/// require their enclosing cross-domain admission transaction.
pub(super) fn domain_descriptor(
    slot: usize,
) -> Option<super::scheduling_context::SchedulingDomainState> {
    let runtime = domain_cell(slot)?.lock();
    let policy = runtime.policy?;
    assert!(
        runtime.generation != 0 && runtime.banks.is_some(),
        "incomplete scheduling domain authority"
    );
    super::scheduling_context::SchedulingDomainState::admitted(policy, runtime.generation)
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
    crate::multitask::irq_leaf::Guard<
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
    cpu: usize,
    now_ns: u64,
) -> bool {
    domain_budget_for(slot, generation, policy)
        .and_then(|runtime| runtime.bank(cpu).copied())
        .is_some_and(|budget| budget.is_eligible(now_ns))
}

/// Exact executing-funding custody. Non-Copy; the context and domain cannot
/// be recycled until the CPU interval has charged and dropped this pin.
#[derive(Debug)]
pub(super) struct FundingPin {
    stamp: PagerChargeStamp,
    context_slot: usize,
    domain_slot: usize,
    /// CPU whose domain bank admitted this interval and is charged for it.
    cpu: usize,
    owner_task_id: u64,
    reply: u64,
}

impl FundingPin {
    /// Metadata only. Caller retains the SAME original Current interval borrow
    /// across its later paired read; these numbers never authorize a new grant.
    pub(super) fn observation(&self) -> (PagerChargeStamp, u64) {
        (self.stamp, self.owner_task_id)
    }

    #[cfg(test)]
    pub(super) fn test_admission_exclusion(&self) -> (bool, bool) {
        // Native observation of the ACTUAL cells, not lockdep/hardware proof.
        (cell(self.context_slot).unwrap().try_lock().is_none(),
         domain_cell(self.domain_slot).unwrap().try_lock().is_none())
    }

    /// Borrow THIS original lifetime pin while joining credit/close admission
    /// to the exact claim CAS. Caller already owns rq/class custody; the bounded
    /// callback may acquire only higher-ranked claim leaves, never publish CPU
    /// banks, drop grants, notify, or perform architecture/external work.
    pub(super) fn with_dispatch_admission<T>(
        &self, cpu: usize, now_ns: u64, claim: impl FnOnce() -> Option<T>,
    ) -> Option<T> {
        with_published_budget_pair(self.stamp, cpu, |context, domain| {
            if !context.is_eligible(now_ns) || !domain.is_eligible(now_ns) {
                return None;
            }
            claim()
        }).flatten()
    }
}

impl FundingPin {
    /// Charge `elapsed_ns` to this still-held funding and report whether both
    /// exact budgets stay available to a successor interval on the SAME funding
    /// (the IPC lane's shuttle). The pin is neither dropped nor re-pinned.
    pub(super) fn charge_in_place(
        &self, now_ns: u64, elapsed_ns: u64,
    ) -> Option<(u64, ChargeOutcome, bool)> {
        let (context, domain, continues) =
            charge_published_stamp_pair_continuing(self.stamp, self.cpu, now_ns, elapsed_ns, self.reply)?;
        Some((
            self.owner_task_id,
            ChargeOutcome {
                charged_ns: context.charged_ns.min(domain.charged_ns),
                overrun_ns: context.overrun_ns.max(domain.overrun_ns),
                exhausted: context.exhausted || domain.exhausted,
            },
            continues,
        ))
    }

    /// Hand this funding to the next interval of the same donation chain, whose
    /// reply label is `reply`. Identity, stamp and counts are untouched.
    pub(super) fn relabeled(mut self, reply: u64) -> Self {
        self.reply = reply;
        self
    }

    /// Root owner task and reply label this funding is currently labeled with.
    pub(super) fn label(&self) -> (u64, u64) {
        (self.owner_task_id, self.reply)
    }

    pub(super) fn charge(self, now_ns: u64, elapsed_ns: u64) -> (u64, ChargeOutcome) {
        let (context, domain) =
            charge_published_stamp_pair(self.stamp, self.cpu, now_ns, elapsed_ns, self.reply)
                .expect("pinned execution funding lost exact runtime authority");
        (
            self.owner_task_id,
            ChargeOutcome {
                charged_ns: context.charged_ns.min(domain.charged_ns),
                overrun_ns: context.overrun_ns.max(domain.overrun_ns),
                exhausted: context.exhausted || domain.exhausted,
            },
        )
    }
}

impl Drop for FundingPin {
    fn drop(&mut self) {
        let ready_to_reap = {
            let mut context = cell(self.context_slot)
                .expect("funding context vanished")
                .lock();
            let mut domain = domain_cell(self.domain_slot)
                .expect("funding domain vanished")
                .lock();
            let identity = context.live_identity.expect("pinned context was released");
            assert_eq!(identity.slot(), self.stamp.context_slot);
            assert_eq!(identity.generation(), self.stamp.context_generation);
            assert_eq!(u64::from(domain.generation), self.stamp.domain_generation);
            assert!(
                context.active_intervals != 0 && domain.active_intervals != 0,
                "funding pin released without custody"
            );
            context.active_intervals -= 1;
            domain.active_intervals -= 1;
            context.closing && context.active_intervals == 0
        };
        if ready_to_reap {
            super::retirement_cleanup::mark_reap_pending();
        }
    }
}

/// Closing is irreversible for this identity: new grants fail, existing
/// executing intervals still charge. Check-and-close precedes terminal reset.
pub(super) fn close_for_reclaim(slot: usize, identity: ObjectIdentity) -> bool {
    let Some(cell) = cell(slot) else {
        return true;
    };
    let mut runtime = cell.lock();
    if runtime.live_identity != Some(identity) {
        return true;
    }
    runtime.closing = true;
    runtime.active_intervals == 0
}

pub(super) fn pin_published_funding(
    stamp: PagerChargeStamp,
    cpu: usize,
    owner_task_id: u64,
    reply: u64,
) -> Option<FundingPin> {
    let context_slot = usize::try_from(stamp.context_slot.checked_sub(1)?).ok()?;
    let mut context = cell(context_slot)?.lock();
    let identity = context.live_identity?;
    let policy = context.policy?;
    let domain_slot = usize::from(context.domain_slot?);
    let generation = context.domain_generation?;
    let cpu_bit = 1_u64.checked_shl(u32::try_from(cpu).ok()?)?;
    if context.closing
        || super::scheduling_context::SchedulingContext::derived_identity(
            context_slot,
            owner_task_id,
        ) != Some(identity)
        || !rustos_scheduler_frame_gate::published_budget_identity_custody(
            stamp.context_slot,
            stamp.context_generation,
            stamp.domain_generation,
            identity.slot(),
            identity.generation(),
            u64::from(generation),
            policy.cpu_mask & cpu_bit != 0,
        )
        || policy.domain != stamp.scheduling_domain
        || policy.policy_epoch != stamp.policy_epoch
        || policy.period_ns != stamp.period_ns
    {
        return None;
    }
    let mut domain = domain_cell(domain_slot)?.lock();
    if domain.generation != generation || domain.policy != Some(policy) {
        return None;
    }
    // Preparation and pin acquisition may be separated by another CPU's
    // debit. Authorize a grant only while both exact banks still have credit.
    if !context.budget.as_ref()?.available() || !domain.bank(cpu)?.available() {
        return None;
    }
    let context_refs = context
        .active_intervals
        .checked_add(1)
        .expect("context funding pin overflow");
    let domain_refs = domain
        .active_intervals
        .checked_add(1)
        .expect("domain funding pin overflow");
    context.active_intervals = context_refs;
    domain.active_intervals = domain_refs;
    Some(FundingPin {
        stamp,
        context_slot,
        domain_slot,
        cpu,
        owner_task_id,
        reply,
    })
}

/// Holds the exact context and domain cells together while validating a
/// published stamp. This is the sole identity/CPU-policy gate for both a
/// read-only eligibility check and a dispatch preparation; no catalog policy
/// can be used to prepare another generation's runtime budget.
fn with_published_budget_pair<T>(
    stamp: PagerChargeStamp,
    cpu: usize,
    use_pair: impl FnOnce(&mut BudgetState, &mut BudgetState) -> T,
) -> Option<T> {
    let context_slot = stamp
        .context_slot
        .checked_sub(1)
        .and_then(|slot| usize::try_from(slot).ok())?;
    let mut context = cell(context_slot)?.lock();
    if context.closing {
        return None;
    }
    let identity = context.live_identity?;
    let policy = context.policy?;
    let cpu_bit = u32::try_from(cpu)
        .ok()
        .and_then(|cpu| 1_u64.checked_shl(cpu))?;
    let domain_slot = context.domain_slot?;
    let domain_generation = context.domain_generation?;
    if !rustos_scheduler_frame_gate::published_budget_identity_custody(
        stamp.context_slot,
        stamp.context_generation,
        stamp.domain_generation,
        identity.slot(),
        identity.generation(),
        u64::from(domain_generation),
        policy.cpu_mask & cpu_bit != 0,
    ) || policy.domain != stamp.scheduling_domain
        || policy.policy_epoch != stamp.policy_epoch
        || policy.period_ns != stamp.period_ns
    {
        return None;
    }
    let mut domain = domain_cell(usize::from(domain_slot))?.lock();
    if domain.generation != domain_generation || domain.policy != Some(policy) {
        return None;
    }
    Some(use_pair(context.budget.as_mut()?, domain.bank_mut(cpu)?))
}

/// `None` means a stale stamp, unavailable cell or disallowed CPU, not a
/// proven budget denial. Only `Some(false)` can omit one fair candidate from a
/// complete local scan; this read matures no refill and spends no runtime.
pub(super) fn published_stamp_admission(
    stamp: PagerChargeStamp,
    cpu: usize,
    now_ns: u64,
) -> Option<bool> {
    with_published_budget_pair(stamp, cpu, |context, domain| {
        context.is_eligible(now_ns) && domain.is_eligible(now_ns)
    })
}

/// Validates a lock-free identity stamp against the exact live context and
/// domain budget cells. The lock order is Context -> Domain, matching charge
/// and dispatch preparation; any reused generation or policy fails closed.
pub(super) fn published_stamp_is_eligible(
    stamp: PagerChargeStamp,
    cpu: usize,
    now_ns: u64,
) -> bool {
    published_stamp_admission(stamp, cpu, now_ns).unwrap_or(false)
}

/// The exact runtime outcome of a published dispatch preparation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum PublishedDispatchPreparation {
    Ready,
    Invalid,
    ContextRefused,
    DomainRefused,
}

/// Matures both exact runtime budgets for a dispatch selected from a stable
/// publication. Preparation never charges elapsed runtime; a lost final owner
/// claim may safely abandon this turn. Domain refusal remains distinguishable
/// so diagnostic latching never confuses it with a context or stamp failure.
pub(super) fn prepare_published_stamp_dispatch_status(
    stamp: PagerChargeStamp,
    cpu: usize,
    now_ns: u64,
) -> PublishedDispatchPreparation {
    with_published_budget_pair(stamp, cpu, |context, domain| {
        if !domain.replenish(now_ns) || !domain.available() {
            return PublishedDispatchPreparation::DomainRefused;
        }
        if !context.replenish(now_ns) || !context.available() {
            return PublishedDispatchPreparation::ContextRefused;
        }
        PublishedDispatchPreparation::Ready
    })
    .unwrap_or(PublishedDispatchPreparation::Invalid)
}

pub(super) fn prepare_published_stamp_dispatch(
    stamp: PagerChargeStamp,
    cpu: usize,
    now_ns: u64,
) -> bool {
    prepare_published_stamp_dispatch_status(stamp, cpu, now_ns)
        == PublishedDispatchPreparation::Ready
}

pub(super) fn charge_domain_runtime(
    slot: usize,
    generation: u32,
    policy: SchedulingContextPolicy,
    cpu: usize,
    now_ns: u64,
    elapsed_ns: u64,
) -> Option<ChargeOutcome> {
    let mut runtime = domain_budget_for(slot, generation, policy)?;
    runtime.bank_mut(cpu)?.charge(now_ns, elapsed_ns)
}

/// Charges a scheduling context and its shared domain as one transaction.
///
/// Lock order is mandatory: ContextBudget then DomainBudget. Both exact keys
/// are checked and both charges are preflighted while the pair is held, so a
/// stale domain generation or a bounded-refill failure cannot debit only one
/// side of temporal accounting.
#[cfg(test)]
pub(super) fn charge_pair(
    context_slot: usize,
    identity: ObjectIdentity,
    policy: SchedulingContextPolicy,
    domain_slot: usize,
    domain_generation: u32,
    cpu: usize,
    now_ns: u64,
    elapsed_ns: u64,
) -> Option<(ChargeOutcome, ChargeOutcome)> {
    let mut context = budget_for(context_slot, identity, policy)?;
    let context_budget = context.budget.as_mut()?;
    let mut domain = domain_budget_for(domain_slot, domain_generation, policy)?;
    let domain_budget = domain.bank_mut(cpu)?;
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

/// Charges an executing task's exact published Context→Domain pair. Unlike
/// dispatch admission, accounting must not refuse elapsed runtime just because
/// a policy changed CPU affinity after the interval began. Identity, policy
/// epoch and both generations still match under the paired locks. Record a
/// timeout fault in that same context cell before releasing the pair.
pub(super) fn charge_published_stamp_pair(
    stamp: PagerChargeStamp,
    cpu: usize,
    now_ns: u64,
    elapsed_ns: u64,
    reply: u64,
) -> Option<(ChargeOutcome, ChargeOutcome)> {
    charge_published_stamp_pair_continuing(stamp, cpu, now_ns, elapsed_ns, reply)
        .map(|(context, domain, _)| (context, domain))
}

/// `charge_published_stamp_pair` that also reports, under the same paired
/// locks, whether both budgets still have credit for a successor on this funding.
fn charge_published_stamp_pair_continuing(
    stamp: PagerChargeStamp,
    cpu: usize,
    now_ns: u64,
    elapsed_ns: u64,
    reply: u64,
) -> Option<(ChargeOutcome, ChargeOutcome, bool)> {
    let context_slot = stamp
        .context_slot
        .checked_sub(1)
        .and_then(|slot| usize::try_from(slot).ok())?;
    let mut context = cell(context_slot)?.lock();
    let identity = context.live_identity?;
    let policy = context.policy?;
    let domain_slot = context.domain_slot?;
    let domain_generation = context.domain_generation?;
    // Charge an already-executed interval regardless of a *new* CPU mask,
    // while retaining the same proven exact-generation gate as admission.
    if !rustos_scheduler_frame_gate::published_budget_identity_custody(
        stamp.context_slot,
        stamp.context_generation,
        stamp.domain_generation,
        identity.slot(),
        identity.generation(),
        u64::from(domain_generation),
        true,
    ) || policy.domain != stamp.scheduling_domain
        || policy.policy_epoch != stamp.policy_epoch
        || policy.period_ns != stamp.period_ns
    {
        return None;
    }
    let mut domain = domain_cell(usize::from(domain_slot))?.lock();
    if domain.generation != domain_generation || domain.policy != Some(policy) {
        return None;
    }
    let context_budget = context.budget.as_mut()?;
    let domain_budget = domain.bank_mut(cpu)?;
    if !context_budget.can_charge(now_ns, elapsed_ns)
        || !domain_budget.can_charge(now_ns, elapsed_ns)
    {
        return None;
    }
    let context_outcome = context_budget
        .charge(now_ns, elapsed_ns)
        .expect("preflighted published context charge refused");
    let domain_outcome = domain_budget
        .charge(now_ns, elapsed_ns)
        .expect("preflighted published domain charge refused");
    if (context_outcome.exhausted || domain_outcome.exhausted) && context_outcome.charged_ns != 0 {
        context_budget.record_timeout_fault(reply);
    }
    let continues = context_budget.available() && domain_budget.available();
    Some((context_outcome, domain_outcome, continues))
}

/// Observation of an ORIGINAL still-borrowed interval, including borrowed donor
/// funding. The interval leaf has already ended; retain its non-Copy loan while
/// joining Context48 -> Domain49, not Interval52 -> Context48. Closing prevents
/// new funding, never this existing interval's read or final settlement.
pub(super) fn observe_current_funding(
    original: &super::execution_interval::FundingObservation<'_, '_, '_>,
) -> Option<crate::multitask::SchedulingContextRuntimeSnapshot> {
    let (stamp, executing_task_id, context_owner_task_id) = original.metadata();
    let slot = usize::try_from(stamp.context_slot.checked_sub(1)?).ok()?;
    let context = cell(slot)?.lock();
    let identity = context.live_identity?;
    let policy = context.policy?;
    let domain_slot = usize::from(context.domain_slot?);
    let generation = context.domain_generation?;
    if context.active_intervals == 0
        || identity.slot() != stamp.context_slot
        || identity.generation() != stamp.context_generation
        || u64::from(generation) != stamp.domain_generation
        || policy.domain != stamp.scheduling_domain
        || policy.policy_epoch != stamp.policy_epoch
        || policy.period_ns != stamp.period_ns {
        return None;
    }
    let domain = domain_cell(domain_slot)?.lock();
    if domain.active_intervals == 0 || domain.generation != generation || domain.policy != Some(policy) {
        return None;
    }
    let budget = context.budget.as_ref()?.snapshot()?;
    let domain_budget = domain.bank(original.cpu())?.snapshot()?;
    Some(crate::multitask::SchedulingContextRuntimeSnapshot {
        executing_task_id, context_owner_task_id,
        context_identity_slot: identity.slot(), context_identity_generation: identity.generation(),
        domain: policy.domain, policy_epoch: policy.policy_epoch,
        domain_generation: u64::from(generation), budget_ns: policy.budget_ns, period_ns: policy.period_ns,
        context_available_ns: budget.available_ns,
        context_pending_refill_ns: budget.pending_refill_ns,
        context_next_eligible_ns: budget.next_eligible_ns,
        context_consumed_ns: budget.consumed_ns,
        context_exhaustion_count: budget.exhaustion_count,
        context_refill_count: budget.refill_count,
        context_overflow_merge_count: budget.overflow_merge_count,
        timeout_fault_count: budget.timeout_fault_count,
        timeout_fault_consumed_ns: budget.timeout_fault_consumed_ns,
        timeout_fault_budget_ns: policy.budget_ns, timeout_fault_period_ns: policy.period_ns,
        timeout_fault_reply: budget.timeout_fault_reply, timeout_endpoint_cap: policy.timeout_endpoint_cap,
        timeout_fault_action: budget.timeout_fault_action,
        domain_available_ns: domain_budget.available_ns,
        domain_pending_refill_ns: domain_budget.pending_refill_ns,
        domain_next_eligible_ns: domain_budget.next_eligible_ns,
        domain_consumed_ns: domain_budget.consumed_ns,
        domain_exhaustion_count: domain_budget.exhaustion_count,
        domain_refill_count: domain_budget.refill_count,
        domain_overflow_merge_count: domain_budget.overflow_merge_count,
    })
}

pub(super) fn domain_snapshot(
    slot: usize,
    generation: u32,
    policy: SchedulingContextPolicy,
) -> Option<BudgetSnapshot> {
    domain_budget_for(slot, generation, policy)?.aggregate_snapshot()
}

pub(super) fn domain_dispatch_refusal(
    slot: usize,
    generation: u32,
    policy: SchedulingContextPolicy,
    cpu: usize,
    now_ns: u64,
) -> super::scheduling_context::DomainRefusalCause {
    let Some(runtime) = domain_budget_for(slot, generation, policy) else {
        return super::scheduling_context::DomainRefusalCause::Policy;
    };
    let Some(budget) = runtime.bank(cpu).copied() else {
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

/// Diagnostic-only lookup after a published Context→Domain pair refused
/// dispatch. Recheck its identity and policy under the runtime context lock;
/// a concurrent recycle can at most suppress or change a refusal label,
/// never grant dispatch or charge another generation.
pub(super) fn published_domain_refusal_cause(
    stamp: PagerChargeStamp,
    cpu: usize,
    now_ns: u64,
) -> Option<(usize, super::scheduling_context::DomainRefusalCause)> {
    let context_slot = stamp
        .context_slot
        .checked_sub(1)
        .and_then(|slot| usize::try_from(slot).ok())?;
    let context = cell(context_slot)?.lock();
    let identity = context.live_identity?;
    let policy = context.policy?;
    let domain_slot = context.domain_slot?;
    let domain_generation = context.domain_generation?;
    if identity.slot() != stamp.context_slot
        || identity.generation() != stamp.context_generation
        || u64::from(domain_generation) != stamp.domain_generation
        || policy.domain != stamp.scheduling_domain
        || policy.policy_epoch != stamp.policy_epoch
        || policy.period_ns != stamp.period_ns
    {
        return None;
    }
    drop(context);
    let domain_slot = usize::from(domain_slot);
    Some((
        domain_slot,
        domain_dispatch_refusal(domain_slot, domain_generation, policy, cpu, now_ns),
    ))
}

#[cfg(test)]
mod domain_tests;
#[cfg(test)]
mod namespace_tests;
