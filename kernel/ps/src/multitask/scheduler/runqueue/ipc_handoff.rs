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

/// Exact scalar handoff CAS. The caller owns the ORIGINAL receiver binding,
/// reader and typed wait phase; this helper never retries a newer observation.
/// FIFO56 -> Wait58 encloses the CAS, and both leaves end before mailbox work.
pub(in crate::multitask::scheduler) fn claim_blocked_handoff(
    slot: usize,
    expected: RunOwnerSnapshot,
    cpu: usize,
    local: bool,
) -> Option<RunOwnerSnapshot> {
    validate_cpu(cpu);
    if expected.state != RunOwnerState::Blocked || expected.wait_armed {
        return None;
    }
    let state = if local { RunOwnerState::DirectHandoff } else { RunOwnerState::RemoteQueued };
    let next = expected.next_preserving_wait(state, Some(cpu));
    OWNER_WORDS[slot].compare_exchange(expected, next).ok()?;
    // Receipt of this CAS plus the enclosing typed clear, NOT a fresh owner
    // lookup. Terminal cancellation can subsequently make the FIFO token stale.
    Some(next.with_wait(false, 0).with_runnable(true))
}

/// Publish only the mailbox record belonging to the original successful CAS.
/// A retained physical reader excludes execution until this publication ends.
pub(in crate::multitask::scheduler) fn publish_handoff_mailbox(
    slot: usize,
    original: RunOwnerSnapshot,
    weight: u32,
) -> bool {
    assert_eq!(original.state, RunOwnerState::RemoteQueued);
    let cpu = original.cpu.expect("original remote handoff lost target");
    REMOTE_WAKE_MAILBOXES[cpu].lock().publish(RunTransfer {
        slot,
        generation: original.generation,
        weight,
    });
    MAILBOX_PENDING[cpu].compare_exchange(0, 1, Ordering::AcqRel, Ordering::Acquire).is_ok()
}

/// Transfers a blocked task directly to the current CPU's synchronous IPC
/// handoff owner without inserting it into the fair runqueue. The caller must
/// publish the matching bounded handoff record before releasing Scheduler.
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
        let next = owner.next_preserving_wait(RunOwnerState::DirectHandoff, Some(cpu));
        if OWNER_WORDS[slot].compare_exchange(owner, next).is_ok() {
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
    let observed = owner(slot);
    if expected_generation.is_some_and(|generation| generation != observed.generation)
        || observed.state != RunOwnerState::DirectHandoff
        || observed.cpu != Some(cpu)
        || !observed.runnable
    {
        return false;
    }
    let mut rq = RUN_QUEUES[cpu].inner.lock();
    // No other writer may gain new queue custody while this lock is held;
    // the owner CAS nevertheless rechecks lock-free metadata writers.
    rq.insert(slot, weight);
    let next = observed.next(RunOwnerState::Local, Some(cpu));
    if OWNER_WORDS[slot].compare_exchange(observed, next).is_err() {
        rq.remove(slot, weight);
        RUN_QUEUES[cpu].publish_load(&rq);
        return false;
    }
    frame_publication::bind_local_owner_generation(slot, next.generation);
    RUN_QUEUES[cpu].publish_load(&rq);
    true
}

/// Returns a not-yet-dispatched direct receiver to exact blocked custody.
/// No runqueue or mailbox entry exists while `DirectHandoff` is owned, so one
/// owner-word CAS restores the pre-reservation representation.
pub(in crate::multitask::scheduler) fn rollback_direct_handoff(slot: usize, cpu: usize) -> bool {
    validate_cpu(cpu);
    let owner = owner(slot);
    if owner.state != RunOwnerState::DirectHandoff || owner.cpu != Some(cpu) || !owner.runnable {
        return false;
    }
    OWNER_WORDS[slot]
        .compare_exchange(
            owner,
            owner.next_preserving_wait(RunOwnerState::Blocked, None),
        )
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
        let next = owner.next_preserving_wait(RunOwnerState::RemoteQueued, Some(target_cpu));
        if OWNER_WORDS[slot].compare_exchange(owner, next).is_err() {
            continue;
        }
        {
            let mut mailbox = REMOTE_WAKE_MAILBOXES[target_cpu].lock();
            mailbox.publish(RunTransfer {
                slot,
                generation: next.generation,
                weight,
            });
        }
        // ORDERING: the mailbox lock release publishes the record before this
        // 0-to-1 edge grants notification custody to the winning producer.
        let notify = MAILBOX_PENDING[target_cpu]
            .compare_exchange(0, 1, Ordering::AcqRel, Ordering::Acquire)
            .is_ok();
        return RemoteWakeOutcome::Published {
            cpu: target_cpu,
            notify,
        };
    }
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
    for record in records.into_iter().take(count) {
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
        let next = observed.next(RunOwnerState::Local, Some(cpu));
        OWNER_WORDS[record.slot]
            .compare_exchange(observed, next)
            .unwrap_or_else(|winner| {
                panic!("scheduler mailbox adoption lost owner race observed={winner:?}")
            });
        frame_publication::bind_local_owner_generation(record.slot, next.generation);
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
    let migrating = owner.next(RunOwnerState::Migrating, Some(target_cpu));
    let admitted = super::affinity_payload::with_rehome_admission(slot, target_cpu, None, || {
        // The original physical reader is still live here; this cannot spend
        // a replacement generation or wake a continuation that became blocked.
        if !frame.is_current() { return Err(super::owner(slot)); }
        OWNER_WORDS[slot].compare_exchange(owner, migrating)
    });
    if !matches!(admitted, Some(Ok(_))) { return RemoteWakeOutcome::Rejected; }
    // Read-only native witness observes the ACTUAL intermediate owner CAS,
    // not its repaired final mailbox target or the proposed descriptor.
    #[cfg(test)]
    let transferred = super::owner(slot);
    source.remove(slot, weight);
    RUN_QUEUES[source_cpu].publish_load(&source);
    drop(source);
    #[cfg(test)]
    record_test_local_migrating_owner(transferred);
    publish_migrating_record(slot, migrating, target_cpu, weight)
}

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
                let migrating = owner.next(RunOwnerState::Migrating, Some(target_cpu));
                // Identity/cohort and live affinity qualify the SAME owner CAS.
                // Ordinary source policy cannot spend a stale pre-seal target.
                let admitted = super::affinity_payload::with_rehome_admission(
                    slot,
                    target_cpu,
                    custody,
                    || OWNER_WORDS[slot].compare_exchange(owner, migrating),
                );
                match admitted {
                    None => return RemoteWakeOutcome::Rejected,
                    Some(Err(_)) => continue,
                    Some(Ok(_)) => {}
                }
                source.remove(slot, weight);
                #[cfg(test)]
                record_test_local_migrating_owner(migrating);
                RUN_QUEUES[source_cpu].publish_load(&source);
                drop(source);
                return publish_migrating_record(slot, migrating, target_cpu, weight);
            }
            RunOwnerState::RemoteQueued | RunOwnerState::DirectHandoff => {
                if owner.cpu == Some(target_cpu) {
                    return RemoteWakeOutcome::AlreadyOwned {
                        cpu: Some(target_cpu),
                    };
                }
                let migrating = owner.next(RunOwnerState::Migrating, Some(target_cpu));
                match super::affinity_payload::with_rehome_admission(
                    slot,
                    target_cpu,
                    custody,
                    || OWNER_WORDS[slot].compare_exchange(owner, migrating),
                ) {
                    None => return RemoteWakeOutcome::Rejected,
                    Some(Err(_)) => continue,
                    Some(Ok(_)) => {
                        return publish_migrating_record(slot, migrating, target_cpu, weight);
                    }
                }
            }
            RunOwnerState::Migrating => core::hint::spin_loop(),
            RunOwnerState::Dormant | RunOwnerState::Retiring | RunOwnerState::Retired => {
                return RemoteWakeOutcome::Rejected;
            }
        }
    }
}

pub(super) fn publish_migrating_record(
    slot: usize,
    migrating: RunOwnerSnapshot,
    target_cpu: usize,
    weight: u32,
) -> RemoteWakeOutcome {
    let queued = migrating.next(RunOwnerState::RemoteQueued, Some(target_cpu));
    if let Err(observed) = OWNER_WORDS[slot].compare_exchange(migrating, queued) {
        if observed.state.is_terminal() {
            return RemoteWakeOutcome::Rejected;
        }
        panic!("scheduler migration publication raced observed={observed:?}");
    }
    {
        let mut mailbox = REMOTE_WAKE_MAILBOXES[target_cpu].lock();
        mailbox.publish(RunTransfer {
            slot,
            generation: queued.generation,
            weight,
        });
    }
    // ORDERING: the mailbox lock release publishes the exact generation before
    // the pending edge makes that record visible to the target CPU.
    let notify = MAILBOX_PENDING[target_cpu]
        .compare_exchange(0, 1, Ordering::AcqRel, Ordering::Acquire)
        .is_ok();
    RemoteWakeOutcome::Published {
        cpu: target_cpu,
        notify,
    }
}
