//! CPU-owned scheduler policy state.
//!
//! Task lifecycle metadata is still serialized by the scheduler catalog, but
//! dispatch hints, class budgets, and virtual timelines belong to exactly one
//! CPU. Keeping this state out of the catalog's singleton policy prevents one
//! CPU from consuming another CPU's fairness or first-turn authority and is
//! the ownership boundary required before local runqueue locks can replace the
//! legacy catalog lock.

use core::sync::atomic::AtomicU8;
use core::sync::atomic::{AtomicBool, Ordering};

use super::{
    MAX_ATOMIC_ACTIVATION_HANDOFFS, MAX_LATENCY_HANDOFF_HINTS, MAX_TASK, SlotHandoffQueue,
    handoff_queue::FirstTurnHandoff,
};
use nucleus_core::util::lockdep::{LockClass, MAX_TRACKED_CPUS};
use crate::multitask::irq_leaf::{Guard, IrqLeaf};

/// Lock-free "this CPU may have a committed cohort activation waiting".
///
/// The dispatcher's first act was to take this CPU's policy lock only to learn
/// that the activation queue was empty - true on about 96 percent of
/// dispatches - and then drop it again so the synchronous FIFO could be
/// consumed without nesting. Two acquisitions of the hottest step in the
/// handoff chain, 58,412 of them per second at eight vCPUs, to answer one
/// boolean.
///
/// The hint is deliberately one-sided. An enqueue sets it, and only the
/// guarded consumer clears it, after it has seen the queue drained under the
/// lock. A stale `true` costs exactly one lock acquisition - what every
/// dispatch paid before - while a stale `false` would lose an activation, and
/// cannot arise: the only publisher of new work sets the flag under the same
/// lock the consumer must take to clear it.
static ATOMIC_ACTIVATION_PENDING: [AtomicBool; MAX_TRACKED_CPUS] =
    [const { AtomicBool::new(false) }; MAX_TRACKED_CPUS];

/// Called by the enqueueing CPU while it holds `cpu`'s policy lock.
pub(super) fn publish_atomic_activation_pending(cpu: usize) {
    if let Some(pending) = ATOMIC_ACTIVATION_PENDING.get(cpu) {
        // ORDERING: release publishes the enqueued slot before the flag that
        // advertises it, so an acquiring dispatcher cannot see the flag alone.
        pending.store(true, Ordering::Release);
    }
}

/// Called by the owning CPU under its own policy lock, once it has observed the
/// queue drained. Clearing anywhere else could race a concurrent enqueue.
pub(super) fn clear_atomic_activation_pending(cpu: usize) {
    if let Some(pending) = ATOMIC_ACTIVATION_PENDING.get(cpu) {
        pending.store(false, Ordering::Release);
    }
}

pub(super) fn atomic_activation_pending(cpu: usize) -> bool {
    ATOMIC_ACTIVATION_PENDING
        .get(cpu)
        .is_some_and(|pending| pending.load(Ordering::Acquire))
}

/// Inspect the actual publisher flag, not the catalog dispatch fixture's hint.
#[cfg(test)]
pub(super) fn test_published_atomic_activation_pending(cpu: usize) -> bool {
    ATOMIC_ACTIVATION_PENDING
        .get(cpu)
        .is_some_and(|pending| pending.load(Ordering::Acquire))
}

/// CPU-local fairness streaks are single-writer dispatch state. Production
/// keeps them outside the hint lock so an otherwise exact handoff does not
/// acquire a queue lock merely to update two bytes; host fixtures retain the
/// fields in `CpuDispatchPolicy` for isolated state-machine tests.
static SYSTEM_DISPATCH_STREAK: [AtomicU8; MAX_TRACKED_CPUS] =
    [const { AtomicU8::new(0) }; MAX_TRACKED_CPUS];
static LATENCY_HANDOFF_STREAK: [AtomicU8; MAX_TRACKED_CPUS] =
    [const { AtomicU8::new(0) }; MAX_TRACKED_CPUS];

pub(super) fn system_dispatch_streak(cpu: usize) -> u8 {
    SYSTEM_DISPATCH_STREAK[cpu].load(Ordering::Relaxed)
}

pub(super) fn latency_handoff_streak(cpu: usize) -> u8 {
    LATENCY_HANDOFF_STREAK[cpu].load(Ordering::Relaxed)
}

/// Any successful priority arm can satisfy a spawn's first turn. Borrow the
/// SAME original claim, never infer completion from a scalar Running/UID read.
/// This only withdraws ordering metadata, not frame, funding or wait custody.
/// The FIFO arm already consumed its own front, so repeated discharge is inert.
pub(super) fn discharge_claimed_first_turn(turn: &crate::multitask::cpu_local::dispatch_turn::CpuDispatchTurn<'static>) {
    let (cpu, _) = turn.original_current().expect("first-turn completion lost CPU owner");
    assert!(turn.owns_priority_choice(), "first-turn completion lacks priority choice");
    let receipt = turn.claimed_return().expect("first-turn completion lacks original claim");
    let record = FirstTurnHandoff { slot: receipt.slot(), task_id: receipt.id };
    let mut policy = lock(cpu);
    policy.spawn_pick_hints.remove_exact(record);
    if policy.atomic_activation_pick_hints.remove_exact(record) {
        policy.atomic_activation_handoff_remaining = policy.atomic_activation_handoff_remaining
            .checked_sub(1).expect("completed prefix count underflow");
        if policy.atomic_activation_pick_hints.is_empty() {
            assert_eq!(policy.atomic_activation_handoff_remaining, 0);
            clear_atomic_activation_pending(cpu);
        }
    }
}

/// Records the completed choice, after the actual physical/funding claim.
/// These per-CPU policy counters are not frame, UID or execution authority.
/// Native policy witnesses drive this same transition with explicit class input.
pub(super) fn record_completed_choice(cpu: usize, system_class: bool, latency_handoff: bool) {
    let system = if system_class {
        system_dispatch_streak(cpu).saturating_add(1).min(super::MAX_CONSECUTIVE_SYSTEM_DISPATCHES)
    } else { 0 };
    let latency = if latency_handoff {
        latency_handoff_streak(cpu).saturating_add(1).min(super::MAX_CONSECUTIVE_LATENCY_HANDOFFS)
    } else { 0 };
    store_dispatch_streaks(cpu, system, latency);
}

pub(super) fn store_dispatch_streaks(cpu: usize, system: u8, latency: u8) {
    SYSTEM_DISPATCH_STREAK[cpu].store(system, Ordering::Relaxed);
    LATENCY_HANDOFF_STREAK[cpu].store(latency, Ordering::Relaxed);
}

pub(super) type CpuDispatchLock =
    IrqLeaf<CpuDispatchPolicy, { LockClass::SchedulerPolicy as u8 }>;
pub(super) type CpuDispatchGuard<'a> =
    Guard<'a, CpuDispatchPolicy, { LockClass::SchedulerPolicy as u8 }>;

// Production FIFO and admission policy belong to the destination CPU, not
// to the lifecycle catalog. The host fixture keeps isolated policy arrays in
// each Scheduler instance so its state-machine tests cannot share a static.
static CPU_DISPATCH_POLICY: [CpuDispatchLock; MAX_TRACKED_CPUS] =
    [const { CpuDispatchLock::new(CpuDispatchPolicy::new()) }; MAX_TRACKED_CPUS];

pub(super) fn lock(cpu: usize) -> CpuDispatchGuard<'static> {
    CPU_DISPATCH_POLICY[cpu].lock()
}

pub(super) fn reset() {
    for policy in &CPU_DISPATCH_POLICY {
        *policy.lock() = CpuDispatchPolicy::new();
    }
}

/// One bounded User latency exception belongs to the exact task that
/// published it; a recycled slot cannot inherit its priority budget.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct LatencyPickHint {
    pub(super) slot: usize,
    pub(super) task_id: u64,
}

/// Physical all-CPU hint withdrawal; the exact retired resource owner excludes reuse.
pub(super) fn remove_reclaimed_slot(slot: usize) {
    for cpu in 0..MAX_TRACKED_CPUS {
        let mut policy = lock(cpu);
        if policy.next_pick_hint.is_some_and(|hint| hint.slot == slot) {
            policy.next_pick_hint = None;
        }
        let mut compact = [None; MAX_LATENCY_HANDOFF_HINTS];
        let mut retained = 0;
        for offset in 0..policy.latency_pick_hint_len {
            let index = (policy.latency_pick_hint_head + offset) % MAX_LATENCY_HANDOFF_HINTS;
            if let Some(candidate) = policy.latency_pick_hints[index]
                && candidate.slot != slot
            {
                compact[retained] = Some(candidate);
                retained += 1;
            }
        }
        policy.latency_pick_hints = compact;
        policy.latency_pick_hint_head = 0;
        policy.latency_pick_hint_len = retained;
        policy.spawn_pick_hints.remove(slot);
        if policy.atomic_activation_pick_hints.remove(slot) {
            policy.atomic_activation_handoff_remaining = policy.atomic_activation_handoff_remaining.checked_sub(1)
                .expect("terminal prefix withdrawal lost original count");
            if policy.atomic_activation_pick_hints.is_empty() {
                assert_eq!(policy.atomic_activation_handoff_remaining, 0);
                clear_atomic_activation_pending(cpu);
            }
        }
    }
    super::sync_handoff::remove_slot_all_cpus(slot);
}

pub(super) fn latency_hint_identity_matches(
    hint: LatencyPickHint,
    published_task_id: Option<u64>,
) -> bool {
    hint.task_id != 0 && published_task_id == Some(hint.task_id)
}

pub(super) struct CpuDispatchPolicy {
    pub(super) next_pick_hint: Option<LatencyPickHint>,
    pub(super) latency_pick_hints: [Option<LatencyPickHint>; MAX_LATENCY_HANDOFF_HINTS],
    pub(super) latency_pick_hint_head: usize,
    pub(super) latency_pick_hint_len: usize,
    pub(super) spawn_pick_hints: SlotHandoffQueue<MAX_TASK, FirstTurnHandoff>,
    pub(super) atomic_activation_pick_hints:
        SlotHandoffQueue<MAX_ATOMIC_ACTIVATION_HANDOFFS, FirstTurnHandoff>,
    pub(super) atomic_activation_handoff_remaining: usize,
    pub(super) ready_validation_turn: u8,
}

impl CpuDispatchPolicy {
    pub(super) const fn new() -> Self {
        Self {
            next_pick_hint: None,
            latency_pick_hints: [None; MAX_LATENCY_HANDOFF_HINTS],
            latency_pick_hint_head: 0,
            latency_pick_hint_len: 0,
            spawn_pick_hints: SlotHandoffQueue::new(),
            atomic_activation_pick_hints: SlotHandoffQueue::new(),
            atomic_activation_handoff_remaining: 0,
            ready_validation_turn: 0,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        ATOMIC_ACTIVATION_PENDING, clear_atomic_activation_pending,
        publish_atomic_activation_pending,
    };
    use core::sync::atomic::Ordering;

    #[test]
    fn latency_hint_does_not_transfer_user_budget_to_a_recycled_slot() {
        let hint = super::LatencyPickHint {
            slot: 23,
            task_id: 51,
        };
        assert!(super::latency_hint_identity_matches(hint, Some(51)));
        assert!(!super::latency_hint_identity_matches(hint, Some(52)));
        assert!(!super::latency_hint_identity_matches(hint, None));
        assert!(!super::latency_hint_identity_matches(
            super::LatencyPickHint {
                slot: 23,
                task_id: 0
            },
            Some(0),
        ));
    }

    #[test]
    fn the_activation_hint_is_one_sided_so_a_missed_clear_only_costs_a_lock() {
        let cpu = 3;
        clear_atomic_activation_pending(cpu);
        assert!(!ATOMIC_ACTIVATION_PENDING[cpu].load(Ordering::Acquire));

        // An enqueue must always advertise; losing this is the only way the
        // dispatcher could skip a queued activation.
        publish_atomic_activation_pending(cpu);
        assert!(ATOMIC_ACTIVATION_PENDING[cpu].load(Ordering::Acquire));

        // A second enqueue before the consumer runs stays advertised: the flag
        // says "may have work", never how much.
        publish_atomic_activation_pending(cpu);
        assert!(ATOMIC_ACTIVATION_PENDING[cpu].load(Ordering::Acquire));

        clear_atomic_activation_pending(cpu);
        assert!(!ATOMIC_ACTIVATION_PENDING[cpu].load(Ordering::Acquire));
    }

    #[test]
    fn publishing_for_an_unknown_cpu_is_ignored_rather_than_clobbering_a_neighbour() {
        // A CPU index of its own: the flags are process-wide statics, so an
        // assertion over the whole array would race the test above.
        let witness = 5;
        clear_atomic_activation_pending(witness);

        let out_of_range = ATOMIC_ACTIVATION_PENDING.len();
        publish_atomic_activation_pending(out_of_range);
        clear_atomic_activation_pending(out_of_range);

        assert!(!ATOMIC_ACTIVATION_PENDING[witness].load(Ordering::Acquire));
    }
}
