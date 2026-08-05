//! Authoritative per-CPU runnable ownership and remote-wake custody.
//!
//! - **Owner:** one exact `RunOwnerWord` owns every admitted task; a `Local`
//!   task is present in exactly one CPU runqueue and a `RemoteQueued` task is
//!   present in exactly one target mailbox.
//! - **Boundary:** task payload remains private to the scheduler.  Other
//!   subsystems may publish only an exact task wake through the scheduler API.
//! - **Lifecycle:** Dormant -> Blocked/Local -> RemoteQueued -> Local ->
//!   Running, with explicit Retiring/Retired terminal custody.
//! - **Concurrency:** the current CPU owns its rq lock. Remote producers take
//!   only the target mailbox lock after winning the owner-word CAS; the target
//!   drains mailbox records and adopts them under its own rq lock.
//! - **Failure:** duplicate membership, stale generation, wrong-CPU dispatch,
//!   or a terminal-state wake is rejected or panics at the exact invariant
//!   boundary. Repeated affinity migration coalesces by slot, so stale records
//!   cannot exhaust a target mailbox.
//! - **Forbidden:** no global runnable scanner, dual-rq normal-path locking,
//!   direct remote rq mutation, broadcast wake notification, or shadow ready
//!   state may authorize dispatch.
//! - **Evidence:** `per-cpu-runqueue-ownership`, `scheduler-dispatch`, and
//!   `smp-reschedule-ipi-lifecycle`.

use core::sync::atomic::{AtomicU64, Ordering};

use nucleus_core::util::lockdep::{LockClass, MAX_TRACKED_CPUS, TrackedSpinLock};

use super::MAX_TASK;

const OWNER_STATE_BITS: u64 = 4;
const OWNER_CPU_BITS: u64 = 8;
const OWNER_STATE_MASK: u64 = (1 << OWNER_STATE_BITS) - 1;
const OWNER_CPU_MASK: u64 = (1 << OWNER_CPU_BITS) - 1;
const OWNER_CPU_SHIFT: u64 = OWNER_STATE_BITS;
/// "Still wants to run", independent of whether a CPU is executing it.
///
/// This is Linux's `p->on_rq == TASK_ON_RQ_QUEUED`, which the kernel documents
/// as covering a task that is "present in a runqueue, either actively executing
/// on a CPU or waiting to run". Without it, `Running` conflates executing with
/// no longer runnable, and the question "does the outgoing task go back to its
/// queue or get published blocked?" has no answer in the owner word — which is
/// why `context.ready` still had readers after stage two of
/// `V5-SCHED-GLOBAL-001`.
const OWNER_RUNNABLE_SHIFT: u64 = OWNER_STATE_BITS + OWNER_CPU_BITS;
const OWNER_RUNNABLE_BIT: u64 = 1 << OWNER_RUNNABLE_SHIFT;
const OWNER_GENERATION_SHIFT: u64 = OWNER_RUNNABLE_SHIFT + 1;
const OWNER_GENERATION_MAX: u64 = u64::MAX >> OWNER_GENERATION_SHIFT;
const NO_CPU: usize = u8::MAX as usize;
const BITMAP_WORDS: usize = MAX_TASK.div_ceil(64);
const MAILBOX_CAPACITY: usize = MAX_TASK;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub(super) enum RunOwnerState {
    Dormant = 0,
    Local = 1,
    RemoteQueued = 2,
    Running = 3,
    Migrating = 4,
    Blocked = 5,
    Retiring = 6,
    Retired = 7,
}

impl RunOwnerState {
    fn decode(raw: u64) -> Self {
        match raw & OWNER_STATE_MASK {
            0 => Self::Dormant,
            1 => Self::Local,
            2 => Self::RemoteQueued,
            3 => Self::Running,
            4 => Self::Migrating,
            5 => Self::Blocked,
            6 => Self::Retiring,
            7 => Self::Retired,
            state => panic!("scheduler owner word contains invalid state {state}"),
        }
    }

    pub(super) const fn is_terminal(self) -> bool {
        matches!(self, Self::Retiring | Self::Retired)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct RunOwnerSnapshot {
    pub(super) state: RunOwnerState,
    pub(super) cpu: Option<usize>,
    pub(super) generation: u64,
    /// Whether the task still wants to run. See [`OWNER_RUNNABLE_SHIFT`].
    pub(super) runnable: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum RemoteWakeOutcome {
    Rejected,
    AlreadyOwned { cpu: Option<usize> },
    Published { cpu: usize, notify: bool },
}

impl RunOwnerSnapshot {
    const fn new(state: RunOwnerState, cpu: Option<usize>, generation: u64) -> Self {
        Self {
            state,
            cpu,
            generation,
            runnable: Self::state_implies_runnable(state),
        }
    }

    /// The runnable bit a state carries when it is first entered.
    ///
    /// `Running` is entered only from `Local`, so it starts runnable and stays
    /// so until the task blocks in place. Everything else is decided by the
    /// state alone.
    const fn state_implies_runnable(state: RunOwnerState) -> bool {
        matches!(
            state,
            RunOwnerState::Local
                | RunOwnerState::RemoteQueued
                | RunOwnerState::Running
                | RunOwnerState::Migrating
        )
    }

    const fn with_runnable(self, runnable: bool) -> Self {
        Self { runnable, ..self }
    }

    fn encode(self) -> u64 {
        assert!(
            self.generation != 0 && self.generation <= OWNER_GENERATION_MAX,
            "scheduler owner generation is outside the packed range"
        );
        let cpu = self.cpu.unwrap_or(NO_CPU);
        assert!(cpu <= NO_CPU, "scheduler owner CPU exceeds packed range");
        (self.generation << OWNER_GENERATION_SHIFT)
            | if self.runnable { OWNER_RUNNABLE_BIT } else { 0 }
            | ((cpu as u64 & OWNER_CPU_MASK) << OWNER_CPU_SHIFT)
            | self.state as u64
    }

    fn decode(raw: u64) -> Self {
        let cpu = ((raw >> OWNER_CPU_SHIFT) & OWNER_CPU_MASK) as usize;
        Self {
            state: RunOwnerState::decode(raw),
            cpu: (cpu != NO_CPU).then_some(cpu),
            generation: raw >> OWNER_GENERATION_SHIFT,
            runnable: raw & OWNER_RUNNABLE_BIT != 0,
        }
    }

    fn next(self, state: RunOwnerState, cpu: Option<usize>) -> Self {
        let generation = self
            .generation
            .checked_add(1)
            .filter(|generation| *generation <= OWNER_GENERATION_MAX)
            .expect("scheduler owner generation exhausted");
        Self::new(state, cpu, generation)
    }
}

struct RunOwnerWord(AtomicU64);

impl RunOwnerWord {
    const fn new() -> Self {
        // Generation one makes the all-zero BSS value invalid as a live
        // publication while still permitting const initialization.
        Self(AtomicU64::new(
            (1 << OWNER_GENERATION_SHIFT)
                | ((NO_CPU as u64) << OWNER_CPU_SHIFT)
                | RunOwnerState::Dormant as u64,
        ))
    }

    fn load(&self) -> RunOwnerSnapshot {
        // ORDERING: Acquire observes queue/mailbox payload published before an
        // owner transition and every task payload write owned by that state.
        RunOwnerSnapshot::decode(self.0.load(Ordering::Acquire))
    }

    fn compare_exchange(
        &self,
        expected: RunOwnerSnapshot,
        next: RunOwnerSnapshot,
    ) -> Result<(), RunOwnerSnapshot> {
        // ORDERING: AcqRel is the task-ownership linearization point. Failure
        // uses Acquire so the loser can act idempotently on the winning state.
        self.0
            .compare_exchange(
                expected.encode(),
                next.encode(),
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .map(|_| ())
            .map_err(RunOwnerSnapshot::decode)
    }

    fn store_reset(&self) {
        // Reset is boot-only, before scheduler publication and AP dispatch.
        self.0.store(
            RunOwnerSnapshot::new(RunOwnerState::Dormant, None, 1).encode(),
            Ordering::Release,
        );
    }
}

#[derive(Clone, Copy)]
struct RunQueueInner {
    runnable: [u64; BITMAP_WORDS],
    runnable_count: usize,
    runnable_weight: u64,
    queue_sequence: u64,
}

impl RunQueueInner {
    const fn new() -> Self {
        Self {
            runnable: [0; BITMAP_WORDS],
            runnable_count: 0,
            runnable_weight: 0,
            queue_sequence: 0,
        }
    }

    fn contains(&self, slot: usize) -> bool {
        let (word, bit) = bitmap_location(slot);
        self.runnable[word] & bit != 0
    }

    fn insert(&mut self, slot: usize, weight: u32) {
        assert!(!self.contains(slot), "scheduler rq duplicate slot={slot}");
        let (word, bit) = bitmap_location(slot);
        self.runnable[word] |= bit;
        self.runnable_count = self
            .runnable_count
            .checked_add(1)
            .expect("scheduler rq runnable count overflow");
        self.runnable_weight = self
            .runnable_weight
            .saturating_add(u64::from(weight.max(1)));
        self.queue_sequence = self
            .queue_sequence
            .checked_add(1)
            .expect("scheduler rq sequence exhausted");
    }

    fn remove(&mut self, slot: usize, weight: u32) {
        assert!(self.contains(slot), "scheduler rq missing slot={slot}");
        let (word, bit) = bitmap_location(slot);
        self.runnable[word] &= !bit;
        self.runnable_count = self
            .runnable_count
            .checked_sub(1)
            .expect("scheduler rq runnable count underflow");
        self.runnable_weight = self
            .runnable_weight
            .saturating_sub(u64::from(weight.max(1)));
        self.queue_sequence = self
            .queue_sequence
            .checked_add(1)
            .expect("scheduler rq sequence exhausted");
    }
}

#[repr(C, align(64))]
struct PerCpuRunQueue {
    inner: TrackedSpinLock<RunQueueInner, { LockClass::SchedulerRunQueue as u8 }>,
    published_load: AtomicU64,
}

impl PerCpuRunQueue {
    const fn new() -> Self {
        Self {
            inner: TrackedSpinLock::new(RunQueueInner::new()),
            published_load: AtomicU64::new(0),
        }
    }

    fn publish_load(&self, inner: &RunQueueInner) {
        let count = u64::try_from(inner.runnable_count).unwrap_or(u64::MAX) & 0xffff;
        let weight = inner.runnable_weight.min((1_u64 << 48) - 1);
        // ORDERING: Release publishes a read-only placement snapshot after
        // exact rq membership and weight accounting are complete.
        self.published_load
            .store((weight << 16) | count, Ordering::Release);
    }
}

#[derive(Clone, Copy)]
struct RemoteWakeRecord {
    slot: usize,
    generation: u64,
    weight: u32,
}

impl RemoteWakeRecord {
    const EMPTY: Self = Self {
        slot: 0,
        generation: 0,
        weight: 0,
    };
}

struct RemoteWakeMailbox {
    records: [RemoteWakeRecord; MAILBOX_CAPACITY],
    head: usize,
    len: usize,
}

impl RemoteWakeMailbox {
    const fn new() -> Self {
        Self {
            records: [RemoteWakeRecord::EMPTY; MAILBOX_CAPACITY],
            head: 0,
            len: 0,
        }
    }

    fn publish(&mut self, record: RemoteWakeRecord) {
        // Rehoming invalidates an earlier generation without synchronously
        // deleting its record from the old target. If the task later returns
        // to this CPU before a drain, replace that stale record in place. One
        // target therefore holds at most one record per scheduler slot and the
        // MAX_TASK mailbox cannot be exhausted by affinity churn.
        for offset in 0..self.len {
            let index = (self.head + offset) % self.records.len();
            if self.records[index].slot == record.slot {
                self.records[index] = record;
                return;
            }
        }
        assert!(
            self.len < self.records.len(),
            "scheduler remote wake mailbox lost unique-slot bound"
        );
        let tail = (self.head + self.len) % self.records.len();
        self.records[tail] = record;
        self.len += 1;
    }

    fn pop(&mut self) -> Option<RemoteWakeRecord> {
        if self.len == 0 {
            return None;
        }
        let record = self.records[self.head];
        self.records[self.head] = RemoteWakeRecord::EMPTY;
        self.head = (self.head + 1) % self.records.len();
        self.len -= 1;
        Some(record)
    }
}

static OWNER_WORDS: [RunOwnerWord; MAX_TASK] = [const { RunOwnerWord::new() }; MAX_TASK];
static RUN_QUEUES: [PerCpuRunQueue; MAX_TRACKED_CPUS] =
    [const { PerCpuRunQueue::new() }; MAX_TRACKED_CPUS];
static REMOTE_WAKE_MAILBOXES: [TrackedSpinLock<
    RemoteWakeMailbox,
    { LockClass::SchedulerMailbox as u8 },
>; MAX_TRACKED_CPUS] = [const { TrackedSpinLock::new(RemoteWakeMailbox::new()) }; MAX_TRACKED_CPUS];
static MAILBOX_PENDING: [AtomicU64; MAX_TRACKED_CPUS] =
    [const { AtomicU64::new(0) }; MAX_TRACKED_CPUS];

fn bitmap_location(slot: usize) -> (usize, u64) {
    assert!(slot < MAX_TASK, "scheduler rq slot exceeds capacity");
    (slot / 64, 1_u64 << (slot % 64))
}

fn validate_cpu(cpu: usize) {
    assert!(cpu < MAX_TRACKED_CPUS, "scheduler rq CPU exceeds capacity");
}

pub(super) fn reset_before_publication() {
    for owner in &OWNER_WORDS {
        owner.store_reset();
    }
    for cpu in 0..MAX_TRACKED_CPUS {
        let mut rq = RUN_QUEUES[cpu].inner.lock();
        *rq = RunQueueInner::new();
        RUN_QUEUES[cpu].publish_load(&rq);
        drop(rq);
        let mut mailbox = REMOTE_WAKE_MAILBOXES[cpu].lock();
        *mailbox = RemoteWakeMailbox::new();
        MAILBOX_PENDING[cpu].store(0, Ordering::Release);
    }
}

pub(super) fn owner(slot: usize) -> RunOwnerSnapshot {
    OWNER_WORDS
        .get(slot)
        .expect("scheduler owner slot exceeds capacity")
        .load()
}

/// Records whether the task still wants to run, without moving its state.
///
/// This is the in-place transition the owner word was missing. A task that
/// blocks while it is executing stays `Running` — the CPU is still on its
/// stack — but stops being runnable, and the next dispatch has to know which of
/// those two it is in order to choose between returning it to its queue and
/// publishing it blocked. Linux keeps the same two facts apart in `p->on_rq`
/// and the task state.
///
/// The generation is deliberately not advanced: this is not a custody change,
/// and bumping it would invalidate a mailbox record that is still correct.
pub(super) fn set_runnable(slot: usize, runnable: bool) {
    let word = OWNER_WORDS
        .get(slot)
        .expect("scheduler owner slot exceeds capacity");
    loop {
        let observed = word.load();
        if observed.runnable == runnable {
            return;
        }
        if word
            .compare_exchange(observed, observed.with_runnable(runnable))
            .is_ok()
        {
            return;
        }
    }
}

pub(super) fn admit_blocked(slot: usize) {
    let owner = owner(slot);
    assert_eq!(
        owner.state,
        RunOwnerState::Dormant,
        "scheduler slot admitted twice"
    );
    OWNER_WORDS[slot]
        .compare_exchange(owner, owner.next(RunOwnerState::Blocked, None))
        .unwrap_or_else(|observed| panic!("scheduler slot admission raced observed={observed:?}"));
}

pub(super) fn admit_running(slot: usize, cpu: usize) {
    validate_cpu(cpu);
    let owner = owner(slot);
    assert_eq!(
        owner.state,
        RunOwnerState::Dormant,
        "scheduler slot admitted twice"
    );
    OWNER_WORDS[slot]
        .compare_exchange(owner, owner.next(RunOwnerState::Running, Some(cpu)))
        .unwrap_or_else(|observed| {
            panic!("scheduler running admission raced observed={observed:?}")
        });
}

pub(super) fn publish_local(slot: usize, cpu: usize, weight: u32) {
    validate_cpu(cpu);
    let owner = owner(slot);
    assert!(
        matches!(owner.state, RunOwnerState::Blocked | RunOwnerState::Running),
        "scheduler local publication from invalid owner={owner:?}"
    );
    assert!(
        owner.cpu.is_none() || owner.cpu == Some(cpu),
        "scheduler local publication crossed CPU without migration owner={owner:?} target={cpu}"
    );
    let mut rq = RUN_QUEUES[cpu].inner.lock();
    rq.insert(slot, weight);
    let next = owner.next(RunOwnerState::Local, Some(cpu));
    if let Err(observed) = OWNER_WORDS[slot].compare_exchange(owner, next) {
        rq.remove(slot, weight);
        RUN_QUEUES[cpu].publish_load(&rq);
        panic!("scheduler local publication lost owner race observed={observed:?}");
    }
    RUN_QUEUES[cpu].publish_load(&rq);
}

pub(super) fn publish_blocked(slot: usize, cpu: usize, weight: u32) {
    validate_cpu(cpu);
    let owner = owner(slot);
    let mut rq = RUN_QUEUES[cpu].inner.lock();
    if owner.state == RunOwnerState::Local {
        assert_eq!(
            owner.cpu,
            Some(cpu),
            "scheduler blocked a foreign local task"
        );
        rq.remove(slot, weight);
    } else {
        assert_eq!(
            owner.state,
            RunOwnerState::Running,
            "scheduler blocked invalid owner"
        );
        assert_eq!(
            owner.cpu,
            Some(cpu),
            "scheduler blocked a foreign running task"
        );
    }
    OWNER_WORDS[slot]
        .compare_exchange(owner, owner.next(RunOwnerState::Blocked, None))
        .unwrap_or_else(|observed| panic!("scheduler block lost owner race observed={observed:?}"));
    RUN_QUEUES[cpu].publish_load(&rq);
}

pub(super) fn publish_remote_wake(
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
        if matches!(
            owner.state,
            RunOwnerState::Local | RunOwnerState::RemoteQueued | RunOwnerState::Running
        ) {
            return RemoteWakeOutcome::AlreadyOwned { cpu: owner.cpu };
        }
        if owner.state != RunOwnerState::Blocked {
            return RemoteWakeOutcome::Rejected;
        }
        let next = owner.next(RunOwnerState::RemoteQueued, Some(target_cpu));
        if OWNER_WORDS[slot].compare_exchange(owner, next).is_err() {
            continue;
        }
        {
            let mut mailbox = REMOTE_WAKE_MAILBOXES[target_cpu].lock();
            mailbox.publish(RemoteWakeRecord {
                slot,
                generation: next.generation,
                weight,
            });
        }
        // ORDERING: the mailbox lock release publishes the record before this
        // 0->1 edge grants notification custody to the winning producer.
        let notify = MAILBOX_PENDING[target_cpu]
            .compare_exchange(0, 1, Ordering::AcqRel, Ordering::Acquire)
            .is_ok();
        return RemoteWakeOutcome::Published {
            cpu: target_cpu,
            notify,
        };
    }
}

/// Move queued custody to a newly admitted affinity target.
///
/// The global lifecycle owner currently serializes affinity mutation against
/// dispatch, but queue authority still follows the same source-owned transfer
/// protocol required by the final per-CPU backend.  An old mailbox record is
/// harmless: its generation no longer matches and the old target discards it.
pub(super) fn rehome_queued(slot: usize, target_cpu: usize, weight: u32) -> RemoteWakeOutcome {
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
            RunOwnerState::Migrating => continue,
            RunOwnerState::Dormant | RunOwnerState::Retiring | RunOwnerState::Retired => {
                return RemoteWakeOutcome::Rejected;
            }
        }
    }
}

fn publish_migrating_record(
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
        mailbox.publish(RemoteWakeRecord {
            slot,
            generation: queued.generation,
            weight,
        });
    }
    let notify = MAILBOX_PENDING[target_cpu]
        .compare_exchange(0, 1, Ordering::AcqRel, Ordering::Acquire)
        .is_ok();
    RemoteWakeOutcome::Published {
        cpu: target_cpu,
        notify,
    }
}

pub(super) fn drain_remote_wakes(cpu: usize) -> usize {
    validate_cpu(cpu);
    let mut records = [RemoteWakeRecord::EMPTY; MAILBOX_CAPACITY];
    let mut count = 0;
    {
        let mut mailbox = REMOTE_WAKE_MAILBOXES[cpu].lock();
        while let Some(record) = mailbox.pop() {
            records[count] = record;
            count += 1;
        }
        // ORDERING: clearing while holding the mailbox owner closes the race
        // with a producer that observes/publishes the next 0->1 edge.
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

fn local_runnable_snapshot(cpu: usize) -> [u64; BITMAP_WORDS] {
    #[cfg(test)]
    {
        let _ = cpu;
        return [u64::MAX; BITMAP_WORDS];
    }
    #[cfg(not(test))]
    {
        validate_cpu(cpu);
        let rq = RUN_QUEUES[cpu].inner.lock();
        rq.runnable
    }
}

pub(super) struct LocalRunnableSlots {
    bitmap: [u64; BITMAP_WORDS],
    word: usize,
}

impl Iterator for LocalRunnableSlots {
    type Item = usize;

    fn next(&mut self) -> Option<Self::Item> {
        while self.word < self.bitmap.len() {
            let bits = self.bitmap[self.word];
            if bits == 0 {
                self.word += 1;
                continue;
            }
            let bit = bits.trailing_zeros() as usize;
            self.bitmap[self.word] &= !(1_u64 << bit);
            return Some(self.word * 64 + bit);
        }
        None
    }
}

pub(super) fn local_runnable_slots(cpu: usize) -> LocalRunnableSlots {
    LocalRunnableSlots {
        bitmap: local_runnable_snapshot(cpu),
        word: 0,
    }
}

pub(super) fn published_runnable_count(cpu: usize) -> usize {
    validate_cpu(cpu);
    // ORDERING: Acquire observes the exact queue membership published with
    // the corresponding count. This is only a bounded steal-search hint; the
    // owner CAS remains the transfer authority.
    (RUN_QUEUES[cpu].published_load.load(Ordering::Acquire) & 0xffff) as usize
}

/// Reports whether the CPU-local dispatch owner has work beyond the task that
/// is already running on this CPU.
///
/// Running tasks are deliberately absent from the runnable bitmap. Therefore a
/// zero published count plus an empty remote mailbox means a periodic
/// clockevent cannot select a different task and may retain the current user
/// continuation without entering lifecycle-global scheduler state.
pub(super) fn local_dispatch_work_pending(cpu: usize) -> bool {
    validate_cpu(cpu);
    // ORDERING: Acquire observes either a complete rq insertion published by
    // `publish_load` or a remote mailbox record published before its pending
    // edge. False is safe only when both independently authoritative sources
    // are empty.
    let load = RUN_QUEUES[cpu].published_load.load(Ordering::Acquire);
    let runnable_count = load & 0xffff;
    runnable_count != 0 || MAILBOX_PENDING[cpu].load(Ordering::Acquire) != 0
}

pub(super) fn is_local_runnable(slot: usize, cpu: usize) -> bool {
    validate_cpu(cpu);
    let owner = owner(slot);
    owner.state == RunOwnerState::Local && owner.cpu == Some(cpu)
}

pub(super) fn claim_dispatch(slot: usize, cpu: usize, weight: u32) -> bool {
    validate_cpu(cpu);
    let owner = owner(slot);
    if owner.state != RunOwnerState::Local || owner.cpu != Some(cpu) {
        return false;
    }
    let mut rq = RUN_QUEUES[cpu].inner.lock();
    if !rq.contains(slot) {
        panic!("scheduler owner says Local but rq omits slot={slot} cpu={cpu}");
    }
    rq.remove(slot, weight);
    let next = owner.next(RunOwnerState::Running, Some(cpu));
    if let Err(observed) = OWNER_WORDS[slot].compare_exchange(owner, next) {
        rq.insert(slot, weight);
        RUN_QUEUES[cpu].publish_load(&rq);
        if observed.state.is_terminal() {
            return false;
        }
        panic!("scheduler dispatch lost owner race observed={observed:?}");
    }
    RUN_QUEUES[cpu].publish_load(&rq);
    true
}

pub(super) fn retire(slot: usize, weight: u32) {
    loop {
        let owner = owner(slot);
        if owner.state == RunOwnerState::Retired {
            return;
        }
        if owner.state == RunOwnerState::Local {
            let cpu = owner.cpu.expect("local scheduler owner omitted CPU");
            let mut rq = RUN_QUEUES[cpu].inner.lock();
            if rq.contains(slot) {
                rq.remove(slot, weight);
            }
            let retiring = owner.next(RunOwnerState::Retiring, Some(cpu));
            if OWNER_WORDS[slot].compare_exchange(owner, retiring).is_err() {
                continue;
            }
            OWNER_WORDS[slot]
                .compare_exchange(retiring, retiring.next(RunOwnerState::Retired, None))
                .unwrap_or_else(|observed| {
                    panic!("scheduler retire completion raced observed={observed:?}")
                });
            RUN_QUEUES[cpu].publish_load(&rq);
            return;
        }
        let retiring = owner.next(RunOwnerState::Retiring, owner.cpu);
        if OWNER_WORDS[slot].compare_exchange(owner, retiring).is_err() {
            continue;
        }
        OWNER_WORDS[slot]
            .compare_exchange(retiring, retiring.next(RunOwnerState::Retired, None))
            .unwrap_or_else(|observed| {
                panic!("scheduler retire completion raced observed={observed:?}")
            });
        return;
    }
}

pub(super) fn release_retired(slot: usize) {
    let owner = owner(slot);
    assert_eq!(
        owner.state,
        RunOwnerState::Retired,
        "scheduler slot storage released before terminal run ownership"
    );
    OWNER_WORDS[slot]
        .compare_exchange(owner, owner.next(RunOwnerState::Dormant, None))
        .unwrap_or_else(|observed| {
            panic!("scheduler retired slot release raced observed={observed:?}")
        });
}

pub(super) fn least_loaded_cpu(eligible_mask: u64, fallback_cpu: usize) -> usize {
    validate_cpu(fallback_cpu);
    let mut best = (eligible_mask & (1_u64 << fallback_cpu) != 0).then(|| {
        // ORDERING: Acquire observes the same complete placement snapshot as
        // the loop below and makes the caller's locality preference the tie
        // breaker rather than always collapsing equal load onto CPU zero.
        (
            fallback_cpu,
            RUN_QUEUES[fallback_cpu]
                .published_load
                .load(Ordering::Acquire),
        )
    });
    for cpu in 0..MAX_TRACKED_CPUS {
        if eligible_mask & (1_u64 << cpu) == 0 {
            continue;
        }
        // ORDERING: Acquire observes a complete rq accounting snapshot. It is
        // a placement hint only; owner CAS and target adoption remain authority.
        let load = RUN_QUEUES[cpu].published_load.load(Ordering::Acquire);
        match best {
            None => best = Some((cpu, load)),
            Some((_, best_load)) if load < best_load => best = Some((cpu, load)),
            _ => {}
        }
    }
    best.map(|(cpu, _)| cpu).unwrap_or(fallback_cpu)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    static TEST_GUARD: Mutex<()> = Mutex::new(());

    #[test]
    fn remote_wake_has_one_mailbox_and_one_local_owner() {
        let _guard = TEST_GUARD.lock().unwrap();
        reset_before_publication();
        admit_blocked(7);
        assert_eq!(
            publish_remote_wake(7, 2, 1024),
            RemoteWakeOutcome::Published {
                cpu: 2,
                notify: true
            }
        );
        assert_eq!(
            owner(7),
            RunOwnerSnapshot::new(RunOwnerState::RemoteQueued, Some(2), 3)
        );
        assert_eq!(drain_remote_wakes(2), 1);
        assert!(is_local_runnable(7, 2));
        assert!(claim_dispatch(7, 2, 1024));
        assert_eq!(owner(7).state, RunOwnerState::Running);
        publish_blocked(7, 2, 1024);
        assert_eq!(owner(7).state, RunOwnerState::Blocked);
    }

    #[test]
    fn duplicate_wake_is_idempotent_and_terminal_wake_fails_closed() {
        let _guard = TEST_GUARD.lock().unwrap();
        reset_before_publication();
        admit_blocked(9);
        assert_eq!(
            publish_remote_wake(9, 1, 100),
            RemoteWakeOutcome::Published {
                cpu: 1,
                notify: true
            }
        );
        assert_eq!(
            publish_remote_wake(9, 1, 100),
            RemoteWakeOutcome::AlreadyOwned { cpu: Some(1) }
        );
        assert_eq!(drain_remote_wakes(1), 1);
        retire(9, 100);
        assert_eq!(owner(9).state, RunOwnerState::Retired);
        assert_eq!(publish_remote_wake(9, 1, 100), RemoteWakeOutcome::Rejected);
        release_retired(9);
        assert_eq!(owner(9).state, RunOwnerState::Dormant);
    }

    #[test]
    fn dispatch_rejects_wrong_cpu_without_changing_owner() {
        let _guard = TEST_GUARD.lock().unwrap();
        reset_before_publication();
        admit_blocked(11);
        publish_local(11, 3, 200);
        assert!(!claim_dispatch(11, 4, 200));
        assert!(is_local_runnable(11, 3));
    }

    #[test]
    fn affinity_rehome_invalidates_and_coalesces_old_mailbox_generations() {
        let _guard = TEST_GUARD.lock().unwrap();
        reset_before_publication();
        admit_blocked(13);
        assert!(matches!(
            publish_remote_wake(13, 1, 300),
            RemoteWakeOutcome::Published { cpu: 1, .. }
        ));
        assert!(matches!(
            rehome_queued(13, 4, 300),
            RemoteWakeOutcome::Published { cpu: 4, .. }
        ));
        assert!(matches!(
            rehome_queued(13, 1, 300),
            RemoteWakeOutcome::Published { cpu: 1, .. }
        ));
        assert_eq!(drain_remote_wakes(4), 1);
        assert!(!is_local_runnable(13, 4));
        // CPU 1 receives one current record rather than the old and new
        // generations occupying two finite mailbox entries.
        assert_eq!(drain_remote_wakes(1), 1);
        assert!(is_local_runnable(13, 1));
    }

    #[test]
    fn running_admission_and_load_placement_are_cpu_exact() {
        let _guard = TEST_GUARD.lock().unwrap();
        reset_before_publication();
        admit_running(17, 5);
        assert_eq!(
            owner(17),
            RunOwnerSnapshot::new(RunOwnerState::Running, Some(5), 2)
        );
        admit_blocked(18);
        publish_local(18, 3, 50);
        assert_eq!(least_loaded_cpu((1 << 3) | (1 << 4), 4), 4);
    }

    #[test]
    fn idle_steal_uses_single_owner_mailbox_transfer() {
        let _guard = TEST_GUARD.lock().unwrap();
        reset_before_publication();
        admit_blocked(19);
        publish_local(19, 1, 75);
        assert_eq!(published_runnable_count(1), 1);
        assert!(matches!(
            rehome_queued(19, 2, 75),
            RemoteWakeOutcome::Published { cpu: 2, .. }
        ));
        assert_eq!(published_runnable_count(1), 0);
        assert_eq!(drain_remote_wakes(2), 1);
        assert!(claim_dispatch(19, 2, 75));
        assert_eq!(owner(19).state, RunOwnerState::Running);
        assert_eq!(owner(19).cpu, Some(2));
    }

    #[test]
    fn local_dispatch_gate_observes_queue_and_remote_mailbox_authority() {
        let _guard = TEST_GUARD.lock().unwrap();
        reset_before_publication();
        admit_running(21, 6);
        assert!(!local_dispatch_work_pending(6));

        admit_blocked(22);
        assert!(matches!(
            publish_remote_wake(22, 6, 250),
            RemoteWakeOutcome::Published { cpu: 6, .. }
        ));
        assert!(local_dispatch_work_pending(6));
        assert_eq!(drain_remote_wakes(6), 1);
        assert!(local_dispatch_work_pending(6));
        assert!(claim_dispatch(22, 6, 250));
        assert!(!local_dispatch_work_pending(6));
    }
}
