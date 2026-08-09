//! Per-CPU custody for synchronous IPC direct handoff.
//!
//! The scheduler catalog remains the authority for task lifecycle and the
//! runqueue owner word remains the authority for execution placement.  This
//! module owns only the bounded ordering record that lets a completed
//! synchronous reply prefer its exact caller without re-entering the catalog.
//!
//! - **Owner:** one target CPU owns each bounded synchronous-handoff FIFO;
//!   reply records retain exact task and run-owner custody.
//! - **Boundary:** Scheduler may mint an opaque ordering token, but only the
//!   runqueue owner word can authorize dispatch.
//! - **Lifecycle:** enqueue or monotonic same-task refresh, exact selection,
//!   bounded fairness deferral, dispatch, stale discard, or retirement purge.
//! - **Concurrency:** reply producers take one target FIFO after Scheduler is
//!   released; generic producers and dispatch use Scheduler -> FIFO, never the
//!   reverse edge or two FIFO locks at once.
//! - **Failure:** stale identity/generation/CPU/state loses urgency, while
//!   overflow or corrupt prefix fails at the exact bounded invariant.
//! - **Forbidden:** no generic fallback, custody downgrade, second runnable
//!   owner, remote runqueue mutation, allocation, or unbounded burst.
//! - **Evidence:** `synchronous-ipc-handoff/SynchronousIpcHandoff`, exact
//!   implementation mutants, host witnesses, and the SMP KVM matrix.

use nucleus_core::util::lockdep::{LockClass, MAX_TRACKED_CPUS, TrackedSpinLock};

use super::{MAX_CONSECUTIVE_SYNC_HANDOFFS, MAX_TASK, runqueue};

pub(super) type SyncHandoffLock =
    TrackedSpinLock<SyncHandoffState, { LockClass::SchedulerPolicy as u8 }>;

/// An ordering record is keyed by the monotonic task identity as well as its
/// storage slot.  A stale record therefore cannot prioritize a later task
/// after retirement and slot reuse.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct SyncHandoffRecord {
    slot: usize,
    task_id: u64,
    custody: SyncHandoffCustody,
}

/// Generic handoffs establish ordering only. Reply-derived handoffs retain the
/// exact runqueue custody observed after the terminal wake, so an old reply
/// can never regain urgency after migration or a generation change.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum SyncHandoffCustody {
    Generic,
    ReplyWake {
        owner_generation: u64,
        target_cpu: usize,
    },
}

impl SyncHandoffRecord {
    pub(super) const fn new(slot: usize, task_id: u64) -> Self {
        Self {
            slot,
            task_id,
            custody: SyncHandoffCustody::Generic,
        }
    }

    const fn reply_wake(
        slot: usize,
        task_id: u64,
        owner_generation: u64,
        target_cpu: usize,
    ) -> Self {
        Self {
            slot,
            task_id,
            custody: SyncHandoffCustody::ReplyWake {
                owner_generation,
                target_cpu,
            },
        }
    }

    pub(super) const fn slot(self) -> usize {
        self.slot
    }

    pub(super) const fn task_id(self) -> u64 {
        self.task_id
    }

    fn reply_target_cpu(self) -> usize {
        match self.custody {
            SyncHandoffCustody::ReplyWake { target_cpu, .. } => target_cpu,
            SyncHandoffCustody::Generic => {
                panic!("generic synchronous handoff cannot be used as a reply token")
            }
        }
    }

    /// Generic records retain ordinary hint semantics; reply records require
    /// the exact target, generation, runnability, and owner state captured
    /// after their wake. This predicate is also the selection-time check.
    fn matches_dispatch_owner(self, owner: runqueue::RunOwnerSnapshot) -> bool {
        match self.custody {
            SyncHandoffCustody::Generic => true,
            SyncHandoffCustody::ReplyWake {
                owner_generation,
                target_cpu,
            } => {
                owner.generation == owner_generation
                    && owner.cpu == Some(target_cpu)
                    && owner.runnable
                    && matches!(
                        owner.state,
                        runqueue::RunOwnerState::Local | runqueue::RunOwnerState::RemoteQueued
                    )
            }
        }
    }

    pub(super) fn has_current_dispatch_custody(self) -> bool {
        self.matches_dispatch_owner(runqueue::owner(self.slot))
    }

    /// Preserve FIFO position while replacing a same-task record only with
    /// equal-or-newer reply custody. A delayed old reply token must not roll a
    /// fresh generation backward.
    fn refreshes(self, existing: Self) -> bool {
        match (self.custody, existing.custody) {
            (
                SyncHandoffCustody::ReplyWake {
                    owner_generation: incoming,
                    ..
                },
                SyncHandoffCustody::ReplyWake {
                    owner_generation: existing,
                    ..
                },
            ) => incoming >= existing,
            (SyncHandoffCustody::ReplyWake { .. }, SyncHandoffCustody::Generic) => true,
            // A generic hint has no runqueue generation to prove it is newer.
            // It may deduplicate at the same FIFO position, but must never
            // weaken an exact reply handoff into task-id-only urgency.
            (SyncHandoffCustody::Generic, SyncHandoffCustody::ReplyWake { .. })
            | (SyncHandoffCustody::Generic, SyncHandoffCustody::Generic) => false,
        }
    }
}

/// Opaque proof emitted while the scheduler catalog still owns the exact wake
/// transaction.  It is consumed only after that catalog guard has dropped.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(in crate::multitask) struct ReplyWakeHandoff {
    record: SyncHandoffRecord,
}

impl ReplyWakeHandoff {
    pub(super) const fn new(
        slot: usize,
        task_id: u64,
        owner_generation: u64,
        target_cpu: usize,
    ) -> Self {
        Self {
            record: SyncHandoffRecord::reply_wake(slot, task_id, owner_generation, target_cpu),
        }
    }

    pub(super) fn from_owner(
        slot: usize,
        task_id: u64,
        owner: runqueue::RunOwnerSnapshot,
    ) -> Option<Self> {
        let target_cpu = owner.cpu?;
        let token = Self::new(slot, task_id, owner.generation, target_cpu);
        token.matches_owner_snapshot(owner).then_some(token)
    }

    fn owner_still_matches(self) -> bool {
        self.record.has_current_dispatch_custody()
    }

    fn matches_owner_snapshot(self, owner: runqueue::RunOwnerSnapshot) -> bool {
        self.record.matches_dispatch_owner(owner)
    }

    fn target_cpu(self) -> usize {
        self.record.reply_target_cpu()
    }
}

/// The sole per-CPU ordering state for synchronous IPC handoffs.
pub(super) struct SyncHandoffState {
    records: [Option<SyncHandoffRecord>; MAX_TASK],
    head: usize,
    len: usize,
    handoff_streak: u8,
}

impl SyncHandoffState {
    pub(super) const fn new() -> Self {
        Self {
            records: [None; MAX_TASK],
            head: 0,
            len: 0,
            handoff_streak: 0,
        }
    }

    pub(super) fn enqueue(&mut self, record: SyncHandoffRecord) -> bool {
        if let Some(index) = (0..self.len).find_map(|offset| {
            let index = (self.head + offset) % MAX_TASK;
            self.records[index]
                .is_some_and(|queued| queued.task_id == record.task_id)
                .then_some(index)
        }) {
            let existing = self.records[index]
                .expect("scheduler synchronous handoff prefix contains an empty entry");
            if record.refreshes(existing) {
                self.records[index] = Some(record);
            }
            return true;
        }
        if self.len >= MAX_TASK {
            panic!("scheduler synchronous IPC handoff queue overflow");
        }
        let tail = (self.head + self.len) % MAX_TASK;
        self.records[tail] = Some(record);
        self.len += 1;
        true
    }

    pub(super) fn take_next_ready(
        &mut self,
        mut ready: impl FnMut(SyncHandoffRecord) -> bool,
    ) -> Option<usize> {
        if self.handoff_streak >= MAX_CONSECUTIVE_SYNC_HANDOFFS {
            return None;
        }
        while self.len != 0 {
            let index = self.head;
            let record = self.records[index]
                .take()
                .expect("scheduler synchronous handoff prefix contains an empty entry");
            self.head = (index + 1) % MAX_TASK;
            self.len -= 1;
            if ready(record) {
                return Some(record.slot);
            }
        }
        None
    }

    pub(super) fn record_dispatch(&mut self, synchronous_handoff: bool) {
        self.handoff_streak = if synchronous_handoff {
            self.handoff_streak
                .saturating_add(1)
                .min(MAX_CONSECUTIVE_SYNC_HANDOFFS)
        } else {
            0
        };
    }

    pub(super) fn remove_slot(&mut self, slot: usize) {
        let mut compact = [None; MAX_TASK];
        let mut retained = 0_usize;
        for offset in 0..self.len {
            let index = (self.head + offset) % MAX_TASK;
            if let Some(record) = self.records[index]
                && record.slot != slot
            {
                compact[retained] = Some(record);
                retained += 1;
            }
        }
        self.records = compact;
        self.head = 0;
        self.len = retained;
    }

    #[cfg(test)]
    pub(super) fn len(&self) -> usize {
        self.len
    }

    #[cfg(test)]
    pub(super) fn handoff_streak(&self) -> u8 {
        self.handoff_streak
    }

    #[cfg(test)]
    pub(super) fn set_handoff_streak(&mut self, handoff_streak: u8) {
        self.handoff_streak = handoff_streak;
    }
}

/// Host scheduler fixtures must remain isolated, but they execute the same
/// FIFO, identity, and fairness state machine as the production per-CPU
/// backend.  Only the storage owner differs under `cfg(test)`.
#[cfg(test)]
pub(super) const fn per_scheduler_locks() -> [SyncHandoffLock; MAX_TRACKED_CPUS] {
    [const { SyncHandoffLock::new(SyncHandoffState::new()) }; MAX_TRACKED_CPUS]
}

static SYNC_HANDOFFS: [SyncHandoffLock; MAX_TRACKED_CPUS] =
    [const { SyncHandoffLock::new(SyncHandoffState::new()) }; MAX_TRACKED_CPUS];

fn state_for_cpu(cpu: usize) -> &'static SyncHandoffLock {
    SYNC_HANDOFFS
        .get(cpu)
        .expect("scheduler synchronous handoff CPU exceeds capacity")
}

/// Enqueues a catalog-validated generic handoff while the caller still holds
/// the scheduler lock.  This is the only production publisher for non-reply
/// synchronous handoffs.
#[cfg(not(test))]
pub(super) fn enqueue(target_cpu: usize, record: SyncHandoffRecord) -> bool {
    state_for_cpu(target_cpu).lock().enqueue(record)
}

/// Runs the post-catalog reply publication against exactly one target FIFO.
/// The callback receives neither Scheduler nor a generic hint destination, so
/// a stale token has no fallback route by construction.
fn enqueue_reply_wake_after_catalog(
    token: ReplyWakeHandoff,
    mut owner_still_matches: impl FnMut(ReplyWakeHandoff) -> bool,
    mut enqueue_target: impl FnMut(usize, SyncHandoffRecord) -> bool,
) -> bool {
    if !owner_still_matches(token) {
        return false;
    }
    let retained = enqueue_target(token.target_cpu(), token.record);
    // Recheck after taking the target queue lock.  A migration in this gap
    // leaves at most a stale ordering record, which selection rejects through
    // the local runqueue owner check; do not reacquire Scheduler or publish a
    // second record as a fallback.
    owner_still_matches(token) && retained
}

/// Consume a reply token without acquiring the scheduler catalog lock.  A
/// changed owner generation, target CPU, terminal state, or withdrawn
/// runnability merely drops urgency; it can never publish another owner.
pub(super) fn enqueue_reply_wake(token: ReplyWakeHandoff) -> bool {
    enqueue_reply_wake_after_catalog(
        token,
        ReplyWakeHandoff::owner_still_matches,
        |target_cpu, record| state_for_cpu(target_cpu).lock().enqueue(record),
    )
}

/// Consumes this CPU's FIFO while the scheduler catalog validates each exact
/// identity and local runqueue custody.
#[cfg(not(test))]
pub(super) fn take_next_ready(
    cpu: usize,
    ready: impl FnMut(SyncHandoffRecord) -> bool,
) -> Option<usize> {
    state_for_cpu(cpu).lock().take_next_ready(ready)
}

#[cfg(not(test))]
pub(super) fn record_dispatch(cpu: usize, synchronous_handoff: bool) {
    state_for_cpu(cpu)
        .lock()
        .record_dispatch(synchronous_handoff);
}

#[cfg(not(test))]
pub(super) fn remove_slot_all_cpus(slot: usize) {
    for state in &SYNC_HANDOFFS {
        state.lock().remove_slot(slot);
    }
}

#[cfg(not(test))]
pub(super) fn reset_all_cpus() {
    for state in &SYNC_HANDOFFS {
        *state.lock() = SyncHandoffState::new();
    }
}

#[cfg(test)]
mod tests {
    use super::{
        MAX_CONSECUTIVE_SYNC_HANDOFFS, ReplyWakeHandoff, SyncHandoffRecord, SyncHandoffState,
        runqueue,
    };

    #[test]
    fn sync_handoff_state_is_fifo_deduplicated_identity_keyed_and_fairness_bounded() {
        let mut state = SyncHandoffState::new();
        let first = SyncHandoffRecord::new(4, 40);
        let second = SyncHandoffRecord::new(5, 50);
        assert!(state.enqueue(first));
        assert!(state.enqueue(first));
        assert!(state.enqueue(second));
        assert_eq!(state.len(), 2);
        assert_eq!(state.take_next_ready(|_| true), Some(4));
        state.record_dispatch(true);
        assert_eq!(state.take_next_ready(|_| true), Some(5));

        assert!(state.enqueue(first));
        assert!(state.enqueue(second));
        state.set_handoff_streak(MAX_CONSECUTIVE_SYNC_HANDOFFS);
        assert_eq!(state.take_next_ready(|_| true), None);
        assert_eq!(state.len(), 2);
        state.record_dispatch(false);
        assert_eq!(state.handoff_streak(), 0);
        assert_eq!(state.take_next_ready(|_| true), Some(4));
    }

    #[test]
    fn slot_reuse_record_is_rejected_by_exact_task_identity() {
        let mut state = SyncHandoffState::new();
        assert!(state.enqueue(SyncHandoffRecord::new(7, 71)));
        assert_eq!(
            state.take_next_ready(|record| record.task_id() == 72),
            None,
            "a previous task in a reused slot must not receive the new task's handoff"
        );
        assert!(state.enqueue(SyncHandoffRecord::new(7, 72)));
        assert_eq!(
            state.take_next_ready(|record| record.task_id() == 72),
            Some(7)
        );
    }

    #[test]
    fn stale_or_migrated_records_only_lose_urgency_without_double_custody() {
        let mut state = SyncHandoffState::new();
        let stale = SyncHandoffRecord::new(9, 91);
        let live = SyncHandoffRecord::new(10, 101);
        assert!(state.enqueue(stale));
        assert!(state.enqueue(live));
        assert_eq!(
            state.take_next_ready(|record| record == live),
            Some(10),
            "a stale/migrated record is discarded instead of selecting a foreign owner"
        );
        assert_eq!(state.len(), 0);
    }

    #[test]
    fn reply_wake_token_rejects_stale_owner_generation_migration_and_retirement() {
        let token = ReplyWakeHandoff::new(12, 121, 7, 2);
        let live = runqueue::RunOwnerSnapshot {
            state: runqueue::RunOwnerState::RemoteQueued,
            cpu: Some(2),
            generation: 7,
            runnable: true,
        };
        assert!(token.matches_owner_snapshot(live));
        assert!(
            !token.matches_owner_snapshot(runqueue::RunOwnerSnapshot {
                generation: 8,
                ..live
            }),
            "a changed owner generation must lose urgency"
        );
        assert!(
            !token.matches_owner_snapshot(runqueue::RunOwnerSnapshot {
                cpu: Some(3),
                generation: 7,
                ..live
            }),
            "migration to another CPU must reject the old target token"
        );
        assert!(
            !token.matches_owner_snapshot(runqueue::RunOwnerSnapshot {
                runnable: false,
                ..live
            }),
            "withdrawn runnability must reject the old reply token"
        );
        assert!(
            !token.matches_owner_snapshot(runqueue::RunOwnerSnapshot {
                state: runqueue::RunOwnerState::Running,
                ..live
            }),
            "an execution owner is not dispatch-handoff custody"
        );
        assert!(
            !token.matches_owner_snapshot(runqueue::RunOwnerSnapshot {
                state: runqueue::RunOwnerState::Retired,
                cpu: None,
                generation: 9,
                runnable: false,
            }),
            "retirement must not leave token dispatch authority"
        );
    }

    #[test]
    fn reply_generation_refresh_replaces_in_place_and_stale_generation_loses_urgency() {
        let mut state = SyncHandoffState::new();
        let old = SyncHandoffRecord::reply_wake(12, 121, 7, 2);
        let fresh = SyncHandoffRecord::reply_wake(12, 121, 8, 2);
        let owner_at_eight = runqueue::RunOwnerSnapshot {
            state: runqueue::RunOwnerState::Local,
            cpu: Some(2),
            generation: 8,
            runnable: true,
        };

        assert!(state.enqueue(old));
        assert_eq!(
            state.take_next_ready(|record| record.matches_dispatch_owner(owner_at_eight)),
            None,
            "generation seven must not regain urgency when the same task returns on generation eight"
        );

        assert!(state.enqueue(old));
        assert!(state.enqueue(fresh));
        assert_eq!(state.len(), 1, "fresh custody replaces in FIFO position");
        assert_eq!(
            state.take_next_ready(|record| record.matches_dispatch_owner(owner_at_eight)),
            Some(12),
            "the exact generation-eight reply handoff remains retained"
        );
        assert_eq!(
            state.len(),
            0,
            "refresh creates neither a duplicate nor a fallback record"
        );

        assert!(state.enqueue(fresh));
        assert!(state.enqueue(old));
        assert_eq!(
            state.len(),
            1,
            "an old reply cannot duplicate fresh custody"
        );
        assert_eq!(
            state.take_next_ready(|record| record.matches_dispatch_owner(owner_at_eight)),
            Some(12),
            "a delayed generation-seven token must not roll generation eight backward"
        );

        assert!(state.enqueue(SyncHandoffRecord::new(12, 121)));
        assert!(state.enqueue(fresh));
        assert_eq!(
            state.len(),
            1,
            "reply custody upgrades one generic position"
        );
        assert_eq!(
            state.take_next_ready(|record| {
                record == fresh && record.matches_dispatch_owner(owner_at_eight)
            }),
            Some(12),
            "a fresh reply must replace task-id-only urgency with exact custody"
        );
    }

    #[test]
    fn generic_handoff_cannot_downgrade_reply_generation_custody() {
        let mut state = SyncHandoffState::new();
        let reply = SyncHandoffRecord::reply_wake(12, 121, 7, 2);
        let generic = SyncHandoffRecord::new(12, 121);
        let changed_owner = runqueue::RunOwnerSnapshot {
            state: runqueue::RunOwnerState::Local,
            cpu: Some(2),
            generation: 8,
            runnable: true,
        };

        assert!(state.enqueue(reply));
        assert!(state.enqueue(generic));
        assert_eq!(state.len(), 1, "same task retains one FIFO position");
        assert_eq!(
            state.take_next_ready(|record| record.matches_dispatch_owner(changed_owner)),
            None,
            "a generic handoff must not strip reply generation custody"
        );
    }

    #[test]
    fn stale_reply_enqueue_has_no_scheduler_or_global_fallback() {
        let token = ReplyWakeHandoff::new(12, 121, 7, 2);
        let mut target_enqueue_calls = 0;
        assert!(
            !super::enqueue_reply_wake_after_catalog(
                token,
                |_| false,
                |_, _| {
                    target_enqueue_calls += 1;
                    true
                }
            ),
            "a stale reply token must only lose urgency"
        );
        assert_eq!(
            target_enqueue_calls, 0,
            "stale reply publication must not enqueue through a second destination"
        );
    }

    #[test]
    fn reply_enqueue_rechecks_owner_after_the_exact_target_queue_mutation() {
        let token = ReplyWakeHandoff::new(12, 121, 7, 2);
        let mut owner_observations = 0;
        let mut target_enqueue_calls = 0;
        assert!(
            !super::enqueue_reply_wake_after_catalog(
                token,
                |_| {
                    owner_observations += 1;
                    owner_observations == 1
                },
                |target_cpu, record| {
                    target_enqueue_calls += 1;
                    assert_eq!(target_cpu, 2);
                    assert_eq!(record.task_id(), 121);
                    true
                }
            ),
            "custody lost after enqueue must not be reported as a retained direct handoff"
        );
        assert_eq!(owner_observations, 2, "the owner is sampled on both sides");
        assert_eq!(target_enqueue_calls, 1, "only the captured CPU is touched");
    }
}
