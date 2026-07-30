//! Lock-free, per-CPU evidence for the last completed scheduler handoff.
//!
//! Exception diagnostics cannot acquire the scheduler: the fault may have
//! interrupted scheduler publication itself. Each CPU therefore writes the
//! inactive half of a two-slot journal and release-publishes its sequence only
//! after every field is complete. A fault during the next write still sees the
//! previous completed record rather than spinning on an interrupted seqlock.

use core::sync::atomic::{AtomicBool, AtomicU8, AtomicU64, AtomicUsize, Ordering};

use super::MAX_TRACKED_CPUS;

const JOURNAL_SLOTS: usize = 2;
const WITNESS_COUNT: usize = MAX_TRACKED_CPUS * JOURNAL_SLOTS;

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct SchedulerDispatchWitness {
    pub sequence: u64,
    pub from_task: u64,
    pub to_task: u64,
    pub from_slot: usize,
    pub to_slot: usize,
    pub next_rsp: usize,
    pub to_idle: bool,
    pub atomic_activation_handoff: bool,
}

struct AtomicDispatchWitness {
    from_task: AtomicU64,
    to_task: AtomicU64,
    from_slot: AtomicUsize,
    to_slot: AtomicUsize,
    next_rsp: AtomicUsize,
    to_idle: AtomicBool,
    atomic_activation_handoff: AtomicBool,
}

impl AtomicDispatchWitness {
    const fn new() -> Self {
        Self {
            from_task: AtomicU64::new(0),
            to_task: AtomicU64::new(0),
            from_slot: AtomicUsize::new(0),
            to_slot: AtomicUsize::new(0),
            next_rsp: AtomicUsize::new(0),
            to_idle: AtomicBool::new(false),
            atomic_activation_handoff: AtomicBool::new(false),
        }
    }
}

static DISPATCH_SEQUENCES: [AtomicU64; MAX_TRACKED_CPUS] =
    [const { AtomicU64::new(0) }; MAX_TRACKED_CPUS];
static DISPATCH_WITNESSES: [AtomicDispatchWitness; WITNESS_COUNT] =
    [const { AtomicDispatchWitness::new() }; WITNESS_COUNT];

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub enum SchedulerObservationKind {
    ReadyWait = 1,
    BlockedWait = 2,
    ExitSnapshot = 3,
}

impl SchedulerObservationKind {
    fn from_raw(raw: u8) -> Option<Self> {
        match raw {
            1 => Some(Self::ReadyWait),
            2 => Some(Self::BlockedWait),
            3 => Some(Self::ExitSnapshot),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SchedulerObservation {
    pub kind: SchedulerObservationKind,
    pub subject_task: u64,
    pub subject_pid: u64,
    pub subject_slot: usize,
    pub peer_task: u64,
    pub peer_pid: u64,
    pub peer_slot: usize,
    pub elapsed_ms: u64,
    pub state_flags: u64,
    pub ready_since_ticks: u64,
    pub blocked_since_ticks: u64,
}

impl SchedulerObservation {
    pub const STATE_READY: u64 = 1 << 0;
    pub const STATE_BLOCKED: u64 = 1 << 1;
    pub const STATE_WAKE_ARMED: u64 = 1 << 2;
    pub const STATE_SUSPENDED: u64 = 1 << 3;
    pub const STATE_STOPPED: u64 = 1 << 4;
    pub const STATE_RETIRED: u64 = 1 << 5;
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SchedulerObservationWitness {
    pub sequence: u64,
    pub observation: SchedulerObservation,
}

struct AtomicObservationWitness {
    kind: AtomicU8,
    subject_task: AtomicU64,
    subject_pid: AtomicU64,
    subject_slot: AtomicUsize,
    peer_task: AtomicU64,
    peer_pid: AtomicU64,
    peer_slot: AtomicUsize,
    elapsed_ms: AtomicU64,
    state_flags: AtomicU64,
    ready_since_ticks: AtomicU64,
    blocked_since_ticks: AtomicU64,
}

impl AtomicObservationWitness {
    const fn new() -> Self {
        Self {
            kind: AtomicU8::new(0),
            subject_task: AtomicU64::new(0),
            subject_pid: AtomicU64::new(0),
            subject_slot: AtomicUsize::new(0),
            peer_task: AtomicU64::new(0),
            peer_pid: AtomicU64::new(0),
            peer_slot: AtomicUsize::new(0),
            elapsed_ms: AtomicU64::new(0),
            state_flags: AtomicU64::new(0),
            ready_since_ticks: AtomicU64::new(0),
            blocked_since_ticks: AtomicU64::new(0),
        }
    }
}

static OBSERVATION_SEQUENCES: [AtomicU64; MAX_TRACKED_CPUS] =
    [const { AtomicU64::new(0) }; MAX_TRACKED_CPUS];
static OBSERVATION_WITNESSES: [AtomicObservationWitness; WITNESS_COUNT] =
    [const { AtomicObservationWitness::new() }; WITNESS_COUNT];

#[allow(clippy::too_many_arguments)]
pub fn record_scheduler_dispatch(
    logical_index: usize,
    from_task: u64,
    to_task: u64,
    from_slot: usize,
    to_slot: usize,
    next_rsp: usize,
    to_idle: bool,
    atomic_activation_handoff: bool,
) {
    assert!(
        logical_index < MAX_TRACKED_CPUS,
        "scheduler diagnostic CPU exceeds capacity"
    );
    let current = DISPATCH_SEQUENCES[logical_index].load(Ordering::Relaxed);
    let next = current
        .checked_add(1)
        .expect("scheduler diagnostic sequence exhausted");
    let slot = &DISPATCH_WITNESSES[journal_index(logical_index, next)];
    slot.from_task.store(from_task, Ordering::Relaxed);
    slot.to_task.store(to_task, Ordering::Relaxed);
    slot.from_slot.store(from_slot, Ordering::Relaxed);
    slot.to_slot.store(to_slot, Ordering::Relaxed);
    slot.next_rsp.store(next_rsp, Ordering::Relaxed);
    slot.to_idle.store(to_idle, Ordering::Relaxed);
    slot.atomic_activation_handoff
        .store(atomic_activation_handoff, Ordering::Relaxed);
    // ORDERING: Release publishes the complete inactive journal slot. A
    // diagnostic Acquire never reads the slot currently being overwritten.
    DISPATCH_SEQUENCES[logical_index].store(next, Ordering::Release);
}

pub fn scheduler_dispatch_witness(logical_index: usize) -> Option<SchedulerDispatchWitness> {
    if logical_index >= MAX_TRACKED_CPUS {
        return None;
    }
    // ORDERING: Acquire observes every field written before the completed
    // journal sequence publication.
    let sequence = DISPATCH_SEQUENCES[logical_index].load(Ordering::Acquire);
    if sequence == 0 {
        return None;
    }
    let slot = &DISPATCH_WITNESSES[journal_index(logical_index, sequence)];
    Some(SchedulerDispatchWitness {
        sequence,
        from_task: slot.from_task.load(Ordering::Relaxed),
        to_task: slot.to_task.load(Ordering::Relaxed),
        from_slot: slot.from_slot.load(Ordering::Relaxed),
        to_slot: slot.to_slot.load(Ordering::Relaxed),
        next_rsp: slot.next_rsp.load(Ordering::Relaxed),
        to_idle: slot.to_idle.load(Ordering::Relaxed),
        atomic_activation_handoff: slot.atomic_activation_handoff.load(Ordering::Relaxed),
    })
}

pub fn record_scheduler_observation(logical_index: usize, observation: SchedulerObservation) {
    assert!(
        logical_index < MAX_TRACKED_CPUS,
        "scheduler observation CPU exceeds capacity"
    );
    let current = OBSERVATION_SEQUENCES[logical_index].load(Ordering::Relaxed);
    let next = current
        .checked_add(1)
        .expect("scheduler observation sequence exhausted");
    let slot = &OBSERVATION_WITNESSES[journal_index(logical_index, next)];
    slot.kind.store(observation.kind as u8, Ordering::Relaxed);
    slot.subject_task
        .store(observation.subject_task, Ordering::Relaxed);
    slot.subject_pid
        .store(observation.subject_pid, Ordering::Relaxed);
    slot.subject_slot
        .store(observation.subject_slot, Ordering::Relaxed);
    slot.peer_task
        .store(observation.peer_task, Ordering::Relaxed);
    slot.peer_pid.store(observation.peer_pid, Ordering::Relaxed);
    slot.peer_slot
        .store(observation.peer_slot, Ordering::Relaxed);
    slot.elapsed_ms
        .store(observation.elapsed_ms, Ordering::Relaxed);
    slot.state_flags
        .store(observation.state_flags, Ordering::Relaxed);
    slot.ready_since_ticks
        .store(observation.ready_since_ticks, Ordering::Relaxed);
    slot.blocked_since_ticks
        .store(observation.blocked_since_ticks, Ordering::Relaxed);
    // ORDERING: release-publish only after the inactive record is complete.
    OBSERVATION_SEQUENCES[logical_index].store(next, Ordering::Release);
}

pub fn scheduler_observation_witness(logical_index: usize) -> Option<SchedulerObservationWitness> {
    if logical_index >= MAX_TRACKED_CPUS {
        return None;
    }
    let sequence = OBSERVATION_SEQUENCES[logical_index].load(Ordering::Acquire);
    if sequence == 0 {
        return None;
    }
    let slot = &OBSERVATION_WITNESSES[journal_index(logical_index, sequence)];
    let kind = SchedulerObservationKind::from_raw(slot.kind.load(Ordering::Relaxed))?;
    Some(SchedulerObservationWitness {
        sequence,
        observation: SchedulerObservation {
            kind,
            subject_task: slot.subject_task.load(Ordering::Relaxed),
            subject_pid: slot.subject_pid.load(Ordering::Relaxed),
            subject_slot: slot.subject_slot.load(Ordering::Relaxed),
            peer_task: slot.peer_task.load(Ordering::Relaxed),
            peer_pid: slot.peer_pid.load(Ordering::Relaxed),
            peer_slot: slot.peer_slot.load(Ordering::Relaxed),
            elapsed_ms: slot.elapsed_ms.load(Ordering::Relaxed),
            state_flags: slot.state_flags.load(Ordering::Relaxed),
            ready_since_ticks: slot.ready_since_ticks.load(Ordering::Relaxed),
            blocked_since_ticks: slot.blocked_since_ticks.load(Ordering::Relaxed),
        },
    })
}

const fn journal_index(logical_index: usize, sequence: u64) -> usize {
    logical_index * JOURNAL_SLOTS + (sequence as usize & 1)
}

#[cfg(test)]
mod tests {
    use super::{
        SchedulerObservation, SchedulerObservationKind, record_scheduler_dispatch,
        record_scheduler_observation, scheduler_dispatch_witness, scheduler_observation_witness,
    };

    #[test]
    fn completed_dispatch_journal_keeps_the_latest_cpu_local_record() {
        record_scheduler_dispatch(0, 11, 12, 3, 4, 0x1234, false, true);
        let first = scheduler_dispatch_witness(0).expect("first witness");
        assert_eq!(first.from_task, 11);
        assert_eq!(first.to_task, 12);
        assert_eq!(first.next_rsp, 0x1234);
        assert!(first.atomic_activation_handoff);

        record_scheduler_dispatch(0, 12, 13, 4, 5, 0x5678, true, false);
        let second = scheduler_dispatch_witness(0).expect("second witness");
        assert_eq!(second.sequence, first.sequence + 1);
        assert_eq!(second.from_slot, 4);
        assert_eq!(second.to_slot, 5);
        assert!(second.to_idle);
        assert!(!second.atomic_activation_handoff);
    }

    #[test]
    fn completed_observation_journal_preserves_scheduler_context() {
        let observation = SchedulerObservation {
            kind: SchedulerObservationKind::ExitSnapshot,
            subject_task: 17,
            subject_pid: 23,
            subject_slot: 4,
            peer_task: 11,
            peer_pid: 13,
            peer_slot: 2,
            elapsed_ms: 0,
            state_flags: SchedulerObservation::STATE_BLOCKED
                | SchedulerObservation::STATE_WAKE_ARMED,
            ready_since_ticks: 29,
            blocked_since_ticks: 31,
        };
        record_scheduler_observation(1, observation);
        let witness = scheduler_observation_witness(1).expect("observation witness");
        assert_eq!(witness.observation, observation);
    }
}
