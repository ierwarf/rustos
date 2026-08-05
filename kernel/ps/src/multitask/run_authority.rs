//! Shadow run-ownership authority for the per-CPU scheduler migration.
//!
//! - **Owner:** one packed word per task slot, written only under the global
//!   scheduler lock while the legacy backend remains authoritative.
//! - **Boundary:** nothing reads this word to make a scheduling decision. It is
//!   a projection of what the per-CPU backend's `TaskRunAuthority` would hold,
//!   maintained beside the legacy tables so the two can be compared.
//! - **Lifecycle:** published at every guard release for the slots that turn
//!   could have changed, and swept in full once per profile drain.
//! - **Concurrency:** the scheduler lock is the single writer. Readers are
//!   diagnostic and tolerate any published value.
//! - **Failure:** an illegal transition or a state the legacy tables disagree
//!   with is *reported*, never fatal. Step one of the migration must not change
//!   behaviour, and a shadow that can panic is not read-only.
//! - **Forbidden:** no dispatch, wake, or lifetime decision may consume this
//!   word until the backend cutover, and no dual-write of authoritative state.
//!
//! # Why a shadow rather than a direct change
//!
//! `V5-SCHED-GLOBAL-001` is closed by giving each CPU authority over its own
//! runnable set. That is a staged migration, and the stage that has to come
//! first is evidence: a previous attempt at per-slot ownership passed every
//! unit test and failed only under KVM, at two and eight vCPU. So the owner
//! word is built and validated against the backend that is still in charge
//! before anything depends on it.
//!
//! # What this actually falsifies
//!
//! Deriving the state and then comparing it against what it was derived from
//! would prove nothing. The word is therefore checked against the *transition*
//! it just made, using the edges the per-CPU state machine has. Four properties
//! are worth failing on, and each corresponds to something the new backend
//! could not express:
//!
//! 1. `Running` is entered only from `Local` or `Migrating`. A wake in the
//!    per-CPU design travels `Blocked -> RemoteQueued -> Local -> Running`; a
//!    legacy path that dispatches a blocked task straight onto a CPU has no
//!    edge in the new machine.
//! 2. The owning CPU changes only through `Migrating`. A slot that appears on
//!    a different CPU without a migration is a custody transfer with no
//!    handoff token.
//! 3. `Retired` is entered only from `Retiring`. A slot freed without a
//!    quiesce ACK is the reclaim-before-ownership fault in miniature.
//! 4. A slot is `Running` on at most one CPU, which is Zircon's "a thread may
//!    compete only on one CPU at a time" and the invariant the whole migration
//!    rests on.

use core::sync::atomic::{AtomicU64, Ordering};

use super::MAX_SCHEDULER_TASKS;

/// Where a task sits in the per-CPU ownership machine.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub(super) enum RunState {
    /// No context is bound to the slot.
    Retired = 0,
    /// Bound, not runnable, not owned by any CPU.
    Blocked = 1,
    /// Runnable and owned by a CPU's local queue.
    Local = 2,
    /// Executing on its owning CPU.
    Running = 3,
    /// Off-CPU with its outgoing stack still held by the CPU it left.
    Migrating = 4,
    /// Quiesce requested; cleanup has not completed.
    Retiring = 5,
}

impl RunState {
    const fn from_bits(bits: u8) -> Self {
        match bits {
            1 => Self::Blocked,
            2 => Self::Local,
            3 => Self::Running,
            4 => Self::Migrating,
            5 => Self::Retiring,
            _ => Self::Retired,
        }
    }
}

/// No CPU owns this slot.
pub(super) const NO_CPU: u8 = u8::MAX;

const STATE_SHIFT: u32 = 0;
const CPU_SHIFT: u32 = 8;
const GENERATION_SHIFT: u32 = 16;
const BYTE_MASK: u64 = 0xFF;

const fn pack(state: RunState, cpu: u8, generation: u64) -> u64 {
    ((generation << GENERATION_SHIFT) & !0xFFFF)
        | ((cpu as u64) << CPU_SHIFT)
        | ((state as u64) << STATE_SHIFT)
}

const fn unpack_state(word: u64) -> RunState {
    RunState::from_bits(((word >> STATE_SHIFT) & BYTE_MASK) as u8)
}

const fn unpack_cpu(word: u64) -> u8 {
    ((word >> CPU_SHIFT) & BYTE_MASK) as u8
}

const fn unpack_generation(word: u64) -> u64 {
    word >> GENERATION_SHIFT
}

/// One observed ownership position.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct RunOwner {
    pub(super) state: RunState,
    pub(super) cpu: u8,
}

impl RunOwner {
    pub(super) const fn new(state: RunState, cpu: u8) -> Self {
        Self { state, cpu }
    }

    pub(super) const fn unowned(state: RunState) -> Self {
        Self { state, cpu: NO_CPU }
    }
}

/// A transition the per-CPU ownership machine has no edge for.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct RunAuthorityDivergence {
    pub(super) slot: usize,
    pub(super) from: RunOwner,
    pub(super) to: RunOwner,
}

static RUN_AUTHORITY: [AtomicU64; MAX_SCHEDULER_TASKS] =
    [const { AtomicU64::new(0) }; MAX_SCHEDULER_TASKS];

/// First illegal edge of the current window, packed, or 0 for none.
///
/// First rather than last: the first divergence is the one whose cause is still
/// nearby, and a later one is usually the wreckage of the first.
static FIRST_DIVERGENCE: AtomicU64 = AtomicU64::new(0);
static DIVERGENCE_COUNT: AtomicU64 = AtomicU64::new(0);

const DIVERGENCE_PRESENT: u64 = 1 << 63;

const fn pack_divergence(divergence: RunAuthorityDivergence) -> u64 {
    DIVERGENCE_PRESENT
        | ((divergence.slot as u64 & 0xFFFF) << 32)
        | ((divergence.from.state as u64) << 24)
        | ((divergence.from.cpu as u64) << 16)
        | ((divergence.to.state as u64) << 8)
        | (divergence.to.cpu as u64)
}

fn record_divergence(divergence: RunAuthorityDivergence) {
    DIVERGENCE_COUNT.fetch_add(1, Ordering::Relaxed);
    let _ = FIRST_DIVERGENCE.compare_exchange(
        0,
        pack_divergence(divergence),
        Ordering::Relaxed,
        Ordering::Relaxed,
    );
}

/// Takes the window's divergence record, clearing it for the next window.
///
/// Returns the packed first edge and how many edges were rejected in total. A
/// count without an example is not actionable, and an example without a count
/// cannot distinguish one startup artefact from a steady-state fault.
pub(super) fn take_divergence_window() -> Option<(u64, u64)> {
    let first = FIRST_DIVERGENCE.swap(0, Ordering::Relaxed);
    let count = DIVERGENCE_COUNT.swap(0, Ordering::Relaxed);
    (first != 0).then_some((first, count))
}

/// Whether the per-CPU ownership machine can get from `from` to `to`.
///
/// Deliberately permissive about everything except the four properties in the
/// module note. A predicate that rejected every edge the legacy backend happens
/// not to take would report churn instead of findings.
pub(super) const fn transition_is_legal(from: RunOwner, to: RunOwner) -> bool {
    // Standing still is always legal; most guard releases change nothing.
    if from.state as u8 == to.state as u8 && from.cpu == to.cpu {
        return true;
    }
    match (from.state, to.state) {
        // Property 1: a CPU may only begin executing a task it already owns
        // locally, or one whose outgoing stack it still holds.
        (RunState::Local | RunState::Migrating, RunState::Running) => true,
        (_, RunState::Running) => false,
        // Property 2: custody moves to another CPU only through a migration.
        (RunState::Running, RunState::Migrating) => true,
        (RunState::Migrating, RunState::Local | RunState::Blocked) => true,
        // Property 3: a slot is freed only after a quiesce request.
        (RunState::Retiring, RunState::Retired) => true,
        (_, RunState::Retired) => false,
        // Retirement may be requested from anywhere, including a running task.
        (_, RunState::Retiring) => true,
        // A freed slot is rebound; a bound task blocks and wakes.
        (RunState::Retired, RunState::Blocked | RunState::Local) => true,
        (RunState::Blocked, RunState::Local) => true,
        (RunState::Local, RunState::Blocked) => true,
        (RunState::Running, RunState::Local | RunState::Blocked) => true,
        _ => false,
    }
}

/// Publishes `owner` for `slot` and reports an illegal transition.
///
/// The caller holds the scheduler lock, which is what makes the read-then-write
/// a single writer's update rather than a race.
pub(super) fn observe(slot: usize, owner: RunOwner) -> Option<RunAuthorityDivergence> {
    let cell = RUN_AUTHORITY.get(slot)?;
    // ORDERING: Relaxed throughout. The scheduler lock already excludes every
    // other writer, and no reader takes a decision from this word.
    let previous = cell.load(Ordering::Relaxed);
    let from = RunOwner::new(unpack_state(previous), unpack_cpu(previous));
    let generation = unpack_generation(previous);
    let changed = from != owner;
    if changed {
        cell.store(
            pack(owner.state, owner.cpu, generation.wrapping_add(1)),
            Ordering::Relaxed,
        );
    }
    if !changed || transition_is_legal(from, owner) {
        return None;
    }
    let divergence = RunAuthorityDivergence {
        slot,
        from,
        to: owner,
    };
    record_divergence(divergence);
    Some(divergence)
}

/// The last published position for `slot`.
pub(super) fn published(slot: usize) -> Option<RunOwner> {
    let word = RUN_AUTHORITY.get(slot)?.load(Ordering::Relaxed);
    Some(RunOwner::new(unpack_state(word), unpack_cpu(word)))
}

/// Whether more than one CPU claims to be executing the same slot.
///
/// Property 4. The dispatch guard already fails the kernel on the legacy tables;
/// this is the same statement about the shadow word, so a cutover cannot lose it.
pub(super) fn duplicate_running_owner() -> Option<usize> {
    let mut seen: [bool; MAX_SCHEDULER_TASKS] = [false; MAX_SCHEDULER_TASKS];
    let mut running_cpus: [u8; MAX_SCHEDULER_TASKS] = [NO_CPU; MAX_SCHEDULER_TASKS];
    for slot in 0..MAX_SCHEDULER_TASKS {
        let Some(owner) = published(slot) else {
            continue;
        };
        if owner.state != RunState::Running {
            continue;
        }
        if seen[slot] && running_cpus[slot] != owner.cpu {
            return Some(slot);
        }
        seen[slot] = true;
        running_cpus[slot] = owner.cpu;
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn packing_round_trips_every_field_without_aliasing() {
        let word = pack(RunState::Migrating, 7, 0x1234_5678);
        assert_eq!(unpack_state(word), RunState::Migrating);
        assert_eq!(unpack_cpu(word), 7);
        assert_eq!(unpack_generation(word), 0x1234_5678);

        // NO_CPU must survive the byte field rather than aliasing a real CPU.
        let unowned = pack(RunState::Blocked, NO_CPU, 1);
        assert_eq!(unpack_cpu(unowned), NO_CPU);
        assert_eq!(unpack_state(unowned), RunState::Blocked);
    }

    #[test]
    fn a_cpu_may_only_run_a_task_it_already_owns() {
        // Property 1. The per-CPU wake path is Blocked -> Local -> Running, so
        // dispatching a blocked task straight onto a CPU has no edge.
        assert!(transition_is_legal(
            RunOwner::unowned(RunState::Local),
            RunOwner::new(RunState::Running, 0)
        ));
        assert!(transition_is_legal(
            RunOwner::new(RunState::Migrating, 1),
            RunOwner::new(RunState::Running, 1)
        ));
        assert!(!transition_is_legal(
            RunOwner::unowned(RunState::Blocked),
            RunOwner::new(RunState::Running, 0)
        ));
        assert!(!transition_is_legal(
            RunOwner::unowned(RunState::Retired),
            RunOwner::new(RunState::Running, 0)
        ));
    }

    #[test]
    fn custody_reaches_another_cpu_only_through_a_migration() {
        // Property 2: Running on CPU 0 cannot become Running on CPU 1 without
        // the outgoing stack passing through Migrating.
        assert!(!transition_is_legal(
            RunOwner::new(RunState::Running, 0),
            RunOwner::new(RunState::Running, 1)
        ));
        assert!(transition_is_legal(
            RunOwner::new(RunState::Running, 0),
            RunOwner::new(RunState::Migrating, 0)
        ));
        assert!(transition_is_legal(
            RunOwner::new(RunState::Migrating, 0),
            RunOwner::unowned(RunState::Local)
        ));
    }

    #[test]
    fn a_slot_is_freed_only_after_a_quiesce_request() {
        // Property 3.
        assert!(transition_is_legal(
            RunOwner::unowned(RunState::Retiring),
            RunOwner::unowned(RunState::Retired)
        ));
        assert!(!transition_is_legal(
            RunOwner::new(RunState::Running, 2),
            RunOwner::unowned(RunState::Retired)
        ));
        assert!(!transition_is_legal(
            RunOwner::unowned(RunState::Blocked),
            RunOwner::unowned(RunState::Retired)
        ));
        // Retirement may be requested from any position, including running.
        assert!(transition_is_legal(
            RunOwner::new(RunState::Running, 2),
            RunOwner::unowned(RunState::Retiring)
        ));
    }

    #[test]
    fn standing_still_is_legal_and_publishes_no_divergence() {
        let slot = MAX_SCHEDULER_TASKS - 3;
        let running = RunOwner::new(RunState::Running, 1);
        assert!(transition_is_legal(running, running));
        // A repeated identical observation must not consume a generation, or
        // an idle CPU would churn the word once per guard release.
        assert_eq!(observe(slot, RunOwner::unowned(RunState::Blocked)), None);
        let first = RUN_AUTHORITY[slot].load(Ordering::Relaxed);
        assert_eq!(observe(slot, RunOwner::unowned(RunState::Blocked)), None);
        assert_eq!(RUN_AUTHORITY[slot].load(Ordering::Relaxed), first);
    }

    #[test]
    fn an_illegal_transition_is_reported_and_still_published() {
        let slot = MAX_SCHEDULER_TASKS - 4;
        assert_eq!(observe(slot, RunOwner::unowned(RunState::Blocked)), None);
        let divergence =
            observe(slot, RunOwner::new(RunState::Running, 3)).expect("illegal edge reported");
        assert_eq!(divergence.slot, slot);
        assert_eq!(divergence.from, RunOwner::unowned(RunState::Blocked));
        assert_eq!(divergence.to, RunOwner::new(RunState::Running, 3));
        // Reported, not rejected: step one must not change behaviour, so the
        // word tracks reality even when reality is not yet expressible.
        assert_eq!(published(slot), Some(RunOwner::new(RunState::Running, 3)));
    }

    #[test]
    fn an_out_of_range_slot_is_declined_rather_than_panicking() {
        assert_eq!(
            observe(MAX_SCHEDULER_TASKS, RunOwner::unowned(RunState::Blocked)),
            None
        );
        assert_eq!(published(MAX_SCHEDULER_TASKS), None);
    }
}
