//! CPU-owned scheduler policy state.
//!
//! Task lifecycle metadata is still serialized by the scheduler catalog, but
//! dispatch hints, class budgets, and virtual timelines belong to exactly one
//! CPU. Keeping this state out of the catalog's singleton policy prevents one
//! CPU from consuming another CPU's fairness or first-turn authority and is
//! the ownership boundary required before local runqueue locks can replace the
//! legacy catalog lock.

use super::{
    MAX_ATOMIC_ACTIVATION_HANDOFFS, MAX_LATENCY_HANDOFF_HINTS, MAX_TASK, SlotHandoffQueue,
};

pub(super) struct CpuDispatchPolicy {
    pub(super) next_pick_hint: Option<usize>,
    pub(super) latency_pick_hints: [Option<usize>; MAX_LATENCY_HANDOFF_HINTS],
    pub(super) latency_pick_hint_head: usize,
    pub(super) latency_pick_hint_len: usize,
    pub(super) spawn_pick_hints: SlotHandoffQueue<MAX_TASK>,
    pub(super) atomic_activation_pick_hints: SlotHandoffQueue<MAX_ATOMIC_ACTIVATION_HANDOFFS>,
    pub(super) atomic_activation_handoff_remaining: usize,
    pub(super) sync_pick_hints: SlotHandoffQueue<MAX_TASK>,
    pub(super) last_min_vruntime_ns: u64,
    pub(super) system_dispatch_streak: u8,
    pub(super) latency_handoff_streak: u8,
    pub(super) sync_handoff_streak: u8,
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
            sync_pick_hints: SlotHandoffQueue::new(),
            last_min_vruntime_ns: 0,
            system_dispatch_streak: 0,
            latency_handoff_streak: 0,
            sync_handoff_streak: 0,
        }
    }
}
