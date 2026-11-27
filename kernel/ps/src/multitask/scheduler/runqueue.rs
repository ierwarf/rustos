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

use core::sync::atomic::{fence, AtomicBool, AtomicU32, AtomicU64, AtomicUsize, Ordering};

use crate::multitask::irq_leaf::IrqLeaf;
use crate::multitask::irq_pool::{SlabPool, SlabQueue};
use crate::multitask::slot_dir::AtomicDir;
use crate::multitask::task_table::{self, NO_SLOT};
use nucleus_core::util::cache_padded::CachePadded;
use nucleus_core::util::lockdep::{LockClass, MAX_TRACKED_CPUS};

use super::frame_publication;
use validation::{validate_cpu, validate_slot};

mod admission;
pub(super) use admission::{admit_blocked, admit_initial};
#[cfg(test)]
pub(super) use admission::{admit_running};
pub(super) mod address_space;
pub(in crate::multitask) mod affinity_payload;
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
pub(super) use execution_time::{charge as charge_cpu_time, consumed as consumed_cpu_time};
mod idle;
pub(super) use idle::{is_idle as is_idle_slot, set as set_idle_slot};
mod ipc_handoff;
pub(in crate::multitask) mod simd_tls;
mod validation;
pub(in crate::multitask) mod wait;
pub(super) mod weight;

#[cfg(test)]
use ipc_handoff::{force_adoption_retire, publish_migrating_record};
pub(super) use ipc_handoff::{claim_blocked_handoff_duty, drain_remote_wakes, materialize_direct_handoff_with_generation, publish_local_wake, publish_remote_wake, rehome_queued_frame, rehome_queued_owned, RemoteWakeOutcome};
#[cfg(test)]
pub(super) use ipc_handoff::{materialize_direct_handoff, publish_direct_handoff, rehome_queued, rollback_direct_handoff};

mod lane;
pub(super) use lane::lane_claim;

pub(in crate::multitask) mod ownership;
pub(super) use ownership::{
    commit_wait, owner, owner_sequenced, set_runnable, transition, DutyKind, EnqueueDuty,
    OwnerEvent, RunOwnerSnapshot, RunOwnerState, Transition, WaitCommitOutcome,
};
use ownership::{clear_wait_arm, publish_wait_arm, set_wait_armed, set_wait_reason, wake_wait_locked};
pub(super) mod conservation;

const RQ_SLOT_WORDS: usize = task_table::MAX_TASK_SLOTS / 64;

/// Per-slot bitmaps owned by whichever `RunQueueInner` currently links `slot`.
/// Keeps `RunQueueInner`'s System-class and missing-weight counts exact in O(1)
/// even if `weight::replace` or `weight::clear` races while `slot` is queued.
static RQ_SLOT_IS_SYSTEM: [AtomicU64; RQ_SLOT_WORDS] =
    [const { AtomicU64::new(0) }; RQ_SLOT_WORDS];
static RQ_SLOT_MISSING_WEIGHT: [AtomicU64; RQ_SLOT_WORDS] =
    [const { AtomicU64::new(0) }; RQ_SLOT_WORDS];

#[inline]
fn slot_bitmap_set(bitmap: &[AtomicU64; RQ_SLOT_WORDS], slot: usize) {
    let bit = 1_u64 << (slot & 63);
    bitmap[slot >> 6].fetch_or(bit, Ordering::Relaxed);
}

#[inline]
fn slot_bitmap_clear(bitmap: &[AtomicU64; RQ_SLOT_WORDS], slot: usize) -> bool {
    let bit = 1_u64 << (slot & 63);
    (bitmap[slot >> 6].fetch_and(!bit, Ordering::Relaxed) & bit) != 0
}

#[inline]
fn slot_bitmap_test(bitmap: &[AtomicU64; RQ_SLOT_WORDS], slot: usize) -> bool {
    let bit = 1_u64 << (slot & 63);
    (bitmap[slot >> 6].load(Ordering::Relaxed) & bit) != 0
}

#[inline]
fn classify_queued_slot(slot: usize) -> (bool, bool) {
    if idle::is_idle(slot) {
        return (false, false);
    }
    match weight::observe(slot) {
        None => (false, true),
        Some(w) => (w & super::SYSTEM_CLASS_WEIGHT_FLAG != 0, false),
    }
}

/// Per-CPU bitmasks for O(1) idle-CPU selection and non-empty runqueue lookup.
/// Each mask's bit `cpu` has a single exclusive writer:
/// - `RUNNING_IDLE_CPU_MASK[cpu]`: `cpu` itself under IRQ exclusion on dispatch/bootstrap.
/// - `NONEMPTY_RQ_CPU_MASK[cpu]`: `RUN_QUEUES[cpu].inner` lock in `publish_load`.
/// - `MAILBOX_PENDING_CPU_MASK[cpu]`: `REMOTE_WAKE_MAILBOXES[cpu]` lock.
static RUNNING_IDLE_CPU_MASK: AtomicU64 = AtomicU64::new(0);
static NONEMPTY_RQ_CPU_MASK: AtomicU64 = AtomicU64::new(0);
static MAILBOX_PENDING_CPU_MASK: AtomicU64 = AtomicU64::new(0);

#[inline]
fn update_cpu_mask_bit(mask: &AtomicU64, cpu: usize, set: bool) {
    let bit = 1_u64 << cpu;
    // ORDERING: Relaxed avoids dirtying a shared cache line on steady-state CPUs.
    if (mask.load(Ordering::Relaxed) & bit != 0) != set {
        if set {
            // ORDERING: AcqRel publishes non-empty/idle transition to placement readers.
            mask.fetch_or(bit, Ordering::AcqRel);
        } else {
            // ORDERING: AcqRel publishes empty/busy transition to placement readers.
            mask.fetch_and(!bit, Ordering::AcqRel);
        }
    }
}

/// Records whether `cpu` is currently executing its idle task.
#[inline]
pub(super) fn set_cpu_running_idle(cpu: usize, idle: bool) {
    validate_cpu(cpu);
    update_cpu_mask_bit(&RUNNING_IDLE_CPU_MASK, cpu, idle);
}

/// Bitmask of CPUs currently running their idle task with empty local runqueue
/// and empty remote-wake mailbox. Advisory placement hint; owner CAS stays authority.
#[inline]
pub(super) fn idle_cpu_mask() -> u64 {
    // ORDERING: Acquire observes Release/AcqRel updates across the three disjoint masks.
    RUNNING_IDLE_CPU_MASK.load(Ordering::Acquire)
        & !NONEMPTY_RQ_CPU_MASK.load(Ordering::Acquire)
        & !MAILBOX_PENDING_CPU_MASK.load(Ordering::Acquire)
}

/// Bitmask of CPUs whose published local runqueue is non-empty.
#[inline]
pub(super) fn nonempty_rq_cpu_mask() -> u64 {
    // ORDERING: Acquire pairs with publish_load's AcqRel mask update.
    NONEMPTY_RQ_CPU_MASK.load(Ordering::Acquire)
}

/// Raw packed `(weight << 16) | count` load snapshot for tie-breaking busiest-CPU selection.
#[inline]
pub(super) fn published_load_word(cpu: usize) -> u64 {
    validate_cpu(cpu);
    // ORDERING: Acquire observes complete placement snapshot.
    RUN_QUEUES[cpu].published_load.load(Ordering::Acquire)
}

/// Marks `cpu`'s cached System-class/ready-age summary dirty so the next
/// `publish_load` recomputes it and readers fall back to scanning until then.
#[inline]
pub(super) fn note_overdue_resync_needed(cpu: usize) {
    if cpu < MAX_TRACKED_CPUS {
        // ORDERING: Release ensures readers observe invalidation after the slot mutation.
        RUN_QUEUES[cpu].overdue_resync_needed.store(true, Ordering::Release);
    }
}

/// Invalidates the owning CPU's overdue summary if `slot` is currently `Local`.
/// In steady-state production `ready_since_ticks` is written while `Blocked` or
/// `Running`, so this performs one L1-hot atomic load and zero writes.
#[inline]
pub(super) fn note_slot_resync_if_local(slot: usize) {
    let snap = owner(slot);
    if snap.state == RunOwnerState::Local
        && let Some(cpu) = snap.cpu
    {
        note_overdue_resync_needed(cpu);
    }
}

// Per-slot CFS accounting, RSP, and kernel-stack fields live in `task_table::Task`.
/// Per-slot saved execution-frame pointer. The owner-word generation bounds
/// its lifetime: admission installs it before publication, only the execution
/// owner replaces it at a trap boundary, and terminal release clears it.
struct RunQueueInner {
    cpu: u32,
    head: u32,
    tail: u32,
    runnable_count: usize,
    runnable_weight: u64,
    queue_sequence: u64,
    system_count: usize,
    missing_weight_count: usize,
    /// Earliest non-zero `ready_since_ticks` among base-System slots in this RQ,
    /// `u64::MAX` if no System slot has non-zero `ready_since`, or `0` if unknown
    /// (after removing the minimum slot) until resynced.
    min_system_ready_since: u64,
    synced_class_epoch: u64,
    // Mutation scratch belongs exclusively to `inner`'s queue lock; only the
    // published mirror below is read by other CPUs. Plain fields avoid locked
    // swaps merely to drain data whose sole writer already holds the lock.
    dirty_slots: [u32; 4],
    dirty_len: u8,
    full_resync: bool,
}

impl RunQueueInner {
    const fn new(cpu: usize) -> Self {
        Self {
            cpu: cpu as u32,
            head: NO_SLOT,
            tail: NO_SLOT,
            runnable_count: 0,
            runnable_weight: 0,
            queue_sequence: 0,
            system_count: 0,
            missing_weight_count: 0,
            min_system_ready_since: u64::MAX,
            synced_class_epoch: 0,
            dirty_slots: [NO_SLOT; 4],
            dirty_len: 0,
            full_resync: false,
        }
    }

    fn resync_class_summary(&mut self, epoch: u64) {
        let mut sys_count = 0_usize;
        let mut missing_count = 0_usize;
        let mut min_ready = u64::MAX;
        let mut cur = self.head;
        while cur != NO_SLOT {
            let slot = cur as usize;
            let (is_sys, missing) = classify_queued_slot(slot);
            if is_sys {
                slot_bitmap_set(&RQ_SLOT_IS_SYSTEM, slot);
                slot_bitmap_clear(&RQ_SLOT_MISSING_WEIGHT, slot);
                sys_count += 1;
                let since = wait::ready_since_ticks(slot);
                if since != 0 && since < min_ready {
                    min_ready = since;
                }
            } else if missing {
                slot_bitmap_clear(&RQ_SLOT_IS_SYSTEM, slot);
                slot_bitmap_set(&RQ_SLOT_MISSING_WEIGHT, slot);
                missing_count += 1;
            } else {
                slot_bitmap_clear(&RQ_SLOT_IS_SYSTEM, slot);
                slot_bitmap_clear(&RQ_SLOT_MISSING_WEIGHT, slot);
            }
            // SAFETY: exclusive `RunQueueInner` lock protects `rq_next` of queued tasks.
            cur = unsafe { *task_table::get(slot).rq_next.get() };
        }
        self.system_count = sys_count;
        self.missing_weight_count = missing_count;
        self.min_system_ready_since = min_ready;
        self.synced_class_epoch = epoch;
    }

    fn mark_dirty(&mut self, slot: u32) {
        let len = self.dirty_len;
        if (len as usize) < 4 {
            self.dirty_slots[len as usize] = slot;
            self.dirty_len = len + 1;
        } else {
            self.full_resync = true;
        }
    }

    /// Empties the queue and clears intrusive runqueue links on queued tasks.
    fn clear(&mut self) {
        let mut cur = self.head;
        while cur != NO_SLOT {
            let slot = cur as usize;
            let task = task_table::get(slot);
            slot_bitmap_clear(&RQ_SLOT_IS_SYSTEM, slot);
            slot_bitmap_clear(&RQ_SLOT_MISSING_WEIGHT, slot);
            // SAFETY: exclusive `RunQueueInner` lock protects `rq_prev/next/cpu` of queued tasks.
            let next = unsafe { *task.rq_next.get() };
            // SAFETY: exclusive `RunQueueInner` lock owns queued links before clearing.
            unsafe {
                *task.rq_prev.get() = NO_SLOT;
                *task.rq_next.get() = NO_SLOT;
                *task.rq_cpu.get() = NO_SLOT;
            }
            cur = next;
        }
        self.head = NO_SLOT;
        self.tail = NO_SLOT;
        self.runnable_count = 0;
        self.runnable_weight = 0;
        self.queue_sequence = 0;
        self.system_count = 0;
        self.missing_weight_count = 0;
        self.min_system_ready_since = u64::MAX;
        self.synced_class_epoch = weight::base_class_epoch();
        self.dirty_len = 0;
        self.full_resync = true;
    }

    fn contains(&self, slot: usize) -> bool {
        let Some(task) = task_table::try_get(slot) else {
            return false;
        };
        // SAFETY: `rq_cpu` transitions between `NO_SLOT` and `self.cpu` only under `RUN_QUEUES[self.cpu].inner`.
        unsafe { *task.rq_cpu.get() == self.cpu }
    }

    fn insert(&mut self, slot: usize, weight: u32) {
        validate_slot(slot);
        assert!(!self.contains(slot), "scheduler rq duplicate slot={slot}");
        let task = task_table::get(slot);
        let slot_u32 = slot as u32;
        let tail = self.tail;
        // SAFETY: exclusive `RunQueueInner` lock protects intrusive DLL links for tasks owned by this queue.
        unsafe {
            assert_eq!(
                *task.rq_cpu.get(),
                NO_SLOT,
                "scheduler rq slot={slot} still linked on foreign cpu"
            );
            *task.rq_prev.get() = tail;
            *task.rq_next.get() = NO_SLOT;
            *task.rq_cpu.get() = self.cpu;
            if tail != NO_SLOT {
                *task_table::get(tail as usize).rq_next.get() = slot_u32;
            } else {
                self.head = slot_u32;
            }
        }
        self.tail = slot_u32;
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
        let (is_sys, missing) = classify_queued_slot(slot);
        if is_sys {
            slot_bitmap_set(&RQ_SLOT_IS_SYSTEM, slot);
            slot_bitmap_clear(&RQ_SLOT_MISSING_WEIGHT, slot);
            self.system_count += 1;
        } else if missing {
            slot_bitmap_clear(&RQ_SLOT_IS_SYSTEM, slot);
            slot_bitmap_set(&RQ_SLOT_MISSING_WEIGHT, slot);
            self.missing_weight_count += 1;
        } else {
            slot_bitmap_clear(&RQ_SLOT_IS_SYSTEM, slot);
            slot_bitmap_clear(&RQ_SLOT_MISSING_WEIGHT, slot);
        }
        self.mark_dirty(slot_u32);
    }

    fn remove(&mut self, slot: usize, weight: u32) {
        assert!(self.contains(slot), "scheduler rq missing slot={slot}");
        let task = task_table::get(slot);
        // SAFETY: exclusive `RunQueueInner` lock owns all DLL links on this CPU queue.
        unsafe {
            let prev = *task.rq_prev.get();
            let next = *task.rq_next.get();
            if prev != NO_SLOT {
                *task_table::get(prev as usize).rq_next.get() = next;
            } else {
                self.head = next;
            }
            if next != NO_SLOT {
                *task_table::get(next as usize).rq_prev.get() = prev;
            } else {
                self.tail = prev;
            }
            *task.rq_prev.get() = NO_SLOT;
            *task.rq_next.get() = NO_SLOT;
            *task.rq_cpu.get() = NO_SLOT;
        }
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
        let was_sys = slot_bitmap_clear(&RQ_SLOT_IS_SYSTEM, slot);
        let was_missing = slot_bitmap_clear(&RQ_SLOT_MISSING_WEIGHT, slot);
        if was_missing {
            self.missing_weight_count = self.missing_weight_count.saturating_sub(1);
        }
        if was_sys {
            self.system_count = self.system_count.saturating_sub(1);
            if self.system_count == 0 {
                self.min_system_ready_since = u64::MAX;
            } else if self.min_system_ready_since != 0 && self.min_system_ready_since != u64::MAX {
                let since = wait::ready_since_ticks(slot);
                if since == 0 || since <= self.min_system_ready_since {
                    // Removed slot may have been the minimum; mark unknown until scanned/resynced.
                    self.min_system_ready_since = 0;
                }
            }
        }
        self.mark_dirty(slot as u32);
    }
}

#[repr(C, align(64))]
struct PerCpuRunQueue {
    inner: IrqLeaf<RunQueueInner, { LockClass::SchedulerRunQueue as u8 }>,
    published_load: AtomicU64,
    published_count: AtomicUsize,
    /// Even/odd publication epoch bracketing the dense published runnable slice.
    published_sequence: AtomicU64,
    published_system_count: AtomicUsize,
    published_missing_weight: AtomicBool,
    published_min_system_ready: AtomicU64,
    published_class_epoch: AtomicU64,
    overdue_resync_needed: AtomicBool,
    published_slots: AtomicDir<AtomicU32, 64, 4096>,
}

impl PerCpuRunQueue {
    const fn new(cpu: usize) -> Self {
        Self {
            inner: IrqLeaf::new(RunQueueInner::new(cpu)),
            published_load: AtomicU64::new(0),
            published_count: AtomicUsize::new(0),
            published_sequence: AtomicU64::new(0),
            published_system_count: AtomicUsize::new(0),
            published_missing_weight: AtomicBool::new(false),
            published_min_system_ready: AtomicU64::new(u64::MAX),
            published_class_epoch: AtomicU64::new(0),
            overdue_resync_needed: AtomicBool::new(false),
            published_slots: AtomicDir::new(),
        }
    }

    fn publish_load(&self, inner: &mut RunQueueInner) {
        // ORDERING: AcqRel marks publication odd before updating dense published_slots.
        let sequence = self.published_sequence.fetch_add(1, Ordering::AcqRel);
        assert_eq!(sequence & 1, 0, "scheduler rq publication overlapped a writer");
        let committed_sequence = sequence.checked_add(2)
            .expect("scheduler rq publication sequence exhausted");
        let current_epoch = weight::base_class_epoch();
        let need_summary_resync = self.overdue_resync_needed.swap(false, Ordering::AcqRel)
            || inner.synced_class_epoch != current_epoch
            || inner.full_resync;
        if core::mem::take(&mut inner.full_resync) {
            inner.dirty_len = 0;
            let old_pub = self.published_count.load(Ordering::Relaxed);
            for i in 0..old_pub {
                if let Some(cell) = self.published_slots.get(i) {
                    let old = cell.load(Ordering::Relaxed);
                    cell.store(NO_SLOT, Ordering::Relaxed);
                    if old != NO_SLOT {
                        // SAFETY: `rq_pub_idx` is exclusively owned by this queue lock.
                        unsafe {
                            *task_table::get(old as usize).rq_pub_idx.get() = NO_SLOT;
                        }
                    }
                }
            }
            let mut idx = 0_usize;
            let mut cur = inner.head;
            while cur != NO_SLOT {
                let task = task_table::get(cur as usize);
                if self
                    .published_slots
                    .get(idx)
                    .is_none()
                {
                    let _ = ensure_pub_slots_for_count(idx + 1);
                }
                if let Some(cell) = self.published_slots.get(idx) {
                    // ORDERING: Release publishes slot entry before raising published_count/seq.
                    cell.store(cur, Ordering::Release);
                }
                // SAFETY: `rq_pub_idx` and `rq_next` are protected by `inner`.
                unsafe {
                    *task.rq_pub_idx.get() = idx as u32;
                    cur = *task.rq_next.get();
                }
                idx += 1;
            }
            // ORDERING: Release publishes updated dense slice length.
            self.published_count.store(idx, Ordering::Release);
        } else {
            let mut pub_len = self.published_count.load(Ordering::Relaxed);
            let dlen = usize::from(core::mem::take(&mut inner.dirty_len));
            for i in 0..dlen {
                let s = inner.dirty_slots[i];
                let task = task_table::get(s as usize);
                let in_rq = inner.contains(s as usize);
                // SAFETY: `rq_pub_idx` is protected by `inner`.
                let cur_idx = unsafe { *task.rq_pub_idx.get() };
                if in_rq && cur_idx == NO_SLOT {
                    if !need_summary_resync
                        && slot_bitmap_test(&RQ_SLOT_IS_SYSTEM, s as usize)
                        && inner.min_system_ready_since != 0
                    {
                        let since = wait::ready_since_ticks(s as usize);
                        if since != 0 && since < inner.min_system_ready_since {
                            inner.min_system_ready_since = since;
                        }
                    }
                    if self.published_slots.get(pub_len).is_none() {
                        ensure_pub_slots_for_count(pub_len + 1);
                    }
                    if let Some(cell) = self.published_slots.get(pub_len) {
                        // ORDERING: Release publishes new dense slot before updating count/seq.
                        cell.store(s, Ordering::Release);
                    }
                    // SAFETY: `rq_pub_idx` is protected by `inner`.
                    unsafe {
                        *task.rq_pub_idx.get() = pub_len as u32;
                    }
                    pub_len += 1;
                } else if !in_rq && cur_idx != NO_SLOT {
                    let idx = cur_idx as usize;
                    pub_len -= 1;
                    if idx != pub_len {
                        let last = self
                            .published_slots
                            .get(pub_len)
                            .map_or(NO_SLOT, |c| c.load(Ordering::Relaxed));
                        if let Some(cell) = self.published_slots.get(idx) {
                            // ORDERING: Release moves last entry into freed dense slot.
                            cell.store(last, Ordering::Release);
                        }
                        if last != NO_SLOT {
                            // SAFETY: `rq_pub_idx` is protected by `inner`.
                            unsafe {
                                *task_table::get(last as usize).rq_pub_idx.get() = idx as u32;
                            }
                        }
                    }
                    if let Some(cell) = self.published_slots.get(pub_len) {
                        // ORDERING: Release clears vacated tail slot.
                        cell.store(NO_SLOT, Ordering::Release);
                    }
                    // SAFETY: `rq_pub_idx` is protected by `inner`.
                    unsafe {
                        *task.rq_pub_idx.get() = NO_SLOT;
                    }
                }
            }
            // ORDERING: Release publishes updated dense slice length.
            self.published_count.store(pub_len, Ordering::Release);
        }
        if need_summary_resync {
            inner.resync_class_summary(current_epoch);
        }
        // ORDERING: Release publishes System-class / ready-age summary before closing sequence.
        self.published_system_count
            .store(inner.system_count, Ordering::Release);
        self.published_missing_weight
            .store(inner.missing_weight_count != 0, Ordering::Release);
        self.published_min_system_ready
            .store(inner.min_system_ready_since, Ordering::Release);
        self.published_class_epoch
            .store(inner.synced_class_epoch, Ordering::Release);
        update_cpu_mask_bit(
            &NONEMPTY_RQ_CPU_MASK,
            inner.cpu as usize,
            inner.runnable_count != 0,
        );
        let count = u64::try_from(inner.runnable_count).unwrap_or(u64::MAX) & 0xffff;
        let weight = inner.runnable_weight.min((1_u64 << 48) - 1);
        // ORDERING: Release publishes read-only placement snapshot after exact rq updates.
        self.published_load
            .store((weight << 16) | count, Ordering::Release);
        // ORDERING: Release commits all dense updates to readers' Acquire
        // sequence checks. The queue lock excludes another writer, so the
        // closing publication needs no second locked read-modify-write.
        self.published_sequence.store(committed_sequence, Ordering::Release);
    }
}

static MAILBOX_POOL: SlabPool<RunTransfer> = SlabPool::new();

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

struct RemoteWakeMailbox {
    records: SlabQueue<RunTransfer>,
}

impl RemoteWakeMailbox {
    const fn new() -> Self {
        Self {
            records: SlabQueue::new(),
        }
    }

    /// Empties the mailbox and returns slab blocks to the pool.
    fn clear(&mut self) {
        self.records.clear(&MAILBOX_POOL);
    }

    fn publish(&mut self, record: RunTransfer) {
        if let Some(existing) = self
            .records
            .find_mut(&MAILBOX_POOL, |queued| queued.slot == record.slot)
        {
            *existing = record;
            return;
        }
        assert!(
            self.records.push_back(&MAILBOX_POOL, record),
            "scheduler remote wake mailbox slab pool exhausted"
        );
    }

    fn pop(&mut self) -> Option<RunTransfer> {
        self.records.pop_front(&MAILBOX_POOL)
    }

    /// Exact mailbox receipt for the slow, aged custody-conservation probe.
    fn holds_generation(&self, slot: usize, generation: u64) -> bool {
        self.records
            .any(&MAILBOX_POOL, |record| record.slot == slot && record.generation == generation)
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
    {
        let mut mailbox = REMOTE_WAKE_MAILBOXES[cpu].lock();
        mailbox.publish(RunTransfer {
            slot,
            generation,
            weight,
        });
        update_cpu_mask_bit(&MAILBOX_PENDING_CPU_MASK, cpu, true);
    }
    // ORDERING: the mailbox lock release publishes the record before this
    // 0-to-1 edge grants notification custody to the winning producer.
    MAILBOX_PENDING[cpu]
        .compare_exchange(0, 1, Ordering::AcqRel, Ordering::Acquire)
        .is_ok()
}

static RUN_QUEUES: [PerCpuRunQueue; MAX_TRACKED_CPUS] = const {
    let mut queues = [const { PerCpuRunQueue::new(0) }; MAX_TRACKED_CPUS];
    let mut cpu = 0;
    while cpu < MAX_TRACKED_CPUS {
        queues[cpu] = PerCpuRunQueue::new(cpu);
        cpu += 1;
    }
    queues
};
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
    for slot in task_table::allocated_slots() {
        let task = task_table::get(slot);
        // ORDERING: Release resets per-slot runqueue/stack state under boot fixture isolation.
        task.vruntime_ns.store(0, Ordering::Release);
        task.exec_start_ticks.store(0, Ordering::Release);
        task.rq_saved_rsp.store(0, Ordering::Release);
        task.rq_stack_base.store(0, Ordering::Release);
        task.rq_stack_top.store(0, Ordering::Release);
        task.rq_alt_version.store(0, Ordering::Release);
        task.rq_alt_base.store(0, Ordering::Release);
        task.rq_alt_top.store(0, Ordering::Release);
        // SAFETY: `reset_before_publication` runs before CPU scheduling starts or under `TEST_GUARD`.
        unsafe {
            *task.rq_prev.get() = NO_SLOT;
            *task.rq_next.get() = NO_SLOT;
            *task.rq_cpu.get() = NO_SLOT;
            *task.rq_pub_idx.get() = NO_SLOT;
        }
    }
    execution_time::reset_before_publication();
    idle::reset_before_publication();
    address_space::reset_before_publication();
    simd_tls::reset_before_publication();
    affinity_payload::reset_before_publication();
    wait::reset_before_publication();
    weight::reset_before_publication();
    super::pair::reset_before_publication();
    RUNNING_IDLE_CPU_MASK.store(0, Ordering::Release);
    NONEMPTY_RQ_CPU_MASK.store(0, Ordering::Release);
    MAILBOX_PENDING_CPU_MASK.store(0, Ordering::Release);
    for slot in task_table::allocated_slots() {
        slot_bitmap_clear(&RQ_SLOT_IS_SYSTEM, slot);
        slot_bitmap_clear(&RQ_SLOT_MISSING_WEIGHT, slot);
    }
    for cpu in 0..MAX_TRACKED_CPUS {
        let mut rq = RUN_QUEUES[cpu].inner.lock();
        rq.clear();
        RUN_QUEUES[cpu]
            .overdue_resync_needed
            .store(false, Ordering::Release);
        RUN_QUEUES[cpu]
            .published_sequence
            .store(0, Ordering::Release);
        RUN_QUEUES[cpu].publish_load(&mut rq);
        drop(rq);
        let mut mailbox = REMOTE_WAKE_MAILBOXES[cpu].lock();
        mailbox.clear();
        MAILBOX_PENDING[cpu].store(0, Ordering::Release);
    }
}

#[inline]
pub(super) fn vruntime(slot: usize) -> u64 {
    // ORDERING: Acquire pairs with initializing/updating Release stores.
    task_table::try_get(slot)
        .expect("scheduler vruntime slot exceeds capacity")
        .vruntime_ns
        .load(Ordering::Acquire)
}

/// Installs a fresh fair-share key before the slot's owner is admitted.
pub(super) fn initialize_vruntime(slot: usize, value: u64) {
    assert_eq!(
        owner(slot).state,
        RunOwnerState::Dormant,
        "scheduler vruntime initialized after owner publication"
    );
    // ORDERING: Release pairs with Acquire vruntime reads.
    task_table::get(slot).vruntime_ns.store(value, Ordering::Release);
}

/// Installs the initial execution-accounting baseline before owner admission.
pub(super) fn initialize_exec_start_ticks(slot: usize, value: u64) {
    assert_eq!(
        owner(slot).state,
        RunOwnerState::Dormant,
        "scheduler execution baseline initialized after owner publication"
    );
    // ORDERING: Release pairs with Acquire exec_start_ticks readers.
    task_table::get(slot).exec_start_ticks.store(value, Ordering::Release);
    execution_time::clear(slot);
}

#[inline]
pub(super) fn exec_start_ticks(slot: usize) -> u64 {
    // ORDERING: Acquire pairs with Release stores.
    task_table::try_get(slot)
        .expect("scheduler execution baseline slot exceeds capacity")
        .exec_start_ticks
        .load(Ordering::Acquire)
}

/// Replaces the running interval baseline. The execution owner alone chooses
/// this value; AtomicU64 keeps readers from requiring the global task catalog.
#[inline]
pub(super) fn set_exec_start_ticks(slot: usize, value: u64) {
    // ORDERING: Release pairs with Acquire readers.
    task_table::try_get(slot)
        .expect("scheduler execution baseline slot exceeds capacity")
        .exec_start_ticks
        .store(value, Ordering::Release);
}

pub(super) fn initialize_saved_rsp(slot: usize, value: usize) {
    assert_eq!(
        owner(slot).state,
        RunOwnerState::Dormant,
        "scheduler saved context initialized after owner publication"
    );
    // ORDERING: Release pairs with Acquire saved_rsp readers.
    task_table::get(slot).rq_saved_rsp.store(value, Ordering::Release);
}

#[inline]
pub(super) fn saved_rsp(slot: usize) -> usize {
    // ORDERING: Acquire pairs with Release stores.
    task_table::try_get(slot)
        .expect("scheduler saved context slot exceeds capacity")
        .rq_saved_rsp
        .load(Ordering::Acquire)
}

#[inline]
pub(super) fn set_saved_rsp(slot: usize, value: usize) {
    // ORDERING: Release pairs with Acquire readers.
    task_table::try_get(slot)
        .expect("scheduler saved context slot exceeds capacity")
        .rq_saved_rsp
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
    let task = task_table::get(slot);
    // ORDERING: Release publishes stack bounds prior to task owner publication.
    task.rq_stack_base.store(base, Ordering::Release);
    task.rq_stack_top.store(top, Ordering::Release);
}

#[inline]
pub(super) fn kernel_stack_bounds(slot: usize) -> (u64, u64) {
    let task = task_table::try_get(slot).expect("scheduler stack bounds slot exceeds capacity");
    // ORDERING: Acquire pairs with initializing Release stores.
    let base = task.rq_stack_base.load(Ordering::Acquire);
    // ORDERING: Acquire pairs with initializing Release stores.
    let top = task.rq_stack_top.load(Ordering::Acquire);
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
    let task = task_table::get(slot);
    let version = task.rq_alt_version.load(Ordering::Relaxed);
    assert_eq!(
        version & 1,
        0,
        "scheduler alternate stack replacement raced for slot {slot}"
    );
    // ORDERING: the odd version is visible before either payload half changes.
    task.rq_alt_version.store(version.wrapping_add(1), Ordering::Relaxed);
    fence(Ordering::Release);
    task.rq_alt_base.store(base, Ordering::Relaxed);
    task.rq_alt_top.store(top, Ordering::Relaxed);
    // ORDERING: this Release commits a matched base/top pair to Acquire readers.
    task.rq_alt_version.store(version.wrapping_add(2), Ordering::Release);
}

/// Reads one coherent alternate-stack pair. An observation that overlaps a
/// writer has no valid range, which causes frame validation to reject it.
#[inline]
pub(super) fn alternate_kernel_stack_bounds(slot: usize) -> (u64, u64) {
    let task = task_table::try_get(slot).expect("scheduler alternate stack version slot exceeds capacity");
    // ORDERING: Acquire pairs with Release commit below.
    let version = task.rq_alt_version.load(Ordering::Acquire);
    if version & 1 != 0 {
        return (0, 0);
    }
    let base = task.rq_alt_base.load(Ordering::Relaxed);
    let top = task.rq_alt_top.load(Ordering::Relaxed);
    // ORDERING: neither payload read may move after the validating version.
    fence(Ordering::Acquire);
    if task.rq_alt_version.load(Ordering::Relaxed) != version {
        return (0, 0);
    }
    (base, top)
}

fn clear_alternate_kernel_stack_bounds(slot: usize) {
    replace_alternate_kernel_stack_bounds(slot, 0, 0);
}

pub(super) fn add_vruntime(slot: usize, delta: u64) -> u64 {
    let cell = &task_table::get(slot).vruntime_ns;
    // ORDERING: Acquire pairs with concurrent CAS updates on vruntime_ns.
    let mut observed = cell.load(Ordering::Acquire);
    loop {
        let next = observed.saturating_add(delta);
        // ORDERING: AcqRel serializes fair-share vruntime increments.
        match cell.compare_exchange_weak(observed, next, Ordering::AcqRel, Ordering::Acquire) {
            Ok(_) => return next,
            Err(current) => observed = current,
        }
    }
}

/// Raises the fair-share key only when a sleeper floor is ahead of it.
pub(super) fn raise_vruntime_floor(slot: usize, floor: u64) -> u64 {
    let cell = &task_table::get(slot).vruntime_ns;
    // ORDERING: Acquire pairs with concurrent CAS updates on vruntime_ns.
    let mut observed = cell.load(Ordering::Acquire);
    loop {
        let next = observed.max(floor);
        if next == observed {
            return observed;
        }
        // ORDERING: AcqRel advances vruntime_ns monotonically.
        match cell.compare_exchange_weak(observed, next, Ordering::AcqRel, Ordering::Acquire) {
            Ok(_) => return next,
            Err(current) => observed = current,
        }
    }
}

/// Lowers the fair-share key only for an authenticated IPC donation.
pub(super) fn lower_vruntime_ceiling(slot: usize, ceiling: u64) -> u64 {
    let cell = &task_table::get(slot).vruntime_ns;
    // ORDERING: Acquire pairs with concurrent CAS updates on vruntime_ns.
    let mut observed = cell.load(Ordering::Acquire);
    loop {
        let next = observed.min(ceiling);
        if next == observed {
            return observed;
        }
        // ORDERING: AcqRel lowers vruntime_ns under donation.
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
pub(in crate::multitask::scheduler) fn pair_claim(
    slot: usize,
    expected: RunOwnerSnapshot,
    cpu: usize,
) -> Result<RunOwnerSnapshot, RunOwnerSnapshot> {
    validate_cpu(cpu);
    let committed = transition(slot, expected, OwnerEvent::PairClaim, Some(cpu))?;
    Ok(committed.settled())
}

pub(in crate::multitask::scheduler) fn pair_park(
    slot: usize,
    expected: RunOwnerSnapshot,
    cpu: usize,
) -> Result<RunOwnerSnapshot, RunOwnerSnapshot> {
    validate_cpu(cpu);
    let committed = transition(slot, expected, OwnerEvent::PairPark, Some(cpu))?;
    Ok(committed.settled())
}

pub(in crate::multitask::scheduler) fn pair_resume(
    slot: usize,
    expected: RunOwnerSnapshot,
    cpu: usize,
) -> Result<RunOwnerSnapshot, RunOwnerSnapshot> {
    validate_cpu(cpu);
    let committed = transition(slot, expected, OwnerEvent::PairResume, Some(cpu))?;
    Ok(committed.settled())
}

pub(in crate::multitask::scheduler) fn pair_break_blocked(
    slot: usize,
    expected: RunOwnerSnapshot,
) -> Result<RunOwnerSnapshot, RunOwnerSnapshot> {
    let committed = transition(slot, expected, OwnerEvent::PairBreakBlocked, None)?;
    Ok(committed.settled())
}

pub(in crate::multitask::scheduler) fn pair_break_local(
    slot: usize,
    cpu: usize,
    weight: u32,
) -> Option<RunOwnerSnapshot> {
    validate_cpu(cpu);
    let mut rq = RUN_QUEUES[cpu].inner.lock();
    loop {
        let expected = owner(slot);
        if expected.state != RunOwnerState::Paired || expected.cpu != Some(cpu) {
            return None;
        }
        rq.insert(slot, weight);
        match transition(slot, expected, OwnerEvent::PairBreakLocal, Some(cpu)) {
            Ok(committed) => {
                let next = committed.next;
                committed.into_duty().run_queue_inserted(&rq);
                frame_publication::bind_local_owner_generation(slot, next.generation);
                RUN_QUEUES[cpu].publish_load(&mut rq);
                return Some(next);
            }
            Err(_) => {
                rq.remove(slot, weight);
            }
        }
    }
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
            RUN_QUEUES[cpu].publish_load(&mut rq);
            panic!("scheduler local publication lost owner race observed={observed:?}");
        }
    };
    let next = transition.next;
    transition.into_duty().run_queue_inserted(&rq);
    // The owner CAS is the custody linearization point. Bind the already
    // complete frame only afterwards, so a local reader can never accept a
    // frame for the preceding owner generation.
    frame_publication::bind_local_owner_generation(slot, next.generation);
    RUN_QUEUES[cpu].publish_load(&mut rq);
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
        RUN_QUEUES[cpu].publish_load(&mut rq);
        panic!("scheduler local block lost owner race observed={observed:?}");
    }
    RUN_QUEUES[cpu].publish_load(&mut rq);
}

pub(super) fn local_runnable_sequence(cpu: usize) -> u64 {
    validate_cpu(cpu);
    RUN_QUEUES[cpu].published_sequence.load(Ordering::Acquire)
}

/// Whether `cpu`'s published runnable mirror holds `slot`. Read-only; never an
/// owner or claim grant (the conservation scan and native witnesses read it).
pub(super) fn published_local_membership(slot: usize, cpu: usize) -> bool {
    validate_cpu(cpu);
    validate_slot(slot);
    local_runnable_slots(cpu).any(|candidate| candidate == slot)
}

fn snapshot_published_range(cpu: usize) -> (u64, usize) {
    let rq = &RUN_QUEUES[cpu];
    for _ in 0..64 {
        // ORDERING: Acquire pairs with publish_load's AcqRel sequence bump.
        let s1 = rq.published_sequence.load(Ordering::Acquire);
        if s1 & 1 != 0 {
            core::hint::spin_loop();
            continue;
        }
        // ORDERING: Acquire pairs with Release published_count store.
        let count = rq.published_count.load(Ordering::Acquire);
        // ORDERING: Acquire orders count load before checking sequence unchanged.
        fence(Ordering::Acquire);
        // ORDERING: Acquire observes concurrent publish_load start.
        if rq.published_sequence.load(Ordering::Acquire) == s1 {
            return (s1, count);
        }
    }
    // ORDERING: Acquire observes current epoch if writer continuously contended.
    let s = rq.published_sequence.load(Ordering::Acquire);
    // ORDERING: Acquire observes current published_count.
    let count = if s & 1 != 0 { 0 } else { rq.published_count.load(Ordering::Acquire) };
    (s, count)
}

/// Lazily walks one CPU's dense published runnable slots, O(runnable on `cpu`).
pub(super) struct LocalRunnableSlots {
    cpu: usize,
    seq: u64,
    idx: usize,
    count: usize,
    restarts: u8,
}

impl Iterator for LocalRunnableSlots {
    type Item = usize;

    fn next(&mut self) -> Option<Self::Item> {
        if self.seq & 1 != 0 {
            return None;
        }
        let rq = &RUN_QUEUES[self.cpu];
        loop {
            if self.idx >= self.count {
                return None;
            }
            let idx = self.idx;
            self.idx += 1;
            let Some(cell) = rq.published_slots.get(idx) else {
                return None;
            };
            // ORDERING: Acquire observes Release slot write in publish_load.
            let slot = cell.load(Ordering::Acquire);
            // ORDERING: Acquire checks whether publish_load interleaved and swapped entries.
            if rq.published_sequence.load(Ordering::Acquire) != self.seq {
                if self.restarts < 2 {
                    self.restarts += 1;
                    let (seq, count) = snapshot_published_range(self.cpu);
                    self.seq = seq;
                    self.count = count;
                    self.idx = 0;
                    if self.seq & 1 != 0 {
                        return None;
                    }
                    continue;
                }
                return None;
            }
            if slot != NO_SLOT {
                return Some(slot as usize);
            }
        }
    }
}

pub(super) fn local_runnable_slots(cpu: usize) -> LocalRunnableSlots {
    validate_cpu(cpu);
    let (seq, count) = snapshot_published_range(cpu);
    LocalRunnableSlots {
        cpu,
        seq,
        idx: 0,
        count,
        restarts: 0,
    }
}

pub(super) fn published_runnable_count(cpu: usize) -> usize {
    validate_cpu(cpu);
    // ORDERING: Acquire observes the exact queue membership published with
    // the corresponding count.
    RUN_QUEUES[cpu].published_count.load(Ordering::Acquire)
}

/// Reports whether the CPU-local dispatch owner has work beyond the task that
/// is already running on this CPU.
pub(super) fn local_dispatch_work_pending(cpu: usize) -> bool {
    validate_cpu(cpu);
    // ORDERING: Acquire observes either a complete rq insertion published by
    // `publish_load` or a remote mailbox record published before its pending edge.
    let runnable_count = RUN_QUEUES[cpu].published_count.load(Ordering::Acquire);
    // ORDERING: Acquire pairs with remote wake pending publication.
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
pub(in crate::multitask::scheduler) enum RetireOutcome {
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
pub(in crate::multitask::scheduler) fn retire_unless_claim_in_flight(
    slot: usize,
    weight: u32,
) -> RetireOutcome {
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
            RUN_QUEUES[cpu].publish_load(&mut rq);
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
        if wait_for_claim && owner.state == RunOwnerState::Paired {
            if let Some(cpu) = owner.cpu {
                super::pair::request_break_for_retire(cpu, slot, owner.generation);
                #[cfg(not(test))]
                crate::multitask::irq::request_target_reschedule(cpu);
            }
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
    let task = task_table::get(slot);
    // ORDERING: Release clears released slot accounting/stack state before slot reuse.
    task.vruntime_ns.store(0, Ordering::Release);
    // ORDERING: Release clears exec_start_ticks before slot reuse.
    task.exec_start_ticks.store(0, Ordering::Release);
    execution_time::clear(slot);
    idle::clear(slot);
    weight::clear(slot);
    address_space::clear(slot);
    wait::clear(slot);
    // ORDERING: Release clears saved_rsp before slot reuse.
    task.rq_saved_rsp.store(0, Ordering::Release);
    simd_tls::clear_tls_fs_base(slot);
    affinity_payload::reset_affinity(slot);
    // ORDERING: Release clears stack_base before slot reuse.
    task.rq_stack_base.store(0, Ordering::Release);
    // ORDERING: Release clears stack_top before slot reuse.
    task.rq_stack_top.store(0, Ordering::Release);
    clear_alternate_kernel_stack_bounds(slot);
    frame_publication::clear(slot);
}

/// O(1) proof that `cpu`'s runqueue at `expected_queue_sequence` contains no
/// base-System slot with an overdue ready age (and no missing-weight slot that
/// must fail closed). Caller must separately check `live_inherited_system_count() == 0`
/// and `class_publication_sequence`.
pub(super) fn overdue_system_short_circuit(
    cpu: usize,
    now_ticks: u64,
    ticks_per_second: u64,
    bound_ms: u64,
    expected_queue_sequence: u64,
) -> bool {
    validate_cpu(cpu);
    let rq = &RUN_QUEUES[cpu];
    // ORDERING: Acquire pairs with publish_load's Release stores.
    if rq.published_count.load(Ordering::Acquire) == 0 {
        return rq.published_sequence.load(Ordering::Acquire) == expected_queue_sequence;
    }
    if rq.overdue_resync_needed.load(Ordering::Acquire)
        || rq.published_class_epoch.load(Ordering::Acquire) != weight::base_class_epoch()
    {
        return false;
    }
    let sys_count = rq.published_system_count.load(Ordering::Acquire);
    let min_ready = rq.published_min_system_ready.load(Ordering::Acquire);
    let has_missing_weight = rq.published_missing_weight.load(Ordering::Acquire);
    fence(Ordering::Acquire);
    if rq.published_sequence.load(Ordering::Acquire) != expected_queue_sequence {
        return false;
    }
    if has_missing_weight {
        return false;
    }
    if sys_count == 0 || min_ready == u64::MAX {
        return true;
    }
    if min_ready == 0 {
        return false;
    }
    let elapsed_ms = now_ticks
        .saturating_sub(min_ready)
        .saturating_mul(1000)
        .saturating_div(ticks_per_second);
    elapsed_ms < bound_ms
}

/// O(1) proof that `cpu`'s runqueue at `expected_queue_sequence` has zero
/// base-System slots and zero missing-weight slots. Caller must separately
/// check `live_inherited_system_count() == 0` and `class_publication_sequence`.
pub(super) fn has_no_system_runnable(cpu: usize, expected_queue_sequence: u64) -> bool {
    validate_cpu(cpu);
    let rq = &RUN_QUEUES[cpu];
    if rq.published_count.load(Ordering::Acquire) == 0 {
        return rq.published_sequence.load(Ordering::Acquire) == expected_queue_sequence;
    }
    if rq.overdue_resync_needed.load(Ordering::Acquire)
        || rq.published_class_epoch.load(Ordering::Acquire) != weight::base_class_epoch()
    {
        return false;
    }
    let sys_count = rq.published_system_count.load(Ordering::Acquire);
    let has_missing_weight = rq.published_missing_weight.load(Ordering::Acquire);
    fence(Ordering::Acquire);
    if rq.published_sequence.load(Ordering::Acquire) != expected_queue_sequence {
        return false;
    }
    sys_count == 0 && !has_missing_weight
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
    let mut remaining = eligible_mask & !(1_u64 << fallback_cpu);
    while remaining != 0 {
        let cpu = remaining.trailing_zeros() as usize;
        remaining &= remaining - 1;
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

pub(in crate::multitask) fn ensure_pub_slots_for_count(slots: usize) {
    let needed = slots.max(1);
    for cpu in 0..MAX_TRACKED_CPUS {
        for idx in 0..needed {
            let _ = RUN_QUEUES[cpu].published_slots.ensure(idx, || AtomicU32::new(NO_SLOT));
        }
    }
}

pub(in crate::multitask) fn preallocate_mailbox_pool(min_free: usize) {
    SlabQueue::<RunTransfer>::preallocate_for_pool(&MAILBOX_POOL, min_free);
}
