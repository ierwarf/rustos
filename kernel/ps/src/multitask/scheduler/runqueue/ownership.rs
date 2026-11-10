//! The one definition of task scheduling custody: the packed owner word, its
//! transition table, the in-place edit rules and the flight recorder.
//!
//! - **Owner:** `OWNER_WORDS` is private to this module. Custody changes only
//!   through [`transition`], which accepts exactly the rows of
//!   [`TRANSITIONS`]; run intent and wait metadata change only through the
//!   in-place edit functions below.
//! - **Invariants:** I1 - a transition into a runnable custody state returns
//!   exactly one [`EnqueueDuty`], consumed only by the path that performs that
//!   enqueue; dropping it panics. I2 - `Dormant`, `Retiring` and `Retired`
//!   change only by the table's exact-value CAS. I3 - the generation advances by
//!   exactly one on every custody change and never otherwise.
//! - **Effects order:** CAS -> resident-cell settle (hook, or the lane's
//!   `ResidentTake` duty) -> flight-recorder store -> the duty -> caller's frame
//!   bind / published load -> IPI outside every leaf.
//! - **Concurrency:** the CAS is SeqCst (custody linearization point and the
//!   owner side of the resident handshake); the ring store after it is Relaxed
//!   diagnostics into the same, already exclusive cache line.
//! - **Forbidden:** raw owner CAS anywhere else, a same-generation custody
//!   change, editing a settled word, dropping an enqueue duty.
//! - **Evidence:** `ownership::tests` (exhaustive table walk, duty, I2/I3
//!   regressions); design in `docs/ai/smp/scheduler.md` "Owner transition table".

use core::sync::atomic::{AtomicU64, Ordering};

use super::MAX_TASK;

const OWNER_STATE_BITS: u64 = 4;
const OWNER_CPU_BITS: u64 = 8;
const OWNER_STATE_MASK: u64 = (1 << OWNER_STATE_BITS) - 1;
const OWNER_CPU_MASK: u64 = (1 << OWNER_CPU_BITS) - 1;
const OWNER_CPU_SHIFT: u64 = OWNER_STATE_BITS;
/// This is Linux's `p->on_rq == TASK_ON_RQ_QUEUED`, which the kernel documents
/// as covering a task that is "present in a runqueue, either actively executing
/// on a CPU or waiting to run". Without it, `Running` conflates executing with
/// no longer runnable, and the question "does the outgoing task go back to its
/// queue or get published blocked?" has no answer in the owner word.
const OWNER_RUNNABLE_SHIFT: u64 = OWNER_STATE_BITS + OWNER_CPU_BITS;
const OWNER_RUNNABLE_BIT: u64 = 1 << OWNER_RUNNABLE_SHIFT;
// Four typed waits already consume the old two-bit range. Keep the fault-slot
// identity distinct from endpoint/reply waits instead of overloading a reply
// handle into exception state.
pub(super) const OWNER_WAIT_REASON_BITS: u64 = 3;
const OWNER_WAIT_REASON_SHIFT: u64 = OWNER_RUNNABLE_SHIFT + 1;
const OWNER_WAIT_REASON_MASK: u64 = ((1 << OWNER_WAIT_REASON_BITS) - 1) << OWNER_WAIT_REASON_SHIFT;
const OWNER_WAIT_ARMED_SHIFT: u64 = OWNER_WAIT_REASON_SHIFT + OWNER_WAIT_REASON_BITS;
const OWNER_WAIT_ARMED_BIT: u64 = 1 << OWNER_WAIT_ARMED_SHIFT;
const OWNER_GENERATION_SHIFT: u64 = OWNER_WAIT_ARMED_SHIFT + 1;
const OWNER_GENERATION_MAX: u64 = u64::MAX >> OWNER_GENERATION_SHIFT;
const OWNER_LOW_MASK: u64 = (1 << OWNER_GENERATION_SHIFT) - 1;
const NO_CPU: usize = u8::MAX as usize;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub(in crate::multitask::scheduler) enum RunOwnerState {
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
    /// synchronous-handoff FIFO (or a `DirectSwitch`) is its sole ordering
    /// owner until dispatch.
    DirectHandoff = 8,
}

impl RunOwnerState {
    #[cfg(test)]
    pub(super) const ALL: [Self; 9] = [
        Self::Dormant,
        Self::Local,
        Self::RemoteQueued,
        Self::Running,
        Self::Migrating,
        Self::Blocked,
        Self::Retiring,
        Self::Retired,
        Self::DirectHandoff,
    ];

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

    pub(in crate::multitask::scheduler) const fn is_terminal(self) -> bool {
        matches!(self, Self::Retiring | Self::Retired)
    }

    /// Whether run-intent and wait metadata may be edited in place: the slot
    /// holds an admitted, not yet withdrawn task (I2).
    pub(in crate::multitask::scheduler) const fn is_editable(self) -> bool {
        !self.is_terminal() && !matches!(self, Self::Dormant)
    }

    /// States whose custody record (CPU execution, rq membership, mailbox or
    /// handoff record, migration publication) a dispatcher can find. Only these
    /// may carry run intent; raising it anywhere else would be an enqueue
    /// without a duty (I1).
    pub(in crate::multitask::scheduler) const fn holds_run_custody(self) -> bool {
        matches!(
            self,
            Self::Local | Self::RemoteQueued | Self::Running | Self::Migrating | Self::DirectHandoff
        )
    }

    /// States in which run intent may be withdrawn in place without losing the
    /// task's custody record: an executing task (its return publishes it
    /// blocked) and a queued one (it keeps its rq membership until woken).
    pub(in crate::multitask::scheduler) const fn may_lower_run_intent(self) -> bool {
        matches!(self, Self::Running | Self::Local)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(in crate::multitask::scheduler) struct RunOwnerSnapshot {
    pub(in crate::multitask::scheduler) state: RunOwnerState,
    pub(in crate::multitask::scheduler) cpu: Option<usize>,
    pub(in crate::multitask::scheduler) generation: u64,
    /// Whether the task still wants to run. See [`OWNER_RUNNABLE_SHIFT`].
    pub(in crate::multitask::scheduler) runnable: bool,
    /// Exact wait kind retained from arm through blocked custody. Values use
    /// the stable `runqueue::wait::REASON_*` encoding.
    pub(in crate::multitask::scheduler) wait_reason_kind: u8,
    /// An uncommitted wait arm. Commit and a racing wake update this bit with
    /// `runnable` in the same owner-word CAS.
    pub(in crate::multitask::scheduler) wait_armed: bool,
}

impl RunOwnerSnapshot {
    pub(super) const fn new(state: RunOwnerState, cpu: Option<usize>, generation: u64) -> Self {
        Self {
            state,
            cpu,
            generation,
            runnable: state.holds_run_custody(),
            wait_reason_kind: 0,
            wait_armed: false,
        }
    }

    pub(super) const fn with_runnable(self, runnable: bool) -> Self {
        Self { runnable, ..self }
    }

    pub(super) const fn with_wait(self, wait_armed: bool, wait_reason_kind: u8) -> Self {
        Self {
            wait_reason_kind,
            wait_armed,
            ..self
        }
    }

    pub(super) fn encode(self) -> u64 {
        assert!(
            self.generation != 0 && self.generation <= OWNER_GENERATION_MAX,
            "scheduler owner generation is outside the packed range"
        );
        let cpu = self.cpu.unwrap_or(NO_CPU);
        assert!(cpu <= NO_CPU, "scheduler owner CPU exceeds packed range");
        assert!(
            u64::from(self.wait_reason_kind) < (1 << OWNER_WAIT_REASON_BITS),
            "scheduler wait reason exceeds packed range"
        );
        (self.generation << OWNER_GENERATION_SHIFT)
            | if self.wait_armed {
                OWNER_WAIT_ARMED_BIT
            } else {
                0
            }
            | (u64::from(self.wait_reason_kind) << OWNER_WAIT_REASON_SHIFT)
            | if self.runnable { OWNER_RUNNABLE_BIT } else { 0 }
            | ((cpu as u64 & OWNER_CPU_MASK) << OWNER_CPU_SHIFT)
            | self.state as u64
    }

    pub(super) fn decode(raw: u64) -> Self {
        let cpu = ((raw >> OWNER_CPU_SHIFT) & OWNER_CPU_MASK) as usize;
        Self {
            state: RunOwnerState::decode(raw),
            cpu: (cpu != NO_CPU).then_some(cpu),
            generation: raw >> OWNER_GENERATION_SHIFT,
            runnable: raw & OWNER_RUNNABLE_BIT != 0,
            wait_reason_kind: ((raw & OWNER_WAIT_REASON_MASK) >> OWNER_WAIT_REASON_SHIFT) as u8,
            wait_armed: raw & OWNER_WAIT_ARMED_BIT != 0,
        }
    }

    /// The word `row` produces from this one: generation + 1 (I3), the row's
    /// state, CPU and wait rule, and the run intent the new state implies.
    fn successor(self, row: &Row, cpu: Option<usize>) -> Self {
        let generation = self
            .generation
            .checked_add(1)
            .filter(|generation| *generation <= OWNER_GENERATION_MAX)
            .expect("scheduler owner generation exhausted");
        let cpu = match row.cpu {
            CpuRule::None => None,
            CpuRule::Keep => self.cpu,
            CpuRule::Target => cpu,
        };
        let next = Self::new(row.to, cpu, generation);
        match row.wait {
            WaitRule::Clear => next,
            WaitRule::Keep => next.with_wait(self.wait_armed, self.wait_reason_kind),
        }
    }
}

/// A custody event. Each `(from, event)` pair names at most one table row.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub(in crate::multitask::scheduler) enum OwnerEvent {
    Admit = 1,
    /// TEST-HARNESS: fixture-only admission straight into `Running`; the
    /// row stays in the one table so the exhaustive witness covers it.
    #[cfg_attr(not(test), allow(dead_code))]
    AdmitRunning = 2,
    AdmitInitial = 3,
    WakeLocal = 4,
    WakeRemote = 5,
    HandoffDirect = 6,
    HandoffRemote = 7,
    LaneClaim = 8,
    Claim = 9,
    ReturnLocal = 10,
    ReturnBlocked = 11,
    Adopt = 12,
    Materialize = 13,
    /// TEST-HARNESS: fixture-only return of an undispatched direct receiver
    /// to `Blocked`; kept in the one table for the exhaustive witness.
    #[cfg_attr(not(test), allow(dead_code))]
    RollbackHandoff = 14,
    Rehome = 15,
    PublishMigration = 16,
    Withdraw = 17,
    CompleteRetire = 18,
    Release = 19,
}

#[cfg(test)]
impl OwnerEvent {
    pub(super) const ALL: [Self; 19] = [
        Self::Admit,
        Self::AdmitRunning,
        Self::AdmitInitial,
        Self::WakeLocal,
        Self::WakeRemote,
        Self::HandoffDirect,
        Self::HandoffRemote,
        Self::LaneClaim,
        Self::Claim,
        Self::ReturnLocal,
        Self::ReturnBlocked,
        Self::Adopt,
        Self::Materialize,
        Self::RollbackHandoff,
        Self::Rehome,
        Self::PublishMigration,
        Self::Withdraw,
        Self::CompleteRetire,
        Self::Release,
    ];
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum WaitRule {
    Clear,
    Keep,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum CpuRule {
    /// The new word names no CPU.
    None,
    /// The new word names the caller's target CPU.
    Target,
    /// The new word keeps the old word's CPU.
    Keep,
}

/// What a successful transition obliges its caller to do (I1).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(in crate::multitask::scheduler) enum DutyKind {
    /// Nothing: the new state needs no enqueue.
    None,
    /// The task must be a member of the target CPU's run queue.
    RunQueue,
    /// The target CPU's mailbox must hold this generation's record.
    Mailbox,
    /// The target CPU's synchronous-handoff FIFO (or a `DirectSwitch`) must
    /// hold this generation's record.
    HandoffFifo,
    /// The `Migrating` word must be published `RemoteQueued` with its record.
    Migration,
    /// The resident cell of the left generation must be taken by the caller.
    ResidentTake,
}

pub(super) struct Row {
    pub(super) from: RunOwnerState,
    pub(super) event: OwnerEvent,
    pub(super) to: RunOwnerState,
    pub(super) wait: WaitRule,
    pub(super) cpu: CpuRule,
    pub(super) duty: DutyKind,
}

const fn row(
    from: RunOwnerState,
    event: OwnerEvent,
    to: RunOwnerState,
    wait: WaitRule,
    cpu: CpuRule,
    duty: DutyKind,
) -> Row {
    Row { from, event, to, wait, cpu, duty }
}

use CpuRule::{Keep as KeepCpu, None as NoCpu, Target};
use DutyKind::{HandoffFifo, Mailbox, Migration, ResidentTake, RunQueue};
use OwnerEvent as E;
use RunOwnerState as S;
use WaitRule::{Clear, Keep};

/// The single definition of task custody transitions. See
/// `docs/ai/smp/scheduler.md` "Owner transition table".
pub(super) const TRANSITIONS: &[Row] = &[
    row(S::Dormant, E::Admit, S::Blocked, Clear, NoCpu, DutyKind::None),
    row(S::Dormant, E::AdmitRunning, S::Running, Clear, Target, DutyKind::None),
    row(S::Blocked, E::AdmitInitial, S::Running, Clear, Target, DutyKind::None),
    row(S::Blocked, E::WakeLocal, S::Local, Clear, Target, RunQueue),
    row(S::Blocked, E::WakeRemote, S::RemoteQueued, Keep, Target, Mailbox),
    row(S::Blocked, E::HandoffDirect, S::DirectHandoff, Keep, Target, HandoffFifo),
    row(S::Blocked, E::HandoffRemote, S::RemoteQueued, Keep, Target, Mailbox),
    row(S::Blocked, E::LaneClaim, S::Running, Clear, Target, ResidentTake),
    row(S::Local, E::Claim, S::Running, Clear, Target, DutyKind::None),
    row(S::DirectHandoff, E::Claim, S::Running, Clear, Target, DutyKind::None),
    row(S::Running, E::ReturnLocal, S::Local, Clear, Target, RunQueue),
    row(S::Running, E::ReturnBlocked, S::Blocked, Keep, NoCpu, DutyKind::None),
    row(S::RemoteQueued, E::Adopt, S::Local, Clear, Target, RunQueue),
    row(S::DirectHandoff, E::Materialize, S::Local, Clear, Target, RunQueue),
    row(S::DirectHandoff, E::RollbackHandoff, S::Blocked, Keep, NoCpu, DutyKind::None),
    row(S::Local, E::Rehome, S::Migrating, Clear, Target, Migration),
    row(S::RemoteQueued, E::Rehome, S::Migrating, Clear, Target, Migration),
    row(S::DirectHandoff, E::Rehome, S::Migrating, Clear, Target, Migration),
    row(S::Migrating, E::PublishMigration, S::RemoteQueued, Clear, Target, Mailbox),
    row(S::Blocked, E::Withdraw, S::Retiring, Clear, KeepCpu, DutyKind::None),
    row(S::Local, E::Withdraw, S::Retiring, Clear, KeepCpu, DutyKind::None),
    row(S::RemoteQueued, E::Withdraw, S::Retiring, Clear, KeepCpu, DutyKind::None),
    row(S::Running, E::Withdraw, S::Retiring, Clear, KeepCpu, DutyKind::None),
    row(S::Migrating, E::Withdraw, S::Retiring, Clear, KeepCpu, DutyKind::None),
    row(S::DirectHandoff, E::Withdraw, S::Retiring, Clear, KeepCpu, DutyKind::None),
    row(S::Retiring, E::CompleteRetire, S::Retired, Clear, NoCpu, DutyKind::None),
    row(S::Retired, E::Release, S::Dormant, Clear, NoCpu, DutyKind::None),
];

const EVENT_SLOTS: usize = 20;
const NO_ROW: u8 = u8::MAX;

/// `TRANSITIONS` indexed by `[state][event]`, built at compile time, so a
/// transition's row lookup is one load instead of a scan of the table. A
/// duplicate `(from, event)` pair fails the build.
const ROW_INDEX: [[u8; EVENT_SLOTS]; 9] = {
    let mut index = [[NO_ROW; EVENT_SLOTS]; 9];
    let mut row = 0;
    while row < TRANSITIONS.len() {
        let from = TRANSITIONS[row].from as usize;
        let event = TRANSITIONS[row].event as usize;
        assert!(index[from][event] == NO_ROW, "duplicate owner transition row");
        index[from][event] = row as u8;
        row += 1;
    }
    index
};

pub(super) fn lookup(from: RunOwnerState, event: OwnerEvent) -> Option<&'static Row> {
    match ROW_INDEX[from as usize][event as usize] {
        NO_ROW => None,
        row => Some(&TRANSITIONS[row as usize]),
    }
}

/// Exactly one enqueue owed by a transition into runnable custody (I1).
///
/// Not `Clone`; consumed only by the operation that performs the enqueue.
/// Dropping it panics with the row it came from: a lost enqueue is a loud
/// invariant failure at the dropping site, never a silently stranded task.
#[must_use = "a transition into runnable custody owes exactly one enqueue (I1)"]
pub(in crate::multitask::scheduler) struct EnqueueDuty {
    slot: usize,
    generation: u64,
    cpu: usize,
    kind: DutyKind,
}

impl EnqueueDuty {
    pub(in crate::multitask::scheduler) fn slot(&self) -> usize {
        self.slot
    }

    pub(in crate::multitask::scheduler) fn generation(&self) -> u64 {
        self.generation
    }

    pub(in crate::multitask::scheduler) fn cpu(&self) -> usize {
        self.cpu
    }

    pub(in crate::multitask::scheduler) fn kind(&self) -> DutyKind {
        self.kind
    }

    /// Discharges the duty; `kind` must be the one the row produced.
    pub(super) fn discharge(self, kind: DutyKind) {
        assert_eq!(
            self.kind, kind,
            "owner duty discharged as the wrong kind slot={} generation={}",
            self.slot, self.generation
        );
        core::mem::forget(self);
    }

    /// The synchronous-handoff FIFO now holds this generation's record (or a
    /// `DirectSwitch` carries it). Called by the FIFO publisher.
    pub(in crate::multitask::scheduler) fn handoff_recorded(self) {
        self.discharge(DutyKind::HandoffFifo);
    }
}

impl Drop for EnqueueDuty {
    fn drop(&mut self) {
        #[cfg(test)]
        if std::thread::panicking() {
            return;
        }
        panic!(
            "scheduler I1: {:?} enqueue duty dropped slot={} generation={} cpu={}",
            self.kind, self.slot, self.generation, self.cpu
        );
    }
}

/// A committed custody transition and the duty its caller owes.
#[must_use = "a committed owner transition may carry an enqueue duty (I1)"]
pub(in crate::multitask::scheduler) struct Transition {
    pub(in crate::multitask::scheduler) next: RunOwnerSnapshot,
    duty: Option<EnqueueDuty>,
}

impl Transition {
    /// The duty of a row that has one. Panics for a duty-free row.
    pub(in crate::multitask::scheduler) fn into_duty(self) -> EnqueueDuty {
        self.duty
            .expect("scheduler owner transition carries no enqueue duty")
    }

    /// The new word of a duty-free row. Panics if the row owes a duty: such a
    /// caller must use [`Self::into_duty`].
    pub(in crate::multitask::scheduler) fn settled(self) -> RunOwnerSnapshot {
        assert!(
            self.duty.is_none(),
            "scheduler owner transition owes a duty its caller ignored"
        );
        self.next
    }
}

/// How [`RunOwnerWord::edit_in_place`] ended.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum InPlace {
    /// The word is `Dormant`, `Retiring` or `Retired`; nothing was written.
    Settled,
    /// The edit declined the word it saw; nothing was written.
    Refused,
    /// The edit produced the word that was already there.
    Unchanged,
    /// The edit's word replaced the one it was computed from.
    Applied,
}

/// Flight-recorder depth: the owner word's 64-byte line has room for seven
/// more words, so the ring costs no extra cache line on any transition.
pub(super) const FLIGHT_DEPTH: usize = 7;
const ENTRY_EVENT_SHIFT: u64 = OWNER_GENERATION_SHIFT;
const ENTRY_ACTOR_SHIFT: u64 = ENTRY_EVENT_SHIFT + 5;
const ENTRY_GENERATION_SHIFT: u64 = ENTRY_ACTOR_SHIFT + 8;
const ENTRY_OLD_SHIFT: u64 = ENTRY_GENERATION_SHIFT + 16;
/// Actor CPU recorded when the actor is the CPU named by the new word (or by
/// the old word when the new one names none).
pub(super) const ACTOR_IS_TARGET: u8 = u8::MAX;

/// One recorded custody transition, decoded from a ring entry.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct FlightEntry {
    pub(super) raw: u64,
}

impl FlightEntry {
    fn pack(old: RunOwnerSnapshot, next: RunOwnerSnapshot, event: OwnerEvent, actor: u8) -> u64 {
        let old_low = (old.state as u64)
            | ((old.cpu.unwrap_or(NO_CPU) as u64 & OWNER_CPU_MASK) << OWNER_CPU_SHIFT)
            | if old.runnable { OWNER_RUNNABLE_BIT } else { 0 };
        (next.encode() & OWNER_LOW_MASK)
            | (u64::from(event as u8) << ENTRY_EVENT_SHIFT)
            | (u64::from(actor) << ENTRY_ACTOR_SHIFT)
            | ((next.generation & 0xffff) << ENTRY_GENERATION_SHIFT)
            | (old_low << ENTRY_OLD_SHIFT)
    }

    #[cfg(test)]
    pub(super) fn event(self) -> u8 {
        ((self.raw >> ENTRY_EVENT_SHIFT) & 0x1f) as u8
    }

    pub(super) fn generation_low(self) -> u64 {
        (self.raw >> ENTRY_GENERATION_SHIFT) & 0xffff
    }

    #[cfg(test)]
    pub(super) fn next_state(self) -> RunOwnerState {
        RunOwnerState::decode(self.raw)
    }

    #[cfg(test)]
    pub(super) fn old_state(self) -> RunOwnerState {
        RunOwnerState::decode(self.raw >> ENTRY_OLD_SHIFT)
    }

    #[cfg(test)]
    pub(super) fn actor(self) -> u8 {
        ((self.raw >> ENTRY_ACTOR_SHIFT) & 0xff) as u8
    }
}

/// One cache line per task: the word plus its flight ring. Owner CASes from
/// different CPUs on neighbouring slots do not invalidate each other.
#[repr(C, align(64))]
struct RunOwnerWord {
    word: AtomicU64,
    ring: [AtomicU64; FLIGHT_DEPTH],
}

const _: () = assert!(core::mem::size_of::<RunOwnerWord>() == 64);

impl RunOwnerWord {
    const fn new() -> Self {
        // Generation one makes the all-zero BSS value invalid as a live
        // publication while still permitting const initialization.
        Self {
            word: AtomicU64::new(
                (1 << OWNER_GENERATION_SHIFT)
                    | ((NO_CPU as u64) << OWNER_CPU_SHIFT)
                    | RunOwnerState::Dormant as u64,
            ),
            ring: [const { AtomicU64::new(0) }; FLIGHT_DEPTH],
        }
    }

    fn load(&self) -> RunOwnerSnapshot {
        // ORDERING: Acquire observes queue/mailbox payload published before an
        // owner transition and every task payload write owned by that state.
        RunOwnerSnapshot::decode(self.word.load(Ordering::Acquire))
    }

    /// [`Self::load`] for the one reader that must be totally ordered against
    /// the transition CAS: the resident parker's re-read after it publishes its
    /// cell (the other half of the handshake is `resident::on_owner_transition`).
    fn load_sequenced(&self) -> RunOwnerSnapshot {
        RunOwnerSnapshot::decode(self.word.load(Ordering::SeqCst))
    }

    fn cas(&self, expected: RunOwnerSnapshot, next: RunOwnerSnapshot) -> Result<(), RunOwnerSnapshot> {
        // ORDERING: SeqCst is the task-ownership linearization point and the
        // owner side of the resident handshake; failure uses Acquire so the
        // loser acts on the winning state. On x86 this is the same locked RMW
        // an AcqRel CAS would be.
        self.word
            .compare_exchange(
                expected.encode(),
                next.encode(),
                Ordering::SeqCst,
                Ordering::Acquire,
            )
            .map(|_| ())
            .map_err(RunOwnerSnapshot::decode)
    }

    /// The only generation-preserving mutation of an owner word: run intent and
    /// wait-arm metadata. `edit` maps the freshly loaded snapshot to the word it
    /// wants (`None` refuses) and may run again on a lost race. Settled words
    /// are never edited (I2); run intent follows the custody rules of
    /// [`RunOwnerState::holds_run_custody`] / [`RunOwnerState::may_lower_run_intent`].
    fn edit_in_place(
        &self,
        mut edit: impl FnMut(RunOwnerSnapshot) -> Option<RunOwnerSnapshot>,
    ) -> InPlace {
        let mut observed = self.load();
        loop {
            if !observed.state.is_editable() {
                return InPlace::Settled;
            }
            let Some(next) = edit(observed) else {
                return InPlace::Refused;
            };
            assert!(
                next.generation == observed.generation
                    && next.state == observed.state
                    && next.cpu == observed.cpu,
                "in-place owner edit changed custody observed={observed:?} next={next:?}"
            );
            if next.runnable != observed.runnable
                && !(if next.runnable {
                    observed.state.holds_run_custody()
                } else {
                    observed.state.may_lower_run_intent()
                })
            {
                return InPlace::Refused;
            }
            if next == observed {
                return InPlace::Unchanged;
            }
            match self.cas(observed, next) {
                Ok(()) => return InPlace::Applied,
                Err(current) => observed = current,
            }
        }
    }

    fn record(&self, old: RunOwnerSnapshot, next: RunOwnerSnapshot, event: OwnerEvent, actor: u8) {
        // ORDERING: Relaxed diagnostics. The CAS winner of `next.generation`
        // is the only writer of this entry until the ring wraps, and the line
        // is already exclusive after that CAS.
        self.ring[(next.generation % FLIGHT_DEPTH as u64) as usize]
            .store(FlightEntry::pack(old, next, event, actor), Ordering::Relaxed);
    }

    fn reset(&self) {
        // Reset is boot-only, before scheduler publication and AP dispatch.
        self.word.store(
            RunOwnerSnapshot::new(RunOwnerState::Dormant, None, 1).encode(),
            Ordering::Release,
        );
        for entry in &self.ring {
            entry.store(0, Ordering::Relaxed);
        }
    }
}

static OWNER_WORDS: [RunOwnerWord; MAX_TASK] = [const { RunOwnerWord::new() }; MAX_TASK];

fn word(slot: usize) -> &'static RunOwnerWord {
    OWNER_WORDS
        .get(slot)
        .expect("scheduler owner slot exceeds capacity")
}

pub(super) fn reset_before_publication() {
    for owner in &OWNER_WORDS {
        owner.reset();
    }
}

pub(in crate::multitask::scheduler) fn owner(slot: usize) -> RunOwnerSnapshot {
    word(slot).load()
}

/// [`owner`] for the resident parker's re-read after publishing its cell.
pub(in crate::multitask::scheduler) fn owner_sequenced(slot: usize) -> RunOwnerSnapshot {
    word(slot).load_sequenced()
}

/// The raw packed word, for the conservation scanner's exact-word identity.
pub(super) fn owner_raw(slot: usize) -> u64 {
    // ORDERING: Acquire, as `load`.
    word(slot).word.load(Ordering::Acquire)
}

/// The slot's flight ring, newest first (entries whose generation is not one
/// of the last `FLIGHT_DEPTH` are skipped).
pub(super) fn flight_ring(slot: usize) -> ([FlightEntry; FLIGHT_DEPTH], usize) {
    let current = owner(slot).generation;
    let ring = &word(slot).ring;
    let mut out = [FlightEntry { raw: 0 }; FLIGHT_DEPTH];
    let mut len = 0;
    for back in 0..FLIGHT_DEPTH as u64 {
        let Some(generation) = current.checked_sub(back).filter(|g| *g > 1) else {
            break;
        };
        let raw = ring[(generation % FLIGHT_DEPTH as u64) as usize].load(Ordering::Relaxed);
        let entry = FlightEntry { raw };
        if raw == 0 || entry.generation_low() != generation & 0xffff {
            continue;
        }
        out[len] = entry;
        len += 1;
    }
    (out, len)
}

/// The one custody transition: looks up `(expected.state, event)` in
/// [`TRANSITIONS`] (panicking on an impossible pair), computes the successor
/// word, CASes it against the exact `expected` word, settles the resident cell
/// of the generation it leaves (except `LaneClaim`, whose caller takes it), and
/// records the transition. On a lost CAS nothing was written and the winner's
/// word is returned.
pub(in crate::multitask::scheduler) fn transition(
    slot: usize,
    expected: RunOwnerSnapshot,
    event: OwnerEvent,
    cpu: Option<usize>,
) -> Result<Transition, RunOwnerSnapshot> {
    let row = lookup(expected.state, event).unwrap_or_else(|| {
        panic!(
            "scheduler owner event {event:?} from {:?} is not in the owner transition table slot={slot} owner={expected:?}",
            expected.state
        )
    });
    assert!(
        row.cpu != CpuRule::Target || cpu.is_some(),
        "scheduler owner event {event:?} needs a target CPU slot={slot}"
    );
    let next = expected.successor(row, cpu);
    let owner = word(slot);
    owner.cas(expected, next)?;
    if row.duty != DutyKind::ResidentTake {
        super::super::resident::on_owner_transition(slot, expected.generation);
    }
    owner.record(expected, next, event, actor_for(row));
    let duty = (row.duty != DutyKind::None).then(|| EnqueueDuty {
        slot,
        generation: next.generation,
        cpu: next.cpu.unwrap_or(NO_CPU),
        kind: row.duty,
    });
    Ok(Transition { next, duty })
}

/// The actor CPU worth a CPU-index read. Events whose actor is, by
/// construction, the CPU named by the new word (or by the old word when the new
/// one names none: a return publishing Blocked) record [`ACTOR_IS_TARGET`], so
/// no transition on the same-CPU IPC and dispatch paths reads the CPU index.
fn actor_for(row: &Row) -> u8 {
    match row.event {
        E::Claim | E::LaneClaim | E::ReturnLocal | E::ReturnBlocked | E::Adopt | E::Materialize
        | E::WakeLocal | E::HandoffDirect | E::RollbackHandoff | E::AdmitInitial
        | E::AdmitRunning => ACTOR_IS_TARGET,
        _ => current_cpu_byte(),
    }
}

#[cfg(not(test))]
fn current_cpu_byte() -> u8 {
    u8::try_from(nucleus_core::util::lockdep::current_cpu_index()).unwrap_or(ACTOR_IS_TARGET - 1)
}

#[cfg(test)]
fn current_cpu_byte() -> u8 {
    0
}

/// Fixture-only raw CAS for witnesses that need a word no table row produces
/// (an impossible predecessor, a skipped step). Never production custody.
#[cfg(test)]
pub(super) fn test_force_word(
    slot: usize,
    expected: RunOwnerSnapshot,
    next: RunOwnerSnapshot,
) -> Result<(), RunOwnerSnapshot> {
    word(slot).cas(expected, next)
}

/// Fixture-only successor computation matching the table row's word.
#[cfg(test)]
pub(super) fn test_successor(
    expected: RunOwnerSnapshot,
    state: RunOwnerState,
    cpu: Option<usize>,
) -> RunOwnerSnapshot {
    let generation = expected.generation + 1;
    RunOwnerSnapshot::new(state, cpu, generation)
}

// ---------------------------------------------------------------------------
// In-place edits (generation-preserving): run intent and wait metadata.
// ---------------------------------------------------------------------------

/// Records whether the task still wants to run, without moving its state.
///
/// A task that blocks while it is executing stays `Running` but stops being
/// runnable; the next return publishes it `Blocked`. Linux keeps the same two
/// facts apart in `p->on_rq` and the task state. The generation is not
/// advanced: this is not a custody change. Raising is refused outside
/// [`RunOwnerState::holds_run_custody`] and lowering outside
/// [`RunOwnerState::may_lower_run_intent`] panics (I1); settled words are left
/// alone.
pub(in crate::multitask::scheduler) fn set_runnable(slot: usize, runnable: bool) {
    let mut refused = None;
    let outcome = word(slot).edit_in_place(|observed| {
        refused = Some(observed);
        Some(observed.with_runnable(runnable))
    });
    // A refusal is a caller asking for run intent without custody (or for a
    // withdrawal that would strand a custody record): never a silent no-op.
    assert!(
        outcome != InPlace::Refused,
        "scheduler I1: run intent {runnable} refused for slot={slot} owner={:?}",
        refused
    );
}

/// Result of atomically converting the current running task's wait arm into a
/// non-runnable continuation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(in crate::multitask::scheduler) enum WaitCommitOutcome {
    Committed,
    WakeWon,
    InvalidOwner,
}

pub(super) fn publish_wait_arm(slot: usize, reason_kind: u8) -> bool {
    assert!(reason_kind != 0 && reason_kind < (1 << OWNER_WAIT_REASON_BITS) as u8);
    let outcome = word(slot).edit_in_place(|observed| {
        (observed.state == RunOwnerState::Running && observed.runnable)
            .then(|| observed.with_wait(true, reason_kind))
    });
    matches!(outcome, InPlace::Applied | InPlace::Unchanged)
}

pub(super) fn clear_wait_arm(slot: usize) {
    word(slot).edit_in_place(|observed| {
        (observed.wait_armed || observed.wait_reason_kind != 0)
            .then(|| observed.with_wait(false, 0))
    });
}

pub(super) fn set_wait_armed(slot: usize, armed: bool) {
    word(slot).edit_in_place(|observed| Some(observed.with_wait(armed, observed.wait_reason_kind)));
}

pub(super) fn set_wait_reason(slot: usize, reason_kind: u8) {
    assert!(reason_kind < (1 << OWNER_WAIT_REASON_BITS) as u8);
    word(slot).edit_in_place(|observed| Some(observed.with_wait(observed.wait_armed, reason_kind)));
}

pub(in crate::multitask::scheduler) fn commit_wait(slot: usize) -> WaitCommitOutcome {
    let mut outcome = WaitCommitOutcome::InvalidOwner;
    let edit = word(slot).edit_in_place(|observed| {
        if observed.state != RunOwnerState::Running || !observed.runnable {
            outcome = WaitCommitOutcome::InvalidOwner;
            return None;
        }
        if !observed.wait_armed {
            outcome = WaitCommitOutcome::WakeWon;
            return None;
        }
        outcome = WaitCommitOutcome::Committed;
        Some(
            observed
                .with_runnable(false)
                .with_wait(false, observed.wait_reason_kind),
        )
    });
    if edit == InPlace::Settled {
        return WaitCommitOutcome::InvalidOwner;
    }
    outcome
}

/// Clears the wait metadata and, while the task holds run custody, restores
/// its run intent in the same CAS that defeats a pending block commit. Called
/// only while the SAME wait-payload epoch leaf is retained.
pub(super) fn wake_wait_locked(slot: usize) {
    word(slot).edit_in_place(|observed| {
        let runnable = observed.runnable || observed.state.holds_run_custody();
        Some(observed.with_runnable(runnable).with_wait(false, 0))
    });
}

#[cfg(test)]
mod tests;
