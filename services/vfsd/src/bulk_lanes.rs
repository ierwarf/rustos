//! Parallel storaged bulk-read lanes for one large vfsd read.
//!
//! - **Owner:** vfsd owns how a big range is split across storaged calls;
//!   storaged owns caching and read-ahead of the device (`read_engine`).
//! - **Why:** one bulk read is a synchronous IPC call of just under 64 KiB, so a
//!   single caller pays the whole call and wake-up latency per chunk even when
//!   storaged answers from cache. Several lanes keep several calls in flight and
//!   overlap that latency.
//! - **Shape:** the caller publishes one job, runs a lane itself and waits for
//!   every chunk. Helper threads and the caller claim chunk indexes from one
//!   shared cursor, so adjacent chunks are read concurrently and the device sees
//!   one near-sequential stream.
//! - **Concurrency:** one job at a time (`JOB`); a second concurrent large read
//!   reads on its own thread instead of waiting. Chunks are disjoint ranges of
//!   the caller's buffer and the caller does not return before every claimed
//!   chunk completed, including after an error.
//! - **Lookahead:** a read that continues the previous one (same generation,
//!   starting at its end) leaves the helpers reading the next range of the same
//!   size into a private buffer while the caller hashes, writes or copies what it
//!   just received. The next read adopts that job, helps finish it and copies the
//!   bytes out; a different range, a prefetch error, or a write in between
//!   (`invalidate_prefetch`) discards it and reads normally, so the lookahead
//!   can only change latency, never the bytes or the error a caller sees.
//! - **Context:** lanes are worker threads, never the receive owner, and use only
//!   storaged IPC and kernel-direct futexes, so no lane can wait on syscalld.
//! - **Failure:** the first error aborts unclaimed chunks and is returned; a
//!   lane pool that cannot start is reported as unavailable and callers read
//!   sequentially.
use core::cell::UnsafeCell;
use core::mem::size_of;
use core::sync::atomic::{
    AtomicBool, AtomicI32, AtomicPtr, AtomicU32, AtomicU64, AtomicU8, AtomicUsize, Ordering,
};
extern crate alloc;

#[cfg(not(test))]
use rustos_user_abi::linux as linux_abi;
use rustos_user_abi::syscall::{
    StoragedBulkReadResponse, COMMERCIAL_MAX_STORAGED_OP_DVM_BLOCK_READ_BULK,
};

use super::block::{call_storaged_bulk, storage_request};
use vfsd::bulk_plan::{lookahead_bytes, LOOKAHEAD_ENABLED, PARALLEL_MIN_BYTES, PREFETCH_BYTES};

/// Helper threads; the caller is one more lane.
const HELPER_LANES: usize = 3;
const LANE_STACK_BYTES: usize = 128 * 1024;
const RESPONSE_WORDS: usize = size_of::<StoragedBulkReadResponse>().div_ceil(size_of::<u64>());
const HELPER_IDLE_WAIT_NS: i64 = 100_000_000;

#[repr(C, align(16))]
struct LaneStack([u8; LANE_STACK_BYTES]);

struct LaneResponse(UnsafeCell<[u64; RESPONSE_WORDS]>);

// SAFETY: each helper owns exactly one response slot, chosen once at entry from
// `NEXT_LANE`; no two threads share a slot.
unsafe impl Sync for LaneResponse {}

#[cfg(not(test))]
static mut LANE_STACKS: [LaneStack; HELPER_LANES] =
    [const { LaneStack([0; LANE_STACK_BYTES]) }; HELPER_LANES];
static LANE_RESPONSES: [LaneResponse; HELPER_LANES] =
    [const { LaneResponse(UnsafeCell::new([0; RESPONSE_WORDS])) }; HELPER_LANES];
static NEXT_LANE: AtomicUsize = AtomicUsize::new(0);

const POOL_NEW: u8 = 0;
const POOL_STARTING: u8 = 1;
const POOL_READY: u8 = 2;
const POOL_UNAVAILABLE: u8 = 3;
static POOL_STATE: AtomicU8 = AtomicU8::new(POOL_NEW);
/// Who owns the one job slot: nobody, a `read_parallel` caller, or a finished or
/// running lookahead job that the next caller may adopt.
const JOB_FREE: u8 = 0;
const JOB_CALLER: u8 = 1;
const JOB_PREFETCH: u8 = 2;
static JOB: AtomicU8 = AtomicU8::new(JOB_FREE);
/// The lookahead job's bytes are still wanted (not consumed, not invalidated).
static PREFETCH_VALID: AtomicBool = AtomicBool::new(false);
/// One leaked `PREFETCH_BYTES` buffer, allocated by the first lookahead.
static PREFETCH_BUF: AtomicPtr<u8> = AtomicPtr::new(core::ptr::null_mut());
/// Block after the last parallel read and its generation, for continuation.
static LAST_END: AtomicU64 = AtomicU64::new(u64::MAX);
static LAST_GENERATION: AtomicU64 = AtomicU64::new(0);

// The job. Plain atomics rather than a shared struct: a helper that wakes late
// may read fields of a newer job, and `CLAIM` carries the epoch that stops it
// from acting on them.
static EPOCH: AtomicU32 = AtomicU32::new(0);
/// `epoch << 32 | next chunk index`.
static CLAIM: AtomicU64 = AtomicU64::new(0);
static DONE: AtomicU32 = AtomicU32::new(0);
static ERROR: AtomicI32 = AtomicI32::new(0);
static GENERATION: AtomicU64 = AtomicU64::new(0);
static BASE_LBA: AtomicU64 = AtomicU64::new(0);
static BLOCK_SIZE: AtomicUsize = AtomicUsize::new(0);
static CHUNK_BLOCKS: AtomicU64 = AtomicU64::new(0);
static OUT_PTR: AtomicPtr<u8> = AtomicPtr::new(core::ptr::null_mut());
static OUT_LEN: AtomicUsize = AtomicUsize::new(0);

#[derive(Clone, Copy)]
struct Job {
    generation: u64,
    base_lba: u64,
    block_size: usize,
    chunk_blocks: u64,
    out_ptr: *mut u8,
    out_len: usize,
}

impl Job {
    fn load() -> Self {
        Self {
            generation: GENERATION.load(Ordering::Relaxed),
            base_lba: BASE_LBA.load(Ordering::Relaxed),
            block_size: BLOCK_SIZE.load(Ordering::Relaxed),
            chunk_blocks: CHUNK_BLOCKS.load(Ordering::Relaxed),
            out_ptr: OUT_PTR.load(Ordering::Relaxed),
            out_len: OUT_LEN.load(Ordering::Relaxed),
        }
    }

    fn chunk_bytes(&self) -> usize {
        self.chunk_blocks as usize * self.block_size
    }

    fn chunks(&self) -> usize {
        match self.chunk_bytes() {
            0 => 0,
            bytes => self.out_len.div_ceil(bytes),
        }
    }
}

/// Reads `out` (a whole number of blocks starting at `lba`) through parallel
/// lanes. `None` means lanes are unavailable right now and the caller must read
/// sequentially; the buffer is untouched in that case.
pub(super) fn read_parallel(
    generation: u64,
    block_size: usize,
    chunk_blocks: u64,
    device_blocks: u64,
    lba: u64,
    out: &mut [u8],
    caller_response: &mut [u64],
) -> Option<Result<(), i32>> {
    if block_size == 0 || chunk_blocks == 0 || out.len() < PARALLEL_MIN_BYTES {
        return None;
    }
    if !ensure_helpers() {
        return None;
    }
    let adopted =
        match JOB.compare_exchange(JOB_FREE, JOB_CALLER, Ordering::AcqRel, Ordering::Acquire) {
            Ok(_) => false,
            Err(JOB_PREFETCH) => {
                if JOB
                    .compare_exchange(
                        JOB_PREFETCH,
                        JOB_CALLER,
                        Ordering::AcqRel,
                        Ordering::Acquire,
                    )
                    .is_err()
                {
                    return None;
                }
                true
            }
            Err(_) => return None,
        };
    let continues = LAST_GENERATION.load(Ordering::Relaxed) == generation
        && LAST_END.load(Ordering::Relaxed) == lba;
    let blocks = (out.len() / block_size) as u64;

    let from_lookahead =
        adopted && take_lookahead(generation, block_size, lba, out, caller_response);
    let outcome = if from_lookahead {
        Ok(())
    } else {
        run_job(
            generation,
            block_size,
            chunk_blocks,
            lba,
            out,
            caller_response,
        )
    };

    LAST_GENERATION.store(generation, Ordering::Relaxed);
    LAST_END.store(lba.saturating_add(blocks), Ordering::Relaxed);
    let looking_ahead = LOOKAHEAD_ENABLED
        && outcome.is_ok()
        && (continues || from_lookahead)
        && start_lookahead(
            generation,
            block_size,
            chunk_blocks,
            lba + blocks,
            out.len(),
            device_blocks,
        );
    JOB.store(
        if looking_ahead {
            JOB_PREFETCH
        } else {
            JOB_FREE
        },
        Ordering::Release,
    );
    Some(outcome)
}

/// Drops any lookahead bytes: they may predate a mutation of the device.
pub(super) fn invalidate_prefetch() {
    PREFETCH_VALID.store(false, Ordering::Release);
    LAST_END.store(u64::MAX, Ordering::Relaxed);
}

/// Runs one ordinary job: publishes it, reads as a lane and waits for it.
fn run_job(
    generation: u64,
    block_size: usize,
    chunk_blocks: u64,
    lba: u64,
    out: &mut [u8],
    caller_response: &mut [u64],
) -> Result<(), i32> {
    publish_job(
        generation,
        block_size,
        chunk_blocks,
        lba,
        out.as_mut_ptr(),
        out.len(),
    );
    let job = Job::load();
    let epoch = EPOCH.load(Ordering::Relaxed);
    run_lane(epoch, job, caller_response);
    wait_done(job.chunks());
    let error = ERROR.load(Ordering::Acquire);
    if error == 0 {
        Ok(())
    } else {
        Err(error)
    }
}

/// Publishes a job to the helpers and returns without reading.
fn publish_job(
    generation: u64,
    block_size: usize,
    chunk_blocks: u64,
    lba: u64,
    out_ptr: *mut u8,
    out_len: usize,
) {
    GENERATION.store(generation, Ordering::Relaxed);
    BASE_LBA.store(lba, Ordering::Relaxed);
    BLOCK_SIZE.store(block_size, Ordering::Relaxed);
    CHUNK_BLOCKS.store(chunk_blocks, Ordering::Relaxed);
    OUT_PTR.store(out_ptr, Ordering::Relaxed);
    OUT_LEN.store(out_len, Ordering::Relaxed);
    ERROR.store(0, Ordering::Relaxed);
    DONE.store(0, Ordering::Relaxed);
    let epoch = EPOCH.load(Ordering::Relaxed).wrapping_add(1);
    // ORDERING: the Release stores publish the job fields above; a helper that
    // Acquires `EPOCH` (or `CLAIM`) observes them.
    CLAIM.store(u64::from(epoch) << 32, Ordering::Release);
    EPOCH.store(epoch, Ordering::Release);
    futex_wake(&EPOCH, HELPER_LANES);
}

fn wait_done(chunks: usize) {
    loop {
        let done = DONE.load(Ordering::Acquire);
        if done as usize >= chunks {
            return;
        }
        futex_wait(&DONE, done, 1_000_000);
    }
}

/// Finishes the adopted lookahead job and, when it is exactly the requested
/// range and succeeded, copies its bytes into `out`. The job is always fully
/// finished on return, so the slot can be reused either way.
fn take_lookahead(
    generation: u64,
    block_size: usize,
    lba: u64,
    out: &mut [u8],
    caller_response: &mut [u64],
) -> bool {
    let job = Job::load();
    run_lane(EPOCH.load(Ordering::Relaxed), job, caller_response);
    wait_done(job.chunks());
    let wanted = PREFETCH_VALID.swap(false, Ordering::AcqRel);
    if !wanted
        || ERROR.load(Ordering::Acquire) != 0
        || job.generation != generation
        || job.block_size != block_size
        || job.base_lba != lba
        || job.out_len < out.len()
        || job.out_ptr.is_null()
    {
        return false;
    }
    // SAFETY: `out_ptr` is the leaked `PREFETCH_BUF` allocation of
    // `PREFETCH_BYTES` bytes, `out_len <= PREFETCH_BYTES`, every chunk
    // completed (`wait_done`), and the job slot is ours, so nothing writes it.
    let bytes = unsafe { core::slice::from_raw_parts(job.out_ptr, out.len()) };
    out.copy_from_slice(bytes);
    true
}

/// Starts the helpers reading the range after a read into the private buffer.
fn start_lookahead(
    generation: u64,
    block_size: usize,
    chunk_blocks: u64,
    lba: u64,
    len: usize,
    device_blocks: u64,
) -> bool {
    let Some(bytes) = lookahead_bytes(lba, len, block_size, device_blocks) else {
        return false;
    };
    let mut buf = PREFETCH_BUF.load(Ordering::Acquire);
    if buf.is_null() {
        let mut allocation = alloc::vec::Vec::<u8>::new();
        if allocation.try_reserve_exact(PREFETCH_BYTES).is_err() {
            return false;
        }
        allocation.resize(PREFETCH_BYTES, 0);
        buf = allocation.leak().as_mut_ptr();
        PREFETCH_BUF.store(buf, Ordering::Release);
    }
    PREFETCH_VALID.store(true, Ordering::Release);
    publish_job(generation, block_size, chunk_blocks, lba, buf, bytes);
    true
}

/// Claims and reads chunks of epoch `epoch` until none are left.
fn run_lane(epoch: u32, job: Job, response: &mut [u64]) {
    let chunks = job.chunks();
    loop {
        let claim = CLAIM.load(Ordering::Acquire);
        if (claim >> 32) as u32 != epoch {
            return;
        }
        let index = (claim & 0xffff_ffff) as usize;
        if index >= chunks {
            return;
        }
        if CLAIM
            .compare_exchange(claim, claim + 1, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            continue;
        }
        if ERROR.load(Ordering::Relaxed) == 0 {
            if let Err(errno) = read_chunk(&job, index, response) {
                let _ = ERROR.compare_exchange(0, errno, Ordering::AcqRel, Ordering::Relaxed);
            }
        }
        // ORDERING: AcqRel publishes this chunk's bytes to the caller's Acquire
        // of `DONE`.
        let done = DONE.fetch_add(1, Ordering::AcqRel) + 1;
        if done as usize == chunks {
            futex_wake(&DONE, 1);
        }
    }
}

fn read_chunk(job: &Job, index: usize, response: &mut [u64]) -> Result<(), i32> {
    let offset = index * job.chunk_bytes();
    let len = job.chunk_bytes().min(job.out_len - offset);
    let mut request = storage_request(COMMERCIAL_MAX_STORAGED_OP_DVM_BLOCK_READ_BULK);
    request.arg0 = job.generation;
    request.arg1 = job.base_lba + index as u64 * job.chunk_blocks;
    request.arg2 = (len / job.block_size) as u64;
    // SAFETY: the chunk is a range of the caller's buffer, which stays borrowed
    // by `read_parallel` until `DONE` reaches the chunk count, and chunk indexes
    // are claimed exactly once so no two lanes hold the same bytes.
    let chunk = unsafe { core::slice::from_raw_parts_mut(job.out_ptr.add(offset), len) };
    call_storaged_bulk(&request, chunk, response)
}

fn ensure_helpers() -> bool {
    match POOL_STATE.load(Ordering::Acquire) {
        POOL_READY => return true,
        POOL_NEW => {}
        _ => return false,
    }
    if POOL_STATE
        .compare_exchange(POOL_NEW, POOL_STARTING, Ordering::AcqRel, Ordering::Acquire)
        .is_err()
    {
        return false;
    }
    let mut started = 0;
    for index in 0..HELPER_LANES {
        if spawn_helper(index).is_err() {
            break;
        }
        started += 1;
    }
    // A partial pool is still correct (the caller is a lane too); no helper at
    // all means parallelism is simply unavailable.
    POOL_STATE.store(
        if started == 0 {
            POOL_UNAVAILABLE
        } else {
            POOL_READY
        },
        Ordering::Release,
    );
    started != 0
}

#[cfg(not(test))]
fn spawn_helper(index: usize) -> Result<(), i64> {
    // SAFETY: computes one static stack's one-past-the-end address without
    // dereferencing it; each index is spawned at most once (`POOL_STARTING`).
    let stack_top = unsafe {
        core::ptr::addr_of_mut!(LANE_STACKS[index])
            .cast::<u8>()
            .add(LANE_STACK_BYTES) as u64
    };
    super::snapshot_worker::spawn_thread_on_stack(stack_top, lane_entry)
}

#[cfg(test)]
fn spawn_helper(_index: usize) -> Result<(), i64> {
    Err(-38)
}

#[cfg(not(test))]
extern "C" fn lane_entry() -> ! {
    let lane = NEXT_LANE.fetch_add(1, Ordering::AcqRel) % HELPER_LANES;
    if super::SNAPSHOT_WORKER_DEMOTED.load(Ordering::Acquire) {
        super::snapshot_worker::demote_current_thread_or_exit("bulk-lane");
    }
    // SAFETY: `lane` is unique to this helper (see `LaneResponse`).
    let response = unsafe { &mut *LANE_RESPONSES[lane].0.get() };
    let mut seen = EPOCH.load(Ordering::Acquire);
    loop {
        let epoch = EPOCH.load(Ordering::Acquire);
        if epoch == seen {
            futex_wait(&EPOCH, epoch, HELPER_IDLE_WAIT_NS);
            continue;
        }
        seen = epoch;
        run_lane(epoch, Job::load(), response);
    }
}

#[cfg(not(test))]
fn futex_wake(word: &AtomicU32, waiters: usize) {
    // SAFETY: the aligned atomic is static process memory. The wake is only a
    // hint: the timed waits re-check the word, so a refused wake costs latency,
    // never progress.
    let _ = unsafe {
        rustos_svc_runtime::syscall::syscall6(
            linux_abi::SYS_FUTEX,
            word.as_ptr() as u64,
            (linux_abi::FUTEX_WAKE | linux_abi::FUTEX_PRIVATE_FLAG) as u64,
            waiters as u64,
            0,
            0,
            0,
        )
    };
}

#[cfg(not(test))]
fn futex_wait(word: &AtomicU32, expected: u32, timeout_ns: i64) {
    let timeout = linux_abi::LinuxTimespec {
        tv_sec: 0,
        tv_nsec: timeout_ns,
    };
    // SAFETY: the static word and the initialized timeout outlive the
    // synchronous call; WAIT compares and registers atomically with WAKE.
    let status = unsafe {
        rustos_svc_runtime::syscall::syscall6(
            linux_abi::SYS_FUTEX,
            word.as_ptr() as u64,
            (linux_abi::FUTEX_WAIT | linux_abi::FUTEX_PRIVATE_FLAG) as u64,
            u64::from(expected),
            (&timeout as *const linux_abi::LinuxTimespec) as u64,
            0,
            0,
        )
    };
    // Wake, stale value, interruption and timeout all re-check the caller's
    // condition; a refused wait must not become a busy loop.
    if !matches!(status, 0 | -11 | -4 | -110) {
        rustos_svc_runtime::syscall::sleep_millis(1);
    }
}

#[cfg(test)]
fn futex_wake(_word: &AtomicU32, _waiters: usize) {}

#[cfg(test)]
fn futex_wait(_word: &AtomicU32, _expected: u32, _timeout_ns: i64) {
    core::hint::spin_loop();
}
