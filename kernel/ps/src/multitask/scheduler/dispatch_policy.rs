//! CPU-owned scheduler policy state.
//!
//! Task lifecycle metadata is still serialized by the scheduler catalog, but
//! dispatch hints, class budgets, and virtual timelines belong to exactly one
//! CPU. Keeping this state out of the catalog's singleton policy prevents one
//! CPU from consuming another CPU's fairness or first-turn authority and is
//! the ownership boundary required before local runqueue locks can replace the
//! legacy catalog lock.

use core::sync::atomic::{AtomicBool, Ordering};

use super::{
    MAX_ATOMIC_ACTIVATION_HANDOFFS, MAX_LATENCY_HANDOFF_HINTS, MAX_TASK, SlotHandoffQueue,
};
use nucleus_core::util::lockdep::{LockClass, MAX_TRACKED_CPUS, TrackedSpinGuard, TrackedSpinLock};

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
    // Test schedulers own private `cpu_dispatch` arrays and drive policy
    // directly rather than through the enqueue path, so the hint has no
    // publisher there. Take the guarded path unconditionally instead.
    #[cfg(test)]
    {
        let _ = cpu;
        true
    }
    #[cfg(not(test))]
    ATOMIC_ACTIVATION_PENDING
        .get(cpu)
        .is_some_and(|pending| pending.load(Ordering::Acquire))
}

pub(super) type CpuDispatchLock =
    TrackedSpinLock<CpuDispatchPolicy, { LockClass::SchedulerPolicy as u8 }>;
pub(super) type CpuDispatchGuard<'a> =
    TrackedSpinGuard<'a, CpuDispatchPolicy, { LockClass::SchedulerPolicy as u8 }>;

pub(super) struct CpuDispatchPolicy {
    pub(super) next_pick_hint: Option<usize>,
    pub(super) latency_pick_hints: [Option<usize>; MAX_LATENCY_HANDOFF_HINTS],
    pub(super) latency_pick_hint_head: usize,
    pub(super) latency_pick_hint_len: usize,
    pub(super) spawn_pick_hints: SlotHandoffQueue<MAX_TASK>,
    pub(super) atomic_activation_pick_hints: SlotHandoffQueue<MAX_ATOMIC_ACTIVATION_HANDOFFS>,
    pub(super) atomic_activation_handoff_remaining: usize,
    pub(super) last_min_vruntime_ns: u64,
    pub(super) system_dispatch_streak: u8,
    pub(super) latency_handoff_streak: u8,
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
            last_min_vruntime_ns: 0,
            system_dispatch_streak: 0,
            latency_handoff_streak: 0,
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
