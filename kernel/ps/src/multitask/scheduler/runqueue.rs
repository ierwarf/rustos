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

use core::sync::atomic::{fence, AtomicU64, AtomicUsize, Ordering};

use crate::multitask::irq_leaf::IrqLeaf;
use nucleus_core::util::cache_padded::CachePadded;
use nucleus_core::util::lockdep::{LockClass, MAX_TRACKED_CPUS};

use super::{frame_publication, MAX_TASK};
use validation::{bitmap_location, validate_cpu};

mod admission;
pub(super) use admission::{admit_blocked, admit_initial};
#[cfg(test)]
pub(super) use admission::{admit_running};
pub(super) mod address_space;
pub(super) mod affinity_payload;
pub(super) mod claim;
mod return_publication;
pub(super) use claim::{claim_validated_dispatch};
#[cfg(test)]
pub(super) use claim::{claim_dispatch, claim_dispatch_with_owner_generation};
pub(super) use return_publication::{publish_return, ReturnPublication};
#[cfg(test)]
pub(super) use return_publication::{test_blocked_return, test_running_claim};
mod execution_time;
pub(super) use execution_time::set as set_exec_start_local_ns;
mod idle;
pub(super) use idle::{is_idle as is_idle_slot, set as set_idle_slot};
mod ipc_handoff;
pub(super) mod simd_tls;
mod validation;
pub(super) mod wait;
pub(super) mod weight;

#[cfg(test)]
use ipc_handoff::{force_adoption_retire, publish_migrating_record};
pub(super) use ipc_handoff::{claim_blocked_handoff_duty, drain_remote_wakes, materialize_direct_handoff_with_generation, publish_local_wake, publish_remote_wake, rehome_queued_frame, rehome_queued_owned, RemoteWakeOutcome};
#[cfg(test)]
pub(super) use ipc_handoff::{materialize_direct_handoff, publish_direct_handoff, rehome_queued, rollback_direct_handoff};

mod lane;
pub(super) use lane::lane_claim;

pub(super) mod ownership;
pub(super) use ownership::{
    commit_wait, owner, owner_sequenced, set_runnable, transition, DutyKind, EnqueueDuty,
    OwnerEvent, RunOwnerSnapshot, RunOwnerState, Transition, WaitCommitOutcome,
};
use ownership::{clear_wait_arm, publish_wait_arm, set_wait_armed, set_wait_reason, wake_wait_locked};
pub(super) mod conservation;

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
static VRUNTIME_NS: [CachePadded<AtomicU64>; MAX_TASK] =
    [const { CachePadded::new(AtomicU64::new(0)) }; MAX_TASK];

/// Per-slot execution-accounting baseline. Zero means that the slot is not
/// currently charging a running interval.
static EXEC_START_TICKS: [CachePadded<AtomicU64>; MAX_TASK] =
    [const { CachePadded::new(AtomicU64::new(0)) }; MAX_TASK];
/// Per-slot saved execution-frame pointer. The owner-word generation bounds
/// its lifetime: admission installs it before publication, only the execution
/// owner replaces it at a trap boundary, and terminal release clears it.
static SAVED_RSP: [CachePadded<AtomicUsize>; MAX_TASK] =
    [const { CachePadded::new(AtomicUsize::new(0)) }; MAX_TASK];

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
#[repr(align(64))]
struct PerCpuRunQueue {
    inner: IrqLeaf<RunQueueInner, { LockClass::SchedulerRunQueue as u8 }>,
    published_load: AtomicU64,
    /// Even/odd publication epoch bracketing the multiword runnable bitmap.
    published_sequence: AtomicU64,
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
            inner: IrqLeaf::new(RunQueueInner::new()),
            published_load: AtomicU64::new(0),
            published_sequence: AtomicU64::new(0),
            published_runnable: [const { AtomicU64::new(0) }; BITMAP_WORDS],
        }
    }

    fn publish_load(&self, inner: &RunQueueInner) {
        // A local-authority scan must not silently combine membership words
        // from opposite sides of a wake/removal. Writers are serialized by
        // `inner`; odd means publication is in progress and the following even
        // value commits the bitmap and load as one observation epoch.
        self.published_sequence.fetch_add(1, Ordering::AcqRel);
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
        self.published_sequence.fetch_add(1, Ordering::AcqRel);
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

    /// Exact mailbox receipt for the slow, aged custody-conservation probe.
    /// The global pending edge is not a per-task proof: another queued task
    /// can keep it raised while this task's record is missing.
    fn holds_generation(&self, slot: usize, generation: u64) -> bool {
        (0..self.len).any(|offset| {
            let record = self.records[(self.head + offset) % self.records.len()];
            record.slot == slot && record.generation == generation
        })
    }
}

impl EnqueueDuty {
    /// `RunQueue` duty: the caller inserted this slot into the target CPU's
    /// queue under the rq lock it still holds (before the CAS, rolled back on
    /// a lost CAS), so membership is checked here, not assumed.
    fn run_queue_inserted(self, rq: &RunQueueInner) {
        assert!(
            rq.contains(self.slot()),
            "scheduler I1: run-queue duty without membership slot={} generation={}",
            self.slot(),
            self.generation()
        );
        self.discharge(DutyKind::RunQueue);
    }

    /// `Mailbox` duty: publish this generation's record to the target CPU and
    /// raise its pending edge. Returns whether this producer won the 0-to-1
    /// edge and therefore owes the target its reschedule IPI.
    pub(super) fn publish_mailbox(self, weight: u32) -> bool {
        let (slot, generation, cpu) = (self.slot(), self.generation(), self.cpu());
        self.discharge(DutyKind::Mailbox);
        publish_mailbox_record(cpu, slot, generation, weight)
    }
}

/// Publishes one generation's mailbox record to `cpu` and raises its pending
/// edge; true when this producer won the 0-to-1 edge (it owes the IPI).
fn publish_mailbox_record(cpu: usize, slot: usize, generation: u64, weight: u32) -> bool {
    REMOTE_WAKE_MAILBOXES[cpu].lock().publish(RunTransfer {
        slot,
        generation,
        weight,
    });
    // ORDERING: the mailbox lock release publishes the record before this
    // 0-to-1 edge grants notification custody to the winning producer.
    MAILBOX_PENDING[cpu]
        .compare_exchange(0, 1, Ordering::AcqRel, Ordering::Acquire)
        .is_ok()
}

static RUN_QUEUES: [PerCpuRunQueue; MAX_TRACKED_CPUS] =
    [const { PerCpuRunQueue::new() }; MAX_TRACKED_CPUS];
static REMOTE_WAKE_MAILBOXES: [IrqLeaf<RemoteWakeMailbox, { LockClass::SchedulerMailbox as u8 }>;
    MAX_TRACKED_CPUS] = [const { IrqLeaf::new(RemoteWakeMailbox::new()) }; MAX_TRACKED_CPUS];
static MAILBOX_PENDING: [CachePadded<AtomicU64>; MAX_TRACKED_CPUS] =
    [const { CachePadded::new(AtomicU64::new(0)) }; MAX_TRACKED_CPUS];

/// Exact read-only check used only after a RemoteQueued owner has remained
/// suspect for two seconds. This does not authorize placement or dispatch.
pub(super) fn mailbox_holds_generation(cpu: usize, slot: usize, generation: u64) -> bool {
    validate_cpu(cpu);
    REMOTE_WAKE_MAILBOXES[cpu].lock().holds_generation(slot, generation)
}
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
pub(super) fn take_test_local_migrating_owner() -> Option<RunOwnerSnapshot> {
    TEST_LOCAL_MIGRATING_OWNER.lock().unwrap().take()
}

#[cfg(test)]
fn reset_test_local_migrating_owner() {
    *TEST_LOCAL_MIGRATING_OWNER.lock().unwrap() = None;
}

pub(super) fn reset_before_publication() {
    frame_publication::reset_before_publication();
    ownership::reset_before_publication();
    conservation::reset_before_publication();
    for (
        ((((vruntime, exec_start), saved_rsp), stack_base), alternate_version),
        alternate_base,
    ) in VRUNTIME_NS
        .iter()
        .zip(EXEC_START_TICKS.iter())
        .zip(SAVED_RSP.iter())
        .zip(KERNEL_STACK_BASE.iter())
        .zip(ALTERNATE_KERNEL_STACK_VERSION.iter())
        .zip(ALTERNATE_KERNEL_STACK_BASE.iter())
    {
        vruntime.store(0, Ordering::Release);
        exec_start.store(0, Ordering::Release);
        saved_rsp.store(0, Ordering::Release);
        stack_base.store(0, Ordering::Release);
        alternate_version.store(0, Ordering::Release);
        alternate_base.store(0, Ordering::Release);
    }
    execution_time::reset_before_publication();
    idle::reset_before_publication();
    for stack_top in &KERNEL_STACK_TOP {
        stack_top.store(0, Ordering::Release);
    }
    for alternate_top in &ALTERNATE_KERNEL_STACK_TOP {
        alternate_top.store(0, Ordering::Release);
    }
    address_space::reset_before_publication();
    simd_tls::reset_before_publication();
    affinity_payload::reset_before_publication();
    wait::reset_before_publication();
    weight::reset_before_publication();
    for cpu in 0..MAX_TRACKED_CPUS {
        let mut rq = RUN_QUEUES[cpu].inner.lock();
        *rq = RunQueueInner::new();
        RUN_QUEUES[cpu]
            .published_sequence
            .store(0, Ordering::Release);
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
    execution_time::clear(slot);
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

/// Clears any wait epoch and, while the task holds run custody, restores its
/// run intent in the same CAS that defeats a pending block commit.
pub(super) fn wake_wait(slot: usize) {
    wait::wake(slot);
}

pub(super) fn wait_blocked(owner: RunOwnerSnapshot) -> bool {
    owner.state == RunOwnerState::Blocked || (!owner.runnable && owner.state.holds_run_custody())
}

#[cfg(test)]
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
    let event = if owner.state == RunOwnerState::Running {
        OwnerEvent::ReturnLocal
    } else {
        OwnerEvent::WakeLocal
    };
    let mut rq = RUN_QUEUES[cpu].inner.lock();
    rq.insert(slot, weight);
    let transition = match transition(slot, owner, event, Some(cpu)) {
        Ok(transition) => transition,
        Err(observed) => {
            rq.remove(slot, weight);
            RUN_QUEUES[cpu].publish_load(&rq);
            panic!("scheduler local publication lost owner race observed={observed:?}");
        }
    };
    let next = transition.next;
    transition.into_duty().run_queue_inserted(&rq);
    // The owner CAS is the custody linearization point. Bind the already
    // complete frame only afterwards, so a local reader can never accept a
    // frame for the preceding owner generation.
    frame_publication::bind_local_owner_generation(slot, next.generation);
    RUN_QUEUES[cpu].publish_load(&rq);
}

#[cfg(test)]
pub(super) fn publish_blocked(slot: usize, cpu: usize, weight: u32) {
    validate_cpu(cpu);
    let owner = owner(slot);
    assert_eq!(
        owner.cpu,
        Some(cpu),
        "scheduler blocked a foreign task owner={owner:?}"
    );
    if owner.state == RunOwnerState::Running {
        // Running tasks are never members of the fair runqueue. Their block
        // transition therefore changes only the authoritative owner word; a
        // queue acquisition cannot remove or publish any additional state.
        let _ = transition(slot, owner, OwnerEvent::ReturnBlocked, None)
            .unwrap_or_else(|observed| {
                panic!("scheduler running block lost owner race observed={observed:?}")
            })
            .settled();
        return;
    }

    assert_eq!(
        owner.state,
        RunOwnerState::Local,
        "scheduler blocked invalid owner"
    );
    // Fixture-only: production never blocks a queued task in place (a Local
    // task loses run intent and keeps its membership); no table row exists.
    let mut blocked = ownership::test_successor(owner, RunOwnerState::Blocked, None);
    blocked = blocked.with_wait(owner.wait_armed, owner.wait_reason_kind);
    let mut rq = RUN_QUEUES[cpu].inner.lock();
    rq.remove(slot, weight);
    if let Err(observed) = ownership::test_force_word(slot, owner, blocked) {
        rq.insert(slot, weight);
        RUN_QUEUES[cpu].publish_load(&rq);
        panic!("scheduler local block lost owner race observed={observed:?}");
    }
    RUN_QUEUES[cpu].publish_load(&rq);
}

pub(super) fn local_runnable_sequence(cpu: usize) -> u64 {
    validate_cpu(cpu);
    RUN_QUEUES[cpu].published_sequence.load(Ordering::Acquire)
}

/// Whether `cpu`'s published runnable mirror holds `slot`. Read-only; never an
/// owner or claim grant (the conservation scan and native witnesses read it).
pub(super) fn published_local_membership(slot: usize, cpu: usize) -> bool {
    validate_cpu(cpu);
    assert!(slot < MAX_TASK);
    RUN_QUEUES[cpu].published_runnable[slot / 64].load(Ordering::Acquire)
        & (1_u64 << (slot % 64)) != 0
}

fn local_runnable_snapshot(cpu: usize) -> [u64; BITMAP_WORDS] {
    validate_cpu(cpu);
    let published = &RUN_QUEUES[cpu].published_runnable;
    // ORDERING: Acquire observes each word's publication and orders the
    // caller's later reads of the slots it names after it.
    core::array::from_fn(|word| published[word].load(Ordering::Acquire))
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

/// Whether `slot` holds queued run custody bound for `cpu` that no CPU has
/// dispatched yet: a runnable `Local` member of that CPU's queue, or a
/// `RemoteQueued` / `Migrating` owner on its way there. Observation only.
pub(super) fn queued_on(slot: usize, cpu: usize) -> bool {
    validate_cpu(cpu);
    let owner = owner(slot);
    owner.runnable
        && owner.cpu == Some(cpu)
        && matches!(
            owner.state,
            RunOwnerState::Local | RunOwnerState::RemoteQueued | RunOwnerState::Migrating
        )
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

pub(super) fn retire(slot: usize, weight: u32) {
    // Unconditional: every caller reaches here with the slot already
    // quiescent or being retired by its own CPU, so no claim can be in flight.
    let _ = retire_slot(slot, weight, false);
}

/// Result of a withdrawal that may have to wait for a dispatch claim.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum RetireOutcome {
    /// The owner word is `Retired` (it already was, or this call retired it).
    Retired,
    /// Another CPU has claimed the slot (`Running`) but has not yet published
    /// it as Current. That CPU will install the claim and still expects the
    /// owner word to be `Running`, so nothing was changed.
    ClaimInFlight,
}

/// `retire` for a remote terminal withdrawal.
///
/// The decision and the owner CAS are one step: each iteration re-reads the
/// owner word and the CAS is against that exact value, so a claim that wins the
/// race changes the word, fails this CAS, and is seen as in flight on the next
/// iteration. A check made before calling could not give that guarantee.
pub(super) fn retire_unless_claim_in_flight(slot: usize, weight: u32) -> RetireOutcome {
    retire_slot(slot, weight, true)
}

fn retire_slot(slot: usize, weight: u32, wait_for_claim: bool) -> RetireOutcome {
    let mut retiring_spins = 0_u32;
    loop {
        let owner = owner(slot);
        match owner.state {
            RunOwnerState::Retired => return RetireOutcome::Retired,
            // I2: another withdrawal owns this word between its two exact CASes
            // (nothing runs between them but an rq removal under its lock).
            // Re-withdrawing it would break that withdrawal's exact completion
            // CAS, so wait for the completion instead. The wait is bounded: a
            // withdrawal interrupted on THIS CPU by a second one would never
            // complete, and that must be a loud failure, not a hang.
            RunOwnerState::Retiring => {
                retiring_spins += 1;
                assert!(
                    retiring_spins < RETIRING_SPIN_LIMIT,
                    "scheduler second withdrawal waited too long for Retiring slot={slot} owner={owner:?}"
                );
                core::hint::spin_loop();
                continue;
            }
            _ => {}
        }
        if owner.state == RunOwnerState::Local {
            let cpu = owner.cpu.expect("local scheduler owner omitted CPU");
            let mut rq = RUN_QUEUES[cpu].inner.lock();
            // The owner CAS precedes the membership removal. Claim, block and
            // rehome all need this rq lock, so only lock-free run-intent or
            // wait metadata can make the CAS fail; unlinking first would leave
            // a `Local` owner without membership once the lock drops on the
            // retry, which the next claim or rehome treats as a broken queue.
            let Ok(withdrawn) = transition(slot, owner, OwnerEvent::Withdraw, None) else {
                continue;
            };
            let retiring = withdrawn.settled();
            if rq.contains(slot) {
                rq.remove(slot, weight);
            }
            complete_retire(slot, retiring);
            RUN_QUEUES[cpu].publish_load(&rq);
            return RetireOutcome::Retired;
        }
        // A dispatch that re-selects the task its CPU was running claims it again
        // while it is still that CPU's published Current, so the execution role
        // below cannot tell that claim from an ordinary executing task. The
        // claiming CPU announces the claim before its owner CAS; seeing `Running`
        // from that CAS implies seeing the announcement.
        if wait_for_claim
            && owner.state == RunOwnerState::Running
            && crate::multitask::cpu_local::dispatch_turn::claim_announced(slot)
        {
            return RetireOutcome::ClaimInFlight;
        }
        if wait_for_claim
            && rustos_scheduler_frame_gate::terminal_withdraw_must_wait(
                owner.state == RunOwnerState::Running,
                crate::multitask::cpu_local::task_execution_owner(slot).is_some(),
            )
        {
            return RetireOutcome::ClaimInFlight;
        }
        let Ok(withdrawn) = transition(slot, owner, OwnerEvent::Withdraw, None) else {
            continue;
        };
        complete_retire(slot, withdrawn.settled());
        return RetireOutcome::Retired;
    }
}

/// Spins a second withdrawal may wait for the first one's completion: far
/// beyond two back-to-back CASes (even across a host vCPU preemption), short
/// enough (about a second of `pause`) that a same-CPU nesting panics instead
/// of hanging.
const RETIRING_SPIN_LIMIT: u32 = 1 << 24;

/// The exact-value completion of a withdrawal this caller started (I2).
fn complete_retire(slot: usize, retiring: RunOwnerSnapshot) {
    let _ = transition(slot, retiring, OwnerEvent::CompleteRetire, None)
        .unwrap_or_else(|observed| {
            panic!("scheduler retire completion raced observed={observed:?}")
        })
        .settled();
}

pub(super) fn release_retired(slot: usize) {
    let owner = owner(slot);
    assert_eq!(
        owner.state,
        RunOwnerState::Retired,
        "scheduler slot storage released before terminal run ownership"
    );
    let _ = transition(slot, owner, OwnerEvent::Release, None)
        .unwrap_or_else(|observed| {
            panic!("scheduler retired slot release raced observed={observed:?}")
        })
        .settled();
    VRUNTIME_NS[slot].store(0, Ordering::Release);
    EXEC_START_TICKS[slot].store(0, Ordering::Release);
    execution_time::clear(slot);
    idle::clear(slot);
    weight::clear(slot);
    address_space::clear(slot);
    wait::clear(slot);
    SAVED_RSP[slot].store(0, Ordering::Release);
    simd_tls::clear_tls_fs_base(slot);
    affinity_payload::reset_affinity(slot);
    KERNEL_STACK_BASE[slot].store(0, Ordering::Release);
    KERNEL_STACK_TOP[slot].store(0, Ordering::Release);
    clear_alternate_kernel_stack_bounds(slot);
    frame_publication::clear(slot);
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
#[cfg(test)]
pub(super) use tests::test_serial_guard;
