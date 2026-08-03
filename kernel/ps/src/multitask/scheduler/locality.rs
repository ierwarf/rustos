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
        if slot == self.current_task {
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
        let mut best_by_class = [None::<(usize, u64)>; SchedClass::COUNT];
        let mut local_by_class = [None::<(usize, u64)>; SchedClass::COUNT];
        for slot in 0..MAX_TASK {
            if !self.is_fair_candidate_slot(slot) {
                continue;
            }
            let Some(context) = self.contexts[slot] else {
                continue;
            };
            if !context.ready || !self.context_is_schedulable(slot, context) {
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
        let mut best_by_class = [None::<(usize, u64, usize)>; SchedClass::COUNT];
        let mut local_by_class = [None::<(usize, u64, usize)>; SchedClass::COUNT];
        for slot in 0..MAX_TASK {
            if slot == excluded || !self.is_fair_candidate_slot(slot) {
                continue;
            }
            let Some(context) = self.contexts[slot] else {
                continue;
            };
            if !context.ready || !self.context_is_schedulable(slot, context) {
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
        for slot in 0..MAX_TASK {
            if !self.is_fair_candidate_slot(slot) {
                continue;
            }
            let Some(ctx) = self.contexts[slot] else {
                continue;
            };
            if !ctx.ready || !self.context_is_schedulable(slot, ctx) {
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
        for slot in 0..MAX_TASK {
            if slot == current || !self.is_fair_candidate_slot(slot) {
                continue;
            }
            let Some(ctx) = self.contexts[slot] else {
                continue;
            };
            if !ctx.ready || !self.context_is_schedulable(slot, ctx) {
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
