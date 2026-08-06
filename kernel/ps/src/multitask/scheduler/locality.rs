//! Bounded CPU-local fair selection.
//!
//! - **Owner:** the serialized scheduler owns exact per-task last-CPU policy
//!   metadata; the runtime profiler owns a separate resettable observation.
//! - **Boundary:** locality is considered only after class, affinity,
//!   lifecycle, idle ownership, and remote-running-owner admission.
//! - **Invariant:** a local candidate may beat the same-class global minimum
//!   by at most `SCHED_CPU_LOCALITY_LAG_NS`; exact handoffs never enter here.
//! - **Failure:** slot reuse clears locality and an out-of-range CPU panics.

use super::*;

impl Scheduler {
    fn candidate_is_local_to_current_cpu(&self, slot: usize) -> bool {
        if slot == self.current_task_slot() {
            return true;
        }
        let logical_cpu = u8::try_from(nucleus_core::util::lockdep::current_cpu_index())
            .expect("logical CPU index exceeds scheduler locality capacity");
        self.task_last_cpu[slot] == logical_cpu
    }

    /// Select a local fair candidate only while its virtual-runtime lag from
    /// the same-class global minimum is explicitly bounded. The caller has
    /// already enforced class, affinity, lifecycle, and remote-owner rules.
    fn prefer_local_candidate(
        global: Option<(usize, u64)>,
        local: Option<(usize, u64)>,
    ) -> Option<usize> {
        match (global, local) {
            (Some((_global_slot, global_vruntime)), Some((local_slot, local_vruntime)))
                if local_vruntime <= global_vruntime.saturating_add(SCHED_CPU_LOCALITY_LAG_NS) =>
            {
                Some(local_slot)
            }
            (Some((global_slot, _)), _) => Some(global_slot),
            (None, Some((local_slot, _))) => Some(local_slot),
            (None, None) => None,
        }
    }

    pub(super) fn record_task_dispatch_cpu(&mut self, slot: usize, logical_cpu: usize) {
        self.task_last_cpu[slot] = u8::try_from(logical_cpu)
            .expect("logical CPU index exceeds scheduler locality capacity");
    }

    /// Walk scheduling classes in priority order and select the global
    /// least-vruntime task or one bounded local alternative in that class.
    pub(super) fn pick_min_vruntime(&self, current: usize) -> Option<usize> {
        let started_ns = crate::arch::clock::monotonic_nanos();
        let picked = self.pick_min_vruntime_inner(current);
        charge_pick_scan(crate::arch::clock::monotonic_nanos().saturating_sub(started_ns));
        picked
    }

    fn pick_min_vruntime_inner(&self, current: usize) -> Option<usize> {
        let mut best_by_class = [None::<(usize, u64)>; SchedClass::COUNT];
        let mut local_by_class = [None::<(usize, u64)>; SchedClass::COUNT];
        let current_cpu = nucleus_core::util::lockdep::current_cpu_index();
        for slot in runqueue::local_runnable_slots(current_cpu) {
            if !self.is_fair_candidate_slot(slot) {
                continue;
            }
            let Some(context) = self.contexts[slot] else {
                continue;
            };
            if !self.slot_is_runnable(slot) || !self.context_is_schedulable(slot, context) {
                continue;
            }
            let Some(class) = self.slot_class(slot) else {
                continue;
            };
            let key = if slot == current {
                context.vruntime_ns.saturating_add(1)
            } else {
                context.vruntime_ns
            };
            let candidate = &mut best_by_class[class.index()];
            if candidate
                .map(|(_, best_key)| key < best_key)
                .unwrap_or(true)
            {
                *candidate = Some((slot, key));
            }
            if self.candidate_is_local_to_current_cpu(slot) {
                let local = &mut local_by_class[class.index()];
                if local.map(|(_, best_key)| key < best_key).unwrap_or(true) {
                    *local = Some((slot, key));
                }
            }
        }
        for class in 0..SchedClass::COUNT {
            if best_by_class[class].is_some() {
                return Self::prefer_local_candidate(best_by_class[class], local_by_class[class]);
            }
        }
        None
    }

    pub(super) fn pick_min_vruntime_excluding(&self, excluded: usize) -> Option<usize> {
        let started_ns = crate::arch::clock::monotonic_nanos();
        let picked = self.pick_min_vruntime_excluding_inner(excluded);
        charge_pick_scan(crate::arch::clock::monotonic_nanos().saturating_sub(started_ns));
        picked
    }

    fn pick_min_vruntime_excluding_inner(&self, excluded: usize) -> Option<usize> {
        let mut best_by_class = [None::<(usize, u64, usize)>; SchedClass::COUNT];
        let mut local_by_class = [None::<(usize, u64, usize)>; SchedClass::COUNT];
        let current_cpu = nucleus_core::util::lockdep::current_cpu_index();
        for slot in runqueue::local_runnable_slots(current_cpu) {
            if slot == excluded || !self.is_fair_candidate_slot(slot) {
                continue;
            }
            let Some(context) = self.contexts[slot] else {
                continue;
            };
            if !self.slot_is_runnable(slot) || !self.context_is_schedulable(slot, context) {
                continue;
            }
            let Some(class) = self.slot_class(slot) else {
                continue;
            };
            let distance = (slot + MAX_TASK - excluded) % MAX_TASK;
            let candidate = &mut best_by_class[class.index()];
            if candidate
                .map(|(_, best_vruntime, best_distance)| {
                    context.vruntime_ns < best_vruntime
                        || (context.vruntime_ns == best_vruntime && distance < best_distance)
                })
                .unwrap_or(true)
            {
                *candidate = Some((slot, context.vruntime_ns, distance));
            }
            if self.candidate_is_local_to_current_cpu(slot) {
                let local = &mut local_by_class[class.index()];
                if local
                    .map(|(_, best_vruntime, best_distance)| {
                        context.vruntime_ns < best_vruntime
                            || (context.vruntime_ns == best_vruntime && distance < best_distance)
                    })
                    .unwrap_or(true)
                {
                    *local = Some((slot, context.vruntime_ns, distance));
                }
            }
        }
        for class in 0..SchedClass::COUNT {
            if best_by_class[class].is_some() {
                return Self::prefer_local_candidate(
                    best_by_class[class].map(|(slot, vruntime, _)| (slot, vruntime)),
                    local_by_class[class].map(|(slot, vruntime, _)| (slot, vruntime)),
                );
            }
        }
        None
    }

    /// Pick one task in an explicitly selected class without permitting the
    /// locality preference to cross that class boundary.
    pub(super) fn pick_min_vruntime_in_class(
        &self,
        current: usize,
        class: SchedClass,
    ) -> Option<usize> {
        let mut best: Option<(usize, u64)> = None;
        let mut local: Option<(usize, u64)> = None;
        let current_cpu = nucleus_core::util::lockdep::current_cpu_index();
        for slot in runqueue::local_runnable_slots(current_cpu) {
            if !self.is_fair_candidate_slot(slot) {
                continue;
            }
            let Some(ctx) = self.contexts[slot] else {
                continue;
            };
            if !self.slot_is_runnable(slot) || !self.context_is_schedulable(slot, ctx) {
                continue;
            }
            if self.slot_class(slot) != Some(class) {
                continue;
            }
            let key = if slot == current {
                ctx.vruntime_ns.saturating_add(1)
            } else {
                ctx.vruntime_ns
            };
            match best {
                None => best = Some((slot, key)),
                Some((_, best_key)) if key < best_key => best = Some((slot, key)),
                _ => {}
            }
            if self.candidate_is_local_to_current_cpu(slot) {
                match local {
                    None => local = Some((slot, key)),
                    Some((_, local_key)) if key < local_key => local = Some((slot, key)),
                    _ => {}
                }
            }
        }
        Self::prefer_local_candidate(best, local)
    }

    pub(super) fn pick_burst_alternate_in_current_class(&self, current: usize) -> Option<usize> {
        let class = self.slot_class(current)?;
        let mut best: Option<(usize, u64)> = None;
        let mut local: Option<(usize, u64)> = None;
        let current_cpu = nucleus_core::util::lockdep::current_cpu_index();
        for slot in runqueue::local_runnable_slots(current_cpu) {
            if slot == current || !self.is_fair_candidate_slot(slot) {
                continue;
            }
            let Some(ctx) = self.contexts[slot] else {
                continue;
            };
            if !self.slot_is_runnable(slot) || !self.context_is_schedulable(slot, ctx) {
                continue;
            }
            if self.slot_class(slot) != Some(class) {
                continue;
            }
            let key = ctx.vruntime_ns;
            if best
                .map(|(_, current_key)| key < current_key)
                .unwrap_or(true)
            {
                best = Some((slot, key));
            }
            if self.candidate_is_local_to_current_cpu(slot)
                && local
                    .map(|(_, current_key)| key < current_key)
                    .unwrap_or(true)
            {
                local = Some((slot, key));
            }
        }
        Self::prefer_local_candidate(best, local)
    }
}

/// Self-timed cost of the fair-class pick scan.
///
/// The phase marks charge this to `SelectHandoff`, because both pick functions
/// are lexically inside that span, so the scheduler profile reported handoff at
/// 238 ms per second of lock hold — 40 percent of the total — while
/// `SelectPick` read under 1 ms. A segment that names the wrong work invites
/// optimising the wrong thing; one reorder of the handoff predicates was
/// already spent that way and moved nothing.
///
/// Static rather than a `Scheduler` field because both functions take `&self`.
/// The scheduler lock already serializes every writer.
static PICK_SCAN_NS: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
static PICK_SCAN_CALLS: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);

fn charge_pick_scan(elapsed_ns: u64) {
    // ORDERING: Relaxed. Diagnostic counters with no other state attached, read
    // only by the once-per-second drain.
    PICK_SCAN_NS.fetch_add(elapsed_ns, core::sync::atomic::Ordering::Relaxed);
    PICK_SCAN_CALLS.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
}

/// Self-timed cost of the handoff chain's own scans.
///
/// Charged by the overdue-class and reserved-user scans, which are the other
/// two O(local runnable) walks inside the `SelectHandoff` span. Splitting them
/// from the fair pick is what says which of the three costs; the fair pick
/// turned out to be 29 ms of a 216 ms segment, so it is not the one.
static HANDOFF_SCAN_NS: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
static HANDOFF_SCAN_CALLS: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);

pub(in crate::multitask) fn charge_handoff_scan(elapsed_ns: u64) {
    // ORDERING: Relaxed; diagnostic counters drained once per second.
    HANDOFF_SCAN_NS.fetch_add(elapsed_ns, core::sync::atomic::Ordering::Relaxed);
    HANDOFF_SCAN_CALLS.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
}

/// Takes the window's handoff-scan cost, clearing it for the next window.
pub(in crate::multitask) fn take_handoff_scan_window() -> (u64, u64) {
    (
        HANDOFF_SCAN_NS.swap(0, core::sync::atomic::Ordering::Relaxed),
        HANDOFF_SCAN_CALLS.swap(0, core::sync::atomic::Ordering::Relaxed),
    )
}

/// Takes the window's pick-scan cost, clearing it for the next window.
pub(in crate::multitask) fn take_pick_scan_window() -> (u64, u64) {
    (
        PICK_SCAN_NS.swap(0, core::sync::atomic::Ordering::Relaxed),
        PICK_SCAN_CALLS.swap(0, core::sync::atomic::Ordering::Relaxed),
    )
}

/// Per-member cost of the handoff chain itself.
///
/// Splitting the fair pick and the two class scans out of `SelectHandoff` left
/// 63 percent of the segment — about 5.6 us of every dispatch — attributed to
/// nothing but the chain's own six steps. Each step is a separate hint queue
/// with its own per-CPU policy lock traffic, so which of them owns that time is
/// not derivable from the source; it has to be measured before any of them is
/// touched.
///
/// Steps 2 and 5 re-enter the overdue class scan, so their totals overlap
/// `HANDOFF_SCAN_NS` by construction. Every other step is disjoint.
///
/// Step 6 is the chain's single acquisition of this CPU's dispatch policy.
///
/// The first measurement returned every step between 0.95 and 2.5 us regardless
/// of how much work it does — the step that finds an empty queue and returns
/// cost nearly as much as the one that scans. Two controls priced that: an
/// empty timed span read 0.032 us and one bare policy acquisition read
/// 0.724 us, against a 0.72 us per-step floor. The chain was paying for
/// acquisitions, not decisions, about ten of them per dispatch. It now takes
/// the guard once and threads it, so step 6 is the entire acquisition cost the
/// chain pays.
pub(in crate::multitask) const HANDOFF_STEP_COUNT: usize = 7;
static HANDOFF_STEP_NS: [core::sync::atomic::AtomicU64; HANDOFF_STEP_COUNT] =
    [const { core::sync::atomic::AtomicU64::new(0) }; HANDOFF_STEP_COUNT];
static HANDOFF_STEP_CALLS: [core::sync::atomic::AtomicU64; HANDOFF_STEP_COUNT] =
    [const { core::sync::atomic::AtomicU64::new(0) }; HANDOFF_STEP_COUNT];

pub(in crate::multitask) fn charge_handoff_step(step: usize, elapsed_ns: u64) {
    let Some(total) = HANDOFF_STEP_NS.get(step) else {
        return;
    };
    // ORDERING: Relaxed; diagnostic counters drained once per second.
    total.fetch_add(elapsed_ns, core::sync::atomic::Ordering::Relaxed);
    HANDOFF_STEP_CALLS[step].fetch_add(1, core::sync::atomic::Ordering::Relaxed);
}

/// Takes the window's per-step chain cost, clearing it for the next window.
pub(in crate::multitask) fn take_handoff_step_window() -> [(u64, u64); HANDOFF_STEP_COUNT] {
    core::array::from_fn(|step| {
        (
            HANDOFF_STEP_NS[step].swap(0, core::sync::atomic::Ordering::Relaxed),
            HANDOFF_STEP_CALLS[step].swap(0, core::sync::atomic::Ordering::Relaxed),
        )
    })
}
