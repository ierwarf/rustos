//! Fixed-custody wake, synchronous handoff, and affinity-transfer publication.
//!
//! - **Owner:** the scheduler runqueue owns every transition through the exact
//!   parent `RunOwnerWord`; this module owns only the transfer protocol.
//! - **Boundary:** callers provide slot, CPU, weight, and endpoint-derived wake
//!   intent; none authorizes execution until the live owner word accepts it.
//! - **State machine:** `Blocked` publishes to `Local`, `RemoteQueued`, or
//!   `DirectHandoff`; migration passes through `Migrating`; rejected, duplicate,
//!   materialized, and rolled-back transfers have explicit terminal outcomes.
//! - **Invariants:** one slot has one custody owner and generation, each remote
//!   publication has one mailbox record, and typed wait identity survives every
//!   provisional handoff until the exact wake consumes it.
//! - **Concurrency:** owner-word CAS is the transfer linearization point;
//!   producers lock only the target mailbox and each CPU mutates only its local
//!   runqueue. Mailbox notification uses a release/acquire 0-to-1 edge.
//! - **Failure/recovery:** stale and terminal owners reject, duplicates dedup,
//!   failed bounded handoffs materialize or roll back, and stale mailbox records
//!   lose to their newer generation without reviving a task.
//! - **Forbidden:** no global runnable scan, foreign runqueue mutation,
//!   broadcast wake, identity-only admission, or second execution owner.
//! - **Evidence:** `per-cpu-runqueue-ownership`, `scheduler-dispatch`, and
//!   `ipc-scheduling-context-handoff`; focused witnesses are in
//!   `runqueue::tests` and `synchronous_handoff_tests`.

use core::sync::atomic::Ordering;

use super::super::frame_publication;
use super::*;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(in crate::multitask::scheduler) enum RemoteWakeOutcome {
    Rejected,
    AlreadyOwned { cpu: Option<usize> },
    Published { cpu: usize, notify: bool },
}

pub(super) const fn local_wake_owner_is_already_owned(state: RunOwnerState) -> bool {
    matches!(
        state,
        RunOwnerState::Local
            | RunOwnerState::RemoteQueued
            | RunOwnerState::Running
            | RunOwnerState::DirectHandoff
    )
}

const fn remote_wake_owner_is_already_owned(state: RunOwnerState) -> bool {
    matches!(
        state,
        RunOwnerState::Local
            | RunOwnerState::RemoteQueued
            | RunOwnerState::Running
            | RunOwnerState::DirectHandoff
    )
}

pub(in crate::multitask::scheduler) fn publish_local_wake(
    slot: usize,
    cpu: usize,
    weight: u32,
) -> RemoteWakeOutcome {
    // Actual CAS/queue transaction tolerates raced wait updates, remote wake
    // and terminal stop. No global serializer or execution grant is inferred.
    super::return_publication::publish_blocked_wake(slot, cpu, weight)
}

/// Exact scalar handoff CAS returning the enqueue duty it creates (I1). The
/// caller owns the ORIGINAL receiver binding, reader and typed wait phase; this
/// never retries a newer observation. FIFO56 -> Wait58 encloses the CAS, and
/// both leaves end before mailbox work. The duty is a
/// `HandoffFifo` duty for a same-CPU receiver (discharged by the FIFO publisher
/// with `handoff_recorded`, or carried by a `DirectSwitch`) or a `Mailbox` duty
/// for a remote one (discharged by `EnqueueDuty::publish_mailbox`).
pub(in crate::multitask::scheduler) fn claim_blocked_handoff_duty(
    slot: usize,
    expected: RunOwnerSnapshot,
    cpu: usize,
    local: bool,
) -> Option<(RunOwnerSnapshot, EnqueueDuty)> {
    validate_cpu(cpu);
    if expected.state != RunOwnerState::Blocked || expected.wait_armed {
        return None;
    }
    let event = if local { OwnerEvent::HandoffDirect } else { OwnerEvent::HandoffRemote };
    let committed = transition(slot, expected, event, Some(cpu)).ok()?;
    let next = committed.next;
    // Receipt of this CAS plus the enclosing typed clear, NOT a fresh owner
    // lookup. Terminal cancellation can subsequently make the FIFO token stale.
    Some((next.with_wait(false, 0).with_runnable(true), committed.into_duty()))
}

/// Transfers a blocked task directly to the current CPU's synchronous IPC
/// handoff owner without inserting it into the fair runqueue. The caller must
/// publish the matching bounded handoff record before releasing Scheduler.
#[cfg(test)]
pub(in crate::multitask::scheduler) fn publish_direct_handoff(
    slot: usize,
    cpu: usize,
) -> RemoteWakeOutcome {
    validate_cpu(cpu);
    loop {
        let owner = owner(slot);
        if owner.state.is_terminal() || owner.state == RunOwnerState::Dormant {
            return RemoteWakeOutcome::Rejected;
        }
        if remote_wake_owner_is_already_owned(owner.state) {
            return RemoteWakeOutcome::AlreadyOwned { cpu: owner.cpu };
        }
        if owner.state != RunOwnerState::Blocked {
            return RemoteWakeOutcome::Rejected;
        }
        if let Ok(committed) = transition(slot, owner, OwnerEvent::HandoffDirect, Some(cpu)) {
            let next = committed.next;
            // Fixture: the caller publishes the matching FIFO record.
            committed.into_duty().handoff_recorded();
            // Publish the exact direct generation only after the owner CAS.
            // A reader between the two steps rejects the stale frame token.
            frame_publication::bind_local_owner_generation(slot, next.generation);
            return RemoteWakeOutcome::Published { cpu, notify: false };
        }
    }
}

/// Restores fair-runqueue custody when the bounded synchronous-handoff FIFO
/// cannot retain a freshly published direct transfer. Scheduler serialization
/// guarantees the task has not been selected between publication and rollback.
#[cfg(test)]
pub(in crate::multitask::scheduler) fn materialize_direct_handoff(
    slot: usize,
    cpu: usize,
    weight: u32,
) -> bool {
    materialize_direct_handoff_with_generation(slot, cpu, weight, None)
}

/// A consumed exact FIFO record may restore only its own owner generation.
/// Another transaction can republish a DirectHandoff owner before this CAS.
pub(in crate::multitask::scheduler) fn materialize_direct_handoff_with_generation(
    slot: usize,
    cpu: usize,
    weight: u32,
    expected_generation: Option<u64>,
) -> bool {
    validate_cpu(cpu);
    let mut observed = owner(slot);
    if expected_generation.is_some_and(|generation| generation != observed.generation)
        || observed.state != RunOwnerState::DirectHandoff
        || observed.cpu != Some(cpu)
    {
        return false;
    }
    let mut rq = RUN_QUEUES[cpu].inner.lock();
    // No other writer may gain new queue custody while this lock is held;
    // the owner CAS nevertheless rechecks lock-free metadata writers. Run
    // intent cannot be withdrawn from a DirectHandoff word in place (see
    // `RunOwnerState::may_lower_run_intent`), so only a custody change ends
    // this consumed record's claim on the task: a metadata write retries.
    let next = loop {
        rq.insert(slot, weight);
        match transition(slot, observed, OwnerEvent::Materialize, Some(cpu)) {
            Ok(committed) => {
                let next = committed.next;
                committed.into_duty().run_queue_inserted(&rq);
                break next;
            }
            Err(winner) => {
                rq.remove(slot, weight);
                RUN_QUEUES[cpu].publish_load(&rq);
                if winner.generation != observed.generation
                    || winner.state != RunOwnerState::DirectHandoff
                {
                    return false;
                }
                observed = winner;
            }
        }
    };
    frame_publication::bind_local_owner_generation(slot, next.generation);
    RUN_QUEUES[cpu].publish_load(&rq);
    true
}

/// Returns a not-yet-dispatched direct receiver to exact blocked custody.
/// No runqueue or mailbox entry exists while `DirectHandoff` is owned, so one
/// owner-word CAS restores the pre-reservation representation.
#[cfg(test)]
pub(in crate::multitask::scheduler) fn rollback_direct_handoff(slot: usize, cpu: usize) -> bool {
    validate_cpu(cpu);
    let owner = owner(slot);
    if owner.state != RunOwnerState::DirectHandoff || owner.cpu != Some(cpu) || !owner.runnable {
        return false;
    }
    transition(slot, owner, OwnerEvent::RollbackHandoff, None)
        .map(Transition::settled)
        .is_ok()
}

pub(in crate::multitask::scheduler) fn publish_remote_wake(
    slot: usize,
    target_cpu: usize,
    weight: u32,
) -> RemoteWakeOutcome {
    validate_cpu(target_cpu);
    loop {
        let owner = owner(slot);
        if owner.state.is_terminal() || owner.state == RunOwnerState::Dormant {
            return RemoteWakeOutcome::Rejected;
        }
        if remote_wake_owner_is_already_owned(owner.state) {
            return RemoteWakeOutcome::AlreadyOwned { cpu: owner.cpu };
        }
        if owner.state != RunOwnerState::Blocked {
            return RemoteWakeOutcome::Rejected;
        }
        let Ok(committed) = transition(slot, owner, OwnerEvent::WakeRemote, Some(target_cpu)) else {
            continue;
        };
        let notify = committed.into_duty().publish_mailbox(weight);
        return RemoteWakeOutcome::Published {
            cpu: target_cpu,
            notify,
        };
    }
}

#[cfg(test)]
static TEST_ADOPTION_RETIRE_SLOT: core::sync::atomic::AtomicUsize =
    core::sync::atomic::AtomicUsize::new(usize::MAX);

/// Makes the next adoption of `slot` lose its owner CAS to a terminal withdrawal.
#[cfg(test)]
pub(super) fn force_adoption_retire(slot: usize) {
    TEST_ADOPTION_RETIRE_SLOT.store(slot, Ordering::SeqCst);
}

pub(in crate::multitask::scheduler) fn drain_remote_wakes(cpu: usize) -> usize {
    validate_cpu(cpu);
    // ORDERING: Acquire pairs with the producer's 0-to-1 edge, published only
    // after the mailbox record. A later producer wins a new notification edge.
    if MAILBOX_PENDING[cpu].load(Ordering::Acquire) == 0 {
        return 0;
    }
    let mut records = [RunTransfer::EMPTY; MAILBOX_CAPACITY];
    let mut count = 0;
    {
        let mut mailbox = REMOTE_WAKE_MAILBOXES[cpu].lock();
        while let Some(record) = mailbox.pop() {
            records[count] = record;
            count += 1;
        }
        // ORDERING: clearing under the mailbox owner closes the race with a
        // producer that publishes the next 0-to-1 notification edge.
        MAILBOX_PENDING[cpu].store(0, Ordering::Release);
    }
    if count == 0 {
        return 0;
    }
    let mut rq = RUN_QUEUES[cpu].inner.lock();
    for record in &records[..count] {
        let observed = owner(record.slot);
        if observed.state != RunOwnerState::RemoteQueued
            || observed.cpu != Some(cpu)
            || observed.generation != record.generation
        {
            if observed.state.is_terminal() || observed.generation > record.generation {
                continue;
            }
            panic!(
                "scheduler mailbox record lost exact owner slot={} record_gen={} observed={observed:?}",
                record.slot, record.generation
            );
        }
        rq.insert(record.slot, record.weight);
        // Lock-free run-intent and wait writers may change metadata bits of
        // the `RemoteQueued` owner under this adoption; custody (state, CPU,
        // generation) can only end in a terminal withdrawal while the exact
        // mailbox record is held.
        #[cfg(test)]
        if TEST_ADOPTION_RETIRE_SLOT
            .compare_exchange(record.slot, usize::MAX, Ordering::SeqCst, Ordering::SeqCst)
            .is_ok()
        {
            // A remote terminal withdrawal between the load and the CAS.
            retire(record.slot, record.weight);
        }
        let mut expected = observed;
        let adopted = loop {
            match transition(record.slot, expected, OwnerEvent::Adopt, Some(cpu)) {
                Ok(committed) => {
                    let next = committed.next;
                    committed.into_duty().run_queue_inserted(&rq);
                    break Some(next);
                }
                Err(winner) if winner.state.is_terminal() || winner.generation > record.generation => {
                    // A remote terminal withdrawal (possibly already released and
                    // the slot reused) took the task off its mailbox custody after
                    // the load above; it no longer needs the queue.
                    rq.remove(record.slot, record.weight);
                    break None;
                }
                Err(winner) => {
                    if winner.state != RunOwnerState::RemoteQueued
                        || winner.cpu != Some(cpu)
                        || winner.generation != record.generation
                    {
                        panic!("scheduler mailbox adoption lost owner race observed={winner:?}");
                    }
                    expected = winner;
                }
            }
        };
        if let Some(next) = adopted {
            frame_publication::bind_local_owner_generation(record.slot, next.generation);
        }
    }
    RUN_QUEUES[cpu].publish_load(&rq);
    count
}

/// Transfer only the original reader-qualified Local owner. Unlike generic
/// affinity/wake rehome, a raced Blocked/Direct/new generation is NOT reacquired.
pub(in crate::multitask::scheduler) fn rehome_queued_frame(
    frame: &super::super::dispatcher::maintenance::MigrationFrame<'_>,
    source_cpu: usize,
    target_cpu: usize,
    weight: u32,
) -> RemoteWakeOutcome {
    validate_cpu(source_cpu);
    validate_cpu(target_cpu);
    if source_cpu == target_cpu { return RemoteWakeOutcome::Rejected; }
    let slot = frame.token.slot;
    let owner = owner(slot);
    if owner.state != RunOwnerState::Local || owner.cpu != Some(source_cpu)
        || owner.generation != frame.token.owner_generation {
        return RemoteWakeOutcome::Rejected;
    }
    let mut source = RUN_QUEUES[source_cpu].inner.lock();
    if !source.contains(slot) || !frame.is_current() { return RemoteWakeOutcome::Rejected; }
    let admitted = super::affinity_payload::with_rehome_admission(slot, target_cpu, None, || {
        // The original physical reader is still live here; this cannot spend
        // a replacement generation or wake a continuation that became blocked.
        if !frame.is_current() { return Err(super::owner(slot)); }
        transition(slot, owner, OwnerEvent::Rehome, Some(target_cpu))
    });
    let Some(Ok(committed)) = admitted else { return RemoteWakeOutcome::Rejected; };
    // Read-only native witness observes the ACTUAL intermediate owner CAS,
    // not its repaired final mailbox target or the proposed descriptor.
    #[cfg(test)]
    let transferred = super::owner(slot);
    source.remove(slot, weight);
    RUN_QUEUES[source_cpu].publish_load(&source);
    drop(source);
    #[cfg(test)]
    record_test_local_migrating_owner(transferred);
    publish_migration(committed, weight)
}

#[cfg(test)]
pub(in crate::multitask::scheduler) fn rehome_queued(
    slot: usize,
    target_cpu: usize,
    weight: u32,
) -> RemoteWakeOutcome {
    rehome_queued_inner(slot, target_cpu, weight, None)
}

pub(in crate::multitask::scheduler) fn rehome_queued_owned(
    slot: usize,
    target_cpu: usize,
    weight: u32,
    cohort: &crate::multitask::current_identity::cohort::Cohort,
    id: u64,
) -> RemoteWakeOutcome {
    rehome_queued_inner(slot, target_cpu, weight, Some((cohort, id)))
}

fn rehome_queued_inner(
    slot: usize,
    target_cpu: usize,
    weight: u32,
    custody: Option<(&crate::multitask::current_identity::cohort::Cohort, u64)>,
) -> RemoteWakeOutcome {
    validate_cpu(target_cpu);
    loop {
        let owner = owner(slot);
        match owner.state {
            // A captured Running peer can block before its affinity rehome.
            // Affinity owns masks, never a wake of that now-blocked continuation.
            RunOwnerState::Blocked if custody.is_some() => return RemoteWakeOutcome::Rejected,
            RunOwnerState::Blocked => return publish_remote_wake(slot, target_cpu, weight),
            RunOwnerState::Running => return RemoteWakeOutcome::AlreadyOwned { cpu: owner.cpu },
            RunOwnerState::Local => {
                let source_cpu = owner.cpu.expect("local scheduler owner omitted CPU");
                if source_cpu == target_cpu {
                    return RemoteWakeOutcome::AlreadyOwned {
                        cpu: Some(source_cpu),
                    };
                }
                let mut source = RUN_QUEUES[source_cpu].inner.lock();
                if !source.contains(slot) {
                    let observed = super::owner(slot);
                    if observed.state != RunOwnerState::Local || observed.cpu != Some(source_cpu) {
                        continue;
                    }
                    panic!(
                        "scheduler rehome found Local owner without source membership slot={slot} cpu={source_cpu}"
                    );
                }
                // Identity/cohort and live affinity qualify the SAME owner CAS.
                // Ordinary source policy cannot spend a stale pre-seal target.
                let admitted = super::affinity_payload::with_rehome_admission(
                    slot,
                    target_cpu,
                    custody,
                    || transition(slot, owner, OwnerEvent::Rehome, Some(target_cpu)),
                );
                let committed = match admitted {
                    None => return RemoteWakeOutcome::Rejected,
                    Some(Err(_)) => continue,
                    Some(Ok(committed)) => committed,
                };
                source.remove(slot, weight);
                #[cfg(test)]
                record_test_local_migrating_owner(committed.next);
                RUN_QUEUES[source_cpu].publish_load(&source);
                drop(source);
                return publish_migration(committed, weight);
            }
            RunOwnerState::RemoteQueued | RunOwnerState::DirectHandoff => {
                if owner.cpu == Some(target_cpu) {
                    return RemoteWakeOutcome::AlreadyOwned {
                        cpu: Some(target_cpu),
                    };
                }
                match super::affinity_payload::with_rehome_admission(
                    slot,
                    target_cpu,
                    custody,
                    || transition(slot, owner, OwnerEvent::Rehome, Some(target_cpu)),
                ) {
                    None => return RemoteWakeOutcome::Rejected,
                    Some(Err(_)) => continue,
                    Some(Ok(committed)) => return publish_migration(committed, weight),
                }
            }
            RunOwnerState::Migrating => core::hint::spin_loop(),
            RunOwnerState::Dormant | RunOwnerState::Retiring | RunOwnerState::Retired => {
                return RemoteWakeOutcome::Rejected;
            }
        }
    }
}

/// Discharges a `Migration` duty: publishes the `Migrating` word as
/// `RemoteQueued` on its target and that generation's mailbox record. A
/// terminal withdrawal that beat the publication voids the migration (the
/// withdrawal owns the task now), which also ends the duty.
fn publish_migration(committed: Transition, weight: u32) -> RemoteWakeOutcome {
    let migrating = committed.next;
    let duty = committed.into_duty();
    let slot = duty.slot();
    let target_cpu = duty.cpu();
    duty.discharge(DutyKind::Migration);
    publish_migrating_record(slot, migrating, target_cpu, weight)
}

pub(super) fn publish_migrating_record(
    slot: usize,
    migrating: RunOwnerSnapshot,
    target_cpu: usize,
    weight: u32,
) -> RemoteWakeOutcome {
    let mut observed = migrating;
    // Lock-free run-intent and wait writers may change metadata bits of the
    // `Migrating` owner without a custody change; only a different custody
    // generation is a broken transfer.
    let queued = loop {
        match transition(slot, observed, OwnerEvent::PublishMigration, Some(target_cpu)) {
            Ok(committed) => break committed,
            // A terminal stop beat the publication, and the slot may already
            // have been released and reused: either way the record is void.
            Err(current) if current.state.is_terminal() || current.generation > migrating.generation => {
                return RemoteWakeOutcome::Rejected;
            }
            Err(current) => {
                if current.state != RunOwnerState::Migrating
                    || current.generation != migrating.generation
                {
                    panic!("scheduler migration publication raced observed={current:?}");
                }
                observed = current;
            }
        }
    };
    let notify = queued.into_duty().publish_mailbox(weight);
    RemoteWakeOutcome::Published {
        cpu: target_cpu,
        notify,
    }
}
