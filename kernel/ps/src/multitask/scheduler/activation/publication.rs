//! Owner: complete activation of original physical Cohort members, not a facade.
//! Boundary: own Current bank plus exact target context, frame bank/readers and policy leaves.
//! Lifecycle: capture -> all-frame preflight -> external authority -> effects -> queues -> one ACK.
//! Concurrency: no raw leaf spans callback; readers exclude byte writes/claims through publication.
//! Failure: preflight refuses without consuming authority; terminal overlap skips, effects poison.
//! Forbidden: ID rescans, telemetry or backing loans as execution grants, global serializer.
//! Evidence: shared actual constructor/queue witnesses; modeled Current is not hardware return proof.
use super::super::*;
use super::{Activation, CAP};
use crate::multitask::{cpu_local, current_identity as identity};
use nucleus_core::util::lockdep::DispatchPreemption;

fn caller(turn: &DispatchPreemption, original: usize) {
    assert_eq!(turn.cpu(), nucleus_core::util::lockdep::current_cpu_index());
    assert_eq!(cpu_local::current_cpu_task_slot(), Some(original));
    assert!(stack_resources::cpu_leases::owns_current(
        turn.cpu(),
        original
    ));
}

/// Non-Copy Cohort and physical frame loans survive the callback and every
/// queue/policy effect. FrameReader holds a count, never a raw Resources leaf.
pub(super) fn commit(
    mut owner: Activation,
    turn: &DispatchPreemption,
    eligible_cpus: impl Fn(usize) -> u64,
    commit_authority: impl FnOnce(),
) -> bool {
    let current = super::super::current_terminal::OriginalCurrent::borrow(turn)
        .expect("activation lacks original Current banks/interval");
    let original = current.member().0;
    caller(turn, original);
    let members = &owner.members[..owner.len];
    let mut pins = [const { None }; CAP];
    let mut tokens = [None; CAP];
    for (i, &(slot, id)) in members.iter().enumerate() {
        // Activation::capture sealed this immutable member list and acquired
        // its original Cohort pins. owns_member is deliberately an effect-only
        // grant and must not be requested before begin_effects below.
        let lifecycle = identity::lifecycle(slot);
        if lifecycle.retired || !lifecycle.suspended {
            return false;
        }
        assert!(
            slot != original && !crate::multitask::task_slot_is_running(slot),
            "scheduler activation invariant: suspended task {id} is already running"
        );
        let context = stack_resources::context::read(slot)
            .expect("scheduler activation invariant: suspended task lost its context");
        assert!(
            context.user_mode
                && context.scheduling_context.is_bound_to(id)
                && stack_resources::task_start::read(slot).map(|s| s.id) == Some(id),
            "activation changed original physical member"
        );
        let Some(token) = frame_publication::observed_frame_token(slot, turn.cpu(), false) else {
            return false;
        };
        let Some(pin) = frame_publication::pin_for_identity(token, id) else {
            return false;
        };
        pins[i] = Some(pin);
        tokens[i] = Some(token);
    }
    let mut readers = [const { None }; CAP];
    for (i, (&(slot, id), pin)) in members.iter().zip(pins.iter()).enumerate() {
        let token = tokens[i].unwrap();
        // SAFETY: borrowed own-CPU IRQ/preemption scope, exact original binding
        // and physical bank exclude byte reuse; readers additionally exclude claims.
        let Some(reader) =
            (unsafe { stack_resources::frame_readers::acquire(pin.as_ref().unwrap(), turn.cpu()) })
        else {
            return false;
        };
        let current = frame_publication::observed_frame_token(slot, turn.cpu(), false);
        if current.is_none_or(|current| {
            current.frame_epoch != token.frame_epoch
                || current.owner_generation != token.owner_generation
                || current.saved_rsp != token.saved_rsp
        }) {
            return false;
        }
        if let Err(reason) =
            context_validation::validate_published_saved_context(slot, token.saved_rsp, token.view)
        {
            report_invalid_frame(id, slot, token.saved_rsp, token.view, &reader);
            panic!(
                "scheduler activation invariant: suspended task {id} has invalid context: {reason}"
            );
        }
        readers[i] = Some(reader);
    }
    // No raw owner leaf is held. These real loans exclude image writes/reclaim
    // and fresh execution, rather than relying on a copied preflight result.
    commit_authority();
    current.check();
    caller(turn, original);
    for &(slot, id) in members {
        assert!(
            identity::lifecycle(slot).suspended,
            "scheduler activation invariant: authority commit changed suspension before cohort publication for task {id}"
        );
        assert!(
            slot != original && !crate::multitask::task_slot_is_running(slot),
            "scheduler activation invariant: authority commit made task {id} runnable before cohort publication"
        );
    }
    owner.cohort.begin_effects();
    let prioritize = members.len() > 1
        && (0..nucleus_core::util::lockdep::MAX_TRACKED_CPUS).all(|cpu| {
            let policy = dispatch_policy::lock(cpu);
            policy.atomic_activation_pick_hints.is_empty()
                && policy.atomic_activation_handoff_remaining == 0
        });
    if members.len() > 1 && !prioritize {
        crate::debug::record_milestone(
            crate::debug::LogCategory::Sched,
            "activation-cohort-priority-degraded",
            members[0].1,
            members.len() as u64,
        );
    }
    for &(slot, id) in members {
        assert!(
            owner.cohort.unsuspend(slot, id),
            "activation lost exact cohort member"
        );
        let lifecycle = identity::lifecycle(slot);
        if lifecycle.retired {
            continue;
        }
        let system = runqueue::weight::value(slot) & SYSTEM_CLASS_WEIGHT_FLAG != 0
            || donation_ledger::inherited_system(slot);
        let floor = kernel_creation::ready_floor(turn.cpu(), Some(system));
        runqueue::raise_vruntime_floor(slot, floor.saturating_sub(SLEEPER_LATENCY_BONUS_NS));
        runqueue::wait::set_blocked(slot, false);
        runqueue::wait::set_ready_since_ticks(slot, crate::arch::rtc::ticks());
        runqueue::wait::set_blocked_since_ticks(slot, 0);
        runqueue::wait::set_armed(slot, false);
        runqueue::wait::set_reason(slot, 0, 0);
        if lifecycle.job_stopped || lifecycle.exec_quiesced {
            continue;
        }
        let target = runqueue_policy::affinity_target_cpu(slot, eligible_cpus(slot));
        let weight = runqueue::weight::value(slot);
        let wake = if target == turn.cpu() {
            runqueue::publish_local_wake(slot, target, weight)
        } else {
            runqueue::publish_remote_wake(slot, target, weight)
        };
        if identity::lifecycle(slot).retired {
            continue;
        }
        assert!(
            !matches!(wake, runqueue::RemoteWakeOutcome::Rejected)
                && runqueue::owner(slot).runnable,
            "scheduler activation invariant: preflighted task {id} could not wake"
        );
        #[cfg(not(test))]
        if let runqueue::RemoteWakeOutcome::Published { cpu, notify: true } = wake {
            crate::multitask::irq::request_target_reschedule(cpu);
        }
        let record = handoff_queue::FirstTurnHandoff { slot, task_id: id };
        let prioritized = if prioritize {
            // This original producer waits OUTSIDE all leaves, retaining its
            // Cohort/frame loans. A prior receiver choice must publish Current
            // before this new prefix can become visible on that CPU.
            let _publication = super::super::dispatch_priority::Publication::begin(turn, target);
            let mut policy = dispatch_policy::lock(target);
            let inserted = super::enqueue_priority(&mut policy, record);
            if inserted {
                dispatch_policy::publish_atomic_activation_pending(target);
            }
            inserted
        } else {
            false
        };
        if !prioritized {
            let inserted = dispatch_policy::lock(target)
                .spawn_pick_hints
                .enqueue(record)
                .expect("scheduler spawn handoff queue overflow");
            if inserted && target == turn.cpu() {
                donate_floor(slot, &current, turn.cpu(), system);
            }
        }
    }
    // Exactly one batch Release admits all members. Readers still prevent any
    // receiver from claiming frames until the complete producer scope drains.
    owner.cohort.complete();
    caller(turn, original);
    drop(readers);
    drop(pins);
    current.check();
    true
}

fn donate_floor(slot: usize, current: &super::super::current_terminal::OriginalCurrent<'_, '_>, cpu: usize, system: bool) {
    let original = current.member().0;
    if slot == original
        || runqueue::is_idle_slot(slot)
        || runqueue::is_idle_slot(original)
        || !runqueue::is_current_cpu_dispatchable(slot, cpu)
        || local_dispatch::published_candidate_admission(
            slot,
            cpu,
            crate::arch::clock::monotonic_nanos(),
        ) != Ok(true)
    {
        return;
    }
    // Match the ordinary spawn donation policy: recompute the strict-band
    // floor AFTER the new ready member joins, not the earlier sleeper floor.
    let class_floor = kernel_creation::ready_floor(cpu, Some(system));
    let caller_floor = runqueue::vruntime(original).saturating_sub(IPC_DONATION_BONUS_NS);
    let floor = caller_floor.min(class_floor.saturating_sub(IPC_DONATION_BONUS_NS));
    runqueue::lower_vruntime_ceiling(slot, floor);
    current.check();
}

/// Preserve rejection milestones under the already qualified original reader;
/// malformed geometry is reported as absent data, never dereferenced.
fn report_invalid_frame(
    id: u64,
    slot: usize,
    saved_rsp: usize,
    view: context_validation::PublishedFrameView,
    _reader: &stack_resources::frame_readers::FrameReader<'_>,
) {
    let record =
        |name, a, b| crate::debug::record_milestone(crate::debug::LogCategory::Sched, name, a, b);
    record(
        "sched-activation-invalid-slot",
        (id << 32) | slot as u64,
        saved_rsp as u64,
    );
    record(
        "sched-activation-invalid-stack",
        view.kernel_stack_base,
        view.kernel_stack_top,
    );
    let snapshot = saved_rsp
        .checked_add(SAVED_CONTEXT_BYTES)
        .filter(|&end| view.context_stack_contains(saved_rsp, end))
        .and_then(|_| {
            // SAFETY: complete initialized bounds checked above; original pin,
            // frame currency and byte stability remain held by the caller's reader.
            unsafe { context_validation::copy_saved_context(saved_rsp) }
        })
        .map(frame_observation::FrameSnapshot::from_saved);
    let (flags, cs, rip, rsp, zero) = snapshot.map_or((0, 0, 0, 0, false), |s| {
        (
            s.saved.rflags,
            s.saved.cs,
            s.saved.rip,
            s.saved.rsp,
            s.all_zero,
        )
    });
    record(
        "sched-activation-invalid-frame",
        flags,
        (cs << 32) | (rip & u64::from(u32::MAX)),
    );
    record("sched-activation-invalid-frame-rsp", rsp, u64::from(zero));
}

#[cfg(test)]
pub(in crate::multitask::scheduler) mod tests;
