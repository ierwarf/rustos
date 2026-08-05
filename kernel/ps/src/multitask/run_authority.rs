//! Dual-authority divergence check for the per-CPU scheduler cutover.
//!
//! - **Owner:** nothing. This module holds no ownership state and grants no
//!   authority; it compares two sources that already exist.
//! - **Boundary:** `scheduler/runqueue.rs` owns the authoritative
//!   `RunOwnerWord` per slot. The legacy tables — `context.ready`, the
//!   published current/transition pair, and the retire flag — are the second
//!   source. Both are mutated under the global scheduler lock today.
//! - **Lifecycle:** swept once per profile drain while the lock is held, so
//!   both sources are one stable observation.
//! - **Concurrency:** read-only.
//! - **Failure:** a disagreement is reported, never fatal.
//! - **Forbidden:** no third owner word. A shadow copy of `RunOwnerWord` is
//!   exactly the "shadow ready state" `runqueue.rs` forbids from authorizing
//!   dispatch, and it would need its own publication sites in order to go
//!   stale.
//!
//! # What this is for, and what it replaced
//!
//! `V5-SCHED-GLOBAL-001` reads as "build a per-CPU runqueue", and that work is
//! already done: `runqueue.rs` owns per-slot owner words, per-CPU rq locks,
//! remote wake mailboxes with exact notification, and migration custody. The
//! first attempt at this module shadowed those owner words in a second table,
//! which was redundant on arrival.
//!
//! What actually remains is narrower and harder. Every one of those transitions
//! still runs while the single global `SCHEDULER` lock is held, so the queue is
//! per-CPU and the *authority* is not. Removing that lock makes the owner word
//! the only synchronisation between CPUs, and before that can be safe the two
//! sources have to be proven to agree while the lock is still there — because
//! after the cutover a disagreement stops being a diagnostic and becomes a task
//! running on two CPUs.
//!
//! `V5-FORMAL-SCHED-019` asks for this and names the mutant it must kill: dual
//! divergence between the legacy state and the per-CPU state.

use core::sync::atomic::{AtomicU64, Ordering};

/// The position the legacy tables imply, independent of the runqueue word.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum LegacyPosition {
    /// No context is bound.
    Absent,
    /// Retire requested; cleanup outstanding.
    Retiring,
    /// Executing on this CPU.
    Running(u8),
    /// Off-CPU with its outgoing stack still held by this CPU.
    Transition(u8),
    /// Runnable and not executing.
    Runnable,
    /// Bound, not runnable, not executing.
    Blocked,
}

/// How the two sources disagree about one slot.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub(super) enum Mismatch {
    /// Both sources say the slot is executing, on different CPUs. After the
    /// cutover this is literally a task running on two CPUs.
    RunningOnDifferentCpus = 1,
    /// The runqueue owns the slot as running and the published
    /// current/transition pair does not. A dispatch decision taken from the
    /// owner word alone would then execute a task the CPU is not on.
    QueueRunningLegacyNot = 5,
    /// The published pair says a CPU is executing the slot and the runqueue
    /// does not own it as running. The owner word would be free to hand the
    /// same slot to another CPU.
    LegacyRunningQueueNot = 6,
    /// The runqueue holds the slot in a queue while the legacy tables call it
    /// blocked, so a dispatch would find no runnable task where one is queued.
    QueuedButBlocked = 2,
    /// The legacy tables call the slot runnable while the runqueue owns it
    /// nowhere, so a task is runnable with no CPU responsible for it.
    RunnableButUnqueued = 3,
    /// One source considers the slot alive and the other does not.
    Lifetime = 4,
    /// The slot is executing and the two sources disagree about whether it
    /// still wants to run. The owner word's runnable bit decides whether a
    /// preempted task returns to its queue or is published blocked, so a stale
    /// bit either strands a runnable task or re-queues a blocked one.
    RunningRunnableBit = 7,
}

/// The runqueue owner states this check distinguishes.
///
/// Mirrors `runqueue::RunOwnerState` without importing it so the comparison
/// stays a pure function a unit test can drive over every combination.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum QueueOwner {
    Dormant,
    Blocked,
    Queued(Option<u8>),
    /// The CPU executing it, and whether the owner word still calls it
    /// runnable. The second field is Linux's `p->on_rq` for a running task.
    Running(u8, bool),
    Migrating(Option<u8>),
    Retiring,
    Retired,
}

/// Compares the two sources for one slot.
///
/// Deliberately silent about states that are legitimately in flight. A
/// `Migrating` slot has, by construction, published its outgoing transition and
/// not yet been adopted, so the two sources are allowed to describe different
/// halves of that handoff.
///
/// `legacy_running_is_runnable` is `context.ready` for the executing task. The
/// owner word cannot be compared against a position for that case, because
/// `Running` is a position and "still wants to run" is not; they are the two
/// facts Linux keeps apart in `p->on_rq` and the task state.
pub(super) const fn compare(
    queue: QueueOwner,
    legacy: LegacyPosition,
    legacy_running_is_runnable: bool,
) -> Option<Mismatch> {
    match (queue, legacy) {
        // Exact execution ownership must agree on both the fact and the CPU,
        // and on whether the executing task still wants to run.
        (QueueOwner::Running(queue_cpu, _), LegacyPosition::Running(legacy_cpu))
            if queue_cpu != legacy_cpu =>
        {
            Some(Mismatch::RunningOnDifferentCpus)
        }
        (QueueOwner::Running(_, runnable), LegacyPosition::Running(_)) => {
            if runnable == legacy_running_is_runnable {
                None
            } else {
                Some(Mismatch::RunningRunnableBit)
            }
        }
        (QueueOwner::Running(_, _), LegacyPosition::Transition(_)) => None,
        (QueueOwner::Running(_, _), _) => Some(Mismatch::QueueRunningLegacyNot),
        (_, LegacyPosition::Running(_)) => Some(Mismatch::LegacyRunningQueueNot),

        // A queued task must be runnable, and a runnable task must be queued.
        (QueueOwner::Queued(_), LegacyPosition::Blocked) => Some(Mismatch::QueuedButBlocked),
        (QueueOwner::Blocked | QueueOwner::Dormant, LegacyPosition::Runnable) => {
            Some(Mismatch::RunnableButUnqueued)
        }

        // Lifetime has to agree in both directions.
        (QueueOwner::Retired, LegacyPosition::Runnable | LegacyPosition::Blocked) => {
            Some(Mismatch::Lifetime)
        }
        (QueueOwner::Queued(_) | QueueOwner::Blocked, LegacyPosition::Absent) => {
            Some(Mismatch::Lifetime)
        }

        // Everything else is a handoff half or an agreed position.
        _ => None,
    }
}

/// First mismatch of the window, packed, or 0 for none.
static FIRST_MISMATCH: AtomicU64 = AtomicU64::new(0);
static MISMATCH_COUNT: AtomicU64 = AtomicU64::new(0);

const MISMATCH_PRESENT: u64 = 1 << 63;

/// Records one disagreement.
///
/// First rather than last: a later mismatch is usually the wreckage of the
/// first, and the first is the one whose cause is still nearby.
pub(super) fn record(slot: usize, kind: Mismatch) {
    MISMATCH_COUNT.fetch_add(1, Ordering::Relaxed);
    let packed = MISMATCH_PRESENT | ((slot as u64 & 0xFFFF) << 32) | (kind as u64);
    // ORDERING: Relaxed. The scheduler lock already orders every writer and the
    // value is read only for reporting.
    let _ = FIRST_MISMATCH.compare_exchange(0, packed, Ordering::Relaxed, Ordering::Relaxed);
}

/// Takes the window's record, clearing it for the next window.
pub(super) fn take_window() -> Option<(u64, u64)> {
    let first = FIRST_MISMATCH.swap(0, Ordering::Relaxed);
    let count = MISMATCH_COUNT.swap(0, Ordering::Relaxed);
    (first != 0).then_some((first, count))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn execution_ownership_must_agree_on_the_fact_and_the_cpu() {
        // The mismatch that becomes a task on two CPUs once the global lock is
        // gone, so it is checked from both sides.
        assert_eq!(
            compare(
                QueueOwner::Running(2, true),
                LegacyPosition::Running(2),
                true
            ),
            None
        );
        // The three directions are distinguished, because at one vCPU only two
        // of them are even reachable and a single kind cannot say which.
        assert_eq!(
            compare(
                QueueOwner::Running(2, true),
                LegacyPosition::Running(3),
                true
            ),
            Some(Mismatch::RunningOnDifferentCpus)
        );
        assert_eq!(
            compare(QueueOwner::Running(2, true), LegacyPosition::Blocked, true),
            Some(Mismatch::QueueRunningLegacyNot)
        );
        assert_eq!(
            compare(
                QueueOwner::Queued(Some(1)),
                LegacyPosition::Running(1),
                true
            ),
            Some(Mismatch::LegacyRunningQueueNot)
        );
    }

    #[test]
    fn an_executing_task_must_agree_about_whether_it_still_wants_to_run() {
        // Linux keeps these two apart: `p->on_rq == TASK_ON_RQ_QUEUED` covers a
        // task "actively executing on a CPU or waiting to run", while the task
        // state says whether it has blocked. Without the bit, the owner word
        // cannot answer whether a preempted task returns to its queue.
        assert_eq!(
            compare(
                QueueOwner::Running(1, true),
                LegacyPosition::Running(1),
                true
            ),
            None
        );
        assert_eq!(
            compare(
                QueueOwner::Running(1, false),
                LegacyPosition::Running(1),
                false
            ),
            None
        );
        // A task that blocked while executing but whose bit is still set would
        // be re-queued; the reverse strands a runnable task.
        assert_eq!(
            compare(
                QueueOwner::Running(1, true),
                LegacyPosition::Running(1),
                false
            ),
            Some(Mismatch::RunningRunnableBit)
        );
        assert_eq!(
            compare(
                QueueOwner::Running(1, false),
                LegacyPosition::Running(1),
                true
            ),
            Some(Mismatch::RunningRunnableBit)
        );
        // A different CPU still outranks the bit: that one is fatal after the
        // cutover, this one is a scheduling error.
        assert_eq!(
            compare(
                QueueOwner::Running(1, true),
                LegacyPosition::Running(2),
                false
            ),
            Some(Mismatch::RunningOnDifferentCpus)
        );
    }

    #[test]
    fn a_running_slot_may_still_be_publishing_its_outgoing_transition() {
        // The one legitimate half-state: the owner word has moved on while the
        // outgoing stack is still held. Reporting it would be noise.
        assert_eq!(
            compare(
                QueueOwner::Running(0, true),
                LegacyPosition::Transition(0),
                true
            ),
            None
        );
        assert_eq!(
            compare(
                QueueOwner::Migrating(Some(0)),
                LegacyPosition::Blocked,
                true
            ),
            None
        );
    }

    #[test]
    fn queued_and_runnable_must_mean_the_same_thing() {
        assert_eq!(
            compare(QueueOwner::Queued(Some(0)), LegacyPosition::Blocked, true),
            Some(Mismatch::QueuedButBlocked)
        );
        assert_eq!(
            compare(QueueOwner::Blocked, LegacyPosition::Runnable, true),
            Some(Mismatch::RunnableButUnqueued)
        );
        assert_eq!(
            compare(QueueOwner::Dormant, LegacyPosition::Runnable, true),
            Some(Mismatch::RunnableButUnqueued)
        );
        assert_eq!(
            compare(QueueOwner::Queued(Some(0)), LegacyPosition::Runnable, true),
            None
        );
        assert_eq!(
            compare(QueueOwner::Blocked, LegacyPosition::Blocked, true),
            None
        );
    }

    #[test]
    fn lifetime_must_agree_in_both_directions() {
        assert_eq!(
            compare(QueueOwner::Retired, LegacyPosition::Runnable, true),
            Some(Mismatch::Lifetime)
        );
        assert_eq!(
            compare(QueueOwner::Blocked, LegacyPosition::Absent, true),
            Some(Mismatch::Lifetime)
        );
        assert_eq!(
            compare(QueueOwner::Retired, LegacyPosition::Absent, true),
            None
        );
        assert_eq!(
            compare(QueueOwner::Retiring, LegacyPosition::Retiring, true),
            None
        );
    }

    #[test]
    fn the_window_reports_the_first_mismatch_with_a_total() {
        assert_eq!(take_window(), None);
        record(9, Mismatch::QueuedButBlocked);
        record(11, Mismatch::RunningOnDifferentCpus);
        let (first, count) = take_window().expect("window recorded");
        assert_eq!((first >> 32) & 0xFFFF, 9);
        assert_eq!(first & 0xFF, Mismatch::QueuedButBlocked as u64);
        assert_eq!(count, 2);
        assert_eq!(take_window(), None);
    }
}
