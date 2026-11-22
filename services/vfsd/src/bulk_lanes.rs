//! Parallel storaged bulk-read lanes and the snapshot pipeline's stage threads.
//!
//! - **Owner:** vfsd owns how a big range is split across storaged calls;
//!   storaged owns caching and read-ahead of the device (`read_engine`).
//! - **Why:** one bulk read is a synchronous IPC call of just under 64 KiB, so a
//!   single caller pays the whole call and wake-up latency per chunk even when
//!   storaged answers from cache. Several lanes keep several calls in flight and
//!   overlap that latency.
//! - **Shape:** a caller builds one [`LaneJob`] on its own stack (a
//!   `ChunkCursor` plus the geometry of its buffer), publishes it in a free
//!   [`JobSlot`], runs a lane itself and waits until every chunk completed.
//!   Helper threads scan the slots, pin a published job and claim chunk indexes
//!   from its cursor, so adjacent chunks are read concurrently and the device
//!   sees one near-sequential stream. Chunks are disjoint ranges of the
//!   caller's buffer.
//! - **Concurrency:** `JOB_SLOTS` jobs run at once and the helpers serve
//!   whichever has unclaimed chunks. Only when every slot is taken does a read
//!   fall back to the caller's own thread (it never waits for a slot).
//! - **Lifetime:** the job lives in the caller's frame. A slot's `state` word
//!   holds a `PUBLISHED` bit and a pin count; a helper pins with one CAS that
//!   fails once the bit is gone, and the caller unpublishes then waits for the
//!   pin count to reach zero before returning, so no helper can touch a job
//!   after its frame is gone. The caller also never returns before every
//!   claimed chunk completed, including after an error.
//! - **Read-ahead:** none lives here. A sequential stream is overlapped by the
//!   snapshot pipeline (`vfsd::snapshot_pipeline`), which keeps whole windows
//!   in flight in its own bounded ring and reads each directly into its slot, so
//!   there is no private lookahead buffer, no extra copy, and no cached bytes
//!   that a volume write could make stale (every window is read under the
//!   source's own revalidation, and a write after the read invalidates the
//!   read map that the snapshot commit re-checks).
//! - **Stage threads:** the pipeline's writer and hasher stages run on transient
//!   threads (`StageThreads`) cloned onto a free static stack for one stage and
//!   gone when it ends, so an idle vfsd holds no stage thread (a service thread
//!   is a slot of the machine-wide task table that applications are admitted
//!   against).
//! - **Context:** lanes and stages are worker threads, never the receive owner.
//!   Lanes use only storaged IPC and kernel-direct futexes, so no lane can wait
//!   on syscalld.
//! - **Failure:** the first chunk error aborts unclaimed chunks and is
//!   returned; a lane pool that cannot start is reported as unavailable and
//!   callers read sequentially (stages then run on the snapshot thread).
use core::cell::UnsafeCell;
use core::mem::size_of;
use core::ops::Deref;
use core::sync::atomic::{AtomicBool, AtomicPtr, AtomicU32, AtomicU8, AtomicUsize, Ordering};

use rustos_user_abi::linux as linux_abi;
use rustos_user_abi::syscall::{
    StoragedBulkReadResponse, COMMERCIAL_MAX_STORAGED_OP_DVM_BLOCK_READ_BULK,
};
use vfsd::bulk_plan::{chunk_bytes, chunk_count, chunk_span, is_parallel_read, ChunkCursor};
use vfsd::snapshot_pipeline::{StageHost, StageToken};

use super::block::{call_storaged_bulk, storage_request};

/// Helper threads; the caller is one more lane.
const HELPER_LANES: usize = 3;
/// Reads that can be in flight at once before one falls back to its own thread.
const JOB_SLOTS: usize = 4;
const THREAD_STACK_BYTES: usize = 128 * 1024;
const RESPONSE_WORDS: usize = size_of::<StoragedBulkReadResponse>().div_ceil(size_of::<u64>());
const HELPER_IDLE_WAIT_NS: i64 = 100_000_000;
/// Backstop for every park: a lost wake costs this much latency, never progress.
const PARK_NS: i64 = 1_000_000;

/// Pipeline stage stacks: two stages per snapshot, two snapshots at a time.
const STAGE_THREADS: usize = 4;
/// Highest waiter count a wake needs to reach (caller plus both stages).
const MAX_WAITERS: usize = 4;

#[repr(C, align(16))]
struct ThreadStack([u8; THREAD_STACK_BYTES]);

struct LaneResponse(UnsafeCell<[u64; RESPONSE_WORDS]>);

// SAFETY: each helper owns exactly one response slot, chosen once at entry from
// `NEXT_LANE`; no two threads share a slot.
unsafe impl Sync for LaneResponse {}

static mut LANE_STACKS: [ThreadStack; HELPER_LANES] =
    [const { ThreadStack([0; THREAD_STACK_BYTES]) }; HELPER_LANES];
static LANE_RESPONSES: [LaneResponse; HELPER_LANES] =
    [const { LaneResponse(UnsafeCell::new([0; RESPONSE_WORDS])) }; HELPER_LANES];
static NEXT_LANE: AtomicUsize = AtomicUsize::new(0);
static mut STAGE_STACKS: [ThreadStack; STAGE_THREADS] =
    [const { ThreadStack([0; THREAD_STACK_BYTES]) }; STAGE_THREADS];

/// A lazily started, never-stopped set of static-stack threads.
///
/// The threads are process-lifetime by design: their stacks are static memory,
/// they park on a futex when idle, and no job or buffer is ever owned by them
/// beyond one claimed task or chunk. There is therefore nothing to tear down.
struct ThreadPool {
    state: AtomicU8,
    started: AtomicUsize,
}

impl ThreadPool {
    const NEW: u8 = 0;
    const STARTING: u8 = 1;
    const READY: u8 = 2;

    const fn new() -> Self {
        Self {
            state: AtomicU8::new(Self::NEW),
            started: AtomicUsize::new(0),
        }
    }

    /// Starts up to `wanted` threads once and returns how many are running;
    /// 0 means the pool is unavailable. A partial pool is still correct.
    fn ensure(&self, wanted: usize, spawn: impl Fn(usize) -> Result<(), i64>) -> usize {
        loop {
            match self.state.load(Ordering::Acquire) {
                Self::READY => return self.started.load(Ordering::Acquire),
                Self::NEW => {
                    if self
                        .state
                        .compare_exchange(
                            Self::NEW,
                            Self::STARTING,
                            Ordering::AcqRel,
                            Ordering::Acquire,
                        )
                        .is_err()
                    {
                        continue;
                    }
                    let started = (0..wanted).take_while(|index| spawn(*index).is_ok()).count();
                    self.started.store(started, Ordering::Release);
                    // A pool that started nothing stays "ready" with zero
                    // threads: it is not retried on every read.
                    self.state.store(Self::READY, Ordering::Release);
                    return started;
                }
                // Another thread is spawning; the wait is bounded by a few
                // clone calls.
                _ => rustos_svc_runtime::syscall::sleep_millis(1),
            }
        }
    }
}

static LANE_POOL: ThreadPool = ThreadPool::new();

/// One read: the caller's buffer split into storaged-call-sized chunks.
struct LaneJob {
    cursor: ChunkCursor,
    generation: u64,
    base_lba: u64,
    block_size: usize,
    chunk_blocks: u64,
    chunk_bytes: usize,
    out_ptr: *mut u8,
    out_len: usize,
}

/// Rendezvous between one caller and the helpers for one [`LaneJob`].
struct JobSlot {
    /// The slot is reserved by a caller (published or not).
    owned: AtomicBool,
    /// `PUBLISHED` bit plus the number of pinned helpers; also the futex word a
    /// retiring caller waits on.
    state: AtomicU32,
    job: AtomicPtr<LaneJob>,
}

const PUBLISHED: u32 = 1 << 31;

/// A helper's pin on a published job; the caller cannot retire while it lives.
struct Pinned<'a> {
    slot: &'a JobSlot,
    job: &'a LaneJob,
}

impl Deref for Pinned<'_> {
    type Target = LaneJob;
    fn deref(&self) -> &LaneJob {
        self.job
    }
}

impl Drop for Pinned<'_> {
    fn drop(&mut self) {
        // ORDERING: Release orders every access to the job before the caller's
        // Acquire observation of a zero pin count.
        let previous = self.slot.state.fetch_sub(1, Ordering::AcqRel);
        if previous & !PUBLISHED == 1 {
            futex_wake(&self.slot.state, 1);
        }
    }
}

impl JobSlot {
    const fn new() -> Self {
        Self {
            owned: AtomicBool::new(false),
            state: AtomicU32::new(0),
            job: AtomicPtr::new(core::ptr::null_mut()),
        }
    }

    fn reserve(&self) -> bool {
        self.owned
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
    }

    /// Makes `job` visible to helpers. The caller must `retire` before `job`
    /// goes out of scope.
    fn publish(&self, job: &LaneJob) {
        self.job
            .store(core::ptr::from_ref(job).cast_mut(), Ordering::Relaxed);
        // ORDERING: Release publishes the job pointer and its fields to a
        // helper whose pinning CAS Acquires this state.
        self.state.fetch_or(PUBLISHED, Ordering::Release);
    }

    fn pin(&self) -> Option<Pinned<'_>> {
        loop {
            let state = self.state.load(Ordering::Acquire);
            if state & PUBLISHED == 0 {
                return None;
            }
            if self
                .state
                .compare_exchange(state, state + 1, Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
            {
                // SAFETY: the pin was taken while PUBLISHED was set, and the
                // owner keeps the job alive until the pin count is zero.
                let job = unsafe { &*self.job.load(Ordering::Acquire) };
                return Some(Pinned { slot: self, job });
            }
        }
    }

    /// Unpublishes, waits for every pinned helper to let go, and frees the slot.
    fn retire(&self) {
        self.state.fetch_and(!PUBLISHED, Ordering::AcqRel);
        loop {
            let state = self.state.load(Ordering::Acquire);
            if state == 0 {
                break;
            }
            futex_wait(&self.state, state, PARK_NS);
        }
        self.owned.store(false, Ordering::Release);
    }
}

static JOB_SLOTS_TABLE: [JobSlot; JOB_SLOTS] = [const { JobSlot::new() }; JOB_SLOTS];
/// Bumped on every publish; idle helpers park on it.
static WORK: AtomicU32 = AtomicU32::new(0);

/// Reads `out` (a whole number of blocks starting at `lba`) through parallel
/// lanes. `None` means lanes are unavailable right now and the caller must read
/// sequentially; the buffer is untouched in that case.
pub(super) fn read_parallel(
    generation: u64,
    block_size: usize,
    chunk_blocks: u64,
    lba: u64,
    out: &mut [u8],
    caller_response: &mut [u64],
) -> Option<Result<(), i32>> {
    if !is_parallel_read(block_size, chunk_blocks, out.len()) {
        return None;
    }
    let chunk_bytes = chunk_bytes(block_size, chunk_blocks)?;
    let cursor = ChunkCursor::new(chunk_count(out.len(), chunk_bytes))?;
    if !ensure_helpers() {
        return None;
    }
    let slot = JOB_SLOTS_TABLE.iter().find(|slot| slot.reserve())?;
    let job = LaneJob {
        cursor,
        generation,
        base_lba: lba,
        block_size,
        chunk_blocks,
        chunk_bytes,
        out_ptr: out.as_mut_ptr(),
        out_len: out.len(),
    };
    slot.publish(&job);
    WORK.fetch_add(1, Ordering::Release);
    futex_wake(&WORK, HELPER_LANES);
    run_lane(&job, caller_response);
    wait_finished(&job);
    slot.retire();
    let error = job.cursor.error();
    Some(if error == 0 { Ok(()) } else { Err(error) })
}

fn wait_finished(job: &LaneJob) {
    loop {
        if job.cursor.finished() {
            return;
        }
        let done = job.cursor.done_word().load(Ordering::Acquire);
        futex_wait(job.cursor.done_word(), done, PARK_NS);
    }
}

/// Claims and reads chunks of `job` until none are left.
fn run_lane(job: &LaneJob, response: &mut [u64]) {
    while let Some(index) = job.cursor.claim() {
        let result = if job.cursor.error() == 0 {
            read_chunk(job, index, response)
        } else {
            // A claimed chunk is always completed, but not read after an error.
            Ok(())
        };
        if job.cursor.complete(result) {
            futex_wake(job.cursor.done_word(), 1);
        }
    }
}

fn read_chunk(job: &LaneJob, index: usize, response: &mut [u64]) -> Result<(), i32> {
    let (offset, len) = chunk_span(index, job.chunk_bytes, job.out_len);
    let mut request = storage_request(COMMERCIAL_MAX_STORAGED_OP_DVM_BLOCK_READ_BULK);
    request.arg0 = job.generation;
    request.arg1 = job.base_lba + index as u64 * job.chunk_blocks;
    request.arg2 = (len / job.block_size) as u64;
    // SAFETY: the chunk is a range of the caller's buffer, which stays borrowed
    // by `read_parallel` until every chunk completed and the slot is retired,
    // and chunk indexes are claimed exactly once so no two lanes hold the same
    // bytes.
    let chunk = unsafe { core::slice::from_raw_parts_mut(job.out_ptr.add(offset), len) };
    call_storaged_bulk(&request, chunk, response)
}

fn ensure_helpers() -> bool {
    LANE_POOL.ensure(HELPER_LANES, |index| {
        // SAFETY: computes one static stack's one-past-the-end address without
        // dereferencing it; `ThreadPool::ensure` spawns each index once.
        let stack_top = unsafe {
            core::ptr::addr_of_mut!(LANE_STACKS[index])
                .cast::<u8>()
                .add(THREAD_STACK_BYTES) as u64
        };
        super::snapshot_worker::spawn_thread_on_stack(stack_top, lane_entry)
    }) != 0
}

extern "C" fn lane_entry() -> ! {
    let lane = NEXT_LANE.fetch_add(1, Ordering::AcqRel) % HELPER_LANES;
    // SAFETY: `lane` is unique to this helper (see `LaneResponse`).
    let response = unsafe { &mut *LANE_RESPONSES[lane].0.get() };
    let mut demoted = false;
    loop {
        if !demoted && super::SNAPSHOT_WORKER_DEMOTED.load(Ordering::Acquire) {
            super::snapshot_worker::demote_current_thread_or_exit("bulk-lane");
            demoted = true;
        }
        let seen = WORK.load(Ordering::Acquire);
        let mut worked = false;
        for slot in &JOB_SLOTS_TABLE {
            if let Some(job) = slot.pin() {
                if job.cursor.has_unclaimed() {
                    run_lane(&job, response);
                    worked = true;
                }
            }
        }
        if !worked {
            // A publish after the scan bumps `WORK`, so the wait returns at
            // once instead of sleeping through it.
            futex_wait(&WORK, seen, HELPER_IDLE_WAIT_NS);
        }
    }
}

// ---------------------------------------------------------------------------
// Pipeline stage threads
// ---------------------------------------------------------------------------

const STAGE_IDLE: u32 = 0;
const STAGE_READY: u32 = 1;
const STAGE_RUNNING: u32 = 2;
const STAGE_DONE: u32 = 3;

#[derive(Clone, Copy)]
struct TaskPtr(*const (dyn Fn() + Sync));

/// One started stage task: `IDLE -> READY -> RUNNING -> DONE -> IDLE`. The slot
/// index is also the index of the static stack the stage thread runs on.
struct StageSlot {
    state: AtomicU32,
    task: UnsafeCell<Option<TaskPtr>>,
}

// SAFETY: `task` is written only by the starter while it owns the stack (the
// stack FREE -> BUSY CAS) and the slot is IDLE, and read only by the one
// thread running on that stack; the Release store of READY and the Acquire CAS
// to RUNNING order the write before the read, and `join` clears it after DONE.
unsafe impl Sync for StageSlot {}

static STAGE_SLOTS: [StageSlot; STAGE_THREADS] = [const {
    StageSlot {
        state: AtomicU32::new(STAGE_IDLE),
        task: UnsafeCell::new(None),
    }
}; STAGE_THREADS];

const STACK_FREE: u32 = 0;
const STACK_BUSY: u32 = 1;
/// A stage stack is BUSY from the starter's claim until the stage thread's last
/// instruction before `exit` (see [`exit_releasing_stack`]); only then may
/// another stage thread run on it.
static STAGE_STACK_STATE: [AtomicU32; STAGE_THREADS] =
    [const { AtomicU32::new(STACK_FREE) }; STAGE_THREADS];

/// The snapshot pipeline's helper threads.
///
/// A stage thread exists only while its stage runs: `start` claims a free
/// static stack and clones a thread onto it, the thread runs the stage once and
/// exits. A service thread is a slot of the machine-wide task table, and the
/// application launch policy reserves what is left, so threads that are only
/// needed for the ~0.5 s of one snapshot must not stay resident.
pub(super) struct StageThreads;

// SAFETY: `start` stores the task pointer in a slot that exactly one stage
// thread runs once; `join` returns only after that run reached DONE and the
// thread no longer touches the task, and the pipeline joins before the borrowed
// task is dropped (the `StageHost` contract).
unsafe impl StageHost for StageThreads {
    unsafe fn start(&self, task: &(dyn Fn() + Sync)) -> Option<StageToken> {
        let index = STAGE_STACK_STATE.iter().position(|state| {
            state
                .compare_exchange(STACK_FREE, STACK_BUSY, Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
        })?;
        // SAFETY: the stack claim above makes this thread the slot's only
        // writer; the slot is IDLE because `join` resets it before the thread
        // releases the stack.
        unsafe {
            *STAGE_SLOTS[index].task.get() = Some(TaskPtr(core::mem::transmute::<
                &(dyn Fn() + Sync),
                *const (dyn Fn() + Sync + 'static),
            >(task)));
        }
        STAGE_SLOTS[index].state.store(STAGE_READY, Ordering::Release);
        // SAFETY: one static stack's one-past-the-end address, claimed above.
        let stack_top = unsafe {
            core::ptr::addr_of_mut!(STAGE_STACKS[index])
                .cast::<u8>()
                .add(THREAD_STACK_BYTES) as u64
        };
        if super::snapshot_worker::spawn_thread_on_stack(stack_top, stage_entry).is_err() {
            // SAFETY: no thread was started, so the slot is still ours alone.
            unsafe { *STAGE_SLOTS[index].task.get() = None };
            STAGE_SLOTS[index].state.store(STAGE_IDLE, Ordering::Release);
            STAGE_STACK_STATE[index].store(STACK_FREE, Ordering::Release);
            return None;
        }
        Some(StageToken(index))
    }

    fn join(&self, token: StageToken) {
        let slot = &STAGE_SLOTS[token.0];
        loop {
            let state = slot.state.load(Ordering::Acquire);
            if state == STAGE_DONE {
                break;
            }
            futex_wait(&slot.state, state, PARK_NS);
        }
        // SAFETY: DONE means the stage thread is finished with the slot. It
        // still owns the stack until it releases it itself, so the next
        // `start` on this index cannot begin before the thread is gone.
        unsafe { *slot.task.get() = None };
        slot.state.store(STAGE_IDLE, Ordering::Release);
    }

    fn wait(&self, word: &AtomicU32, expected: u32) {
        futex_wait(word, expected, PARK_NS);
    }

    fn wake(&self, word: &AtomicU32) {
        futex_wake(word, MAX_WAITERS);
    }

    fn now_ns(&self) -> u64 {
        rustos_svc_runtime::syscall::monotonic_nanos()
    }
}

/// Index of the stage stack the calling thread runs on.
fn current_stage_index() -> usize {
    let sp: usize;
    // SAFETY: reads the stack pointer only.
    unsafe { core::arch::asm!("mov {}, rsp", out(reg) sp, options(nomem, nostack, preserves_flags)) };
    let base = core::ptr::addr_of!(STAGE_STACKS) as usize;
    // `sp` lies in `(base + index * SIZE, base + (index + 1) * SIZE]`.
    (sp - base - 1) / THREAD_STACK_BYTES
}

/// Marks the stack free and ends the calling thread without touching the stack
/// in between: one store, then the exit syscall, in a single asm block with no
/// push or call, so a thread cloned onto the same stack right after the store
/// can never be corrupted by this one.
#[cfg(not(test))]
fn exit_releasing_stack(state: &AtomicU32) -> ! {
    // SAFETY: `state` is a static; the store has release semantics on x86 and
    // the syscall terminates only this thread and returns no value.
    unsafe {
        core::arch::asm!(
            "mov dword ptr [{state}], {free}",
            "syscall",
            state = in(reg) state.as_ptr(),
            free = const STACK_FREE,
            in("rax") linux_abi::SYS_EXIT,
            in("rdi") 0_u64,
            options(noreturn),
        );
    }
}

#[cfg(test)]
fn exit_releasing_stack(_state: &AtomicU32) -> ! {
    unreachable!("stage threads never run in host tests")
}

extern "C" fn stage_entry() -> ! {
    let index = current_stage_index();
    let slot = &STAGE_SLOTS[index];
    // The starter published READY before the clone, so this cannot fail.
    if slot
        .state
        .compare_exchange(STAGE_READY, STAGE_RUNNING, Ordering::AcqRel, Ordering::Acquire)
        .is_ok()
    {
        if super::SNAPSHOT_WORKER_DEMOTED.load(Ordering::Acquire) {
            super::snapshot_worker::demote_current_thread_or_exit("snapshot-stage");
        }
        // SAFETY: the READY -> RUNNING CAS made this thread the slot's sole
        // runner, and Acquire orders the starter's task store before it.
        let task = unsafe { (*slot.task.get()).expect("READY slot has a task") };
        // SAFETY: valid until `join` observes DONE (the `StageHost` contract).
        unsafe { (*task.0)() };
        // ORDERING: Release publishes every effect of the task to `join`.
        slot.state.store(STAGE_DONE, Ordering::Release);
        futex_wake(&slot.state, 1);
    }
    exit_releasing_stack(&STAGE_STACK_STATE[index])
}

const FUTEX_WAIT_PRIVATE: u64 = 128;

/// Sleeps on `word` while it still equals `expected`, for at most `timeout_ns`
/// (relative). Spurious wakes, `EAGAIN` and timeouts all return; every caller
/// re-checks its condition, and the timeout bounds a lost wake to latency.
fn futex_wait(word: &AtomicU32, expected: u32, timeout_ns: i64) {
    let timeout = [timeout_ns / 1_000_000_000, timeout_ns % 1_000_000_000];
    // SAFETY: `word` is a live aligned `u32`, `timeout` is a live
    // `{tv_sec, tv_nsec}` pair for the duration of the call, and FUTEX_WAIT only
    // reads both.
    unsafe {
        rustos_svc_runtime::syscall::syscall4(
            rustos_svc_runtime::syscall::SYS_FUTEX,
            word.as_ptr() as u64,
            FUTEX_WAIT_PRIVATE,
            u64::from(expected),
            timeout.as_ptr() as u64,
        );
    }
}

/// Wakes up to `count` waiters of `word`.
fn futex_wake(word: &AtomicU32, count: usize) {
    // SAFETY: `word` is a live aligned `u32`; FUTEX_WAKE never writes it.
    unsafe {
        rustos_svc_runtime::syscall::futex_wake_private(word.as_ptr(), count as u32);
    }
}
