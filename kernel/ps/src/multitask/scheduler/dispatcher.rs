//! Original CPU dispatch transaction, independent of the legacy catalog.
//!
//! - **Owner:** the original masked CpuDispatchTurn, outgoing CurrentReturn,
//!   selected physical DispatchFrame and its original funding/claim receipt.
//! - **Boundary:** priority observation -> exact frame/claim -> interval ->
//!   architecture -> Current/Transition -> assembly ACK (owned by the turn).
//! - **Lifecycle:** one priority choice spans both fast refusal and ordinary
//!   fallback; neither outgoing settlement nor original frame is recaptured.
//! - **Concurrency:** per-CPU choice custody excludes competing activation
//!   publication; bounded owner leaves do not span validation or architecture.
//! - **Failure:** incomplete earlier policy cannot admit a later arm. Rejected
//!   Direct custody is materialized only with the consumed exact generation.
//! - **Forbidden:** catalog selection/claim, scalar interval opening, saved-RSP
//!   validation without original physical pins, metadata as a lifetime grant.
//! - **Evidence:** actual dispatch/priority/claim/return witnesses; native ACK
//!   and architecture are explicitly distinct from hardware execution.
use super::{
    boot_execution, context_validation_reason_code, current_identity, current_return::CurrentReturn,
    dispatch_claim::{self, DispatchFrame, FrameCapture, FrameCustody, FrameSelection, FrameSelectionError}, dispatch_policy,
    donation_ledger, frame_publication, handoff_queue::FirstTurnHandoff, local_dispatch,
    runqueue, runqueue_policy, runtime_profile, smp, stack_resources, sync_handoff, terminal_quarantine,
    SchedulerDispatch, TaskRetireReason, ROOT_TASK_SLOT, MAX_TASK, MAX_LATENCY_HANDOFF_HINTS,
    MAX_CONSECUTIVE_LATENCY_HANDOFFS,
};
use core::sync::atomic::{AtomicU64, Ordering};
use nucleus_core::util::{cache_padded::CachePadded, lockdep::MAX_TRACKED_CPUS};

pub(in crate::multitask::scheduler) mod maintenance;

#[derive(Clone, Copy)]
enum Urgency { Atomic, Sync, Overdue, Spawn, Latency, Hint, Fair }
/// The selected original reader/backing is consumed, never reconstructed from
/// these copied ordering fields. No new execution authority is granted here.
struct Candidate { slot: usize, id: u64, generation: u64, custody: FrameCustody, urgency: Urgency, frame: FrameSelection }
enum Decision { Selected(Candidate), Claimed(Urgency), Empty, Incomplete }

fn selected_candidate(slot: usize, cpu: usize, id: u64, generation: u64, custody: FrameCustody, urgency: Urgency) -> Decision {
    let token = match custody {
        FrameCustody::Local => frame_publication::local_frame_token(slot, cpu),
        FrameCustody::Direct => frame_publication::direct_handoff_frame_token(slot, cpu),
        FrameCustody::Current => frame_publication::current_local_frame_token(slot, cpu),
    };
    let Some(token) = token else { return Decision::Incomplete; };
    // SAFETY: the dispatcher's original masked selecting CPU scope spans this
    // non-Send loan through final claim or refusal; native policy fixtures model it.
    match unsafe { FrameSelection::capture(cpu, slot, id, generation, token.saved_rsp, custody) } {
        Ok(frame) => Decision::Selected(Candidate {
            slot: frame.slot(), id: frame.id(), generation: frame.generation(),
            custody: frame.custody(), urgency, frame,
        }),
        Err(FrameSelectionError::Stale) => Decision::Incomplete,
        Err(FrameSelectionError::Invalid(reason)) => {
            quarantine(slot, id, token.saved_rsp, reason);
            Decision::Empty
        },
    }
}

fn local_candidate(slot: usize, cpu: usize, urgency: Urgency) -> Decision {
    if runqueue::is_idle_slot(slot) && smp::idle_slot_for_cpu(cpu) != Some(slot) { return Decision::Empty; }
    match local_dispatch::published_candidate_admission(slot, cpu, selection_now(cpu)) {
        Ok(true) => match current_identity::read_dispatch(slot).and_then(|p| p.task_id) {
            Some(id) => selected_candidate(slot, cpu, id, runqueue::owner(slot).generation, FrameCustody::Local, urgency),
            None => Decision::Incomplete,
        },
        Ok(false) => Decision::Empty,
        Err(_) => Decision::Incomplete,
    }
}

/// Peek outside Policy; consume only a complete exact front observation. A
/// partially completed Cohort retains its original FIFO place and pending bit.
fn first_turn(turn: &crate::multitask::cpu_local::dispatch_turn::CpuDispatchTurn<'static>, cpu: usize, atomic: bool, class_sequence: u64) -> Decision {
    for _ in 0..MAX_TASK {
        let front = {
            let policy = dispatch_policy::lock(cpu);
            if atomic { policy.atomic_activation_pick_hints.front() } else { policy.spawn_pick_hints.front() }
        };
        let Some(FirstTurnHandoff { slot, task_id }) = front else {
            if atomic {
                let mut policy = dispatch_policy::lock(cpu);
                if policy.atomic_activation_pick_hints.is_empty() {
                    assert_eq!(policy.atomic_activation_handoff_remaining, 0, "prefix count lost its FIFO");
                    dispatch_policy::clear_atomic_activation_pending(cpu);
                    // Keep the scoped guard above mutable only for this actual
                    // drain invariant, never through qualification/claim.
                    let _ = &mut policy;
                }
            }
            return Decision::Empty;
        };
        let mut decision = if current_identity::read_dispatch(slot).is_some_and(|p| p.task_id != Some(task_id) || !p.lifecycle_dispatchable()) {
            Decision::Empty
        } else {
            local_candidate(slot, cpu, if atomic { Urgency::Atomic } else { Urgency::Spawn })
        };
        if let Decision::Selected(candidate) = decision {
            // Retain the SAME original allocation through FIFO consumption and
            // final claim. Busy original Cohort/reader means no pop, no decrement,
            // and no later arm; a scalar generation cannot replace this loan.
            let rsp = candidate.frame.saved_rsp();
            decision = match DispatchFrame::from_selection(turn, candidate.frame, class_sequence) {
                FrameCapture::Ready(frame) => {
                    if frame.prepare().is_some_and(|funded| funded.claim()) { Decision::Claimed(candidate.urgency) }
                    else { Decision::Incomplete }
                },
                FrameCapture::Stale => Decision::Incomplete,
                FrameCapture::Invalid(reason) => { quarantine(slot, task_id, rsp, reason); Decision::Empty },
            };
        }
        if matches!(decision, Decision::Incomplete) { return decision; }
        let mut policy = dispatch_policy::lock(cpu);
        let unchanged = if atomic { policy.atomic_activation_pick_hints.front() } else { policy.spawn_pick_hints.front() } == front;
        if !unchanged {
            // A terminal withdrawal may have consumed the exact front, but it
            // cannot erase THIS already successful physical claim/interval.
            if matches!(decision, Decision::Claimed(_)) { return decision; }
            continue;
        }
        if atomic {
            policy.atomic_activation_pick_hints.pop();
            policy.atomic_activation_handoff_remaining = policy.atomic_activation_handoff_remaining.checked_sub(1).expect("prefix count underflow");
            if policy.atomic_activation_pick_hints.is_empty() {
                assert_eq!(policy.atomic_activation_handoff_remaining, 0);
                dispatch_policy::clear_atomic_activation_pending(cpu);
            }
        } else { policy.spawn_pick_hints.pop(); }
        drop(policy);
        if matches!(decision, Decision::Claimed(_)) { return decision; }
    }
    Decision::Incomplete
}

fn materialize(record: sync_handoff::SyncHandoffRecord, cpu: usize) {
    if let Some(generation) = record.claim_generation() {
        let _ = runqueue::materialize_direct_handoff_with_generation(record.slot(), cpu,
            runqueue::weight::value(record.slot()), Some(generation));
    }
}
fn synchronous(cpu: usize) -> Decision {
    let _phase = super::runtime_profile::phase_scope(super::SchedulerPhase::SelectHandoff);
    if !sync_handoff::pending(cpu) { return Decision::Empty; }
    let mut selected = None;
    let record = sync_handoff::take_next_ready_with(cpu, || local_dispatch::local_fair_competitor_ready(cpu), |record| {
        selected = None; // discard an unconsumed original if the FIFO raced
        let slot = record.slot();
        if !current_identity::read_dispatch(slot).is_some_and(|p| p.task_id == Some(record.task_id()) && p.lifecycle_dispatchable())
            || !record.has_current_dispatch_custody() {
            materialize(record, cpu);
            return false;
        }
        if (runqueue::is_idle_slot(slot) && smp::idle_slot_for_cpu(cpu) != Some(slot))
            || local_dispatch::published_candidate_admission(slot, cpu, selection_now(cpu)) != Ok(true) {
            materialize(record, cpu);
            return false;
        }
        let custody = if record.bound_direct_generation().is_some() { FrameCustody::Direct } else { FrameCustody::Local };
        let generation = record.bound_direct_generation().unwrap_or_else(|| runqueue::owner(slot).generation);
        match selected_candidate(slot, cpu, record.task_id(), generation, custody, Urgency::Sync) {
            Decision::Selected(candidate) => { selected = Some(candidate); true },
            _ => { materialize(record, cpu); false },
        }
    }).map(|record| record.bind_selected_local_owner(runqueue::owner(record.slot()), cpu));
    match record {
        Some(record) => match selected {
            Some(candidate) if record.slot() == candidate.slot && record.task_id() == candidate.id
                && record.claim_generation() == Some(candidate.generation) => Decision::Selected(candidate),
            _ => { materialize(record, cpu); Decision::Incomplete },
        },
        None => Decision::Empty,
    }
}

fn latency(cpu: usize) -> Decision {
    if dispatch_policy::latency_handoff_streak(cpu) >= MAX_CONSECUTIVE_LATENCY_HANDOFFS { return Decision::Empty; }
    for _ in 0..MAX_LATENCY_HANDOFF_HINTS {
        let hint = {
            let mut policy = dispatch_policy::lock(cpu);
            if policy.latency_pick_hint_len == 0 { return Decision::Empty; }
            let head = policy.latency_pick_hint_head;
            let hint = policy.latency_pick_hints[head].take();
            policy.latency_pick_hint_head = (head + 1) % MAX_LATENCY_HANDOFF_HINTS;
            policy.latency_pick_hint_len -= 1;
            hint
        };
        let Some(hint) = hint else { continue; };
        if !dispatch_policy::latency_hint_identity_matches(hint, current_identity::read_dispatch(hint.slot).and_then(|p| p.task_id))
            || !local_dispatch::stable_user_hint_class(hint.slot) { continue; }
        match local_candidate(hint.slot, cpu, Urgency::Latency) {
            Decision::Empty => continue,
            result => return result,
        }
    }
    Decision::Empty
}
/// Native observer drives the SAME consumer but cannot turn a scalar result
/// into a physical/funding claim. Final dispatch still owns DispatchFrame.
#[cfg(test)]
pub(super) fn test_take_latency(cpu: usize) -> Option<usize> {
    match latency(cpu) { Decision::Selected(candidate) => Some(candidate.slot), _ => None }
}
/// Observation of the actual consumer, not a claim or execution capability.
#[cfg(test)]
pub(super) fn test_hint_is_empty(cpu: usize, current: usize) -> bool {
    matches!(hint(cpu, current), Decision::Empty)
}
#[cfg(test)]
pub(super) fn test_hint_refuses_selection(cpu: usize, current: usize) -> bool {
    !matches!(hint(cpu, current), Decision::Selected(_) | Decision::Claimed(_))
}
fn hint(cpu: usize, current: usize) -> Decision {
    let Some(hint) = dispatch_policy::lock(cpu).next_pick_hint.take() else { return Decision::Empty; };
    if !current_identity::read_dispatch(hint.slot).is_some_and(|p| p.task_id == Some(hint.task_id)) {
        return Decision::Empty;
    }
    let slot = hint.slot;
    if local_dispatch::refuse_unproven_hint_priority(local_dispatch::user_hint_blocked_by_system(
        slot, current, cpu, selection_now(cpu))) { return Decision::Empty; }
    local_candidate(slot, cpu, Urgency::Hint)
}
fn fair(cpu: usize, current: usize, voluntary: bool, runtime: u64) -> Result<Option<Candidate>, local_dispatch::LocalFallbackReason> {
    let mut selected = None;
    let result = local_dispatch::choose_local_next_with(cpu, current, voluntary, runtime,
        selection_now(cpu), runqueue::local_runnable_sequence(cpu),
        donation_ledger::class_publication_sequence(), false, false, |slot, cpu, _now| {
            match local_candidate(slot, cpu, Urgency::Fair) {
                Decision::Selected(candidate) => {
                    let generation = candidate.generation;
                    selected = Some(candidate);
                    Ok(Some(generation))
                },
                Decision::Empty => Ok(None),
                _ => Err(local_dispatch::LocalFallbackReason::FrameUnavailable),
            }
        });
    match result {
        Ok((slot, generation)) => {
            let id = current_identity::read_dispatch(slot).and_then(|p| p.task_id).ok_or(local_dispatch::LocalFallbackReason::CustodyUnavailable)?;
            if let Some(candidate) = selected {
                if candidate.slot != slot || candidate.generation != generation {
                    return Err(local_dispatch::LocalFallbackReason::CustodyUnavailable);
                }
                Ok(Some(candidate))
            } else {
                match selected_candidate(slot, cpu, id, generation,
                    if slot == current { FrameCustody::Current } else { FrameCustody::Local }, Urgency::Fair) {
                    Decision::Selected(candidate) => Ok(Some(candidate)),
                    _ => Err(local_dispatch::LocalFallbackReason::FrameUnavailable),
                }
            }
        }
        Err(local_dispatch::LocalFallbackReason::NoLocalCandidate) => Ok(None),
        Err(reason) => Err(reason),
    }
}

fn retry_fair(cpu: usize, current: usize, voluntary: bool, runtime: u64) -> Result<Option<Candidate>, local_dispatch::LocalFallbackReason> {
    local_dispatch::retry_incomplete_policy(|| fair(cpu, current, voluntary, runtime))
}

/// Drives the actual priority chain without converting its observation into
/// execution authority. First-turn queues are empty in these policy fixtures.
#[cfg(test)]
pub(super) fn test_policy_choice(turn: &crate::multitask::cpu_local::dispatch_turn::CpuDispatchTurn<'static>, ticks: u64) -> Option<usize> {
    let (cpu, current) = turn.original_current().unwrap();
    match choose(turn, cpu, current, false, super::SCHED_MAX_BURST_NS, ticks,
        donation_ledger::class_publication_sequence()) {
        Decision::Selected(candidate) => Some(candidate.slot),
        Decision::Empty => None,
        Decision::Incomplete => panic!("actual policy observation incomplete"),
        Decision::Claimed(_) => panic!("policy fixture unexpectedly contained a physical first turn"),
    }
}

/// One monotonic sample per selection turn. Each priority arm used to read
/// the clock (an `lfence; rdtsc`) for its own eligibility check, three or four
/// times per dispatch; within one turn they now share the turn's sample.
/// Outside `choose` (direct callers and native fixtures) the clock is read.
static SELECTION_NOW: [CachePadded<AtomicU64>; MAX_TRACKED_CPUS] =
    [const { CachePadded::new(AtomicU64::new(0)) }; MAX_TRACKED_CPUS];

fn selection_now(cpu: usize) -> u64 {
    // ORDERING: Relaxed; a CPU-private value written and read by this turn.
    match SELECTION_NOW.get(cpu).map_or(0, |now| now.load(Ordering::Relaxed)) {
        0 => crate::arch::clock::monotonic_nanos(),
        now => now,
    }
}

struct SelectionTurn(usize);

impl SelectionTurn {
    fn begin(cpu: usize) -> Self {
        if let Some(now) = SELECTION_NOW.get(cpu) {
            // ORDERING: Relaxed; see `selection_now`.
            now.store(crate::arch::clock::monotonic_nanos().max(1), Ordering::Relaxed);
        }
        Self(cpu)
    }
}

impl Drop for SelectionTurn {
    fn drop(&mut self) {
        if let Some(now) = SELECTION_NOW.get(self.0) {
            // ORDERING: Relaxed; see `selection_now`.
            now.store(0, Ordering::Relaxed);
        }
    }
}

fn choose(turn: &crate::multitask::cpu_local::dispatch_turn::CpuDispatchTurn<'static>, cpu: usize, current: usize, voluntary: bool, runtime: u64, ticks: u64, class_sequence: u64) -> Decision {
    if class_sequence & 1 != 0 { return Decision::Incomplete; }
    let _turn_clock = SelectionTurn::begin(cpu);
    // Receive the original generation-bound mailbox before any priority arm
    // can return Incomplete. A remote activation prefix is not a Local frame
    // yet; putting this behind that prefix strands its own admission forever.
    let _ = runqueue::drain_remote_wakes(cpu);
    if dispatch_policy::atomic_activation_pending(cpu) {
        match first_turn(turn, cpu, true, class_sequence) { Decision::Empty => {}, result => return result }
    }
    match synchronous(cpu) { Decision::Empty => {}, result => return result }
    maintenance::service(turn, cpu, current, voluntary);
    let overdue = local_dispatch::retry_incomplete_policy(|| local_dispatch::choose_overdue_system(
        cpu, current, ticks, selection_now(cpu),
        runqueue::local_runnable_sequence(cpu), donation_ledger::class_publication_sequence()));
    match overdue {
        Ok(Some((slot, generation))) => {
            let Some(id) = current_identity::read_dispatch(slot).and_then(|p| p.task_id) else { return Decision::Incomplete; };
            return selected_candidate(slot, cpu, id, generation, FrameCustody::Local, Urgency::Overdue);
        }
        Err(_) => return Decision::Incomplete,
        Ok(None) => {},
    }
    match first_turn(turn, cpu, false, class_sequence) { Decision::Empty => {}, result => return result }
    match latency(cpu) { Decision::Empty => {}, result => return result }
    if runqueue::wait::blocked(current) {
        match hint(cpu, current) { Decision::Empty => {}, result => return result }
    }
    if local_dispatch::user_reservation_due(cpu) {
        let class_sequence = donation_ledger::class_publication_sequence();
        if class_sequence & 1 != 0 { return Decision::Incomplete; }
        match retry_fair(cpu, current, voluntary, runtime) {
            Ok(Some(candidate)) => {
                let class = local_dispatch::observed_class(candidate.slot);
                if donation_ledger::class_publication_sequence() != class_sequence {
                    return Decision::Incomplete;
                }
                match class {
                    Ok(local_dispatch::LocalClass::User) => return Decision::Selected(candidate),
                    Ok(local_dispatch::LocalClass::System) => {},
                    Err(_) => return Decision::Incomplete,
                }
            }
            Err(_) => return Decision::Incomplete,
            Ok(None) => {},
        }
    }
    match hint(cpu, current) { Decision::Empty => {}, result => return result }
    match retry_fair(cpu, current, voluntary, runtime) {
        Ok(Some(candidate)) => Decision::Selected(candidate),
        Ok(None) => Decision::Empty,
        Err(_) => Decision::Incomplete,
    }
}

fn quarantine(slot: usize, id: u64, rsp: usize, reason: &'static str) {
    if slot == ROOT_TASK_SLOT { panic!("scheduler root context is corrupted: {reason}"); }
    terminal_quarantine::request(slot, id, TaskRetireReason::CorruptedContext {
        saved_rsp: rsp, reason, reason_code: context_validation_reason_code(reason),
    });
}
fn prologue(outgoing: &mut CurrentReturn<'_>, rsp: usize, voluntary: bool) -> u64 {
    let _phase = super::runtime_profile::phase_scope(super::SchedulerPhase::Account);
    let slot = outgoing.slot();
    let runtime = outgoing.account(crate::arch::rtc::ticks(), voluntary);
    let ready = current_identity::read_dispatch(slot).is_some_and(|p| p.lifecycle_dispatchable()) && !runqueue::wait::blocked(slot);
    if ready {
        if let Some(reason) = outgoing.validate(rsp) {
            outgoing.publish_frame(rsp, false);
            quarantine(slot, outgoing.id(), rsp, reason);
        } else { outgoing.publish_frame(rsp, true); }
    } else { outgoing.publish_frame(rsp, false); }
    let (cpu, _) = outgoing.original_turn().original_current().expect("outgoing lost original Current");
    let (task_mask, process_mask, _) = runqueue::affinity_payload::affinity_snapshot(slot);
    let eligible = task_mask & process_mask;
    let local = eligible & (1_u64 << cpu) != 0;
    if matches!(outgoing.publish_queue(local), runqueue::ReturnPublication::Blocked { was_runnable: true }) && !local {
        let target = runqueue_policy::affinity_target_cpu(slot, eligible);
        let wake = if target == cpu { runqueue::publish_local_wake(slot, target, runqueue::weight::value(slot)) }
            else { runqueue::publish_remote_wake(slot, target, runqueue::weight::value(slot)) };
        assert!(!matches!(wake, runqueue::RemoteWakeOutcome::Rejected) || runqueue::owner(slot).state.is_terminal(), "original Current could not rehome");
        #[cfg(not(test))]
        if let runqueue::RemoteWakeOutcome::Published { cpu, notify: true } = wake { crate::multitask::irq::request_target_reschedule(cpu); }
    }
    runtime
}
fn claim(outgoing: &CurrentReturn<'_>, candidate: Candidate, class_sequence: u64) -> bool {
    let cpu = outgoing.original_turn().original_current().expect("claim lost original Current").0;
    let rsp = candidate.frame.saved_rsp();
    let success = match DispatchFrame::from_selection(outgoing.original_turn(), candidate.frame, class_sequence) {
        FrameCapture::Ready(frame) => frame.prepare().is_some_and(|funded| funded.claim()),
        FrameCapture::Invalid(reason) => { quarantine(candidate.slot, candidate.id, rsp, reason); false },
        FrameCapture::Stale => false,
    };
    if !success && matches!(candidate.custody, FrameCustody::Direct) {
        let _ = runqueue::materialize_direct_handoff_with_generation(candidate.slot, cpu,
            runqueue::weight::value(candidate.slot), Some(candidate.generation));
    }
    success
}
fn complete(outgoing: &CurrentReturn<'_>, urgency: Urgency) -> SchedulerDispatch {
    let turn = outgoing.original_turn();
    let (cpu, previous) = turn.original_current().expect("completion lost original Current");
    assert!(turn.owns_priority_choice(), "completion lacks original priority choice");
    let receipt = turn.claimed_return().expect("completion lacks original physical claim");
    let next = receipt.slot();
    dispatch_policy::discharge_claimed_first_turn(turn);
    runqueue::wait::set_ready_since_ticks(next, 0);
    dispatch_policy::record_completed_choice(cpu,
        !runqueue::is_idle_slot(next) && !local_dispatch::is_user_class(next),
        matches!(urgency, Urgency::Latency));
    sync_handoff::record_dispatch(cpu, matches!(urgency, Urgency::Sync), crate::arch::rtc::ticks());
    // The SAME successful claim retains Prepared physical/UID/placement banks.
    // Process execution additionally owns an activation pin; a pure kernel
    // executor has no process MM to activate and retains its exact kernel image.
    let incoming = stack_resources::context::retained_architecture(cpu, next, receipt.id, receipt.token.view)
        .expect("dispatch lost original claimed physical image");
    let root = if incoming.process {
        turn.claimed_mm_root(next, receipt.id).expect("dispatch lost original claimed MM")
    } else { incoming.root };
    assert_eq!(root, incoming.root, "claimed process activation differs from retained image");
    runtime_profile::owner::local(|row| {
        row.dispatch(next);
        if previous == next { row.runtime_profile_same_task_dispatches = row.runtime_profile_same_task_dispatches.saturating_add(1); }
        else {
            row.runtime_profile_task_switches = row.runtime_profile_task_switches.saturating_add(1);
            if receipt.previous.root != root { row.runtime_profile_address_space_switches = row.runtime_profile_address_space_switches.saturating_add(1); }
        }
    });
    if runtime_profile::owner::migration(next, receipt.id, cpu as u8) {
        runtime_profile::owner::local(|row| row.runtime_profile_cross_cpu_migrations = row.runtime_profile_cross_cpu_migrations.saturating_add(1));
    }
    runqueue::affinity_payload::record_last_cpu(next, cpu as u8);
    crate::multitask::cpu_local::set_scheduler_current_task_scratch_for_cpu(cpu, next);
    nucleus_core::util::lockdep::record_scheduler_dispatch(cpu, outgoing.id(), receipt.id, previous, next,
        receipt.token.saved_rsp, runqueue::is_idle_slot(next), matches!(urgency, Urgency::Atomic));
    nucleus_core::util::lockdep::set_current_task_owner(receipt.id.checked_add(1).expect("task lock owner exhausted"));
    SchedulerDispatch::new(receipt.token.saved_rsp, boot_execution::tick_divisor(), previous, next)
}

pub(in crate::multitask) fn dispatch(outgoing: &mut CurrentReturn<'_>, rsp: usize, voluntary: bool) -> SchedulerDispatch {
    let turn = outgoing.original_turn();
    if !turn.owns_priority_choice() { turn.begin_priority_choice(); }
    let runtime = prologue(outgoing, rsp, voluntary);
    let cpu = turn.original_current().expect("dispatcher lost original turn").0;
    // Capture BEFORE any policy arm, and carry this SAME observation into the
    // original physical claim; later class publication invalidates the choice.
    let class_sequence = donation_ledger::class_publication_sequence();
    match choose(turn, cpu, outgoing.slot(), voluntary, runtime, crate::arch::rtc::ticks(), class_sequence) {
        Decision::Selected(candidate) => {
            let urgency = candidate.urgency;
            if claim(outgoing, candidate, class_sequence) { return complete(outgoing, urgency); }
        }
        Decision::Claimed(urgency) => return complete(outgoing, urgency),
        Decision::Empty | Decision::Incomplete => {},
    }
    let idle = smp::idle_slot_for_cpu(cpu).expect("dispatcher lacks fixed CPU idle");
    dispatch_claim::claim_fallback(turn, idle, class_sequence);
    complete(outgoing, Urgency::Fair)
}

pub(in crate::multitask) fn fast(outgoing: &mut CurrentReturn<'_>, rsp: usize, immediate: Option<u64>) -> Option<SchedulerDispatch> {
    let turn = outgoing.original_turn();
    if !turn.owns_priority_choice() { turn.begin_priority_choice(); }
    let (cpu, current) = turn.original_current()?;
    if immediate.is_none() || !runqueue::wait::blocked(current) || dispatch_policy::atomic_activation_pending(cpu) { return None; }
    prologue(outgoing, rsp, true);
    let class_sequence = donation_ledger::class_publication_sequence();
    if class_sequence & 1 != 0 { return None; }
    match synchronous(cpu) {
        Decision::Selected(candidate) => {
            // FIFO order is authoritative, not the caller's scalar immediate hint.
            if claim(outgoing, candidate, class_sequence) { Some(complete(outgoing, Urgency::Sync)) } else { None }
        }
        _ => None,
    }
}

pub(in crate::multitask) fn selected_is_idle(slot: usize) -> bool { runqueue::is_idle_slot(slot) }

#[cfg(test)]
pub(super) mod tests;
#[cfg(test)]
pub(super) mod fifo_witnesses;
