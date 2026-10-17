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
//!   owner, remote runqueue mutation, allocation, or unbounded burst while a
//!   fair competitor is ready.
//! - **Evidence:** `synchronous-ipc-handoff/SynchronousIpcHandoff`, exact
//!   implementation mutants, host witnesses, and the SMP KVM matrix.

use nucleus_core::util::lockdep::{LockClass, MAX_TRACKED_CPUS};
use crate::multitask::irq_leaf::IrqLeaf;

use super::{MAX_SYNC_HANDOFF_CHAIN_TICKS, MAX_TASK, runqueue};

pub(super) type SyncHandoffLock =
    IrqLeaf<SyncHandoffState, { LockClass::SchedulerPolicy as u8 }>;

/// An ordering record is keyed by the monotonic task identity as well as its
/// storage slot.  A stale record therefore cannot prioritize a later task
/// after retirement and slot reuse.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct SyncHandoffRecord {
    slot: usize,
    task_id: u64,
    custody: SyncHandoffCustody,
    // Bound only after taking a Generic FIFO record (or at an immediate
    // handoff). Queued Generic records remain ordering-only and refreshable.
    selected_local_owner: Option<(u64, usize)>,
}

/// Generic handoffs establish ordering only. Reply-derived handoffs retain the
/// exact runqueue custody observed after the terminal wake, so an old reply
/// can never regain urgency after migration or a generation change.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum SyncHandoffCustody {
    Generic,
    Direct {
        owner_generation: u64,
        target_cpu: usize,
    },
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
            selected_local_owner: None,
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
            selected_local_owner: None,
        }
    }

    pub(super) const fn slot(self) -> usize {
        self.slot
    }

    pub(super) const fn task_id(self) -> u64 {
        self.task_id
    }

    pub(super) fn bound_direct_generation(self) -> Option<u64> {
        match self.custody {
            SyncHandoffCustody::Direct { owner_generation, .. } => Some(owner_generation),
            _ => None,
        }
    }

    /// Exact FIFO proofs and consumed Generic records bound to a Local owner
    /// carry one generation through final frame validation and claim.
    pub(super) fn claim_generation(self) -> Option<u64> {
        match self.custody {
            SyncHandoffCustody::Generic => self.selected_local_owner.map(|(generation, _)| generation),
            SyncHandoffCustody::Direct {
                owner_generation, ..
            }
            | SyncHandoffCustody::ReplyWake {
                owner_generation, ..
            } => Some(owner_generation),
        }
    }

    /// A direct IPC receiver has no fair membership, so its FIFO urgency
    /// belongs to the exact owner generation observed at enqueue. Other
    /// generic hints keep their existing catalog-validated semantics.
    pub(super) fn bind_direct_owner(
        mut self,
        owner: runqueue::RunOwnerSnapshot,
        target_cpu: usize,
    ) -> Self {
        if matches!(self.custody, SyncHandoffCustody::Generic)
            && owner.state == runqueue::RunOwnerState::DirectHandoff
            && owner.cpu == Some(target_cpu)
            && owner.runnable
        {
            self.custody = SyncHandoffCustody::Direct {
                owner_generation: owner.generation,
                target_cpu,
            };
        }
        self
    }

    /// Bind a *consumed* Generic hint to the exact Local owner observed at
    /// selection. Never attach this token while the record is still queued.
    pub(super) fn bind_selected_local_owner(
        mut self,
        owner: runqueue::RunOwnerSnapshot,
        cpu: usize,
    ) -> Self {
        if matches!(self.custody, SyncHandoffCustody::Generic)
            && owner.state == runqueue::RunOwnerState::Local
            && owner.cpu == Some(cpu)
            && owner.runnable
        {
            self.selected_local_owner = Some((owner.generation, cpu));
        }
        self
    }

    fn reply_target_cpu(self) -> usize {
        match self.custody {
            SyncHandoffCustody::ReplyWake { target_cpu, .. } => target_cpu,
            SyncHandoffCustody::Generic | SyncHandoffCustody::Direct { .. } => {
                panic!("generic synchronous handoff cannot be used as a reply token")
            }
        }
    }

    /// Generic records retain ordinary hint semantics; reply records require
    /// the exact target, generation, runnability, and owner state captured
    /// after their wake. This predicate is also the selection-time check.
    fn matches_dispatch_owner(self, owner: runqueue::RunOwnerSnapshot) -> bool {
        match self.custody {
            SyncHandoffCustody::Generic => self.selected_local_owner.is_none_or(|(generation, cpu)| {
                owner.state == runqueue::RunOwnerState::Local
                    && owner.cpu == Some(cpu)
                    && owner.generation == generation
                    && owner.runnable
            }),
            SyncHandoffCustody::Direct {
                owner_generation,
                target_cpu,
            } => {
                owner.state == runqueue::RunOwnerState::DirectHandoff
                    && owner.cpu == Some(target_cpu)
                    && owner.generation == owner_generation
                    && owner.runnable
            }
            SyncHandoffCustody::ReplyWake {
                owner_generation,
                target_cpu,
            } => {
                // Each condition below is negated with `!(...)` rather than
                // by flipping its operator: `formal/implementation-mutations.tsv`
                // pins the positive comparison's exact source text
                // (sync-handoff-owner-{cpu,generation}-bypass) to mutate it.
                #[allow(clippy::nonminimal_bool)]
                if !(owner.generation == owner_generation) {
                    #[cfg(rustos_scheduler_phase_profile)]
                    {
                        super::locality::record_sync_handoff_reply_custody_fail(
                            super::locality::SyncHandoffReplyCustodyFailReason::Generation,
                        );
                        super::locality::record_sync_handoff_generation_fail_state(
                            owner.state as usize,
                        );
                    }
                    return false;
                }
                #[allow(clippy::nonminimal_bool)]
                if !(owner.cpu == Some(target_cpu)) {
                    #[cfg(rustos_scheduler_phase_profile)]
                    super::locality::record_sync_handoff_reply_custody_fail(
                        super::locality::SyncHandoffReplyCustodyFailReason::Cpu,
                    );
                    return false;
                }
                if !runqueue::is_handoff_dispatchable_owner(owner) {
                    #[cfg(rustos_scheduler_phase_profile)]
                    {
                        let reason = if owner.runnable {
                            super::locality::SyncHandoffReplyCustodyFailReason::State
                        } else {
                            super::locality::SyncHandoffReplyCustodyFailReason::NotRunnable
                        };
                        super::locality::record_sync_handoff_reply_custody_fail(reason);
                    }
                    return false;
                }
                true
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
                SyncHandoffCustody::Direct {
                    owner_generation: incoming,
                    ..
                },
                SyncHandoffCustody::Direct {
                    owner_generation: existing,
                    ..
                },
            ) => incoming >= existing,
            (
                SyncHandoffCustody::ReplyWake {
                    owner_generation: incoming,
                    ..
                },
                SyncHandoffCustody::ReplyWake {
                    owner_generation: existing,
                    ..
                }
                | SyncHandoffCustody::Direct {
                    owner_generation: existing,
                    ..
                },
            ) => incoming >= existing,
            (SyncHandoffCustody::ReplyWake { .. }, SyncHandoffCustody::Generic) => true,
            (SyncHandoffCustody::Direct { .. }, SyncHandoffCustody::Generic) => true,
            // A generic hint has no runqueue generation to prove it is newer.
            // It may deduplicate at the same FIFO position, but must never
            // weaken an exact reply handoff into task-id-only urgency.
            (SyncHandoffCustody::Direct { .. }, SyncHandoffCustody::ReplyWake { .. })
            | (SyncHandoffCustody::Generic, SyncHandoffCustody::Direct { .. })
            | (SyncHandoffCustody::Generic, SyncHandoffCustody::ReplyWake { .. })
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

    pub(super) fn target_cpu(self) -> usize {
        self.record.reply_target_cpu()
    }
}

/// The sole per-CPU ordering state for synchronous IPC handoffs.
pub(super) struct SyncHandoffState {
    records: [Option<SyncHandoffRecord>; MAX_TASK],
    head: usize,
    len: usize,
    handoff_streak: u8,
    last_handoff_tick: u64,
}

impl SyncHandoffState {
    pub(super) const fn new() -> Self {
        Self {
            records: [None; MAX_TASK],
            head: 0,
            len: 0,
            handoff_streak: 0,
            last_handoff_tick: 0,
        }
    }

    pub(super) fn enqueue(&mut self, record: SyncHandoffRecord) -> bool {
        self.enqueue_with_direct_owner(record, None)
    }

    /// A fresh, exact DirectHandoff owner may supersede a stale reply-wake
    /// record at the same FIFO position. A generic hint without that owner
    /// proof must never downgrade an exact live reply generation.
    fn enqueue_with_direct_owner(
        &mut self,
        record: SyncHandoffRecord,
        direct_owner: Option<(runqueue::RunOwnerSnapshot, usize)>,
    ) -> bool {
        let record = direct_owner
            .map(|(owner, cpu)| record.bind_direct_owner(owner, cpu))
            .unwrap_or(record);
        if let Some(index) = (0..self.len).find_map(|offset| {
            let index = (self.head + offset) % MAX_TASK;
            self.records[index]
                .is_some_and(|queued| queued.task_id == record.task_id)
                .then_some(index)
        }) {
            let existing = self.records[index]
                .expect("scheduler synchronous handoff prefix contains an empty entry");
            let supersedes_stale_reply =
                matches!(record.custody, SyncHandoffCustody::Direct { .. })
                    && matches!(existing.custody, SyncHandoffCustody::ReplyWake { .. })
                    && direct_owner.is_some_and(|(owner, target_cpu)| {
                        owner.state == runqueue::RunOwnerState::DirectHandoff
                            && owner.runnable
                            && owner.cpu == Some(target_cpu)
                            && (existing.slot != record.slot
                                || !existing.matches_dispatch_owner(owner))
                    });
            if record.refreshes(existing) || supersedes_stale_reply {
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
        ready: impl FnMut(SyncHandoffRecord) -> bool,
    ) -> Option<usize> {
        self.take_next_ready_with_competitor(true, ready)
    }

    pub(super) fn take_next_ready_with_competitor(
        &mut self,
        fair_competitor_ready: bool,
        ready: impl FnMut(SyncHandoffRecord) -> bool,
    ) -> Option<usize> {
        self.take_next_ready_record_with_competitor(fair_competitor_ready, ready)
            .map(SyncHandoffRecord::slot)
    }

    /// Preserve the exact FIFO proof through selection. Returning only a slot
    /// discards the generation that a final dispatch claim must compare.
    pub(super) fn take_next_ready_record_with_competitor(
        &mut self,
        fair_competitor_ready: bool,
        mut ready: impl FnMut(SyncHandoffRecord) -> bool,
    ) -> Option<SyncHandoffRecord> {
        if fair_competitor_ready && self.handoff_streak >= MAX_SYNC_HANDOFF_CHAIN_TICKS {
            #[cfg(rustos_scheduler_phase_profile)]
            super::locality::record_sync_handoff_miss(
                super::locality::SyncHandoffMissReason::ChainBudgetExhausted,
            );
            return None;
        }
        if self.len == 0 {
            #[cfg(rustos_scheduler_phase_profile)]
            super::locality::record_sync_handoff_miss(
                super::locality::SyncHandoffMissReason::StartedEmpty,
            );
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
                return Some(record);
            }
        }
        #[cfg(rustos_scheduler_phase_profile)]
        super::locality::record_sync_handoff_miss(
            super::locality::SyncHandoffMissReason::DrainedStale,
        );
        None
    }

    pub(super) fn record_dispatch(&mut self, synchronous_handoff: bool, now_ticks: u64) {
        if !synchronous_handoff {
            self.handoff_streak = 0;
            self.last_handoff_tick = 0;
            return;
        }
        let now_ticks = now_ticks.max(1);
        if self.last_handoff_tick == 0 {
            self.handoff_streak = 1;
            self.last_handoff_tick = now_ticks;
            return;
        }
        let elapsed = now_ticks.saturating_sub(self.last_handoff_tick);
        if elapsed != 0 {
            self.handoff_streak = self
                .handoff_streak
                .saturating_add(u8::try_from(elapsed).unwrap_or(u8::MAX))
                .min(MAX_SYNC_HANDOFF_CHAIN_TICKS);
            self.last_handoff_tick = now_ticks;
        }
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
        self.last_handoff_tick = (handoff_streak != 0) as u64;
    }
}

static SYNC_HANDOFFS: [SyncHandoffLock; MAX_TRACKED_CPUS] =
    [const { SyncHandoffLock::new(SyncHandoffState::new()) }; MAX_TRACKED_CPUS];

fn state_for_cpu(cpu: usize) -> &'static SyncHandoffLock {
    SYNC_HANDOFFS
        .get(cpu)
        .expect("scheduler synchronous handoff CPU exceeds capacity")
}

/// Lock-free "this CPU's synchronous FIFO may hold a record".
///
/// `take_next_ready` ran on every dispatch and took this CPU's FIFO lock only
/// to read `len == 0`, which is the answer on the large majority of them -
/// 26.5 ms/s of lock time at eight vCPUs, the second largest step in the
/// handoff chain after the dispatch policy guard.
///
/// One-sided in the same way as the atomic-activation hint: an enqueue sets
/// it, and only the consumer clears it, after observing the FIFO drained under
/// the same lock. A stale `true` costs one acquisition, exactly what every
/// dispatch paid before. A stale `false` would strand a committed reply, and
/// cannot arise, because a successful enqueue always publishes and the
/// consumer only clears on an empty queue it has seen under the lock.
static SYNC_HANDOFF_PENDING: [core::sync::atomic::AtomicBool; MAX_TRACKED_CPUS] =
    [const { core::sync::atomic::AtomicBool::new(false) }; MAX_TRACKED_CPUS];

/// Per-CPU consumed scheduling-clock quanta and last charge point for the
/// production dispatcher's synchronous IPC execution chain.
///
/// Only that CPU's interrupt-disabled scheduler transaction reads or writes
/// its element. Keeping the scalar outside `SyncHandoffState` avoids taking
/// the FIFO lock a second time after every selection merely to update one
/// byte; atomics also make reset publication explicit without weakening the
/// bounded eight-tick execution-context budget.
// ORDERING: One interrupt-disabled scheduler transaction owns each CPU-local
// streak. Relaxed accesses preserve the local bound; reset release only
// publishes the cleared diagnostic/scheduling state to a later owner.
static SYNC_HANDOFF_STREAK: [core::sync::atomic::AtomicU8; MAX_TRACKED_CPUS] =
    [const { core::sync::atomic::AtomicU8::new(0) }; MAX_TRACKED_CPUS];
// ORDERING: This timestamp is part of the same single-owner CPU-local budget
// state as the streak above; no cross-CPU publication edge is carried by it.
static SYNC_HANDOFF_LAST_TICK: [core::sync::atomic::AtomicU64; MAX_TRACKED_CPUS] =
    [const { core::sync::atomic::AtomicU64::new(0) }; MAX_TRACKED_CPUS];

/// Enqueue into one target FIFO and advertise it to that CPU's dispatcher.
fn enqueue_and_publish(target_cpu: usize, record: SyncHandoffRecord) -> bool {
    // Publish the record and its advertisement as one FIFO transaction.
    // Dropping the temporary guard before `pending = true` allowed the target
    // CPU to observe an already-committed record as absent and miss the direct
    // handoff for one scheduling turn.
    let mut state = state_for_cpu(target_cpu).lock();
    // Under Scheduler, a newly published direct owner is newer than a
    // queued reply-wake proof for the same task. Compare its *current* owner
    // generation inside this FIFO transaction; otherwise generic dedup keeps
    // the stale reply, whose later rejection silently drops the sole direct
    // ordering token while the task has no fair-runqueue membership.
    let owner = runqueue::owner(record.slot());
    let retained = state.enqueue_with_direct_owner(record, Some((owner, target_cpu)));
    if retained {
        if let Some(pending) = SYNC_HANDOFF_PENDING.get(target_cpu) {
            // ORDERING: release publishes the enqueued record before the flag
            // while the FIFO guard prevents a consumer from draining and
            // clearing the same publication between those two operations.
            pending.store(true, core::sync::atomic::Ordering::Release);
        }
    }
    retained
}

/// Short leaf join for an ORIGINAL IPC receiver. Capacity precedes its exact
/// owner CAS/wait clear, under the same FIFO leaf; no rollback or incomplete
/// prefix is exposed. `commit` may acquire only Wait58 and perform scalar CAS.
/// Physical reader/binding loans and masked CPU custody remain outside leaves.
pub(super) fn publish_original(
    target_cpu: usize,
    record: SyncHandoffRecord,
    commit: impl FnOnce() -> Option<runqueue::RunOwnerSnapshot>,
) -> Option<runqueue::RunOwnerSnapshot> {
    let mut state = state_for_cpu(target_cpu).lock();
    let duplicate = (0..state.len).any(|offset| {
        state.records[(state.head + offset) % MAX_TASK]
            .is_some_and(|queued| queued.task_id == record.task_id)
    });
    if !duplicate && state.len == MAX_TASK {
        return None;
    }
    let owner = commit()?;
    assert!(state.enqueue_with_direct_owner(record, Some((owner, target_cpu))));
    // ORDERING: publish the committed FIFO prefix before any pending consumer.
    SYNC_HANDOFF_PENDING[target_cpu].store(true, core::sync::atomic::Ordering::Release);
    Some(owner)
}

/// Explicit native exhausted-capacity input only. No UID/record is backing,
/// funding or execution permission; the actual caller still owns real loans.
#[cfg(test)]
pub(super) fn with_full_fifo_model<R>(cpu: usize, f: impl FnOnce() -> R) -> R {
    let mut full = SyncHandoffState::new();
    for slot in 0..MAX_TASK {
        full.enqueue(SyncHandoffRecord::new(slot, 900000 + slot as u64));
    }
    let saved = core::mem::replace(&mut *state_for_cpu(cpu).lock(), full);
    // ORDERING: fixture takes and replaces the previous pending publication.
    let pending = SYNC_HANDOFF_PENDING[cpu].swap(true, core::sync::atomic::Ordering::AcqRel);
    struct Restore(usize, Option<SyncHandoffState>, bool);
    impl Drop for Restore {
        fn drop(&mut self) {
            *state_for_cpu(self.0).lock() = self.1.take().unwrap();
            // ORDERING: restore FIFO contents before their original pending flag.
            SYNC_HANDOFF_PENDING[self.0].store(self.2, core::sync::atomic::Ordering::Release);
        }
    }
    let _restore = Restore(cpu, Some(saved), pending);
    f()
}

/// Whether taking the FIFO leaf can find anything. Production and native
/// owner witnesses observe the SAME pending publication and queue.
pub(super) fn pending(cpu: usize) -> bool {
    SYNC_HANDOFF_PENDING.get(cpu).is_some_and(|pending| {
        // ORDERING: acquire pairs with the enqueueing CPU's release, so
        // observing the flag also observes the record it advertises.
        pending.load(core::sync::atomic::Ordering::Acquire)
    })
}

/// Publishes an original caller-qualified handoff into the actual per-CPU
/// FIFO. Exact Direct owner generation is bound before publication; neither
/// the scalar peer hint nor a native fixture queue can substitute for it.
pub(super) fn enqueue(target_cpu: usize, record: SyncHandoffRecord) -> bool {
    enqueue_and_publish(target_cpu, record)
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
    let accepted = enqueue_reply_wake_after_catalog(
        token,
        ReplyWakeHandoff::owner_still_matches,
        enqueue_and_publish,
    );
    #[cfg(rustos_scheduler_phase_profile)]
    super::locality::record_sync_handoff_arm(super::locality::SyncHandoffArmSite::Reply, accepted);
    accepted
}

/// Consumes this CPU's FIFO while the independent dispatcher qualifies exact
/// published identity and runqueue custody. Preserve the consumed record's owner
/// generation for the original physical claim.
pub(super) fn take_next_ready(
    cpu: usize,
    fair_competitor_ready: bool,
    ready: impl FnMut(SyncHandoffRecord) -> bool,
) -> Option<SyncHandoffRecord> {
    if fair_competitor_ready
        && SYNC_HANDOFF_STREAK[cpu].load(core::sync::atomic::Ordering::Relaxed)
            >= MAX_SYNC_HANDOFF_CHAIN_TICKS
    {
        #[cfg(rustos_scheduler_phase_profile)]
        super::locality::record_sync_handoff_miss(
            super::locality::SyncHandoffMissReason::ChainBudgetExhausted,
        );
        return None;
    }
    let mut state = state_for_cpu(cpu).lock();
    debug_assert_eq!(state.handoff_streak, 0);
    let taken = state.take_next_ready_record_with_competitor(fair_competitor_ready, ready);
    // Clear on any queue this CPU has just seen empty under the lock. An
    // enqueue publishes `true` before releasing this same FIFO lock.
    if state.len == 0 {
        if let Some(pending) = SYNC_HANDOFF_PENDING.get(cpu) {
            // ORDERING: release keeps the clear behind the locked drain so a
            // producer observing false cannot miss an unconsumed record.
            pending.store(false, core::sync::atomic::Ordering::Release);
        }
    }
    taken
}

/// Whether an exact same-CPU handoff may bypass the FIFO without weakening
/// the bounded chain rule. The caller still checks for a fair competitor and
/// records the accepted dispatch through `record_dispatch`.
#[cfg(not(test))]
pub(super) fn immediate_handoff_allowed(cpu: usize, fair_competitor_ready: bool) -> bool {
    !fair_competitor_ready
        || SYNC_HANDOFF_STREAK[cpu].load(core::sync::atomic::Ordering::Relaxed)
            < MAX_SYNC_HANDOFF_CHAIN_TICKS
}

pub(super) fn record_dispatch(cpu: usize, synchronous_handoff: bool, now_ticks: u64) {
    let streak = &SYNC_HANDOFF_STREAK[cpu];
    let last_tick = &SYNC_HANDOFF_LAST_TICK[cpu];
    if !synchronous_handoff {
        streak.store(0, core::sync::atomic::Ordering::Relaxed);
        last_tick.store(0, core::sync::atomic::Ordering::Relaxed);
        return;
    }
    let now_ticks = now_ticks.max(1);
    let previous_tick = last_tick.load(core::sync::atomic::Ordering::Relaxed);
    if previous_tick == 0 {
        streak.store(1, core::sync::atomic::Ordering::Relaxed);
        last_tick.store(now_ticks, core::sync::atomic::Ordering::Relaxed);
        return;
    }
    let elapsed = now_ticks.saturating_sub(previous_tick);
    if elapsed != 0 {
        let next = streak
            .load(core::sync::atomic::Ordering::Relaxed)
            .saturating_add(u8::try_from(elapsed).unwrap_or(u8::MAX))
            .min(MAX_SYNC_HANDOFF_CHAIN_TICKS);
        streak.store(next, core::sync::atomic::Ordering::Relaxed);
        last_tick.store(now_ticks, core::sync::atomic::Ordering::Relaxed);
    }
}

pub(super) fn remove_slot_all_cpus(slot: usize) {
    for state in &SYNC_HANDOFFS {
        state.lock().remove_slot(slot);
    }
}

/// Read-only native witness of the actual shared FIFO, not execution permission.
#[cfg(test)]
pub(super) fn model_len(cpu: usize) -> usize {
    state_for_cpu(cpu).lock().len()
}

pub(super) fn reset_all_cpus() {
    for (cpu, state) in SYNC_HANDOFFS.iter().enumerate() {
        *state.lock() = SyncHandoffState::new();
        // ORDERING: Reset publishes the cleared CPU-local streak after the
        // protected FIFO state is empty; subsequent relaxed local accesses
        // cannot revive a pre-reset fairness debt.
        SYNC_HANDOFF_STREAK[cpu].store(0, core::sync::atomic::Ordering::Release);
        SYNC_HANDOFF_LAST_TICK[cpu].store(0, core::sync::atomic::Ordering::Release);
    }
}

#[cfg(test)]
mod tests {
    use super::{
        MAX_SYNC_HANDOFF_CHAIN_TICKS, ReplyWakeHandoff, SyncHandoffRecord, SyncHandoffState,
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
        state.record_dispatch(true, 1);
        state.record_dispatch(true, 1);
        assert_eq!(
            state.handoff_streak(),
            1,
            "message count within one clock quantum must not consume more budget"
        );
        assert_eq!(state.take_next_ready(|_| true), Some(5));
        state.record_dispatch(true, u64::from(MAX_SYNC_HANDOFF_CHAIN_TICKS));
        assert_eq!(state.handoff_streak(), MAX_SYNC_HANDOFF_CHAIN_TICKS);

        assert!(state.enqueue(first));
        assert!(state.enqueue(second));
        assert_eq!(state.take_next_ready(|_| true), None);
        assert_eq!(state.len(), 2);
        assert_eq!(
            state.take_next_ready_with_competitor(false, |_| true),
            Some(4),
            "an uncontended IPC execution chain must remain work-conserving"
        );
        state.set_handoff_streak(MAX_SYNC_HANDOFF_CHAIN_TICKS);
        assert_eq!(state.take_next_ready(|_| true), None);
        assert_eq!(state.len(), 1);
        state.record_dispatch(false, 1);
        assert_eq!(state.handoff_streak(), 0);
        assert_eq!(state.take_next_ready(|_| true), Some(5));
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
            wait_reason_kind: 0,
            wait_armed: false,
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
                wait_reason_kind: 0,
                wait_armed: false,
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
            wait_reason_kind: 0,
            wait_armed: false,
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
            wait_reason_kind: 0,
            wait_armed: false,
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
    fn fresh_direct_owner_replaces_only_a_stale_reply_token_at_the_same_fifo_position() {
        let reply = SyncHandoffRecord::reply_wake(12, 121, 7, 2);
        let generic = SyncHandoffRecord::new(12, 121);
        let owner = runqueue::RunOwnerSnapshot {
            state: runqueue::RunOwnerState::DirectHandoff,
            cpu: Some(2),
            generation: 7,
            runnable: true,
            wait_reason_kind: 0,
            wait_armed: false,
        };
        let mut state = SyncHandoffState::new();
        assert!(state.enqueue(reply));
        assert!(state.enqueue_with_direct_owner(generic, Some((owner, 2))));
        assert_eq!(
            state.records[state.head],
            Some(reply),
            "live exact reply cannot be downgraded"
        );
        let newer = runqueue::RunOwnerSnapshot {
            generation: 8,
            ..owner
        };
        assert!(state.enqueue_with_direct_owner(generic, Some((newer, 1))));
        assert_eq!(
            state.records[state.head],
            Some(reply),
            "wrong-CPU proof is not authority"
        );
        assert!(state.enqueue_with_direct_owner(generic, Some((newer, 2))));
        assert_eq!(state.len(), 1, "replacement retains one FIFO position");
        assert_eq!(
            state.records[state.head],
            Some(generic.bind_direct_owner(newer, 2))
        );
        assert_eq!(
            state.take_next_ready_with_competitor(false, |record| {
                record.matches_dispatch_owner(newer)
            }),
            Some(12),
            "the fresh DirectHandoff must not be discarded with the old reply generation"
        );
    }

    #[test]
    fn consumed_fifo_record_retains_exact_generation_for_final_claim() {
        let mut state = SyncHandoffState::new();
        let direct = runqueue::RunOwnerSnapshot {
            state: runqueue::RunOwnerState::DirectHandoff,
            cpu: Some(2),
            generation: 8,
            runnable: true,
            wait_reason_kind: 0,
            wait_armed: false,
        };
        assert!(
            state.enqueue_with_direct_owner(SyncHandoffRecord::new(12, 121), Some((direct, 2)),)
        );
        let record = state
            .take_next_ready_record_with_competitor(false, |record| {
                record.matches_dispatch_owner(direct)
            })
            .expect("current direct owner must retain FIFO custody");
        assert_eq!(record.slot(), 12);
        assert_eq!(record.claim_generation(), Some(8));
        assert_eq!(record.bound_direct_generation(), Some(8));
        assert_eq!(state.len(), 0);
        assert_eq!(
            SyncHandoffRecord::new(12, 121).bound_direct_generation(),
            None
        );
        assert_eq!(SyncHandoffRecord::new(12, 121).claim_generation(), None);
    }

    #[test]
    fn consumed_generic_fifo_binds_only_the_exact_local_owner() {
        let mut state = SyncHandoffState::new();
        let local = runqueue::RunOwnerSnapshot {
            state: runqueue::RunOwnerState::Local,
            cpu: Some(2),
            generation: 8,
            runnable: true,
            wait_reason_kind: 0,
            wait_armed: false,
        };
        let generic = SyncHandoffRecord::new(12, 121);
        assert!(state.enqueue(generic));
        assert_eq!(
            generic.claim_generation(),
            None,
            "queued hint is ordering only"
        );
        let consumed = state
            .take_next_ready_record_with_competitor(false, |_| true)
            .expect("live generic hint");
        assert_eq!(consumed.claim_generation(), None);
        let bound = consumed.bind_selected_local_owner(local, 2);
        assert_eq!(bound.claim_generation(), Some(8));
        assert!(bound.matches_dispatch_owner(local));
        assert!(!bound.matches_dispatch_owner(runqueue::RunOwnerSnapshot {
            generation: 9,
            ..local
        }));
        assert!(!bound.matches_dispatch_owner(runqueue::RunOwnerSnapshot {
            cpu: Some(3),
            ..local
        }));
        assert!(!bound.matches_dispatch_owner(runqueue::RunOwnerSnapshot {
            state: runqueue::RunOwnerState::RemoteQueued,
            ..local
        }));
        assert_eq!(
            consumed
                .bind_selected_local_owner(
                    runqueue::RunOwnerSnapshot {
                        runnable: false,
                        ..local
                    },
                    2
                )
                .claim_generation(),
            None,
        );
        assert_eq!(
            consumed
                .bind_selected_local_owner(local, 3)
                .claim_generation(),
            None
        );
    }

    #[test]
    fn direct_handoff_record_cannot_reclaim_a_republished_owner_generation() {
        let mut state = SyncHandoffState::new();
        let owner = runqueue::RunOwnerSnapshot {
            state: runqueue::RunOwnerState::DirectHandoff,
            cpu: Some(2),
            generation: 8,
            runnable: true,
            wait_reason_kind: 0,
            wait_armed: false,
        };
        let generic = SyncHandoffRecord::new(12, 121);
        assert!(state.enqueue_with_direct_owner(generic, Some((owner, 2))));
        assert_eq!(state.len(), 1);
        assert_eq!(
            state.take_next_ready(|record| record.matches_dispatch_owner(
                runqueue::RunOwnerSnapshot {
                    generation: 9,
                    ..owner
                }
            )),
            None,
            "a later direct handoff cannot inherit the previous FIFO record"
        );
        assert!(state.enqueue_with_direct_owner(generic, Some((owner, 2))));
        assert_eq!(
            state.take_next_ready(|record| record.matches_dispatch_owner(
                runqueue::RunOwnerSnapshot {
                    state: runqueue::RunOwnerState::Local,
                    ..owner
                }
            )),
            None,
            "materialized fair custody must not retain direct FIFO urgency"
        );
        assert!(state.enqueue_with_direct_owner(generic, Some((owner, 2))));
        assert_eq!(
            state.take_next_ready(|record| record.matches_dispatch_owner(
                runqueue::RunOwnerSnapshot {
                    cpu: Some(1),
                    ..owner
                }
            )),
            None,
            "a migrated direct owner cannot inherit the previous CPU's FIFO urgency"
        );
        assert!(state.enqueue_with_direct_owner(generic, Some((owner, 2))));
        assert_eq!(
            state.take_next_ready(|record| record.matches_dispatch_owner(owner)),
            Some(12)
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

    #[test]
    fn an_exhausted_handoff_budget_must_not_clear_a_queue_that_still_holds_records() {
        // `take_next_ready` returns None on an exhausted budget without draining.
        // Clearing the pending hint there would strand every queued reply
        // until something else happened to enqueue again.
        let mut state = SyncHandoffState::new();
        assert!(state.enqueue(SyncHandoffRecord::new(1, 7)));
        state.set_handoff_streak(MAX_SYNC_HANDOFF_CHAIN_TICKS);

        assert_eq!(state.take_next_ready(|_| true), None);
        assert_ne!(
            state.len, 0,
            "an exhausted chain budget must leave the record queued"
        );
    }

    #[test]
    fn a_take_that_empties_the_queue_leaves_it_clearable() {
        // The production `take_next_ready(cpu, ..)` clears `SYNC_HANDOFF_PENDING`
        // on `state.len == 0` alone, so a successful take that drained the FIFO
        // must leave that condition true. While the clear also required
        // `taken.is_none()`, every emptying hit left the flag set and the next
        // dispatch took this lock only to read `len == 0` -- the acquisition
        // `pending()` exists to avoid, and the one misreported as `DrainedStale`.
        let mut state = SyncHandoffState::new();
        assert!(state.enqueue(SyncHandoffRecord::new(1, 7)));

        assert_eq!(state.take_next_ready(|_| true), Some(1));
        assert_eq!(
            state.len, 0,
            "an emptying take must leave the queue clearable"
        );
    }

    #[test]
    fn an_already_empty_queue_discards_nothing() {
        // `StartedEmpty` exists because this path charged `DrainedStale`, which
        // names a discard that never happened: the readiness predicate is not
        // consulted at all.
        let mut state = SyncHandoffState::new();
        let mut consulted = 0_usize;

        assert_eq!(
            state.take_next_ready(|_| {
                consulted += 1;
                true
            }),
            None
        );
        assert_eq!(consulted, 0, "an empty queue must reject no record");
        assert_eq!(state.len, 0);
    }
}
