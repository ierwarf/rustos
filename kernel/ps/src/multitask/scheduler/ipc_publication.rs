//! Owner: original Current sender and physical donor/root/receiver loans.
//! Boundary: masked CPU -> binding/frame loans -> Identity51/Affinity54/FIFO56/Wait58.
//! Lifecycle: reserve funding -> exact typed receiver -> owner/clear/FIFO join ->
//! mailbox -> sender commit; every refusal before the join preserves its wait.
//! Concurrency: no raw leaf spans frame bytes, funding, mailbox or notification.
//! Failure: raced wait/owner refuses without rollback; original sender WakeWon
//! after peer publication remains runnable. Corruption transfers a stop producer.
//! Forbidden: directory authority, scalar re-resolution as lifetime, fresh-CAS retries.
//! Evidence: native original-resource witnesses; IRQ/RSP/notifications are modeled.
use super::*;
use nucleus_core::util::lockdep::DispatchPreemption;

pub(super) fn scoped(scope: &DispatchPreemption<'_>) -> bool {
    scope.is_masked() && scope.cpu() == nucleus_core::util::lockdep::current_cpu_index()
        && nucleus_core::util::lockdep::held_spin_lock_depth() == 0
}

/// Cold enqueue does NOT arm or commit a sender wait. Its original executing
/// bank is borrowed while the exact receiver pending phase admits caller
/// funding, publishes queue custody and finally the bounded FIFO preference.
pub(in crate::multitask) fn call_before_arm(
    scope: &DispatchPreemption<'_>, endpoint: u64, reply: u64,
    donor_id: u64, receiver_id: u64,
) -> Result<IpcCallHandoffOutcome, FastIpcCallHandoffOutcome> {
    let result = call_before_arm_with(scope, endpoint, reply, donor_id, receiver_id,
        &runqueue_policy::online_affinity_mask, || {});
    #[cfg(rustos_scheduler_phase_profile)]
    locality::record_sync_handoff_arm(locality::SyncHandoffArmSite::Call,
        result.as_ref().is_ok_and(|outcome| outcome.hinted));
    result
}

fn call_before_arm_with(
    scope: &DispatchPreemption<'_>, endpoint: u64, reply: u64,
    donor_id: u64, receiver_id: u64, eligible_cpus: &dyn Fn(usize) -> u64,
    last_effect: impl FnOnce(),
) -> Result<IpcCallHandoffOutcome, FastIpcCallHandoffOutcome> {
    call_before_arm_observing(scope, endpoint, reply, donor_id, receiver_id,
        eligible_cpus, || {}, last_effect)
}

fn call_before_arm_observing(
    scope: &DispatchPreemption<'_>, endpoint: u64, reply: u64,
    donor_id: u64, receiver_id: u64, eligible_cpus: &dyn Fn(usize) -> u64,
    before_receive: impl FnOnce(), last_effect: impl FnOnce(),
) -> Result<IpcCallHandoffOutcome, FastIpcCallHandoffOutcome> {
    use FastIpcCallHandoffOutcome as Failure;
    let mut last_effect = Some(last_effect);
    if !scoped(scope) || endpoint == 0 || reply == 0 || donor_id == receiver_id {
        return Err(Failure::SenderMismatch);
    }
    // SAFETY: this original masked CPU survives all physical and funding loans.
    let sender = unsafe { current_wait::CurrentWait::capture() }
        .ok_or(Failure::SenderMismatch)?;
    if sender.id() != donor_id || runqueue::wait::armed(sender.slot())
        || runqueue::wait::blocked(sender.slot()) {
        return Err(Failure::SenderMismatch);
    }
    let mut donor = Some(donation_publication::participant(sender.id())
        .ok_or(Failure::DonationUnavailable)?);
    if donation_ledger::reply_already_dispatched(sender.original_pin(), reply) {
        last_effect.take().unwrap()();
        return Ok(IpcCallHandoffOutcome { inherited: false, woke: false, hinted: false });
    }
    let receiver = donation_publication::participant(receiver_id)
        .and_then(wake_publication::Binding::from_original);
    if receiver.is_none() && donation_ledger::reply_already_dispatched(sender.original_pin(), reply) {
        last_effect.take().unwrap()();
        return Ok(IpcCallHandoffOutcome { inherited: false, woke: false, hinted: false });
    }
    let receiver = receiver.ok_or(Failure::ReceiverMismatch)?;
    if runqueue::wait::TypedWake::receive_was_withdrawn(&receiver)
        && donation_ledger::retains_awaiting_reply(donor.as_ref().unwrap(), reply) {
        // An earlier timer/ordinary wake already withdrew this exact receive
        // phase, but runtime has not yet removed its endpoint waiter. Keep the
        // original published message/custody queued. Only an actual later recv
        // may bind its worker: no generic wake, funding transfer, FIFO or frame
        // claim is authorized by this negative observation.
        last_effect.take().unwrap()();
        return Ok(IpcCallHandoffOutcome { inherited: false, woke: false, hinted: false });
    }
    before_receive();
    let mut loans = None;
    let mut hinted = false;
    // SAFETY: the sole wake engine holds ONE original typed pending phase from
    // before bind through frame/queue/FIFO. No bare-ID deferred wake or early
    // sender block is allowed; terminal work cannot drain our own donor loans.
    let result = unsafe { wake_publication::receive_call(receiver, endpoint,
        eligible_cpus, |binding| {
            loans = donation_publication::bind_original(scope, reply,
                donor.take().unwrap(), binding.original_pin(), false);
            loans.is_some()
        }, |binding| {
            let owner = runqueue::owner(binding.slot());
            if runqueue::is_handoff_dispatchable_owner(owner) {
                if let Some(target) = owner.cpu {
                    let _publication = dispatch_priority::Publication::begin(scope, target);
                    hinted = sync_handoff::publish_original(target,
                        sync_handoff::SyncHandoffRecord::new(binding.slot(), binding.id()),
                        || Some(runqueue::owner(binding.slot()))).is_some();
                }
            }
            // All original sender/donor/root/receiver and typed loans survive
            // this last effect, including successful wake with a full FIFO.
            last_effect.take().unwrap()();
            // Full FIFO may refuse urgency, never undo a successful typed wake.
            true
        }) };
    let inherited = loans.is_some();
    let woke = result.is_ok();
    if !woke && !inherited && donation_ledger::reply_already_dispatched(sender.original_pin(), reply) {
        // Only an independent receive may explain this refused typed phase.
        // Our own before_wake bind precedes fallible frame/queue admission and
        // therefore cannot certify delivery after that admission refused.
        // Leave an independently bound worker's newer wait phase untouched;
        // otherwise the sender must cancel the exact published request/custody.
        if let Some(last_effect) = last_effect.take() { last_effect(); }
        return Ok(IpcCallHandoffOutcome { inherited: false, woke: false, hinted: false });
    }
    drop(result);
    drop(loans);
    drop(donor);
    drop(sender);
    if !inherited { return Err(Failure::DonationUnavailable); }
    if !woke { return Err(Failure::ReceiverMismatch); }
    Ok(IpcCallHandoffOutcome { inherited, woke, hinted })
}

pub(super) fn eligible(binding: &wake_publication::Binding, cpu: usize) -> bool {
    let slot = binding.slot();
    if slot == ROOT_TASK_SLOT && cpu != 0 || runqueue::is_idle_slot(slot) {
        return false;
    }
    let (task, process, _) = runqueue::affinity_payload::affinity_snapshot(slot);
    let bit = u32::try_from(cpu).ok().and_then(|cpu| 1_u64.checked_shl(cpu)).unwrap_or(0);
    current_identity::read_dispatch(slot).is_some_and(|p| {
        p.task_id == Some(binding.id()) && p.lifecycle_dispatchable()
            && task & process & bit != 0
            && local_dispatch::effective_charge_is_eligible(slot, p.pager_charge, cpu,
                crate::arch::clock::monotonic_nanos())
    })
}

/// Common exact typed receiver producer. The ORIGINAL receiver reader, pending
/// wait phase and publication custody survive validation, the leaf join, exact
/// frame generation publication, remote mailbox and final notification, and
/// also enclose the caller's last effect. This is not endpoint or
/// donation-ledger admission.
pub(super) fn publish_with(
    scope: &DispatchPreemption<'_>,
    binding: &wake_publication::Binding,
    typed: &runqueue::wait::TypedWake<'_>,
    target: usize,
    last_effect: impl FnOnce(),
) -> Result<(), FastIpcCallHandoffOutcome> {
    publish_switching(scope, binding, typed, target, last_effect, None)
}

/// A same-CPU handoff may skip the FIFO record only when selection would take
/// it first anyway: nothing older is queued for this CPU, no activation prefix
/// outranks it, and the bounded chain does not owe a fair competitor a turn.
/// All of this is CPU-private state of the SAME masked scheduler transaction.
pub(super) fn direct_switch_admitted(cpu: usize) -> bool {
    !sync_handoff::pending(cpu) && !dispatch_policy::atomic_activation_pending(cpu)
        && !(sync_handoff::chain::chain_exhausted(cpu) && local_dispatch::local_fair_competitor_ready(cpu))
}

/// `publish_with` that may hand its receiver reader to the dispatcher instead
/// of a FIFO record: a same-CPU receiver is committed DirectHandoff and the
/// still-held reader, not a re-captured one, becomes its selected frame.
/// Nothing fails after the owner CAS: if the switch cannot be built, the
/// ordinary FIFO record is published for it.
pub(super) fn publish_switching(
    scope: &DispatchPreemption<'_>,
    binding: &wake_publication::Binding,
    typed: &runqueue::wait::TypedWake<'_>,
    target: usize,
    last_effect: impl FnOnce(),
    switch: Option<&mut Option<dispatch_claim::DirectSwitch>>,
) -> Result<(), FastIpcCallHandoffOutcome> {
    use FastIpcCallHandoffOutcome as Outcome;
    let cpu = scope.cpu();
    let slot = binding.slot();
    let expected = runqueue::owner(slot);
    if expected.state != runqueue::RunOwnerState::Blocked || expected.wait_armed {
        return Err(Outcome::DirectCustodyUnavailable);
    }
    let token = frame_publication::observed_frame_token(slot, cpu, false)
        .filter(|token| token.owner_generation == expected.generation)
        .ok_or(Outcome::DirectCustodyUnavailable)?;
    // SAFETY: SAME original masked CPU and owned physical receiver pin remain
    // live through all effects. Actual reader excludes claims, not the token.
    let reader = unsafe { frame_publication::read_for_identity(token, binding.id(), cpu) }
        .ok_or(Outcome::DirectCustodyUnavailable)?;
    if frame_publication::observed_frame_token(slot, cpu, false).is_none_or(|now| {
        now.owner_generation != token.owner_generation || now.frame_epoch != token.frame_epoch
            || now.saved_rsp != token.saved_rsp
    }) {
        return Err(Outcome::DirectCustodyUnavailable);
    }
    let (validation, return_to_user) = dispatch_claim::validate_held_frame(
        slot, token.saved_rsp, token.view, &reader);
    if let Err(reason) = validation {
        let stop = terminal_quarantine::begin(slot, Some(binding.id()),
            TaskRetireReason::CorruptedContext { saved_rsp: token.saved_rsp, reason,
                reason_code: context_validation_reason_code(reason) }, false);
        // Never synchronously drain while our sender/donor/root/receiver loans
        // remain live. Transfer the SAME producer to durable quarantine instead.
        drop(reader);
        if let Some(stop) = stop { terminal_quarantine::publish(stop); }
        return Err(Outcome::DirectCustodyUnavailable);
    }
    let local = target == cpu;
    let fused = local && switch.is_some() && direct_switch_admitted(cpu);
    // Priority publication waits OUTSIDE every raw leaf, retaining this SAME
    // original CPU scope and reader. A choice already in flight publishes its
    // Current/Transition pair before this FIFO may become visible. A fused
    // switch makes nothing visible to any dispatcher but this CPU's own.
    let publication = (!fused).then(|| dispatch_priority::Publication::begin(scope, target));
    let original = current_identity::with_ipc_handoff_binding(slot, binding.id(), || {
        runqueue::affinity_payload::with_placement_admission(slot, Some(binding.id()), target, || {
            if fused {
                return typed.publish_and_clear(|| {
                    runqueue::claim_blocked_handoff(slot, expected, target, local)
                });
            }
            sync_handoff::publish_original(target,
                sync_handoff::SyncHandoffRecord::new(slot, binding.id()), || {
                    typed.publish_and_clear(|| {
                        runqueue::claim_blocked_handoff(slot, expected, target, local)
                    })
                })
        }).and_then(|(result, _grant)| result)
    }).flatten().ok_or(Outcome::DirectCustodyUnavailable)?;
    // All leaves have ended. The SAME reader remains alive until the exact
    // generation and remote record are visible; no dispatcher can claim early.
    frame_publication::bind_local_owner_generation(slot, original.generation);
    let notify = !local && runqueue::publish_handoff_mailbox(slot, original,
        runqueue::weight::value(slot));
    runqueue::wait::set_ready_since_ticks(slot, crate::arch::rtc::ticks().max(1));
    runqueue::wait::set_blocked_since_ticks(slot, 0);
    last_effect();
    drop(publication);
    match switch.filter(|_| fused) {
        Some(out) => {
            // SAFETY: this reader was acquired for this receiver on this masked
            // CPU, never released, and validated just above at the same RSP.
            let adopted = match frame_publication::direct_handoff_frame_token(slot, cpu) {
                Some(now) => unsafe {
                    dispatch_claim::FrameSelection::from_held(cpu, binding.id(), now,
                        token.saved_rsp, return_to_user, reader)
                },
                None => Err(reader),
            };
            match adopted {
                Ok(selection) => *out = Some(dispatch_claim::DirectSwitch::new(selection)),
                Err(reader) => {
                    // Ordinary FIFO custody for the committed owner; the dispatcher
                    // re-captures this frame exactly as for any other handoff.
                    let _publication = dispatch_priority::Publication::begin(scope, target);
                    let _ = sync_handoff::enqueue(target,
                        sync_handoff::SyncHandoffRecord::new(slot, binding.id()));
                    drop(reader);
                }
            }
        }
        None => drop(reader),
    }
    #[cfg(not(test))]
    if notify { crate::multitask::irq::request_target_reschedule(target); }
    #[cfg(test)]
    let _ = notify;
    Ok(())
}

/// Call trap entry. Binding/funding observations grant no execution: the actual
/// dispatcher consumes this sole FIFO record and its original claim/interval.
pub(in crate::multitask) fn call(
    scope: &DispatchPreemption<'_>, endpoint: u64, reply: u64, receiver_id: u64,
) -> FastIpcCallHandoffOutcome {
    let _phase = super::runtime_profile::phase_scope(super::SchedulerPhase::IpcCallPublication);
    call_before_sender_commit(scope, endpoint, reply, receiver_id, || {})
}

/// Call trap entry that lets a committed same-CPU handoff carry its receiver
/// frame loan straight into this turn's selection through `switch`.
pub(in crate::multitask) fn call_switching(
    scope: &DispatchPreemption<'_>, endpoint: u64, reply: u64, receiver_id: u64,
    switch: &mut Option<dispatch_claim::DirectSwitch>,
) -> FastIpcCallHandoffOutcome {
    let _phase = super::runtime_profile::phase_scope(super::SchedulerPhase::IpcCallPublication);
    call_published(scope, endpoint, reply, receiver_id, || {}, Some(switch))
}

fn call_before_sender_commit(
    scope: &DispatchPreemption<'_>, endpoint: u64, reply: u64, receiver_id: u64,
    before_commit: impl FnOnce(),
) -> FastIpcCallHandoffOutcome {
    call_published(scope, endpoint, reply, receiver_id, before_commit, None)
}

fn call_published(
    scope: &DispatchPreemption<'_>, endpoint: u64, reply: u64, receiver_id: u64,
    before_commit: impl FnOnce(), switch: Option<&mut Option<dispatch_claim::DirectSwitch>>,
) -> FastIpcCallHandoffOutcome {
    use FastIpcCallHandoffOutcome as Outcome;
    if !scoped(scope) || endpoint == 0 || reply == 0 { return Outcome::SenderMismatch; }
    // SAFETY: original caller scope masks this CPU through final sender commit
    // and loan Drop; CurrentWait authenticates the actual Current allocation bank.
    let Some(sender) = (unsafe { current_wait::CurrentWait::capture() }) else {
        return Outcome::SenderMismatch;
    };
    if !sender.matches_armed_reply(reply) || sender.id() == receiver_id {
        return Outcome::SenderMismatch;
    }
    // The sender already holds an executing-bank pin; take the donor loan from
    // it instead of re-proving the same identity through three leaves.
    let Some(donor) = sender.original_pin().duplicate_donation() else {
        return Outcome::DonationUnavailable;
    };
    let Some(receiver) = donation_publication::participant(receiver_id) else {
        return Outcome::ReceiverMismatch;
    };
    let Some(binding) = wake_publication::Binding::from_original(receiver) else {
        return Outcome::ReceiverMismatch;
    };
    let Some(typed) = runqueue::wait::TypedWake::capture(&binding,
        runqueue::wait::REASON_ENDPOINT_RECEIVE, endpoint) else {
        return Outcome::ReceiverMismatch;
    };
    let receiver_owner = runqueue::owner(binding.slot());
    if receiver_owner.state != runqueue::RunOwnerState::Blocked || receiver_owner.wait_armed {
        return Outcome::ReceiverMismatch;
    }
    let Some(donor) = donation_publication::bind_original(scope, reply, donor,
        binding.original_pin(), false) else {
        return Outcome::DonationUnavailable;
    };
    let cpu = scope.cpu();
    let previous = receiver_owner.cpu.unwrap_or_else(|| {
        let last = runqueue::affinity_payload::last_cpu(binding.slot());
        if last == NO_IDLE_CPU { cpu } else { usize::from(last) }
    });
    // The local CPU is the common target; its eligibility was just proven, so
    // only a different (previous) CPU needs its own check.
    let target = if eligible(&binding, cpu) { cpu } else { previous };
    if target != cpu && !eligible(&binding, target) { return Outcome::EligibilityUnavailable; }
    if let Err(outcome) = publish_switching(scope, &binding, &typed, target, || {}, switch) {
        return outcome;
    }
    // A racing reverse reply may already have won. NEVER overwrite WakeWon
    // with scalar set_blocked; the original Current wait CAS arbitrates once.
    before_commit();
    let _ = sender.commit();
    drop(typed);
    drop(binding);
    drop(donor);
    drop(sender);
    if target == cpu { Outcome::CommittedSameCpu } else { Outcome::CommittedCrossCpu }
}

/// Scheduling-context custody is only exact terminal attribution, never frame
/// or execution permission. The subsequent ORIGINAL typed physical loan owns
/// actual wake, fair custody and FIFO; refusal cannot clear a newer wait.
pub(in crate::multitask) fn reply(
    scope: &DispatchPreemption<'_>, reply: u64, caller: u64, context_owner: u64,
    context: ObjectIdentity,
) -> Option<FastIpcReplyHandoffOutcome> {
    let _phase = super::runtime_profile::phase_scope(super::SchedulerPhase::IpcReplyPublication);
    reply_with(scope, reply, caller, context_owner, context,
        &runqueue_policy::online_affinity_mask)
}

pub(super) fn reply_with(
    scope: &DispatchPreemption<'_>, reply: u64, caller: u64, context_owner: u64,
    context: ObjectIdentity, eligible_cpus: &dyn Fn(usize) -> u64,
) -> Option<FastIpcReplyHandoffOutcome> {
    reply_published(scope, reply, caller, context_owner, context, eligible_cpus, None)
}

/// Reply trap entry that lets a recently blocked same-CPU caller's frame loan
/// travel straight into this turn's selection through `switch`.
pub(in crate::multitask) fn reply_switching(
    scope: &DispatchPreemption<'_>, reply: u64, caller: u64, context_owner: u64,
    context: ObjectIdentity, switch: &mut Option<dispatch_claim::DirectSwitch>,
) -> Option<FastIpcReplyHandoffOutcome> {
    let _phase = super::runtime_profile::phase_scope(super::SchedulerPhase::IpcReplyPublication);
    reply_published(scope, reply, caller, context_owner, context,
        &runqueue_policy::online_affinity_mask, Some(switch))
}

pub(super) fn reply_published(
    scope: &DispatchPreemption<'_>, reply: u64, caller: u64, context_owner: u64,
    context: ObjectIdentity, eligible_cpus: &dyn Fn(usize) -> u64,
    switch: Option<&mut Option<dispatch_claim::DirectSwitch>>,
) -> Option<FastIpcReplyHandoffOutcome> {
    if !scoped(scope) { return None; }
    let (exact, live) = task_directory::reply_context_custody(context_owner, context);
    if !exact && live { return None; }
    Some(if wake_publication::reply_switching(scope, reply, caller, eligible_cpus, switch) {
        FastIpcReplyHandoffOutcome::LocalFallback
    } else { FastIpcReplyHandoffOutcome::Rejected })
}

/// Blocked within the last millisecond; `since == 0` means no commit time was
/// recorded, which the ordinary wake must interpret.
pub(super) const fn recent_sleeper(since: u64, now: u64, ticks_per_second: u64) -> bool {
    since != 0 && now.saturating_sub(since) <= ticks_per_second / 1000
}

/// Wake the caller blocked on `reply` straight into DirectHandoff custody. The
/// ordinary reply wake additionally lifts a long sleeper's virtual-time floor
/// and files blocked-wait telemetry, so only a caller that blocked within the
/// last millisecond on this CPU, which the synchronous wake policy would keep
/// here, qualifies. Everything else, and any refusal before the owner CAS
/// (which preserves the wait), returns false and takes the ordinary wake.
pub(super) fn reply_direct(
    scope: &DispatchPreemption<'_>, reply: u64, binding: &wake_publication::Binding,
    eligible_cpus: &dyn Fn(usize) -> u64, switch: &mut Option<dispatch_claim::DirectSwitch>,
) -> bool {
    let cpu = scope.cpu();
    let slot = binding.slot();
    let owner = runqueue::owner(slot);
    if owner.state != runqueue::RunOwnerState::Blocked || owner.wait_armed
        || owner.cpu.is_some_and(|owner_cpu| owner_cpu != cpu) || !direct_switch_admitted(cpu) {
        return false;
    }
    if !recent_sleeper(runqueue::wait::blocked_since_ticks(slot), crate::arch::rtc::ticks(),
        crate::arch::rtc::ticks_per_second()) {
        return false;
    }
    if runqueue_policy::sync_wake_target_cpu(slot, eligible_cpus(slot)) != cpu || !eligible(binding, cpu) {
        return false;
    }
    let Some(typed) = runqueue::wait::TypedWake::capture(binding,
        runqueue::wait::REASON_ENDPOINT_REPLY, reply) else { return false; };
    publish_switching(scope, binding, &typed, cpu, || {}, Some(switch)).is_ok()
}

#[cfg(test)]
pub(super) mod tests;
