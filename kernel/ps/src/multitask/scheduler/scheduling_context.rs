//! Scheduler-owned execution-context identity and reply custody state.
//!
//! - **Owner:** `kernel-ps` owns one object for every live task context.
//! - **Boundary:** IPC receives only the typed identity and donor task label;
//!   it never imports scheduler policy or mutable accounting state.
//! - **Lifecycle:** create with the task slot/generation, bind 1:1 for the task
//!   lifetime, lend through a reply-owned neutral token, then return exactly
//!   once before the task object can be reused.
//! - **Failure:** zero/stale identities fail admission instead of manufacturing
//!   anonymous execution authority.

use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use kernel_object::api::identity::{ObjectIdentity, ObjectKind, ObjectOwner};

struct RuntimeCounterBank {
    charged_ns: AtomicU64,
    overrun_ns: AtomicU64,
    exhaustions: AtomicU64,
    refill_ns: AtomicU64,
    refills: AtomicU64,
    overflow_merges: AtomicU64,
}

impl RuntimeCounterBank {
    const fn new() -> Self {
        Self {
            charged_ns: AtomicU64::new(0),
            overrun_ns: AtomicU64::new(0),
            exhaustions: AtomicU64::new(0),
            refill_ns: AtomicU64::new(0),
            refills: AtomicU64::new(0),
            overflow_merges: AtomicU64::new(0),
        }
    }

    fn drain(&self) -> BudgetRuntimeCounters {
        BudgetRuntimeCounters {
            charged_ns: self.charged_ns.swap(0, Ordering::AcqRel),
            overrun_ns: self.overrun_ns.swap(0, Ordering::AcqRel),
            exhaustions: self.exhaustions.swap(0, Ordering::AcqRel),
            refill_ns: self.refill_ns.swap(0, Ordering::AcqRel),
            refills: self.refills.swap(0, Ordering::AcqRel),
            overflow_merges: self.overflow_merges.swap(0, Ordering::AcqRel),
        }
    }
}

static CONTEXT_RUNTIME_COUNTERS: RuntimeCounterBank = RuntimeCounterBank::new();
static DOMAIN_RUNTIME_COUNTERS: RuntimeCounterBank = RuntimeCounterBank::new();

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(super) struct BudgetRuntimeCounters {
    pub(super) charged_ns: u64,
    pub(super) overrun_ns: u64,
    pub(super) exhaustions: u64,
    pub(super) refill_ns: u64,
    pub(super) refills: u64,
    pub(super) overflow_merges: u64,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(super) struct SchedulingContextRuntimeCounters {
    pub(super) context: BudgetRuntimeCounters,
    pub(super) domain: BudgetRuntimeCounters,
}

pub(super) fn drain_runtime_counters() -> SchedulingContextRuntimeCounters {
    SchedulingContextRuntimeCounters {
        context: CONTEXT_RUNTIME_COUNTERS.drain(),
        domain: DOMAIN_RUNTIME_COUNTERS.drain(),
    }
}

/// First budget exhaustion of the current profile window: owner task and the
/// quantum that consumed the budget, packed so the pair is published or lost
/// together.
static EXHAUSTION_OWNER: AtomicU64 = AtomicU64::new(0);
static EXHAUSTION_CHARGED_NS: AtomicU64 = AtomicU64::new(0);
static EXHAUSTION_LATCHED: AtomicBool = AtomicBool::new(false);

/// Records one exhaustion transition without touching the debug sink.
///
/// The charge that consumes a budget runs under the global scheduler guard,
/// and a debugcon record is a port write per byte, which is a VM exit per byte
/// under KVM. Latching here and rendering from the profile drain keeps that
/// cost outside the guard; the window's full exhaustion count is already
/// carried by the drained counter bank, so nothing is lost by keeping only the
/// first exact owner.
pub(super) fn latch_budget_exhaustion(owner_task_id: u64, charged_ns: u64) {
    if EXHAUSTION_LATCHED
        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
        .is_err()
    {
        return;
    }
    EXHAUSTION_OWNER.store(owner_task_id, Ordering::Relaxed);
    // ORDERING: Release publishes both fields to the drain that observes the
    // latch through its Acquire load below.
    EXHAUSTION_CHARGED_NS.store(charged_ns, Ordering::Release);
}

/// Takes the window's latched exhaustion, if any. Callers must already have
/// released the scheduler guard.
pub(super) fn take_latched_budget_exhaustion() -> Option<(u64, u64)> {
    if !EXHAUSTION_LATCHED.load(Ordering::Acquire) {
        return None;
    }
    let charged_ns = EXHAUSTION_CHARGED_NS.load(Ordering::Acquire);
    let owner_task_id = EXHAUSTION_OWNER.load(Ordering::Relaxed);
    EXHAUSTION_LATCHED.store(false, Ordering::Release);
    Some((owner_task_id, charged_ns))
}

fn saturating_add(counter: &AtomicU64, value: u64) {
    let _ = counter.fetch_update(Ordering::Relaxed, Ordering::Relaxed, |current| {
        Some(current.saturating_add(value))
    });
}

/// A fixed upper bound keeps accounting allocation-free on the interrupt path.
/// Policy may admit fewer refills, but can never enlarge the kernel object.
pub(super) const MAX_SCHEDULING_CONTEXT_REFILLS: usize = 8;

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
struct Refill {
    eligible_ns: u64,
    amount_ns: u64,
}

/// Time-ordered sporadic-server replenishments.
///
/// When full, a new refill is merged into the latest eligibility point.  This
/// can delay execution authority, but cannot move it earlier or create time.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct BoundedRefillQueue {
    entries: [Refill; MAX_SCHEDULING_CONTEXT_REFILLS],
    len: u8,
    capacity: u8,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum BudgetCounterScope {
    Context,
    Domain,
}

impl BudgetCounterScope {
    fn counters(self) -> &'static RuntimeCounterBank {
        match self {
            Self::Context => &CONTEXT_RUNTIME_COUNTERS,
            Self::Domain => &DOMAIN_RUNTIME_COUNTERS,
        }
    }
}

impl BoundedRefillQueue {
    fn new(capacity: u8) -> Option<Self> {
        if capacity == 0 || usize::from(capacity) > MAX_SCHEDULING_CONTEXT_REFILLS {
            return None;
        }
        Some(Self {
            entries: [Refill::default(); MAX_SCHEDULING_CONTEXT_REFILLS],
            len: 0,
            capacity,
        })
    }

    fn total_ns(&self) -> Option<u64> {
        self.entries[..usize::from(self.len)]
            .iter()
            .try_fold(0_u64, |total, refill| total.checked_add(refill.amount_ns))
    }

    fn push_conservative(&mut self, refill: Refill) -> bool {
        if refill.amount_ns == 0 {
            return true;
        }
        let len = usize::from(self.len);
        if len < usize::from(self.capacity) {
            let insertion = self.entries[..len]
                .iter()
                .position(|existing| refill.eligible_ns < existing.eligible_ns)
                .unwrap_or(len);
            self.entries.copy_within(insertion..len, insertion + 1);
            self.entries[insertion] = refill;
            self.len += 1;
            return true;
        }

        // Merge at the later eligibility point.  In particular, never merge
        // the newest amount into an earlier refill merely to preserve shape:
        // doing so would manufacture immediately consumable CPU authority.
        let latest = &mut self.entries[len - 1];
        let Some(amount_ns) = latest.amount_ns.checked_add(refill.amount_ns) else {
            return false;
        };
        latest.eligible_ns = latest.eligible_ns.max(refill.eligible_ns);
        latest.amount_ns = amount_ns;
        true
    }

    fn pop_eligible(&mut self, now_ns: u64) -> Option<Refill> {
        if self.len == 0 || self.entries[0].eligible_ns > now_ns {
            return None;
        }
        let refill = self.entries[0];
        let len = usize::from(self.len);
        self.entries.copy_within(1..len, 0);
        self.entries[len - 1] = Refill::default();
        self.len -= 1;
        Some(refill)
    }

    fn next_eligible_ns(&self) -> Option<u64> {
        (self.len != 0).then_some(self.entries[0].eligible_ns)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct SchedulingContextPolicy {
    pub(super) budget_ns: u64,
    pub(super) period_ns: u64,
    pub(super) refill_capacity: u8,
    pub(super) cpu_mask: u64,
    pub(super) criticality: u8,
    pub(super) domain: u64,
    pub(super) policy_epoch: u64,
    pub(super) timeout_endpoint_cap: u64,
}

impl SchedulingContextPolicy {
    pub(super) const ABI_VERSION: u16 = 1;

    pub(super) const fn is_valid(self) -> bool {
        self.budget_ns != 0
            && self.period_ns != 0
            && self.budget_ns <= self.period_ns
            && self.refill_capacity != 0
            && (self.refill_capacity as usize) <= MAX_SCHEDULING_CONTEXT_REFILLS
            && self.cpu_mask != 0
            && self.domain != 0
            && self.policy_epoch != 0
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct BudgetState {
    policy: SchedulingContextPolicy,
    counter_scope: BudgetCounterScope,
    available_ns: u64,
    refills: BoundedRefillQueue,
    consumed_ns: u64,
    exhaustion_count: u64,
    refill_count: u64,
    overflow_merge_count: u64,
    timeout_fault: TimeoutFaultState,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
struct TimeoutFaultState {
    count: u64,
    consumed_ns: u64,
    reply: u64,
    action: u64,
}

const TIMEOUT_ACTION_MISSING_HANDLER_THROTTLE: u64 = 1;
const TIMEOUT_ACTION_STALE_HANDLER_THROTTLE: u64 = 2;

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(super) struct BudgetSnapshot {
    pub(super) available_ns: u64,
    pub(super) pending_refill_ns: u64,
    pub(super) next_eligible_ns: u64,
    pub(super) consumed_ns: u64,
    pub(super) exhaustion_count: u64,
    pub(super) refill_count: u64,
    pub(super) overflow_merge_count: u64,
    pub(super) timeout_fault_count: u64,
    pub(super) timeout_fault_consumed_ns: u64,
    pub(super) timeout_fault_reply: u64,
    pub(super) timeout_fault_action: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct ChargeOutcome {
    pub(super) charged_ns: u64,
    pub(super) overrun_ns: u64,
    pub(super) exhausted: bool,
}

impl BudgetState {
    fn admitted(
        policy: SchedulingContextPolicy,
        counter_scope: BudgetCounterScope,
    ) -> Option<Self> {
        if !policy.is_valid() {
            return None;
        }
        Some(Self {
            policy,
            counter_scope,
            available_ns: policy.budget_ns,
            refills: BoundedRefillQueue::new(policy.refill_capacity)?,
            consumed_ns: 0,
            exhaustion_count: 0,
            refill_count: 0,
            overflow_merge_count: 0,
            timeout_fault: TimeoutFaultState::default(),
        })
    }

    pub(super) fn replenish(&mut self, now_ns: u64) -> bool {
        let counters = self.counter_scope.counters();
        while let Some(refill) = self.refills.pop_eligible(now_ns) {
            let Some(available_ns) = self.available_ns.checked_add(refill.amount_ns) else {
                return false;
            };
            if available_ns > self.policy.budget_ns {
                return false;
            }
            self.available_ns = available_ns;
            self.refill_count = self.refill_count.saturating_add(1);
            saturating_add(&counters.refill_ns, refill.amount_ns);
            saturating_add(&counters.refills, 1);
        }
        true
    }

    pub(super) fn charge(&mut self, now_ns: u64, elapsed_ns: u64) -> Option<ChargeOutcome> {
        if !self.replenish(now_ns) {
            return None;
        }
        let charged_ns = elapsed_ns.min(self.available_ns);
        let overrun_ns = elapsed_ns - charged_ns;
        let counters = self.counter_scope.counters();
        self.available_ns -= charged_ns;
        self.consumed_ns = self.consumed_ns.checked_add(charged_ns)?;
        if charged_ns != 0 {
            let refill = Refill {
                eligible_ns: now_ns.checked_add(self.policy.period_ns)?,
                amount_ns: charged_ns,
            };
            let was_full = self.refills.len == self.refills.capacity;
            if !self.refills.push_conservative(refill) {
                return None;
            }
            if was_full {
                self.overflow_merge_count = self.overflow_merge_count.saturating_add(1);
                saturating_add(&counters.overflow_merges, 1);
            }
        }
        let exhausted = self.available_ns == 0;
        if exhausted && charged_ns != 0 {
            self.exhaustion_count = self.exhaustion_count.saturating_add(1);
            saturating_add(&counters.exhaustions, 1);
        }
        saturating_add(&counters.charged_ns, charged_ns);
        saturating_add(&counters.overrun_ns, overrun_ns);
        Some(ChargeOutcome {
            charged_ns,
            overrun_ns,
            exhausted,
        })
    }

    fn conserved_ns(&self) -> Option<u64> {
        self.available_ns.checked_add(self.refills.total_ns()?)
    }

    fn next_eligible_ns(&self) -> Option<u64> {
        self.refills.next_eligible_ns()
    }

    fn snapshot(&self) -> Option<BudgetSnapshot> {
        Some(BudgetSnapshot {
            available_ns: self.available_ns,
            pending_refill_ns: self.refills.total_ns()?,
            next_eligible_ns: self.next_eligible_ns().unwrap_or(0),
            consumed_ns: self.consumed_ns,
            exhaustion_count: self.exhaustion_count,
            refill_count: self.refill_count,
            overflow_merge_count: self.overflow_merge_count,
            timeout_fault_count: self.timeout_fault.count,
            timeout_fault_consumed_ns: self.timeout_fault.consumed_ns,
            timeout_fault_reply: self.timeout_fault.reply,
            timeout_fault_action: self.timeout_fault.action,
        })
    }

    fn record_timeout_fault(&mut self, reply: u64) {
        self.timeout_fault = TimeoutFaultState {
            count: self.timeout_fault.count.saturating_add(1),
            consumed_ns: self.consumed_ns,
            reply,
            // Endpoint delivery is deliberately one-shot. A zero endpoint is
            // an explicit missing handler; a nonzero endpoint that cannot be
            // resolved here is stale. Both take the bounded throttle path
            // until the already-scheduled refill and are never retried.
            action: if self.policy.timeout_endpoint_cap == 0 {
                TIMEOUT_ACTION_MISSING_HANDLER_THROTTLE
            } else {
                TIMEOUT_ACTION_STALE_HANDLER_THROTTLE
            },
        };
    }
}

pub(super) const MAX_SCHEDULING_DOMAINS: usize = 64;
/// Leave at least ten percent of every admitted CPU for ordinary work,
/// interrupt service, and bounded accounting overhead. Deadline admission is
/// conservative: each domain ratio is rounded upward before summation.
const DEADLINE_UTILIZATION_LIMIT_PPM: u64 = 900_000;

fn deadline_utilization_ppm(policy: SchedulingContextPolicy) -> Option<u64> {
    let scaled = u128::from(policy.budget_ns).checked_mul(1_000_000)?;
    let rounded = scaled.checked_add(u128::from(policy.period_ns).checked_sub(1)?)?;
    u64::try_from(rounded / u128::from(policy.period_ns)).ok()
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct SchedulingDomainState {
    policy: SchedulingContextPolicy,
    budget: BudgetState,
}

impl SchedulingDomainState {
    pub(super) fn admitted(policy: SchedulingContextPolicy) -> Option<Self> {
        Some(Self {
            policy,
            budget: BudgetState::admitted(policy, BudgetCounterScope::Domain)?,
        })
    }

    pub(super) const fn domain(self) -> u64 {
        self.policy.domain
    }

    pub(super) const fn policy(self) -> SchedulingContextPolicy {
        self.policy
    }

    pub(super) fn is_eligible(self, now_ns: u64) -> bool {
        self.budget.available_ns != 0
            || self
                .budget
                .next_eligible_ns()
                .is_some_and(|eligible_ns| eligible_ns <= now_ns)
    }

    pub(super) fn prepare_dispatch(&mut self, now_ns: u64) -> bool {
        self.budget.replenish(now_ns) && self.budget.available_ns != 0
    }

    /// Names why this domain would refuse a dispatch, without mutating it.
    ///
    /// Selection admits a slot through `is_eligible` and dispatch commits it
    /// through `prepare_dispatch`. The two are deliberately different -- the
    /// first accepts a due refill that the second must actually apply -- so
    /// when they disagree the panic has to say which of several possible
    /// disagreements happened. One message covering an absent domain, a policy
    /// epoch mismatch, a refused refill, and a plain empty budget alike is what
    /// made the 8-vCPU report unactionable.
    pub(super) fn dispatch_refusal(
        self,
        policy: SchedulingContextPolicy,
        now_ns: u64,
    ) -> DomainRefusalCause {
        if self.policy() != policy {
            return if self.policy().policy_epoch != policy.policy_epoch {
                DomainRefusalCause::PolicyEpoch
            } else {
                DomainRefusalCause::Policy
            };
        }
        if self.budget.available_ns != 0 {
            return DomainRefusalCause::RefillRefused;
        }
        match self.budget.next_eligible_ns() {
            None => DomainRefusalCause::EmptyNoRefill,
            Some(eligible_ns) if eligible_ns > now_ns => DomainRefusalCause::EmptyRefillPending,
            Some(_) => DomainRefusalCause::ConservationRefused,
        }
    }

    pub(super) fn charge_runtime(&mut self, now_ns: u64, elapsed_ns: u64) -> Option<ChargeOutcome> {
        self.budget.charge(now_ns, elapsed_ns)
    }

    pub(super) fn runtime_snapshot(&self) -> Option<BudgetSnapshot> {
        self.budget.snapshot()
    }
}

/// Why a domain refused to fund a dispatch it had already been admitted for.
///
/// A discriminant rather than a message: the refusal is latched inside the
/// scheduler guard and rendered outside it, and a `&'static str` would have to
/// survive that boundary in a static slot for no gain over one number.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum DomainRefusalCause {
    NoDomain = 0,
    Policy = 1,
    PolicyEpoch = 2,
    RefillRefused = 3,
    EmptyNoRefill = 4,
    EmptyRefillPending = 5,
    ConservationRefused = 6,
}

/// The most recent refusal, plus how many happened, for the profile drain.
/// Packed into one word so the render sees a coherent pair.
static DOMAIN_BUDGET_REFUSAL: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
static DOMAIN_BUDGET_REFUSALS: core::sync::atomic::AtomicU64 =
    core::sync::atomic::AtomicU64::new(0);

/// Records one refusal from inside the scheduler guard.
pub(super) fn latch_domain_budget_refusal(
    slot: usize,
    domain_slot: usize,
    cause: DomainRefusalCause,
) {
    // ORDERING: Relaxed; a diagnostic pair drained once per profile window.
    DOMAIN_BUDGET_REFUSAL.store(
        ((slot as u64) << 32) | ((domain_slot as u64) << 8) | cause as u64,
        core::sync::atomic::Ordering::Relaxed,
    );
    DOMAIN_BUDGET_REFUSALS.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
}

/// Takes the window's refusal count and its last exact refusal, if any.
pub(in crate::multitask) fn take_domain_budget_refusal_window() -> Option<(u64, u64)> {
    let refusals = DOMAIN_BUDGET_REFUSALS.swap(0, core::sync::atomic::Ordering::Relaxed);
    (refusals != 0).then(|| {
        (
            refusals,
            DOMAIN_BUDGET_REFUSAL.swap(0, core::sync::atomic::Ordering::Relaxed),
        )
    })
}

impl super::Scheduler {
    /// Commits the selected slot's domain and context budgets for this turn,
    /// reporting whether the funding actually admitted the dispatch.
    ///
    /// Selection admits a candidate with a read-only eligibility check that
    /// accepts a refill which is merely *due*; this is where that refill is
    /// applied and the budget is really spent. The two therefore disagree
    /// whenever the funding state moves between them -- a charge on the same
    /// domain pops the due refill and re-arms it a period later, so the exact
    /// state the scan admitted is gone by the time the commit looks.
    ///
    /// That is a policy outcome, not an invariant break: it is the same answer
    /// the admission scan itself would have produced one instant later, and the
    /// caller must treat a refusal exactly as it treats a candidate the scan
    /// rejected. Failing the dispatch here instead made a lost race fatal, and
    /// it only ever reproduced at 8 vCPU, where charges on a shared domain are
    /// frequent enough to land inside that window.
    #[must_use = "a refused budget must not be dispatched"]
    pub(super) fn commit_scheduling_budget_for_dispatch(&mut self, slot: usize) -> bool {
        let owner_slot = self.effective_scheduling_context_owner_slot(slot);
        let Some(context) = self.contexts[owner_slot].map(|owner| owner.scheduling_context) else {
            return true;
        };
        if !context.is_budgeted() {
            return true;
        }
        let now_ns = crate::arch::clock::monotonic_nanos();
        let policy = context
            .policy()
            .expect("budgeted scheduling context lost its policy");
        let domain_slot = context
            .domain_slot()
            .expect("budgeted scheduling context lost its domain slot");
        if !self.prepare_scheduling_domain_dispatch(domain_slot, policy, now_ns) {
            self.note_domain_budget_refusal(slot, domain_slot, policy, now_ns);
            return false;
        }
        self.contexts[owner_slot]
            .as_mut()
            .expect("selected scheduling context disappeared")
            .scheduling_context
            .prepare_dispatch(now_ns)
    }

    /// Latches one budget refusal for rendering after the scheduler guard.
    ///
    /// The refusal is rare and self-correcting, but a *rise* in it means the
    /// admission scan and the commit are disagreeing more often, so it must
    /// stay visible rather than become a silent reselect. Debugcon inside the
    /// guard is a VM exit per byte, so this only records; the profile drain
    /// renders it.
    #[cold]
    #[inline(never)]
    fn note_domain_budget_refusal(
        &self,
        slot: usize,
        domain_slot: usize,
        policy: SchedulingContextPolicy,
        now_ns: u64,
    ) {
        let cause = self
            .scheduling_domains
            .get(domain_slot)
            .and_then(|domain| *domain)
            .map_or(DomainRefusalCause::NoDomain, |domain| {
                domain.dispatch_refusal(policy, now_ns)
            });
        latch_domain_budget_refusal(slot, domain_slot, cause);
    }

    pub(super) fn admit_scheduling_domain(
        &mut self,
        policy: SchedulingContextPolicy,
    ) -> Option<usize> {
        if !self.deadline_domain_admitted(policy) {
            return None;
        }
        if let Some(index) = self
            .scheduling_domains
            .iter()
            .position(|state| state.is_some_and(|state| state.domain() == policy.domain))
        {
            let current = self.scheduling_domains[index]
                .expect("located scheduling domain disappeared under scheduler owner");
            if current.policy() == policy {
                return Some(index);
            }
            let has_live_member = self.contexts.iter().flatten().any(|context| {
                context
                    .scheduling_context
                    .policy()
                    .is_some_and(|member| member.domain == policy.domain)
            });
            if has_live_member {
                return None;
            }
            self.scheduling_domains[index] = SchedulingDomainState::admitted(policy);
            return self.scheduling_domains[index].is_some().then_some(index);
        }
        let slot = self.scheduling_domains.iter().position(Option::is_none)?;
        self.scheduling_domains[slot] = SchedulingDomainState::admitted(policy);
        self.scheduling_domains[slot].is_some().then_some(slot)
    }

    fn deadline_domain_admitted(&self, policy: SchedulingContextPolicy) -> bool {
        if policy.criticality != 2 {
            return true;
        }
        let Some(candidate_ppm) = deadline_utilization_ppm(policy) else {
            return false;
        };
        for cpu in 0..u64::BITS {
            let bit = 1_u64 << cpu;
            if policy.cpu_mask & bit == 0 {
                continue;
            }
            let Some(total_ppm) = self
                .scheduling_domains
                .iter()
                .flatten()
                .filter(|domain| {
                    let admitted = domain.policy();
                    admitted.domain != policy.domain
                        && admitted.criticality == 2
                        && admitted.cpu_mask & bit != 0
                })
                .try_fold(candidate_ppm, |total, domain| {
                    total.checked_add(deadline_utilization_ppm(domain.policy())?)
                })
            else {
                return false;
            };
            if total_ppm > DEADLINE_UTILIZATION_LIMIT_PPM {
                return false;
            }
        }
        true
    }
}

#[derive(Clone, Copy)]
pub(super) struct SchedulingContext {
    identity: ObjectIdentity,
    bound_task: u64,
    budget: Option<BudgetState>,
    domain_slot: Option<u8>,
}

impl SchedulingContext {
    /// The identity a scheduling context bound to `task_id` in `slot` has.
    ///
    /// Custody is decided entirely by this pair, so a reader that already
    /// knows the live slot/task binding can settle a custody claim without
    /// reading the stored context. Both the binding site below and that reader
    /// derive the value here so the two can never drift apart.
    pub(super) fn derived_identity(slot: usize, task_id: u64) -> Option<ObjectIdentity> {
        ObjectIdentity::new(
            ObjectOwner::Ps,
            ObjectKind::SchedulingContext,
            u64::try_from(slot).ok()?.checked_add(1)?,
            task_id.checked_add(1)?,
        )
    }

    pub(super) fn bind(slot: usize, task_id: u64) -> Self {
        let identity = Self::derived_identity(slot, task_id)
            .expect("live task requires nonzero scheduling-context identity");
        Self {
            identity,
            bound_task: task_id,
            // Runnable publication will switch this to an admitted policy in
            // the same transaction.  Keeping identity construction separate
            // prevents the legacy weight field from being reinterpreted as
            // temporal authority while the versioned rootd ABI is connected.
            budget: None,
            domain_slot: None,
        }
    }

    pub(super) fn admit(&mut self, policy: SchedulingContextPolicy, domain_slot: usize) -> bool {
        let Some(budget) = BudgetState::admitted(policy, BudgetCounterScope::Context) else {
            return false;
        };
        let Ok(domain_slot) = u8::try_from(domain_slot) else {
            return false;
        };
        self.budget = Some(budget);
        self.domain_slot = Some(domain_slot);
        true
    }

    pub(super) const fn is_budgeted(self) -> bool {
        self.budget.is_some()
    }

    pub(super) fn allows_cpu(self, cpu: usize) -> bool {
        self.budget.is_none_or(|budget| {
            u32::try_from(cpu)
                .ok()
                .and_then(|cpu| 1_u64.checked_shl(cpu))
                .is_some_and(|bit| budget.policy.cpu_mask & bit != 0)
        })
    }

    /// Read-only admission check used by candidate scans. A due refill is
    /// considered eligible here and is committed only after the scheduler has
    /// selected the exact slot under its owner lock.
    pub(super) fn is_eligible(self, now_ns: u64) -> bool {
        self.budget.is_none_or(|budget| {
            budget.available_ns != 0
                || budget
                    .next_eligible_ns()
                    .is_some_and(|eligible_ns| eligible_ns <= now_ns)
        })
    }

    pub(super) fn prepare_dispatch(&mut self, now_ns: u64) -> bool {
        let Some(budget) = self.budget.as_mut() else {
            return true;
        };
        budget.replenish(now_ns) && budget.available_ns != 0
    }

    pub(super) fn charge_runtime(&mut self, now_ns: u64, elapsed_ns: u64) -> Option<ChargeOutcome> {
        let Some(budget) = self.budget.as_mut() else {
            return Some(ChargeOutcome {
                charged_ns: elapsed_ns,
                overrun_ns: 0,
                exhausted: false,
            });
        };
        budget.charge(now_ns, elapsed_ns)
    }

    pub(super) fn record_timeout_fault(&mut self, reply: u64) -> bool {
        let Some(budget) = self.budget.as_mut() else {
            return false;
        };
        budget.record_timeout_fault(reply);
        true
    }

    pub(super) const fn identity(self) -> ObjectIdentity {
        self.identity
    }

    pub(super) fn policy(self) -> Option<SchedulingContextPolicy> {
        self.budget.map(|budget| budget.policy)
    }

    pub(super) fn domain_slot(self) -> Option<usize> {
        self.domain_slot.map(usize::from)
    }

    pub(super) fn runtime_snapshot(self) -> Option<BudgetSnapshot> {
        self.budget?.snapshot()
    }

    pub(super) const fn is_bound_to(self, task_id: u64) -> bool {
        self.bound_task == task_id
    }
}

#[cfg(test)]
mod exhaustion_latch_tests {
    use super::{latch_budget_exhaustion, take_latched_budget_exhaustion};

    /// The latch exists so a budget exhaustion is never rendered to the debug
    /// sink from inside the global scheduler guard. It must therefore keep the
    /// first event of a window and refuse to grow, and it must hand the exact
    /// pair to the drain exactly once.
    #[test]
    fn the_first_exhaustion_of_a_window_is_kept_and_taken_once() {
        // Another window's leftover would make this witness order-dependent.
        let _ = take_latched_budget_exhaustion();
        latch_budget_exhaustion(0x4242, 7_000);
        // A throttled context reaches the charge on every scheduling attempt;
        // the later events are counted by the drained counter bank, not
        // latched, so none of them can displace the first.
        latch_budget_exhaustion(0x9999, 1);
        assert_eq!(take_latched_budget_exhaustion(), Some((0x4242, 7_000)));
        assert_eq!(take_latched_budget_exhaustion(), None);
    }
}

#[cfg(test)]
mod tests {
    use super::{
        BudgetCounterScope, BudgetState, Refill, SchedulingContext, SchedulingContextPolicy,
        SchedulingDomainState,
    };

    /// The unlocked custody and call-admission readers reconstruct a bound
    /// context's identity from the slot and task alone rather than reading the
    /// catalog. That is only sound while binding derives it the same way, so
    /// the equivalence is a test rather than a comment.
    #[test]
    fn a_bound_context_identity_is_exactly_its_derived_identity() {
        for (slot, task_id) in [
            (0_usize, 0_u64),
            (1, 7),
            (63, 4_097),
            (127, u32::MAX as u64),
        ] {
            assert_eq!(
                Some(SchedulingContext::bind(slot, task_id).identity()),
                SchedulingContext::derived_identity(slot, task_id),
            );
            assert!(SchedulingContext::bind(slot, task_id).is_bound_to(task_id));
        }
        // A different slot or task can never produce the same identity, which
        // is what makes an unlocked reader's revalidation exact.
        assert_ne!(
            SchedulingContext::derived_identity(3, 9),
            SchedulingContext::derived_identity(4, 9),
        );
        assert_ne!(
            SchedulingContext::derived_identity(3, 9),
            SchedulingContext::derived_identity(3, 10),
        );
    }

    fn policy(refill_capacity: u8) -> SchedulingContextPolicy {
        SchedulingContextPolicy {
            budget_ns: 4_000,
            period_ns: 10_000,
            refill_capacity,
            cpu_mask: 1,
            criticality: 1,
            domain: 7,
            policy_epoch: 3,
            timeout_endpoint_cap: 0,
        }
    }

    #[test]
    fn boot_task_zero_label_receives_nonzero_object_generation() {
        let context = SchedulingContext::bind(0, 0);
        assert!(context.is_bound_to(0));
        assert_eq!(context.identity().slot(), 1);
        assert_eq!(context.identity().generation(), 1);
    }

    #[test]
    fn invalid_policy_never_mints_budget() {
        let mut invalid = policy(2);
        invalid.budget_ns = invalid.period_ns + 1;
        assert!(BudgetState::admitted(invalid, BudgetCounterScope::Context).is_none());
        invalid = policy(0);
        assert!(BudgetState::admitted(invalid, BudgetCounterScope::Context).is_none());

        let mut full = policy(2);
        full.budget_ns = full.period_ns;
        assert!(BudgetState::admitted(full, BudgetCounterScope::Context).is_some());
    }

    #[test]
    fn consumption_and_refill_conserve_exact_budget() {
        let mut state =
            BudgetState::admitted(policy(4), BudgetCounterScope::Context).expect("valid policy");
        let outcome = state.charge(100, 1_500).expect("bounded charge");
        assert_eq!(outcome.charged_ns, 1_500);
        assert_eq!(outcome.overrun_ns, 0);
        assert_eq!(state.conserved_ns(), Some(4_000));
        assert_eq!(state.next_eligible_ns(), Some(10_100));
        assert!(state.replenish(10_099));
        assert_eq!(state.available_ns, 2_500);
        assert!(state.replenish(10_100));
        assert_eq!(state.available_ns, 4_000);
        assert_eq!(state.conserved_ns(), Some(4_000));
    }

    #[test]
    fn refill_overflow_moves_authority_later_without_creating_time() {
        let mut state =
            BudgetState::admitted(policy(2), BudgetCounterScope::Context).expect("valid policy");
        assert!(state.charge(100, 1_000).is_some());
        assert!(state.charge(200, 1_000).is_some());
        assert!(state.charge(300, 1_000).is_some());
        assert_eq!(state.refills.len, 2);
        assert_eq!(
            state.refills.entries[0],
            Refill {
                eligible_ns: 10_100,
                amount_ns: 1_000
            }
        );
        assert_eq!(
            state.refills.entries[1],
            Refill {
                eligible_ns: 10_300,
                amount_ns: 2_000
            }
        );
        assert_eq!(state.overflow_merge_count, 1);
        assert_eq!(state.conserved_ns(), Some(4_000));
        assert!(state.replenish(10_200));
        assert_eq!(state.available_ns, 2_000);
        assert!(state.replenish(10_300));
        assert_eq!(state.available_ns, 4_000);
    }

    #[test]
    fn exhaustion_reports_tick_overrun_without_refunding_it() {
        let mut state =
            BudgetState::admitted(policy(2), BudgetCounterScope::Context).expect("valid policy");
        let outcome = state.charge(55, 5_000).expect("bounded charge");
        assert_eq!(outcome.charged_ns, 4_000);
        assert_eq!(outcome.overrun_ns, 1_000);
        assert!(outcome.exhausted);
        assert_eq!(state.conserved_ns(), Some(4_000));
        assert_eq!(state.exhaustion_count, 1);
    }

    #[test]
    fn timeout_fault_is_one_shot_observable_and_never_retried() {
        let mut state =
            BudgetState::admitted(policy(2), BudgetCounterScope::Context).expect("valid policy");
        assert!(state.charge(55, 5_000).is_some());
        state.record_timeout_fault(77);
        let fault = state.snapshot().expect("timeout fault snapshot");
        assert_eq!(fault.timeout_fault_count, 1);
        assert_eq!(fault.timeout_fault_consumed_ns, 4_000);
        assert_eq!(fault.timeout_fault_reply, 77);
        assert_eq!(fault.timeout_fault_action, 1);

        let mut stale_handler = policy(2);
        stale_handler.timeout_endpoint_cap = 99;
        let mut state = BudgetState::admitted(stale_handler, BudgetCounterScope::Context)
            .expect("valid stale-handler policy");
        assert!(state.charge(55, 5_000).is_some());
        state.record_timeout_fault(88);
        assert_eq!(
            state
                .snapshot()
                .expect("stale handler timeout snapshot")
                .timeout_fault_action,
            2
        );
    }

    #[test]
    fn domain_budget_is_shared_across_independent_task_charges() {
        let mut domain = SchedulingDomainState::admitted(policy(4)).expect("valid domain");
        assert_eq!(domain.budget.counter_scope, BudgetCounterScope::Domain);
        let first = domain.charge_runtime(100, 2_500).expect("first charge");
        let second = domain.charge_runtime(200, 2_500).expect("second charge");
        assert_eq!(first.charged_ns, 2_500);
        assert_eq!(second.charged_ns, 1_500);
        assert_eq!(second.overrun_ns, 1_000);
        assert!(second.exhausted);
        assert!(!domain.is_eligible(10_099));
        assert!(domain.is_eligible(10_100));
    }

    #[test]
    fn context_admission_is_explicit_and_versioned() {
        let mut context = SchedulingContext::bind(2, 9);
        assert!(context.budget.is_none());
        assert!(!context.is_budgeted());
        assert!(context.admit(policy(3), 1));
        assert!(context.is_budgeted());
        assert_eq!(context.domain_slot(), Some(1));
        assert!(context.allows_cpu(0));
        assert!(!context.allows_cpu(1));
        let budget = context.budget.expect("admitted budget");
        assert_eq!(budget.counter_scope, BudgetCounterScope::Context);
        assert_eq!(budget.policy.policy_epoch, 3);
        assert_eq!(SchedulingContextPolicy::ABI_VERSION, 1);
    }
}
