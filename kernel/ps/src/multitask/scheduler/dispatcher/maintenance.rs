//! Bounded queue maintenance under the original CPU choice, not catalog authority.
//! Owner: original turn plus immutable frame reader for the exact queued source.
//! Boundary: Local -> Migrating CAS -> target mailbox; no wake from stale Blocked.
//! Lifecycle: observation -> original allocation/reader -> exact transfer -> drop.
//! Concurrency: one queue leaf at a time; original reader spans the owner CAS.
//! Failure: incomplete/changed source refuses; invalid frames request terminal custody.
//! Forbidden: generic rehome retry acquiring a new owner or waking a blocked peer.
//! Evidence: exact migration/terminal/frame witnesses, native topology only modeled.
use super::super::{context_validation, current_identity, dispatch_policy, frame_publication,
    local_dispatch, runqueue, runqueue_policy, stack_resources, ROOT_TASK_SLOT,
    READY_VALIDATION_INTERVAL_TURNS, SYSTEM_CLASS_WEIGHT_FLAG, donation_ledger};
use crate::multitask::cpu_local::dispatch_turn::CpuDispatchTurn;
use core::sync::atomic::{AtomicU8, Ordering};
use nucleus_core::util::lockdep::MAX_TRACKED_CPUS;
static OPPORTUNITIES: [AtomicU8; MAX_TRACKED_CPUS] = [const { AtomicU8::new(0) }; MAX_TRACKED_CPUS];

/// The source frame is borrowed from its original physical allocation. This
/// is non-Copy and carries an actual reader, not merely an owner generation.
pub(in crate::multitask::scheduler) struct MigrationFrame<'a> {
    turn: &'a CpuDispatchTurn<'static>,
    pub(in crate::multitask::scheduler) token: frame_publication::StableFrameToken,
    id: u64,
    source: usize,
    _reader: stack_resources::frame_readers::FrameReader<'a>,
}
impl MigrationFrame<'_> {
    pub(in crate::multitask::scheduler) fn is_current(&self) -> bool {
        self.turn.original_current().is_some()
            && frame_publication::token_is_current(self.token, self.source)
            && current_identity::read_dispatch(self.token.slot).is_some_and(|p| p.task_id == Some(self.id) && p.lifecycle_dispatchable())
    }
}

#[cfg(test)]
pub(in crate::multitask::scheduler) mod tests;

fn mobile(slot: usize, source: usize) -> bool {
    slot != ROOT_TASK_SLOT && !runqueue::is_idle_slot(slot)
        && !crate::multitask::task_slot_is_running(slot)
        && runqueue::is_local_dispatchable(slot, source)
        && runqueue::weight::observe(slot).is_some()
        && current_identity::read_dispatch(slot).is_some_and(|p| p.task_id.is_some() && p.lifecycle_dispatchable())
}
fn candidate(slot: usize, source: usize, target: usize) -> Option<(u8, u64, usize, usize)> {
    if !mobile(slot, source)
        || local_dispatch::published_candidate_admission(slot, target, crate::arch::clock::monotonic_nanos()) != Ok(true) { return None; }
    let weight = runqueue::weight::observe(slot)?;
    let system = weight & SYSTEM_CLASS_WEIGHT_FLAG != 0 || donation_ledger::inherited_system(slot);
    Some((u8::from(!system), runqueue::vruntime(slot), slot, target))
}
fn transfer(turn: &CpuDispatchTurn<'static>, slot: usize, source: usize, target: usize) -> bool {
    let Some(token) = frame_publication::local_frame_token(slot, source) else { return false; };
    let Some(id) = current_identity::read_dispatch(slot).and_then(|p| p.task_id) else { return false; };
    let Some(pin) = frame_publication::pin_for_identity(token, id) else { return false; };
    let Some((cpu, _)) = turn.original_current() else { return false; };
    // SAFETY: original masked CPU and original allocation precede byte access.
    let Some(reader) = (unsafe { stack_resources::frame_readers::acquire(&pin, cpu) }) else { return false; };
    let validation = context_validation::validate_published_saved_context(slot, token.saved_rsp, token.view);
    if let Err(reason) = validation {
        drop(reader);
        drop(pin);
        super::quarantine(slot, id, token.saved_rsp, reason);
        return false;
    }
    let frame = MigrationFrame { turn, token, id, source, _reader: reader };
    if !frame.is_current() { return false; }
    let moved = runqueue::rehome_queued_frame(&frame, source, target, runqueue::weight::value(slot));
    drop(frame);
    drop(pin);
    match moved {
        runqueue::RemoteWakeOutcome::Published { cpu, notify } => {
            assert_eq!(cpu, target);
            if cpu == turn.original_current().unwrap().0 { let _ = runqueue::drain_remote_wakes(cpu); }
            #[cfg(not(test))]
            if notify && cpu != turn.original_current().unwrap().0 { crate::multitask::irq::request_target_reschedule(cpu); }
            #[cfg(test)]
            let _ = notify;
            true
        }
        _ => false,
    }
}

pub(super) fn service(turn: &CpuDispatchTurn<'static>, cpu: usize, current: usize, voluntary: bool) {
    assert!(turn.owns_priority_choice(), "maintenance lacks original choice");
    // Dispatcher already received remote mailboxes before priority selection.
    // Balance/validation may be deferred by a first-turn prefix, ingress may not.
    let mut selected = None;
    let idle = runqueue::is_idle_slot(current);
    if idle && !local_dispatch::local_fair_competitor_ready(cpu) {
        for source in 0..MAX_TRACKED_CPUS {
            if source == cpu || runqueue::published_runnable_count(source) == 0 { continue; }
            for slot in runqueue::local_runnable_slots(source) {
                if let Some(key) = candidate(slot, source, cpu) {
                    if selected.is_none_or(|(old, _)| key < old) { selected = Some((key, source)); }
                }
            }
        }
    } else if !idle && !voluntary && runqueue::published_runnable_count(cpu) >= 2 {
        let previous = OPPORTUNITIES[cpu].fetch_add(1, Ordering::Relaxed);
        if runqueue_policy::active_balance_opportunity_due(previous) {
            for slot in runqueue::local_runnable_slots(cpu) {
                if !mobile(slot, cpu) { continue; }
                let (task_mask, process_mask, _) = runqueue::affinity_payload::affinity_snapshot(slot);
                if task_mask & process_mask & !(1_u64 << cpu) == 0 { continue; }
                let target = runqueue::least_loaded_cpu(runqueue_policy::online_affinity_mask(slot), cpu);
                if target == cpu || !runqueue_policy::runqueue_is_imbalanced(runqueue::published_runnable_count(cpu), runqueue::published_runnable_count(target)) { continue; }
                if let Some(key) = candidate(slot, cpu, target) {
                    if selected.is_none_or(|(old, _)| key < old) { selected = Some((key, cpu)); }
                }
            }
        }
    }
    if let Some(((_, _, slot, target), source)) = selected { transfer(turn, slot, source, target); }
    let due = {
        let mut policy = dispatch_policy::lock(cpu);
        let next = policy.ready_validation_turn.saturating_add(1);
        let due = next >= READY_VALIDATION_INTERVAL_TURNS;
        policy.ready_validation_turn = if due { 0 } else { next };
        due
    };
    if due {
        for slot in runqueue::local_runnable_slots(cpu) {
            if slot == current || slot == ROOT_TASK_SLOT || crate::multitask::task_slot_is_running(slot) { continue; }
            let Some(token) = frame_publication::local_frame_token(slot, cpu) else { continue; };
            let Some(id) = current_identity::read_dispatch(slot).and_then(|p| p.task_id) else { continue; };
            let Some(pin) = frame_publication::pin_for_identity(token, id) else { continue; };
            // SAFETY: exact allocation and original masked CPU; no leaf across read.
            let Some(reader) = (unsafe { stack_resources::frame_readers::acquire(&pin, cpu) }) else { continue; };
            let invalid = context_validation::validate_published_saved_context(slot, token.saved_rsp, token.view).err();
            let exact = frame_publication::token_is_current(token, cpu);
            drop(reader);
            drop(pin);
            if exact { if let Some(reason) = invalid { super::quarantine(slot, id, token.saved_rsp, reason); } }
        }
    }
}
