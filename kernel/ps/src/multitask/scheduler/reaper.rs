//! Owner: exact retired slot detachment and final external-completion ACK.
//! Boundary: Class50 reset + Identity51/Resources53 reuse + funding closure.
//! Lifecycle: desired stop and all bank/frame/MM pins drain before any reset effect.
//! Concurrency: per-slot seals, never the Scheduler directory or a substitute mutex.
//! Failure: partial/abandoned effects quarantine both rows; no vacancy-based completion.
//! Forbidden: user-stack external loans or constructor rollback as retired authority.
//! Evidence: actual terminal/resource seals remain held through external detach.
use core::sync::atomic::{AtomicU64, Ordering};
use super::{
    FIRST_DYNAMIC_TASK_SLOT, TaskContext, TaskRetireReason, current_identity,
    dispatch_policy, donation_ledger, frame_publication, linux_thread_state,
    reclaim::RetiredSlotReclaim, retirement_cleanup, runqueue, runtime_profile, scheduling_runtime,
    stack_resources,
};
use crate::multitask::current_identity::execution::kernel_executor::Executor;

/// 3-level lock-free radix-64 bitmap of dynamic slots whose retirement row
/// satisfies `retirement_cleanup::reuse::begin` preconditions (no reset seal,
/// task_id present, no active producer/deferred/pending cleanup/kernel work).
/// Never overflows; `claim_next` locates the next retired candidate in O(1)
/// using three `trailing_zeros` instructions instead of scanning `active_dynamic_slots`.
const RETIRED_L1_WORDS: usize = crate::multitask::task_table::MAX_TASK_SLOTS / 64;
const RETIRED_L0_WORDS: usize = RETIRED_L1_WORDS / 64;

static RETIRED_ROOT: AtomicU64 = AtomicU64::new(0);
static RETIRED_L0: [AtomicU64; RETIRED_L0_WORDS] = [const { AtomicU64::new(0) }; RETIRED_L0_WORDS];
static RETIRED_L1: [AtomicU64; RETIRED_L1_WORDS] = [const { AtomicU64::new(0) }; RETIRED_L1_WORDS];

/// Updates whether `slot` is a candidate for `reaper::claim_next`.
/// Called from `retirement_cleanup::busy::StateGuard::drop` while the row's
/// `Cleanup50` lock is still held; concurrent updates to sibling slots in the
/// same 64-slot word are synchronized via SeqCst Dekker re-check on clear.
pub(super) fn set_retired_candidate(slot: usize, candidate: bool) {
    if slot < FIRST_DYNAMIC_TASK_SLOT || slot >= crate::multitask::task_table::MAX_TASK_SLOTS {
        return;
    }
    let l1_idx = slot >> 6;
    let l1_bit = 1_u64 << (slot & 63);
    let l0_idx = l1_idx >> 6;
    let l0_bit = 1_u64 << (l1_idx & 63);
    let root_bit = 1_u64 << l0_idx;
    if candidate {
        // ORDERING: SeqCst bottom-up pairs with SeqCst clear + re-check below.
        RETIRED_L1[l1_idx].fetch_or(l1_bit, Ordering::SeqCst);
        // ORDERING: SeqCst ensures L0 summary is visible before root summary.
        RETIRED_L0[l0_idx].fetch_or(l0_bit, Ordering::SeqCst);
        // ORDERING: SeqCst publishes root bit after lower levels are populated.
        RETIRED_ROOT.fetch_or(root_bit, Ordering::SeqCst);
    } else {
        // ORDERING: SeqCst clears leaf bit before Dekker re-check of parent summary.
        let prev_l1 = RETIRED_L1[l1_idx].fetch_and(!l1_bit, Ordering::SeqCst);
        if (prev_l1 & !l1_bit) == 0 {
            // ORDERING: SeqCst clears L0 bit before re-reading L1 for concurrent sets.
            let prev_l0 = RETIRED_L0[l0_idx].fetch_and(!l0_bit, Ordering::SeqCst);
            // ORDERING: SeqCst Dekker re-check observes any sibling set racing with L0 clear.
            if RETIRED_L1[l1_idx].load(Ordering::SeqCst) != 0 {
                // ORDERING: SeqCst restores L0 summary bit for the racing sibling.
                RETIRED_L0[l0_idx].fetch_or(l0_bit, Ordering::SeqCst);
                // ORDERING: SeqCst restores root summary bit after L0 restore.
                RETIRED_ROOT.fetch_or(root_bit, Ordering::SeqCst);
            } else if (prev_l0 & !l0_bit) == 0 {
                // ORDERING: SeqCst clears root bit before Dekker re-check of L0.
                RETIRED_ROOT.fetch_and(!root_bit, Ordering::SeqCst);
                // ORDERING: SeqCst Dekker re-check observes any sibling L0 set racing with root clear.
                if RETIRED_L0[l0_idx].load(Ordering::SeqCst) != 0 {
                    // ORDERING: SeqCst restores root summary bit for the racing sibling L0 word.
                    RETIRED_ROOT.fetch_or(root_bit, Ordering::SeqCst);
                }
            }
        }
    }
}

/// Iterates retired candidate slots in ascending slot order, O(1) per candidate.
struct RetiredCandidates {
    root: u64,
    l0_idx: usize,
    l0: u64,
    l1_idx: usize,
    l1: u64,
}

impl RetiredCandidates {
    fn snapshot() -> Self {
        Self {
            // ORDERING: Acquire pairs with SeqCst root publication to observe L0/L1 updates.
            root: RETIRED_ROOT.load(Ordering::Acquire),
            l0_idx: 0,
            l0: 0,
            l1_idx: 0,
            l1: 0,
        }
    }
}

impl Iterator for RetiredCandidates {
    type Item = usize;

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            if self.l1 != 0 {
                let bit = self.l1.trailing_zeros() as usize;
                self.l1 &= self.l1 - 1;
                return Some(self.l1_idx * 64 + bit);
            }
            if self.l0 != 0 {
                let off = self.l0.trailing_zeros() as usize;
                self.l0 &= self.l0 - 1;
                self.l1_idx = self.l0_idx * 64 + off;
                // ORDERING: Acquire pairs with SeqCst leaf publication before candidate capture.
                self.l1 = RETIRED_L1[self.l1_idx].load(Ordering::Acquire);
                continue;
            }
            if self.root == 0 {
                return None;
            }
            self.l0_idx = self.root.trailing_zeros() as usize;
            self.root &= self.root - 1;
            // ORDERING: Acquire pairs with SeqCst L0 publication before reading L1 words.
            self.l0 = RETIRED_L0[self.l0_idx].load(Ordering::Acquire);
        }
    }
}

struct Grant {
    slot: usize,
    id: u64,
    context: TaskContext,
    bounds: (u64, u64),
    reason: Option<TaskRetireReason>,
    reset: retirement_cleanup::reuse::ResetSeal,
    reuse: stack_resources::reuse::Reuse,
}
impl Grant {
    fn capture(slot: usize, active_root: Option<u64>) -> Option<Self> {
        if slot < FIRST_DYNAMIC_TASK_SLOT || crate::multitask::task_table::try_get(slot).is_none() {
            return None;
        }
        let id = stack_resources::task_start::read(slot)?.id;
        if !current_identity::terminal_retired_matches(slot, id)
            || crate::multitask::task_slot_is_running(slot)
            || runqueue::owner(slot).state != runqueue::RunOwnerState::Retired
        {
            return None;
        }
        let context = stack_resources::context::read(slot)?;
        if context.user_mode && Some(runqueue::address_space::root(slot)) == active_root {
            return None;
        }
        // The terminal seal excludes producers throughout resource capture and teardown.
        // Failure rolls back only this untouched seal; irreversible funding close
        // retains desired-stop custody and will be retried after interval release.
        let reset = retirement_cleanup::reuse::begin(slot, Some(id))?;
        if !scheduling_runtime::close_for_reclaim(slot, context.scheduling_context.identity()) {
            return None;
        }
        let reuse = stack_resources::reuse::begin_retired(slot, id)?;
        let captured = stack_resources::context::read(slot).expect("retired grant lost context");
        assert_eq!(
            captured.scheduling_context.identity(),
            context.scheduling_context.identity(),
            "retired grant crossed funding replacement"
        );
        assert_eq!(
            captured.process_handle, context.process_handle,
            "retired grant crossed process replacement"
        );
        Some(Self {
            slot,
            id,
            context: captured,
            bounds: runqueue::kernel_stack_bounds(slot),
            reason: retirement_cleanup::reason(slot),
            reset,
            reuse,
        })
    }
    fn detach(mut self) -> Detached {
        // No leaf is retained across these effects. Both owned seals keep the
        // exact original generation closed until external completion, not only
        // until the metadata happens to become empty.
        self.reset.mark_effects();
        self.reset.reset();
        donation_ledger::release_task(self.id);
        runqueue::release_retired(self.slot);
        scheduling_runtime::release(self.slot, self.context.scheduling_context.identity());
        stack_resources::context::clear(self.slot);
        linux_thread_state::clear_reclaimed(self.slot);
        dispatch_policy::remove_reclaimed_slot(self.slot);
        for gate in [
            current_identity::LifecycleGate::Retired,
            current_identity::LifecycleGate::ExecQuiesced,
            current_identity::LifecycleGate::Suspended,
            current_identity::LifecycleGate::JobStopped,
        ] {
            current_identity::publish_lifecycle_change(self.slot, None, gate, false);
        }
        runqueue::simd_tls::reset_simd_state(self.slot);
        stack_resources::task_start::clear(self.slot);
        frame_publication::clear(self.slot);
        current_identity::publish_metadata(self.slot, None);
        runtime_profile::owner::clear_history(self.slot);
        let stack = self.reuse.take_stack();
        super::stack_watermark::record_retired(self.id, self.context.user_mode, stack.as_ref());
        let reclaim = RetiredSlotReclaim::new(
            self.context.process_handle,
            stack,
            self.id,
            self.slot,
            self.context.user_mode,
            self.bounds.0,
            self.bounds.1,
            self.reason,
        );
        Detached {
            reclaim,
            reset: self.reset,
            reuse: self.reuse,
        }
    }
}
struct Detached {
    reclaim: RetiredSlotReclaim,
    reset: retirement_cleanup::reuse::ResetSeal,
    reuse: stack_resources::reuse::Reuse,
}
impl Detached {
    fn finish_with(self, effects: impl FnOnce(RetiredSlotReclaim)) {
        effects(self.reclaim);
        // External process/stack destruction must finish before either reopening ACK.
        self.reset.finish();
        self.reuse.finish();
    }
}
/// External completion borrows kill-safe actual kernel Current, never a user loan.
pub(in crate::multitask) struct Reap<'a> {
    executor: &'a Executor,
    detached: Detached,
}
impl Reap<'_> {
    pub(in crate::multitask) fn complete(self) {
        self.executor.assert_current();
        self.detached.finish_with(|reclaim| {
            reclaim.complete();
            self.executor.assert_current();
        });
    }
}
pub(in crate::multitask) fn claim_next(executor: &Executor) -> Option<Reap<'_>> {
    executor.assert_current();
    if !retirement_cleanup::take_reap_pending() {
        return None;
    }
    let current = crate::multitask::cpu_local::current_cpu_task_slot()?;
    let active_root =
        stack_resources::context::read(current).map(|_| runqueue::address_space::root(current));
    let mut pending = false;
    for slot in RetiredCandidates::snapshot() {
        let Some(start) = stack_resources::task_start::read(slot) else {
            continue;
        };
        if !current_identity::terminal_retired_matches(slot, start.id) {
            continue;
        }
        pending = true;
        if let Some(grant) = Grant::capture(slot, active_root) {
            retirement_cleanup::mark_reap_pending();
            return Some(Reap {
                executor,
                detached: grant.detach(),
            });
        }
    }
    if pending {
        retirement_cleanup::mark_reap_pending();
    }
    None
}

#[cfg(test)]
pub(super) mod tests;
