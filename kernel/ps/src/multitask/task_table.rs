//! Consolidated per-task state (`Task`) and grow-only chunked `TaskDir`.
//!
//! - **Owner:** `kernel-ps` owns task slot assignment, per-task allocation at
//!   creation time, and the lock-free `AtomicPtr<TaskChunk>` directory.
//! - **Boundary:** No compile-time `SLOT_LIMIT` or boot-time `BootTable`s. Slots
//!   are 32-bit (`NO_SLOT = u32::MAX`); `0` is the BSP/root slot,
//!   `1..FIRST_DYNAMIC_TASK_SLOT` are fixed AP idle slots, and slots
//!   `>= FIRST_DYNAMIC_TASK_SLOT` are dynamically allocated on demand and
//!   recycled after retirement reclaim.
//! - **Reader Lifetime:** `Task` objects and `TaskChunk` pointer tables are
//!   published once with Release semantics and never moved or freed for the
//!   lifetime of the kernel (`'static`), matching `slot_dir::AtomicDir`. This
//!   lets lock-free IRQ, idle, and dispatch readers hold `&'static Task` safely
//!   without locks or epochs, while heavy per-task resources (`KernelStack`
//!   virtual slots and physical frames) are freed immediately at reclaim.
//! - **Concurrency:** Chunk publication uses `AcqRel` CAS; slot recycling uses
//!   a 64-bit `(generation << 32) | slot` lock-free freelist stack (ABA-free).
//!   All heap allocation (`Task`, `TaskChunk`, per-chunk metadata growth) happens
//!   exclusively in preemptible, unmasked task creation context—never in IRQs
//!   or while holding masked scheduler/raw locks.
//! - **Evidence:** Unit tests in `task_table::tests` and SMP creation stress.

use alloc::alloc::{Layout, alloc};
use alloc::boxed::Box;
use core::cell::UnsafeCell;
use core::ptr::{self, addr_of_mut};
use core::sync::atomic::{AtomicBool, AtomicPtr, AtomicU32, AtomicU64, AtomicU8, AtomicUsize, Ordering};

use nucleus_core::util::cache_padded::CachePadded;
use nucleus_core::util::lockdep::MAX_TRACKED_CPUS;

use super::{current_identity, scheduler};

pub(in crate::multitask) const NO_SLOT: u32 = u32::MAX;
pub(in crate::multitask) const NO_BORROWED_OWNER: u32 = u32::MAX;
pub(in crate::multitask) const TASK_CHUNK_SIZE: usize = 64;
pub(in crate::multitask) const MAX_TASK_CHUNKS: usize = 4096;
pub(in crate::multitask) const MAX_TASK_SLOTS: usize = TASK_CHUNK_SIZE * MAX_TASK_CHUNKS;

#[cfg(test)]
const MAX_TEST_AUTO_SLOTS: usize = 4096;

/// Consolidated per-task state allocated individually on slot creation.
///
/// Hot dispatch, IPC, identity, and runqueue fields live first so fast paths
/// hit adjacent cache lines without traversing separate global tables.
#[repr(C, align(64))]
pub(in crate::multitask) struct Task {
    pub(super) slot: u32,
    next_free: AtomicU32,
    pub(super) owner: CachePadded<scheduler::runqueue::ownership::RunOwnerWord>,
    pub(super) identity_pub: CachePadded<current_identity::PublishedIdentity>,
    pub(super) signal_pending: AtomicBool,
    pub(super) idle: AtomicBool,
    pub(super) frozen_outgoing: AtomicBool,
    pub(super) inherited_system: AtomicU8,
    pub(super) borrowed_ctx_owner: AtomicU32,
    pub(super) borrowed_ctx_namespace: AtomicU8,
    pub(super) borrowed_ctx_reply: AtomicU64,
    pub(super) frame: CachePadded<scheduler::frame_publication::FramePubSlot>,
    pub(super) start_mirror: scheduler::stack_resources::task_start::StartMirror,
    pub(super) weight: CachePadded<AtomicU32>,
    pub(super) vruntime_ns: CachePadded<AtomicU64>,
    pub(super) exec_start_ticks: AtomicU64,
    pub(super) rq_saved_rsp: CachePadded<AtomicUsize>,
    pub(super) rq_stack_base: AtomicU64,
    pub(super) rq_stack_top: AtomicU64,
    pub(super) rq_alt_version: AtomicU64,
    pub(super) rq_alt_base: AtomicU64,
    pub(super) rq_alt_top: AtomicU64,
    pub(super) exec_start_local_ns: AtomicU64,
    pub(super) settled_cpu_ns: AtomicU64,
    pub(super) address_space_root: AtomicU64,
    pub(super) tls_fs_base: AtomicU64,
    pub(super) exec_refs: AtomicU32,
    pub(super) exec_intent_word: AtomicU64,
    pub(super) cohort_batch: AtomicU64,
    pub(super) cohort_member: AtomicU64,
    pub(super) lazy_edge: CachePadded<scheduler::donation_ledger::LazyDonationEdge>,
    /// Intrusive per-CPU runqueue doubly-linked list links.
    /// Owned exclusively by the CPU's `RunQueueInner` lock while queued.
    pub(super) rq_prev: UnsafeCell<u32>,
    pub(super) rq_next: UnsafeCell<u32>,
    pub(super) rq_cpu: UnsafeCell<u32>,
    pub(super) rq_pub_idx: UnsafeCell<u32>,
    pub(super) wait_ready_since: AtomicU64,
    pub(super) wait_blocked_since: AtomicU64,
    pub(super) wait_reason_id: AtomicU64,
    pub(super) wait_epoch: scheduler::runqueue::wait::WaitEpochLeaf,
    pub(super) affinity: scheduler::runqueue::affinity_payload::SlotAffinity,
    pub(super) last_cpu: AtomicU8,
    pub(super) conservation_suspect_word: AtomicU64,
    pub(super) conservation_suspect_since: AtomicU64,
    pub(super) conservation_reported_word: AtomicU64,
    pub(super) profile_history: AtomicU64,
    pub(super) long_blocked_gen: AtomicU64,
    pub(super) long_blocked_since: AtomicU64,
    pub(super) identity_writer: current_identity::IdentityWriterCell,
    pub(super) resources: scheduler::stack_resources::ResourceCell,
    pub(super) resident: CachePadded<scheduler::resident::Cell>,
    pub(super) retirement: scheduler::retirement_cleanup::CleanupCell,
    pub(super) linux_thread_state: scheduler::linux_thread_state::LinuxThreadStateLock,
    pub(super) context_budget: scheduler::scheduling_runtime::ContextBudgetLock,
    pub(super) creation_work: scheduler::thread_creation::work::CreationRowLeaf,
    pub(super) simd: scheduler::runqueue::simd_tls::SlotSimdState,
}

// SAFETY: Every field in `Task` is atomic, lock-protected, or (`rq_prev/next/cpu`)
// exclusively mutated while holding the owning `RunQueueInner` lock.
unsafe impl Sync for Task {}

impl Task {
    /// Allocates and initializes one `Task` directly on the heap without
    /// staging a ~6 KiB temporary on the caller's stack.
    fn boxed(slot: u32) -> Option<Box<Self>> {
        let layout = Layout::new::<Self>();
        // SAFETY: `layout` has non-zero size (`size_of::<Task>() > 0`).
        let raw = unsafe { alloc(layout) }.cast::<Self>();
        if raw.is_null() {
            return None;
        }
        // SAFETY: `raw` is a fresh unique allocation of exact layout `Task`;
        // every field is initialized below before returning `Box::from_raw`.
        unsafe {
            addr_of_mut!((*raw).slot).write(slot);
            addr_of_mut!((*raw).next_free).write(AtomicU32::new(NO_SLOT));
            addr_of_mut!((*raw).owner).write(CachePadded::new(
                scheduler::runqueue::ownership::RunOwnerWord::new(),
            ));
            addr_of_mut!((*raw).identity_pub).write(CachePadded::new(
                current_identity::PublishedIdentity::empty(),
            ));
            addr_of_mut!((*raw).signal_pending).write(AtomicBool::new(false));
            addr_of_mut!((*raw).idle).write(AtomicBool::new(false));
            addr_of_mut!((*raw).frozen_outgoing).write(AtomicBool::new(false));
            addr_of_mut!((*raw).inherited_system).write(AtomicU8::new(0));
            addr_of_mut!((*raw).borrowed_ctx_owner).write(AtomicU32::new(NO_BORROWED_OWNER));
            addr_of_mut!((*raw).borrowed_ctx_namespace).write(AtomicU8::new(0));
            addr_of_mut!((*raw).borrowed_ctx_reply).write(AtomicU64::new(0));
            addr_of_mut!((*raw).frame).write(CachePadded::new(
                scheduler::frame_publication::FramePubSlot::new(),
            ));
            addr_of_mut!((*raw).start_mirror)
                .write(scheduler::stack_resources::task_start::StartMirror::new());
            addr_of_mut!((*raw).weight).write(CachePadded::new(AtomicU32::new(0)));
            addr_of_mut!((*raw).vruntime_ns).write(CachePadded::new(AtomicU64::new(0)));
            addr_of_mut!((*raw).exec_start_ticks).write(AtomicU64::new(0));
            addr_of_mut!((*raw).rq_saved_rsp).write(CachePadded::new(AtomicUsize::new(0)));
            addr_of_mut!((*raw).rq_stack_base).write(AtomicU64::new(0));
            addr_of_mut!((*raw).rq_stack_top).write(AtomicU64::new(0));
            addr_of_mut!((*raw).rq_alt_version).write(AtomicU64::new(0));
            addr_of_mut!((*raw).rq_alt_base).write(AtomicU64::new(0));
            addr_of_mut!((*raw).rq_alt_top).write(AtomicU64::new(0));
            addr_of_mut!((*raw).exec_start_local_ns).write(AtomicU64::new(0));
            addr_of_mut!((*raw).settled_cpu_ns).write(AtomicU64::new(0));
            addr_of_mut!((*raw).address_space_root).write(AtomicU64::new(0));
            addr_of_mut!((*raw).tls_fs_base).write(AtomicU64::new(0));
            addr_of_mut!((*raw).exec_refs).write(AtomicU32::new(0));
            addr_of_mut!((*raw).exec_intent_word).write(AtomicU64::new(0));
            addr_of_mut!((*raw).cohort_batch).write(AtomicU64::new(0));
            addr_of_mut!((*raw).cohort_member).write(AtomicU64::new(0));
            addr_of_mut!((*raw).lazy_edge).write(CachePadded::new(
                scheduler::donation_ledger::LazyDonationEdge::new(),
            ));
            addr_of_mut!((*raw).rq_prev).write(UnsafeCell::new(NO_SLOT));
            addr_of_mut!((*raw).rq_next).write(UnsafeCell::new(NO_SLOT));
            addr_of_mut!((*raw).rq_cpu).write(UnsafeCell::new(NO_SLOT));
            addr_of_mut!((*raw).rq_pub_idx).write(UnsafeCell::new(NO_SLOT));
            addr_of_mut!((*raw).wait_ready_since).write(AtomicU64::new(0));
            addr_of_mut!((*raw).wait_blocked_since).write(AtomicU64::new(0));
            addr_of_mut!((*raw).wait_reason_id).write(AtomicU64::new(0));
            addr_of_mut!((*raw).wait_epoch)
                .write(scheduler::runqueue::wait::new_wait_epoch_leaf());
            addr_of_mut!((*raw).affinity)
                .write(scheduler::runqueue::affinity_payload::SlotAffinity::new());
            addr_of_mut!((*raw).last_cpu).write(AtomicU8::new(u8::MAX));
            addr_of_mut!((*raw).conservation_suspect_word).write(AtomicU64::new(0));
            addr_of_mut!((*raw).conservation_suspect_since).write(AtomicU64::new(0));
            addr_of_mut!((*raw).conservation_reported_word).write(AtomicU64::new(0));
            addr_of_mut!((*raw).profile_history).write(AtomicU64::new(0));
            addr_of_mut!((*raw).long_blocked_gen).write(AtomicU64::new(0));
            addr_of_mut!((*raw).long_blocked_since).write(AtomicU64::new(0));
            addr_of_mut!((*raw).identity_writer)
                .write(current_identity::new_identity_writer_cell());
            addr_of_mut!((*raw).resources).write(scheduler::stack_resources::ResourceCell::new());
            addr_of_mut!((*raw).resident)
                .write(CachePadded::new(scheduler::resident::Cell::new()));
            addr_of_mut!((*raw).retirement)
                .write(scheduler::retirement_cleanup::CleanupCell::new_for_slot(slot));
            addr_of_mut!((*raw).linux_thread_state)
                .write(scheduler::linux_thread_state::empty_linux_thread_state_lock());
            addr_of_mut!((*raw).context_budget)
                .write(scheduler::scheduling_runtime::new_context_budget_lock());
            addr_of_mut!((*raw).creation_work)
                .write(scheduler::thread_creation::work::new_creation_row_leaf());
            scheduler::runqueue::simd_tls::SlotSimdState::init_in_place(addr_of_mut!((*raw).simd));
            Some(Box::from_raw(raw))
        }
    }
}

pub(in crate::multitask) struct TaskChunk {
    tasks: [AtomicPtr<Task>; TASK_CHUNK_SIZE],
    active_mask: AtomicU64,
    cleanup_busy_mask: AtomicU64,
    pending_work_mask: AtomicU64,
}

impl TaskChunk {
    fn boxed() -> Option<Box<Self>> {
        let layout = Layout::new::<Self>();
        // SAFETY: `TaskChunk` is zero-valid (all-null `AtomicPtr`s and zero `AtomicU64`s).
        let raw = unsafe { alloc::alloc::alloc_zeroed(layout) }.cast::<Self>();
        if raw.is_null() {
            return None;
        }
        // SAFETY: `raw` was freshly zero-allocated with `Layout::new::<Self>()`.
        Some(unsafe { Box::from_raw(raw) })
    }
}

static TASK_CHUNKS: [AtomicPtr<TaskChunk>; MAX_TASK_CHUNKS] =
    [const { AtomicPtr::new(ptr::null_mut()) }; MAX_TASK_CHUNKS];

/// Layout record for the host-side failure snapshot (`xtask kvm`), which reads a
/// stopped guest through the QEMU monitor and cannot link this crate. Words:
/// `[version, chunk_size, task_size, owner, saved_rsp, stack_base, stack_top,
/// wait_reason_id, wait_blocked_since]`; the last six are byte offsets in
/// `Task`. Chunk `i` is `TASK_CHUNKS[i]`, whose first `chunk_size` words are
/// `*Task` pointers (null = never allocated). Bump the version on any change.
#[used]
static TASK_DIAGNOSTIC_LAYOUT: [u64; 9] = [
    1,
    TASK_CHUNK_SIZE as u64,
    core::mem::size_of::<Task>() as u64,
    core::mem::offset_of!(Task, owner) as u64,
    (core::mem::offset_of!(Task, frame) + scheduler::frame_publication::FramePubSlot::SAVED_RSP_OFFSET) as u64,
    core::mem::offset_of!(Task, rq_stack_base) as u64,
    core::mem::offset_of!(Task, rq_stack_top) as u64,
    core::mem::offset_of!(Task, wait_reason_id) as u64,
    core::mem::offset_of!(Task, wait_blocked_since) as u64,
];

/// Highest chunk index + 1 that has ever been published (`<= MAX_TASK_CHUNKS`).
static PUBLISHED_CHUNK_BOUND: AtomicUsize = AtomicUsize::new(0);
/// Highest slot index + 1 that has ever been ensured.
static ALLOCATED_SLOT_BOUND: AtomicUsize = AtomicUsize::new(0);
/// First dynamic slot index. In host unit tests, start above hardcoded
/// fixture slots (`0..512`) so `claim_dynamic_slot()` never collides with
/// direct `reserve(SLOT, ...)` fixtures.
#[cfg(not(test))]
const INITIAL_DYNAMIC_SLOT: u32 = MAX_TRACKED_CPUS as u32;
#[cfg(test)]
const INITIAL_DYNAMIC_SLOT: u32 = 512;

/// Monotonic counter of slots dynamically allocated above initial boot slots.
static NEXT_FRESH_SLOT: AtomicU32 = AtomicU32::new(INITIAL_DYNAMIC_SLOT);
/// Lock-free ABA-safe freelist head: `(tag << 32) | slot` (`NO_SLOT` = empty).
static FREE_SLOT_HEAD: AtomicU64 = AtomicU64::new(NO_SLOT as u64);
static LIVE_ACTIVE_COUNT: AtomicUsize = AtomicUsize::new(0);

#[inline]
pub(in crate::multitask) fn allocated_slot_bound() -> usize {
    // ORDERING: Acquire observes every slot ensured before the bound was raised.
    ALLOCATED_SLOT_BOUND.load(Ordering::Acquire)
}

#[inline]
pub(in crate::multitask) fn active_task_count() -> usize {
    // ORDERING: Acquire observes all active_mask updates synchronized before the count read.
    LIVE_ACTIVE_COUNT.load(Ordering::Acquire)
}

#[inline]
fn chunk_for(chunk_idx: usize) -> Option<&'static TaskChunk> {
    let cell = TASK_CHUNKS.get(chunk_idx)?;
    // ORDERING: Acquire pairs with Release chunk publication in `ensure_chunk`.
    let ptr = cell.load(Ordering::Acquire);
    if ptr.is_null() {
        return None;
    }
    // SAFETY: published `TaskChunk` allocations remain valid for `'static`.
    Some(unsafe { &*ptr })
}

fn ensure_chunk(chunk_idx: usize) -> Option<&'static TaskChunk> {
    let cell = TASK_CHUNKS.get(chunk_idx)?;
    // ORDERING: Acquire pairs with Release chunk publication below.
    let existing = cell.load(Ordering::Acquire);
    if !existing.is_null() {
        // SAFETY: published `TaskChunk` allocations remain valid for `'static`.
        return Some(unsafe { &*existing });
    }
    let candidate = Box::into_raw(TaskChunk::boxed()?);
    // ORDERING: AcqRel publishes initialized chunk before raising `PUBLISHED_CHUNK_BOUND`.
    let ptr = match cell.compare_exchange(
        ptr::null_mut(),
        candidate,
        Ordering::AcqRel,
        Ordering::Acquire,
    ) {
        Ok(_) => {
            PUBLISHED_CHUNK_BOUND.fetch_max(chunk_idx + 1, Ordering::AcqRel);
            candidate
        }
        Err(winner) => {
            // SAFETY: `candidate` lost publication race and has no readers.
            drop(unsafe { Box::from_raw(candidate) });
            winner
        }
    };
    // SAFETY: `ptr` points to a permanently published `TaskChunk`.
    Some(unsafe { &*ptr })
}

/// Ensures slot `slot` exists and returns its `'static` `Task` reference.
///
/// Must be called only in preemptible normal context (never inside IRQs or
/// masked raw locks).
pub(in crate::multitask) fn ensure_slot(slot: usize) -> Option<&'static Task> {
    if slot >= MAX_TASK_SLOTS {
        return None;
    }
    let chunk = ensure_chunk(slot / TASK_CHUNK_SIZE)?;
    let cell = &chunk.tasks[slot % TASK_CHUNK_SIZE];
    // ORDERING: Acquire pairs with Release `Task` publication below.
    let existing = cell.load(Ordering::Acquire);
    if !existing.is_null() {
        // SAFETY: published `Task` allocations remain valid for `'static`.
        return Some(unsafe { &*existing });
    }
    let slot_u32 = u32::try_from(slot).ok()?;
    let candidate = Box::into_raw(Task::boxed(slot_u32)?);
    // ORDERING: AcqRel publishes initialized `Task` before making it readable.
    let ptr = match cell.compare_exchange(
        ptr::null_mut(),
        candidate,
        Ordering::AcqRel,
        Ordering::Acquire,
    ) {
        Ok(_) => {
            ALLOCATED_SLOT_BOUND.fetch_max(slot + 1, Ordering::AcqRel);
            notify_slot_growth(slot + 1);
            candidate
        }
        Err(winner) => {
            // SAFETY: `candidate` lost publication race and has no readers.
            drop(unsafe { Box::from_raw(candidate) });
            winner
        }
    };
    // SAFETY: `ptr` points to a permanently published `Task`.
    Some(unsafe { &*ptr })
}

fn notify_slot_growth(new_bound: usize) {
    scheduler::runqueue::ensure_pub_slots_for_count(new_bound);
    scheduler::donation_ledger::ensure_capacity(new_bound);
    scheduler::runtime_profile::owner::ensure_slots(new_bound);
    scheduler::irq_pool_preallocate(new_bound);
}

#[inline]
pub(in crate::multitask) fn try_get(slot: usize) -> Option<&'static Task> {
    if slot >= MAX_TASK_SLOTS {
        return None;
    }
    if let Some(chunk) = chunk_for(slot / TASK_CHUNK_SIZE) {
        // ORDERING: Acquire pairs with `ensure_slot` publication.
        let ptr = chunk.tasks[slot % TASK_CHUNK_SIZE].load(Ordering::Acquire);
        if !ptr.is_null() {
            // SAFETY: published `Task` allocations remain valid for `'static`.
            return Some(unsafe { &*ptr });
        }
    }
    #[cfg(test)]
    if slot < MAX_TEST_AUTO_SLOTS {
        return ensure_slot(slot);
    }
    None
}

#[inline]
#[track_caller]
pub(in crate::multitask) fn get(slot: usize) -> &'static Task {
    match try_get(slot) {
        Some(task) => task,
        None => panic!("scheduler slot {slot} is not allocated in TaskTable"),
    }
}

/// Allocates initial boot slots `0..MAX_TRACKED_CPUS` (`0` = root, `1..CPUS` = AP idle).
pub(in crate::multitask) fn bootstrap_initial_slots() -> bool {
    for slot in 0..MAX_TRACKED_CPUS {
        let Some(task) = ensure_slot(slot) else {
            return false;
        };
        if slot == 0 {
            set_slot_active(slot, true);
        } else {
            let _ = task;
        }
    }
    true
}

    pub(in crate::multitask) fn set_slot_active(slot: usize, active: bool) {
    let Some(chunk) = chunk_for(slot / TASK_CHUNK_SIZE) else {
        return;
    };
    let bit = 1_u64 << (slot % TASK_CHUNK_SIZE);
    if active {
        // ORDERING: AcqRel serializes bit transition with readers/writers of active_slots.
        let prev = chunk.active_mask.fetch_or(bit, Ordering::AcqRel);
        if prev & bit == 0 {
            LIVE_ACTIVE_COUNT.fetch_add(1, Ordering::Relaxed);
        }
    } else {
        // ORDERING: AcqRel releases slot teardown state before clearing bit from scanners.
        let prev = chunk.active_mask.fetch_and(!bit, Ordering::AcqRel);
        if prev & bit != 0 {
            LIVE_ACTIVE_COUNT.fetch_sub(1, Ordering::Relaxed);
        }
    }
}

pub(in crate::multitask) fn set_cleanup_busy(slot: usize, busy: bool) {
    let Some(chunk) = chunk_for(slot / TASK_CHUNK_SIZE) else {
        return;
    };
    let bit = 1_u64 << (slot % TASK_CHUNK_SIZE);
    if busy {
        // ORDERING: Release pairs with scanner Acquire snapshot.
        chunk.cleanup_busy_mask.fetch_or(bit, Ordering::Release);
    } else {
        // ORDERING: Release clears busy bit after cell transition is visible.
        chunk.cleanup_busy_mask.fetch_and(!bit, Ordering::Release);
    }
}

pub(in crate::multitask) fn set_pending_work(slot: usize, pending: bool) {
    let Some(chunk) = chunk_for(slot / TASK_CHUNK_SIZE) else {
        return;
    };
    let bit = 1_u64 << (slot % TASK_CHUNK_SIZE);
    if pending {
        // ORDERING: Release pairs with claim_next Acquire scan.
        chunk.pending_work_mask.fetch_or(bit, Ordering::Release);
    } else {
        // ORDERING: Release clears pending bit after claim or completion.
        chunk.pending_work_mask.fetch_and(!bit, Ordering::Release);
    }
}

pub(in crate::multitask) fn cleanup_busy_slots() -> impl Iterator<Item = usize> {
    // ORDERING: Acquire pairs with chunk publication in ensure_chunk.
    let chunks = PUBLISHED_CHUNK_BOUND.load(Ordering::Acquire);
    (0..chunks).flat_map(|chunk_idx| {
        // ORDERING: Acquire observes busy bit publication in set_cleanup_busy.
        let mut bits = chunk_for(chunk_idx)
            .map_or(0, |c| c.cleanup_busy_mask.load(Ordering::Acquire));
        core::iter::from_fn(move || {
            if bits == 0 {
                return None;
            }
            let bit = bits.trailing_zeros() as usize;
            bits &= bits - 1;
            Some(chunk_idx * TASK_CHUNK_SIZE + bit)
        })
    })
}

pub(in crate::multitask) fn pending_work_slots() -> impl Iterator<Item = usize> {
    // ORDERING: Acquire pairs with chunk publication in ensure_chunk.
    let chunks = PUBLISHED_CHUNK_BOUND.load(Ordering::Acquire);
    (0..chunks).flat_map(|chunk_idx| {
        // ORDERING: Acquire observes pending work publication in set_pending_work.
        let mut bits = chunk_for(chunk_idx)
            .map_or(0, |c| c.pending_work_mask.load(Ordering::Acquire));
        core::iter::from_fn(move || {
            if bits == 0 {
                return None;
            }
            let bit = bits.trailing_zeros() as usize;
            bits &= bits - 1;
            Some(chunk_idx * TASK_CHUNK_SIZE + bit)
        })
    })
}

/// Iterates every currently active/live slot (`active_mask`), O(active tasks).
pub(in crate::multitask) fn active_slots() -> impl Iterator<Item = usize> {
    // ORDERING: Acquire pairs with chunk publication in ensure_chunk.
    let chunks = PUBLISHED_CHUNK_BOUND.load(Ordering::Acquire);
    (0..chunks).flat_map(|chunk_idx| {
        // ORDERING: Acquire observes active bit publication in set_slot_active.
        let mut bits = chunk_for(chunk_idx)
            .map_or(0, |c| c.active_mask.load(Ordering::Acquire));
        core::iter::from_fn(move || {
            if bits == 0 {
                return None;
            }
            let bit = bits.trailing_zeros() as usize;
            bits &= bits - 1;
            Some(chunk_idx * TASK_CHUNK_SIZE + bit)
        })
    })
}

/// Iterates every dynamic slot (`>= FIRST_DYNAMIC_TASK_SLOT`) that is currently active.
pub(in crate::multitask) fn active_dynamic_slots() -> impl Iterator<Item = usize> {
    active_slots().filter(|&slot| slot >= MAX_TRACKED_CPUS)
}

/// Iterates every slot that has ever been instantiated in `TaskDir`.
pub(in crate::multitask) fn allocated_slots() -> impl Iterator<Item = usize> {
    // ORDERING: Acquire pairs with chunk publication in ensure_chunk.
    let chunks = PUBLISHED_CHUNK_BOUND.load(Ordering::Acquire);
    (0..chunks).flat_map(|chunk_idx| {
        let present = chunk_for(chunk_idx);
        (0..TASK_CHUNK_SIZE).filter_map(move |offset| {
            let chunk = present?;
            // ORDERING: Acquire pairs with Task publication in ensure_slot.
            if chunk.tasks[offset].load(Ordering::Acquire).is_null() {
                None
            } else {
                Some(chunk_idx * TASK_CHUNK_SIZE + offset)
            }
        })
    })
}

fn pop_free_dynamic_slot() -> Option<u32> {
    loop {
        // ORDERING: Acquire pairs with Release push below.
        let packed = FREE_SLOT_HEAD.load(Ordering::Acquire);
        let slot = packed as u32;
        if slot == NO_SLOT {
            return None;
        }
        let tag = (packed >> 32) as u32;
        // ORDERING: Acquire observes next_free stored by release_dynamic_slot.
        let next = get(slot as usize).next_free.load(Ordering::Acquire);
        let next_packed = (u64::from(tag.wrapping_add(1)) << 32) | u64::from(next);
        // ORDERING: AcqRel pops stack node while preventing ABA with incrementing tag.
        if FREE_SLOT_HEAD
            .compare_exchange_weak(packed, next_packed, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
        {
            // ORDERING: Release clears dangling link after exclusive pop.
            get(slot as usize).next_free.store(NO_SLOT, Ordering::Release);
            return Some(slot);
        }
    }
}

pub(in crate::multitask) fn release_dynamic_slot(slot: usize) {
    set_slot_active(slot, false);
    if slot < INITIAL_DYNAMIC_SLOT as usize || slot >= MAX_TASK_SLOTS {
        return;
    }
    let Some(task) = try_get(slot) else {
        return;
    };
    let slot_u32 = slot as u32;
    loop {
        // ORDERING: Acquire reads current head + ABA tag before linking next_free.
        let packed = FREE_SLOT_HEAD.load(Ordering::Acquire);
        let head = packed as u32;
        let tag = (packed >> 32) as u32;
        // ORDERING: Release publishes next_free link before exposing slot via CAS.
        task.next_free.store(head, Ordering::Release);
        let next_packed = (u64::from(tag.wrapping_add(1)) << 32) | u64::from(slot_u32);
        // ORDERING: AcqRel pushes node atomically with incremented ABA tag.
        if FREE_SLOT_HEAD
            .compare_exchange_weak(packed, next_packed, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
        {
            return;
        }
    }
}

/// Acquires a dynamic task slot (`>= MAX_TRACKED_CPUS`), reusing a reclaimed
/// slot first or allocating a fresh `Task` object from heap in normal context.
pub(in crate::multitask) fn any_dynamic_available() -> bool {
    // ORDERING: Acquire pairs with push/pop Release stores on FREE_SLOT_HEAD.
    (FREE_SLOT_HEAD.load(Ordering::Acquire) as u32) != NO_SLOT
        // ORDERING: Acquire observes monotonic NEXT_FRESH_SLOT ceiling check.
        || (NEXT_FRESH_SLOT.load(Ordering::Acquire) as usize) < MAX_TASK_SLOTS
}

pub(in crate::multitask) fn claim_dynamic_slot() -> Option<usize> {
    if let Some(slot) = pop_free_dynamic_slot() {
        let idx = slot as usize;
        set_slot_active(idx, true);
        return Some(idx);
    }
    // ORDERING: AcqRel uniquely reserves the next monotonic dynamic slot index.
    let slot = NEXT_FRESH_SLOT
        .fetch_update(Ordering::AcqRel, Ordering::Acquire, |cur| {
            usize::try_from(cur)
                .ok()
                .filter(|&n| n < MAX_TASK_SLOTS)
                .map(|_| cur + 1)
        })
        .ok()? as usize;
    ensure_slot(slot)?;
    set_slot_active(slot, true);
    Some(slot)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dynamic_slots_grow_past_old_limit_and_recycle_via_freelist() {
        assert!(bootstrap_initial_slots());
        let first = claim_dynamic_slot().expect("first dynamic slot");
        assert!(first >= MAX_TRACKED_CPUS);
        let second = claim_dynamic_slot().expect("second dynamic slot");
        assert_ne!(first, second);
        assert_eq!(get(first).slot as usize, first);
        assert_eq!(get(second).slot as usize, second);
        release_dynamic_slot(first);
        let reused = claim_dynamic_slot().expect("reused slot");
        assert_eq!(reused, first);
        release_dynamic_slot(reused);
        release_dynamic_slot(second);
    }
}
