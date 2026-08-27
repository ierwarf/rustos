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

use super::*;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(in crate::multitask::scheduler) enum RemoteWakeOutcome {
    Rejected,
    AlreadyOwned { cpu: Option<usize> },
    Published { cpu: usize, notify: bool },
}

const fn local_wake_owner_is_already_owned(state: RunOwnerState) -> bool {
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

/// Same-CPU counterpart to `publish_remote_wake`: when a wake's target CPU is
/// the CPU already executing the wake, publish directly into the local
/// runqueue in one step instead of round-tripping through the mailbox. This
/// retains the owner generation captured by a synchronous reply-wake token.
pub(in crate::multitask::scheduler) fn publish_local_wake(
    slot: usize,
    cpu: usize,
    weight: u32,
) -> RemoteWakeOutcome {
    validate_cpu(cpu);
    let owner = owner(slot);
    if owner.state.is_terminal() || owner.state == RunOwnerState::Dormant {
        return RemoteWakeOutcome::Rejected;
    }
    if local_wake_owner_is_already_owned(owner.state) {
        return RemoteWakeOutcome::AlreadyOwned { cpu: owner.cpu };
    }
    if owner.state != RunOwnerState::Blocked {
        return RemoteWakeOutcome::Rejected;
    }
    publish_local(slot, cpu, weight);
    RemoteWakeOutcome::Published { cpu, notify: false }
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
    validate_cpu(cpu);
    let owner = owner(slot);
    if owner.state != RunOwnerState::DirectHandoff || owner.cpu != Some(cpu) || !owner.runnable {
        return false;
    }
    let mut rq = RUN_QUEUES[cpu].inner.lock();
    rq.insert(slot, weight);
    if OWNER_WORDS[slot]
        .compare_exchange(owner, owner.next(RunOwnerState::Local, Some(cpu)))
        .is_err()
    {
        rq.remove(slot, weight);
        RUN_QUEUES[cpu].publish_load(&rq);
        return false;
    }
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
        OWNER_WORDS[record.slot]
            .compare_exchange(observed, observed.next(RunOwnerState::Local, Some(cpu)))
            .unwrap_or_else(|winner| {
                panic!("scheduler mailbox adoption lost owner race observed={winner:?}")
            });
    }
    RUN_QUEUES[cpu].publish_load(&rq);
    count
}

/// Moves queued custody to a newly admitted affinity target.
///
/// The lifecycle owner serializes affinity mutation against dispatch. Queue
/// authority still follows the source-owned transfer protocol, so a stale old
/// mailbox record is discarded when its generation no longer matches.
pub(in crate::multitask::scheduler) fn rehome_queued(
    slot: usize,
    target_cpu: usize,
    weight: u32,
) -> RemoteWakeOutcome {
    validate_cpu(target_cpu);
    loop {
        let owner = owner(slot);
        match owner.state {
            RunOwnerState::Blocked => return publish_remote_wake(slot, target_cpu, weight),
            RunOwnerState::Running => {
                return RemoteWakeOutcome::AlreadyOwned { cpu: owner.cpu };
            }
            RunOwnerState::Local => {
                let source_cpu = owner.cpu.expect("local scheduler owner omitted CPU");
                if source_cpu == target_cpu {
                    return RemoteWakeOutcome::AlreadyOwned {
                        cpu: Some(source_cpu),
                    };
                }
                let mut source = RUN_QUEUES[source_cpu].inner.lock();
                if !source.contains(slot) {
                    panic!(
                        "scheduler rehome found Local owner without source membership slot={slot} cpu={source_cpu}"
                    );
                }
                source.remove(slot, weight);
                let migrating = owner.next(RunOwnerState::Migrating, Some(target_cpu));
                if OWNER_WORDS[slot]
                    .compare_exchange(owner, migrating)
                    .is_err()
                {
                    source.insert(slot, weight);
                    RUN_QUEUES[source_cpu].publish_load(&source);
                    continue;
                }
                #[cfg(test)]
                record_test_local_migrating_owner(migrating);
                RUN_QUEUES[source_cpu].publish_load(&source);
                drop(source);
                return publish_migrating_record(slot, migrating, target_cpu, weight);
            }
            RunOwnerState::RemoteQueued => {
                if owner.cpu == Some(target_cpu) {
                    return RemoteWakeOutcome::AlreadyOwned {
                        cpu: Some(target_cpu),
                    };
                }
                let migrating = owner.next(RunOwnerState::Migrating, Some(target_cpu));
                if OWNER_WORDS[slot]
                    .compare_exchange(owner, migrating)
                    .is_err()
                {
                    continue;
                }
                return publish_migrating_record(slot, migrating, target_cpu, weight);
            }
            RunOwnerState::DirectHandoff => {
                if owner.cpu == Some(target_cpu) {
                    return RemoteWakeOutcome::AlreadyOwned {
                        cpu: Some(target_cpu),
                    };
                }
                let migrating = owner.next(RunOwnerState::Migrating, Some(target_cpu));
                if OWNER_WORDS[slot]
                    .compare_exchange(owner, migrating)
                    .is_err()
                {
                    continue;
                }
                return publish_migrating_record(slot, migrating, target_cpu, weight);
            }
            RunOwnerState::Migrating => continue,
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
    OWNER_WORDS[slot]
        .compare_exchange(migrating, queued)
        .unwrap_or_else(|observed| {
            panic!("scheduler migration publication raced observed={observed:?}")
        });
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
