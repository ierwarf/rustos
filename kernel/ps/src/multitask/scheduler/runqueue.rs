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

use core::sync::atomic::{AtomicU64, AtomicUsize, Ordering, fence};

use nucleus_core::util::lockdep::{LockClass, MAX_TRACKED_CPUS, TrackedSpinLock};

use super::MAX_TASK;

pub(super) mod affinity_payload;
pub(super) mod simd_tls;

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
/// why the old readiness mirror still had readers after stage two of
/// `V5-SCHED-GLOBAL-001`.
const OWNER_RUNNABLE_SHIFT: u64 = OWNER_STATE_BITS + OWNER_CPU_BITS;
const OWNER_RUNNABLE_BIT: u64 = 1 << OWNER_RUNNABLE_SHIFT;
const OWNER_GENERATION_SHIFT: u64 = OWNER_RUNNABLE_SHIFT + 1;
const OWNER_GENERATION_MAX: u64 = u64::MAX >> OWNER_GENERATION_SHIFT;
const NO_CPU: usize = u8::MAX as usize;
const BITMAP_WORDS: usize = MAX_TASK.div_ceil(64);
const MAILBOX_CAPACITY: usize = MAX_TASK;

/// Per-slot CFS accounting payload.
///
/// Queue custody and virtual-runtime accounting have different ownership
/// lifetimes: the owner word decides where a continuation may execute, while
/// this table survives queue migration and is read by the CPU that owns the
/// selected local queue.  Keeping it out of `Scheduler::contexts` means a
/// dispatch or wake does not need the monolithic scheduler payload merely to
/// compare fair-share keys.  Slot admission initializes it before publishing
/// any runnable owner; terminal release clears it before the slot is reused.
///
/// Release stores pair with Acquire readers so a CPU that observes a newly
/// published queue owner also sees the initial key for that generation.  The
/// compare-and-exchange updates preserve donation and accounting changes if a
/// remote scheduling action races a local fair-share update.
static VRUNTIME_NS: [AtomicU64; MAX_TASK] = [const { AtomicU64::new(0) }; MAX_TASK];

/// Per-slot execution-accounting baseline. Zero means that the slot is not
/// currently charging a running interval.
static EXEC_START_TICKS: [AtomicU64; MAX_TASK] = [const { AtomicU64::new(0) }; MAX_TASK];

/// Per-slot saved execution-frame pointer. The owner-word generation bounds
/// its lifetime: admission installs it before publication, only the execution
/// owner replaces it at a trap boundary, and terminal release clears it.
static SAVED_RSP: [AtomicUsize; MAX_TASK] = [const { AtomicUsize::new(0) }; MAX_TASK];

/// Immutable-for-one-generation usable kernel-stack bounds. They are written
/// before owner publication and cleared only after terminal owner release, so
/// frame validation and the execution owner need not consult the scheduler
/// catalog for primary stack geometry.
static KERNEL_STACK_BASE: [AtomicU64; MAX_TASK] = [const { AtomicU64::new(0) }; MAX_TASK];
static KERNEL_STACK_TOP: [AtomicU64; MAX_TASK] = [const { AtomicU64::new(0) }; MAX_TASK];

/// Per-slot temporary execution-stack bounds. Unlike the primary stack, an
/// alternate stack may be installed and removed while its owner is running.
/// The version makes the base/top pair an indivisible observation: odd means
/// a writer is between the two values, and readers reject that transient range
/// rather than accepting a mixed pair.
static ALTERNATE_KERNEL_STACK_VERSION: [AtomicU64; MAX_TASK] =
    [const { AtomicU64::new(0) }; MAX_TASK];
static ALTERNATE_KERNEL_STACK_BASE: [AtomicU64; MAX_TASK] = [const { AtomicU64::new(0) }; MAX_TASK];
static ALTERNATE_KERNEL_STACK_TOP: [AtomicU64; MAX_TASK] = [const { AtomicU64::new(0) }; MAX_TASK];

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
    /// Same-CPU synchronous IPC custody. The task is runnable on exactly one
    /// CPU but is deliberately absent from its fair runqueue: the bounded
    /// synchronous-handoff FIFO is its sole ordering owner until dispatch.
    DirectHandoff = 8,
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
            8 => Self::DirectHandoff,
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
                | RunOwnerState::DirectHandoff
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
    /// Read-only mirror of `inner.runnable`, published with the load beside it.
    ///
    /// Every candidate scan wants the membership bitmap and nothing else, and
    /// taking the queue lock for that copy cost a full tracked acquisition —
    /// about 235ns — twice per dispatch, on a queue whose contents each scan
    /// re-validates against the owner word anyway.
    published_runnable: [AtomicU64; BITMAP_WORDS],
}

impl PerCpuRunQueue {
    const fn new() -> Self {
        Self {
            inner: TrackedSpinLock::new(RunQueueInner::new()),
            published_load: AtomicU64::new(0),
            published_runnable: [const { AtomicU64::new(0) }; BITMAP_WORDS],
        }
    }

    fn publish_load(&self, inner: &RunQueueInner) {
        let count = u64::try_from(inner.runnable_count).unwrap_or(u64::MAX) & 0xffff;
        let weight = inner.runnable_weight.min((1_u64 << 48) - 1);
        for (published, word) in self
            .published_runnable
            .iter()
            .zip(inner.runnable.iter().copied())
        {
            // ORDERING: Release publishes each membership word before the load
            // that summarizes it. A reader may observe one word from before an
            // update and one from after; both are memberships this queue held,
            // and a candidate from either is validated against the owner word
            // before it can be dispatched.
            published.store(word, Ordering::Release);
        }
        // ORDERING: Release publishes a read-only placement snapshot after
        // exact rq membership and weight accounting are complete.
        self.published_load
            .store((weight << 16) | count, Ordering::Release);
    }
}

/// Fixed, allocation-free cross-CPU execution-custody transfer. The owner
/// generation is the exact linearization identity for `RemoteQueued`; a stale
/// mailbox slot can never publish a second runnable owner.
#[repr(C, align(32))]
#[derive(Clone, Copy)]
struct RunTransfer {
    slot: usize,
    generation: u64,
    weight: u32,
}

impl RunTransfer {
    const EMPTY: Self = Self {
        slot: 0,
        generation: 0,
        weight: 0,
    };
}

struct RemoteWakeMailbox {
    records: [RunTransfer; MAILBOX_CAPACITY],
    head: usize,
    len: usize,
}

impl RemoteWakeMailbox {
    const fn new() -> Self {
        Self {
            records: [RunTransfer::EMPTY; MAILBOX_CAPACITY],
            head: 0,
            len: 0,
        }
    }

    fn publish(&mut self, record: RunTransfer) {
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

    fn pop(&mut self) -> Option<RunTransfer> {
        if self.len == 0 {
            return None;
        }
        let record = self.records[self.head];
        self.records[self.head] = RunTransfer::EMPTY;
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
#[cfg(test)]
static TEST_GUARD: std::sync::Mutex<()> = std::sync::Mutex::new(());
#[cfg(test)]
static TEST_LOCAL_MIGRATING_OWNER: std::sync::Mutex<Option<RunOwnerSnapshot>> =
    std::sync::Mutex::new(None);

#[cfg(test)]
fn record_test_local_migrating_owner(owner: RunOwnerSnapshot) {
    *TEST_LOCAL_MIGRATING_OWNER.lock().unwrap() = Some(owner);
}

#[cfg(test)]
fn take_test_local_migrating_owner() -> Option<RunOwnerSnapshot> {
    TEST_LOCAL_MIGRATING_OWNER.lock().unwrap().take()
}

#[cfg(test)]
fn reset_test_local_migrating_owner() {
    *TEST_LOCAL_MIGRATING_OWNER.lock().unwrap() = None;
}

fn bitmap_location(slot: usize) -> (usize, u64) {
    assert!(slot < MAX_TASK, "scheduler rq slot exceeds capacity");
    (slot / 64, 1_u64 << (slot % 64))
}

fn validate_cpu(cpu: usize) {
    assert!(cpu < MAX_TRACKED_CPUS, "scheduler rq CPU exceeds capacity");
}

pub(super) fn reset_before_publication() {
    for (
        (((((owner, vruntime), exec_start), saved_rsp), stack_base), alternate_version),
        alternate_base,
    ) in OWNER_WORDS
        .iter()
        .zip(VRUNTIME_NS.iter())
        .zip(EXEC_START_TICKS.iter())
        .zip(SAVED_RSP.iter())
        .zip(KERNEL_STACK_BASE.iter())
        .zip(ALTERNATE_KERNEL_STACK_VERSION.iter())
        .zip(ALTERNATE_KERNEL_STACK_BASE.iter())
    {
        owner.store_reset();
        vruntime.store(0, Ordering::Release);
        exec_start.store(0, Ordering::Release);
        saved_rsp.store(0, Ordering::Release);
        stack_base.store(0, Ordering::Release);
        alternate_version.store(0, Ordering::Release);
        alternate_base.store(0, Ordering::Release);
    }
    for stack_top in &KERNEL_STACK_TOP {
        stack_top.store(0, Ordering::Release);
    }
    for alternate_top in &ALTERNATE_KERNEL_STACK_TOP {
        alternate_top.store(0, Ordering::Release);
    }
    simd_tls::reset_before_publication();
    affinity_payload::reset_before_publication();
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

#[inline]
pub(super) fn vruntime(slot: usize) -> u64 {
    VRUNTIME_NS
        .get(slot)
        .expect("scheduler vruntime slot exceeds capacity")
        .load(Ordering::Acquire)
}

/// Installs a fresh fair-share key before the slot's owner is admitted.
pub(super) fn initialize_vruntime(slot: usize, value: u64) {
    assert_eq!(
        owner(slot).state,
        RunOwnerState::Dormant,
        "scheduler vruntime initialized after owner publication"
    );
    VRUNTIME_NS[slot].store(value, Ordering::Release);
}

/// Installs the initial execution-accounting baseline before owner admission.
pub(super) fn initialize_exec_start_ticks(slot: usize, value: u64) {
    assert_eq!(
        owner(slot).state,
        RunOwnerState::Dormant,
        "scheduler execution baseline initialized after owner publication"
    );
    EXEC_START_TICKS[slot].store(value, Ordering::Release);
}

#[inline]
pub(super) fn exec_start_ticks(slot: usize) -> u64 {
    EXEC_START_TICKS
        .get(slot)
        .expect("scheduler execution baseline slot exceeds capacity")
        .load(Ordering::Acquire)
}

/// Replaces the running interval baseline. The execution owner alone chooses
/// this value; AtomicU64 keeps readers from requiring the global task catalog.
#[inline]
pub(super) fn set_exec_start_ticks(slot: usize, value: u64) {
    EXEC_START_TICKS
        .get(slot)
        .expect("scheduler execution baseline slot exceeds capacity")
        .store(value, Ordering::Release);
}

pub(super) fn initialize_saved_rsp(slot: usize, value: usize) {
    assert_eq!(
        owner(slot).state,
        RunOwnerState::Dormant,
        "scheduler saved context initialized after owner publication"
    );
    SAVED_RSP[slot].store(value, Ordering::Release);
}

#[inline]
pub(super) fn saved_rsp(slot: usize) -> usize {
    SAVED_RSP
        .get(slot)
        .expect("scheduler saved context slot exceeds capacity")
        .load(Ordering::Acquire)
}

#[inline]
pub(super) fn set_saved_rsp(slot: usize, value: usize) {
    SAVED_RSP
        .get(slot)
        .expect("scheduler saved context slot exceeds capacity")
        .store(value, Ordering::Release);
}

pub(super) fn initialize_kernel_stack_bounds(slot: usize, base: u64, top: u64) {
    assert_eq!(
        owner(slot).state,
        RunOwnerState::Dormant,
        "scheduler stack bounds initialized after owner publication"
    );
    assert!(
        base != 0 && top > base,
        "scheduler stack bounds are invalid"
    );
    KERNEL_STACK_BASE[slot].store(base, Ordering::Release);
    KERNEL_STACK_TOP[slot].store(top, Ordering::Release);
}

#[inline]
pub(super) fn kernel_stack_bounds(slot: usize) -> (u64, u64) {
    let base = KERNEL_STACK_BASE
        .get(slot)
        .expect("scheduler stack base slot exceeds capacity")
        .load(Ordering::Acquire);
    let top = KERNEL_STACK_TOP
        .get(slot)
        .expect("scheduler stack top slot exceeds capacity")
        .load(Ordering::Acquire);
    (base, top)
}

/// Initializes the alternate-stack record before owner publication. A fresh
/// task has no alternate execution stack, but the explicit initialization
/// keeps its lifetime coupled to the owner generation rather than BSS state.
pub(super) fn initialize_alternate_kernel_stack_bounds(slot: usize) {
    assert_eq!(
        owner(slot).state,
        RunOwnerState::Dormant,
        "scheduler alternate stack initialized after owner publication"
    );
    replace_alternate_kernel_stack_bounds(slot, 0, 0);
}

/// Replaces the running task's alternate-stack pair. Scheduler exclusion gives
/// this record one writer; the version still makes accidental lock-free
/// observations fail closed instead of accepting a half-replaced range.
pub(super) fn replace_alternate_kernel_stack_bounds(slot: usize, base: u64, top: u64) {
    assert!(
        (base == 0 && top == 0) || (base != 0 && top > base),
        "scheduler alternate stack bounds are invalid"
    );
    let version = ALTERNATE_KERNEL_STACK_VERSION[slot].load(Ordering::Relaxed);
    assert_eq!(
        version & 1,
        0,
        "scheduler alternate stack replacement raced for slot {slot}"
    );
    // ORDERING: the odd version is visible before either payload half changes.
    ALTERNATE_KERNEL_STACK_VERSION[slot].store(version.wrapping_add(1), Ordering::Relaxed);
    fence(Ordering::Release);
    ALTERNATE_KERNEL_STACK_BASE[slot].store(base, Ordering::Relaxed);
    ALTERNATE_KERNEL_STACK_TOP[slot].store(top, Ordering::Relaxed);
    // ORDERING: this Release commits a matched base/top pair to Acquire readers.
    ALTERNATE_KERNEL_STACK_VERSION[slot].store(version.wrapping_add(2), Ordering::Release);
}

/// Reads one coherent alternate-stack pair. An observation that overlaps a
/// writer has no valid range, which causes frame validation to reject it.
#[inline]
pub(super) fn alternate_kernel_stack_bounds(slot: usize) -> (u64, u64) {
    let version = ALTERNATE_KERNEL_STACK_VERSION
        .get(slot)
        .expect("scheduler alternate stack version slot exceeds capacity")
        .load(Ordering::Acquire);
    if version & 1 != 0 {
        return (0, 0);
    }
    let base = ALTERNATE_KERNEL_STACK_BASE[slot].load(Ordering::Relaxed);
    let top = ALTERNATE_KERNEL_STACK_TOP[slot].load(Ordering::Relaxed);
    // ORDERING: neither payload read may move after the validating version.
    fence(Ordering::Acquire);
    if ALTERNATE_KERNEL_STACK_VERSION[slot].load(Ordering::Relaxed) != version {
        return (0, 0);
    }
    (base, top)
}

fn clear_alternate_kernel_stack_bounds(slot: usize) {
    replace_alternate_kernel_stack_bounds(slot, 0, 0);
}

pub(super) fn add_vruntime(slot: usize, delta: u64) -> u64 {
    let cell = &VRUNTIME_NS[slot];
    let mut observed = cell.load(Ordering::Acquire);
    loop {
        let next = observed.saturating_add(delta);
        match cell.compare_exchange_weak(observed, next, Ordering::AcqRel, Ordering::Acquire) {
            Ok(_) => return next,
            Err(current) => observed = current,
        }
    }
}

/// Raises the fair-share key only when a sleeper floor is ahead of it.
pub(super) fn raise_vruntime_floor(slot: usize, floor: u64) -> u64 {
    let cell = &VRUNTIME_NS[slot];
    let mut observed = cell.load(Ordering::Acquire);
    loop {
        let next = observed.max(floor);
        if next == observed {
            return observed;
        }
        match cell.compare_exchange_weak(observed, next, Ordering::AcqRel, Ordering::Acquire) {
            Ok(_) => return next,
            Err(current) => observed = current,
        }
    }
}

/// Lowers the fair-share key only for an authenticated IPC donation.
pub(super) fn lower_vruntime_ceiling(slot: usize, ceiling: u64) -> u64 {
    let cell = &VRUNTIME_NS[slot];
    let mut observed = cell.load(Ordering::Acquire);
    loop {
        let next = observed.min(ceiling);
        if next == observed {
            return observed;
        }
        match cell.compare_exchange_weak(observed, next, Ordering::AcqRel, Ordering::Acquire) {
            Ok(_) => return next,
            Err(current) => observed = current,
        }
    }
}

#[cfg(test)]
pub(super) fn test_serial_guard() -> std::sync::MutexGuard<'static, ()> {
    TEST_GUARD.lock().unwrap()
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

/// Same-CPU counterpart to `publish_remote_wake`: when a wake's target CPU is
/// the CPU already executing the wake, publish directly into the local
/// runqueue in one step (`publish_local`, the same Blocked -> Local
/// transition Balance already performs for the outgoing task) instead of
/// round-tripping through the cross-CPU mailbox.
///
/// The mailbox path is correct but not free: it always transitions
/// Blocked -> RemoteQueued -> Local, two separate owner-generation bumps, the
/// second one (`drain_remote_wakes`, run unconditionally by every dispatch's
/// Balance phase) landing before the *same* dispatch's Select phase ever
/// checks anything that captured the first bump's generation. A synchronous
/// IPC reply-wake token is exactly such a capture
/// (`SyncHandoffCustody::ReplyWake`, `sync_handoff.rs`): minted right after
/// the RemoteQueued transition, checked one phase later in the very next
/// dispatch, by which time Balance has already promoted it past that
/// generation — a mismatch on every same-CPU reply-wake, deterministically,
/// not a contention-dependent race. Skipping the mailbox for the same-CPU
/// case removes the extra hop, so the token's captured generation is the one
/// it is actually checked against.
///
/// Mirrors `publish_remote_wake`'s exact state dispatch (terminal/Dormant
/// rejects, already-owned states dedup, only `Blocked` proceeds) so every
/// caller's existing rejection/dedup contract is unchanged; only the
/// mechanism for the `Blocked` case differs.
pub(super) fn publish_local_wake(slot: usize, cpu: usize, weight: u32) -> RemoteWakeOutcome {
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
pub(super) fn publish_direct_handoff(slot: usize, cpu: usize) -> RemoteWakeOutcome {
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
        let next = owner.next(RunOwnerState::DirectHandoff, Some(cpu));
        if OWNER_WORDS[slot].compare_exchange(owner, next).is_ok() {
            return RemoteWakeOutcome::Published { cpu, notify: false };
        }
    }
}

/// Restores fair-runqueue custody when the bounded synchronous-handoff FIFO
/// cannot retain a freshly published direct transfer. Scheduler serialization
/// guarantees the task has not been selected between publication and this
/// rollback.
pub(super) fn materialize_direct_handoff(slot: usize, cpu: usize, weight: u32) -> bool {
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
pub(super) fn rollback_direct_handoff(slot: usize, cpu: usize) -> bool {
    validate_cpu(cpu);
    let owner = owner(slot);
    if owner.state != RunOwnerState::DirectHandoff || owner.cpu != Some(cpu) || !owner.runnable {
        return false;
    }
    OWNER_WORDS[slot]
        .compare_exchange(owner, owner.next(RunOwnerState::Blocked, None))
        .is_ok()
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
            RunOwnerState::Local
                | RunOwnerState::RemoteQueued
                | RunOwnerState::Running
                | RunOwnerState::DirectHandoff
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
            mailbox.publish(RunTransfer {
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
        mailbox.publish(RunTransfer {
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
    // ORDERING: Acquire pairs with the producer's 0->1 edge, which it publishes
    // only after the mailbox lock release that publishes the record. Observing
    // zero therefore proves no record is waiting, and a producer that arrives
    // after this load still wins the edge and its notification custody, so the
    // wake is delivered on the next pass rather than lost.
    // `local_dispatch_work_pending` already treats this word as authoritative
    // for the same reason.
    //
    // The early return is what makes that worth reading: every dispatch used to
    // zero the fixed `MAILBOX_CAPACITY` staging array and take the mailbox
    // owner even on an empty mailbox, and `MAILBOX_CAPACITY` is `MAX_TASK`. On
    // the voluntary-yield path neither balance helper below runs, so that
    // unconditional clear and acquire were nearly the whole measured cost of
    // this phase.
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
        let published = &RUN_QUEUES[cpu].published_runnable;
        // ORDERING: Acquire observes each word's publication and orders the
        // caller's later reads of the slots it names after it.
        core::array::from_fn(|word| published[word].load(Ordering::Acquire))
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

/// Returns whether one acquire snapshot authorizes this CPU to dispatch `slot`.
///
/// `Local` names queue custody, but does not by itself authorize execution: a
/// task may have blocked after its queue owner was published. The runnable bit
/// and queue CPU must therefore come from this same owner snapshot.
pub(super) fn is_local_dispatchable(slot: usize, cpu: usize) -> bool {
    validate_cpu(cpu);
    let owner = owner(slot);
    owner.state == RunOwnerState::Local && owner.cpu == Some(cpu) && owner.runnable
}

/// Returns whether `slot` has execution custody that this CPU may claim now.
/// A direct synchronous handoff is deliberately absent from the fair local
/// queue, but it is just as dispatchable by its exact target CPU. Selection
/// must therefore accept it while ordinary CFS scans continue to require
/// [`is_local_dispatchable`].
pub(super) fn is_current_cpu_dispatchable(slot: usize, cpu: usize) -> bool {
    validate_cpu(cpu);
    let owner = owner(slot);
    owner.cpu == Some(cpu)
        && owner.runnable
        && matches!(
            owner.state,
            RunOwnerState::Local | RunOwnerState::DirectHandoff
        )
}

/// Returns whether one acquire snapshot may nominate `slot` for a direct
/// handoff. `Migrating` deliberately remains runnable while its target mailbox
/// is being admitted, but it has no dispatch-handoff custody until that
/// admission publishes `Local` or `RemoteQueued` ownership.
pub(super) const fn is_handoff_dispatchable_owner(owner: RunOwnerSnapshot) -> bool {
    owner.runnable
        && (matches!(
            owner.state,
            RunOwnerState::Local | RunOwnerState::RemoteQueued
        ) || matches!(owner.state, RunOwnerState::DirectHandoff))
}

/// Returns whether the owner word still records execution or queue run intent.
/// This deliberately accepts `Running`, `Local`, `RemoteQueued`, and the
/// transient `Migrating` state when their shared runnable bit is set; callers
/// that require dispatch custody must use the stricter predicate above.
pub(super) const fn owner_has_run_intent(owner: RunOwnerSnapshot) -> bool {
    owner.runnable
}

/// Returns whether a wake is merely a scheduling hint for an already runnable
/// non-current task. The owner word is the runnability authority; blocked and
/// armed remain scheduler lifecycle state because they describe an uncommitted
/// wait epoch rather than queue custody.
pub(super) const fn wake_is_already_runnable(
    owner: RunOwnerSnapshot,
    was_blocked: bool,
    wake_was_armed: bool,
) -> bool {
    owner_has_run_intent(owner) && !was_blocked && !wake_was_armed
}

/// Returns whether the current owner word may nominate `slot` for a direct
/// handoff. The runnable bit and custody state come from the same acquire
/// snapshot, so a just-blocked or migrating slot cannot be selected.
pub(super) fn is_handoff_dispatchable(slot: usize) -> bool {
    is_handoff_dispatchable_owner(owner(slot))
}

pub(super) fn claim_dispatch(slot: usize, cpu: usize, weight: u32) -> bool {
    validate_cpu(cpu);
    let owner = owner(slot);
    if owner.state == RunOwnerState::DirectHandoff {
        if owner.cpu != Some(cpu) || !owner.runnable {
            return false;
        }
        return OWNER_WORDS[slot]
            .compare_exchange(owner, owner.next(RunOwnerState::Running, Some(cpu)))
            .is_ok();
    }
    if owner.state != RunOwnerState::Local || owner.cpu != Some(cpu) || !owner.runnable {
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
    VRUNTIME_NS[slot].store(0, Ordering::Release);
    EXEC_START_TICKS[slot].store(0, Ordering::Release);
    SAVED_RSP[slot].store(0, Ordering::Release);
    simd_tls::clear_tls_fs_base(slot);
    affinity_payload::reset_affinity(slot);
    KERNEL_STACK_BASE[slot].store(0, Ordering::Release);
    KERNEL_STACK_TOP[slot].store(0, Ordering::Release);
    clear_alternate_kernel_stack_bounds(slot);
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
mod tests;
