//! Cached idle-retiring bulk-read workers for `VFS_IPC_OP_BULK_READ`.
//!
//! - **Owner:** `vfsd` owns the worker slots, static stacks, and private memfd
//!   creation/sealing; the caller (`mm_broker_ops`) receives ownership of the
//!   sealed memfd through handle-transfer reply.
//! - **Boundary:** Requests are validated and planned on the receive owner
//!   before dispatch; workers never touch `VfsState` descriptor tables and
//!   never call `syscalld`.
//! - **Lifecycle:** At most `BULK_READ_WORKERS` (2) workers are cloned lazily
//!   onto static stacks. After completing a job and dropping all heap state, a
//!   worker parks in `STATE_IDLE` on a private futex for up to
//!   `WORKER_IDLE_RETIRE_NS` (2 s) so bursty library/file mappings reuse warm
//!   threads with zero `SYS_CLONE` churn. When idle past the grace window, the
//!   worker claims `STATE_IDLE -> STATE_EXITING` and atomically releases its
//!   stack slot to `STATE_EMPTY` before `SYS_EXIT`, keeping zero resident
//!   threads at rest (preserving application launch task-slot headroom).
//! - **Concurrency:** At most `BULK_READ_WORKERS` (2) workers run at once.
//!   Mapped DVM/FAT reads release `VFS_STORAGE` before every device read so
//!   `open`/`stat` never wait behind multi-megabyte transfers.
//! - **Failure:** Slot saturation or `clone` failure returns `EAGAIN` on the
//!   receive thread so callers fall back to `PREAD64`. Mid-transfer remounts,
//!   OOM, or I/O errors close the private memfd and reply with the exact errno.
//! - **Forbidden:** No permanent resident threads at idle, no `syscalld` IPC,
//!   no holding `VFS_STORAGE` across device I/O, and no stack use after
//!   releasing the stack slot.
//! - **Evidence:** `vfsd` unit tests for slot state-machine transitions and
//!   request bounds/proxy admission, plus `mm_broker_ops` bulk-read integration.

use alloc::boxed::Box;
use alloc::ffi::CString;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::cell::UnsafeCell;
use core::mem::MaybeUninit;
use core::sync::atomic::{AtomicU32, Ordering};

use rustos_svc_runtime::ipc;
use rustos_user_abi::linux as linux_abi;
use rustos_user_abi::syscall::{
    IpcReplyWithHandlesArgs, VfsBulkReadResponse, SYS_RUSTOS_IPC_REPLY_WITH_HANDLES,
    VFS_IPC_ABI_VERSION, VFS_IPC_OP_BULK_READ,
};
use storage_core::file_io::{FileReadMap, RangeError, ReadAt};

use super::{
    close_fd, early_system, lock_vfs_storage, map_range_error, snapshot_cache, EAGAIN, EINVAL, EIO,
    ENOMEM, EOVERFLOW,
};

/// Maximum concurrent bulk-read workers (zero resident once idle past grace).
const BULK_READ_WORKERS: usize = 2;
const BULK_READ_STACK_BYTES: usize = 128 * 1024;
/// Transfer window for detached DVM/FAT and early-system reads (512 KiB so
/// `BootBlockDevice` fans out across parallel `bulk_lanes`).
const BULK_READ_WINDOW_BYTES: usize = 512 * 1024;
/// Bounded per-lock chunk for sources that briefly acquire `VFS_STORAGE`.
const BULK_READ_LOCKED_CHUNK_BYTES: usize = 128 * 1024;
/// How long an idle worker stays parked for warm reuse before exiting cleanly.
#[cfg(not(test))]
const WORKER_IDLE_RETIRE_NS: u64 = 2_000_000_000;
/// Bounded futex wait slice so the idle timer stays responsive.
#[cfg(not(test))]
const IDLE_FUTEX_STEP_NS: u64 = 250_000_000;

const STATE_EMPTY: u32 = 0;
const STATE_STARTING: u32 = 1;
const STATE_READY: u32 = 2;
const STATE_RUNNING: u32 = 3;
const STATE_IDLE: u32 = 4;
const STATE_WRITING: u32 = 5;
const STATE_EXITING: u32 = 6;

/// Lock-free ownership gate for one bulk-read worker slot and its static stack.
struct BulkReadSlotState(AtomicU32);

impl BulkReadSlotState {
    const fn new() -> Self {
        Self(AtomicU32::new(STATE_EMPTY))
    }

    fn try_claim_idle(&self) -> bool {
        // ORDERING: AcqRel acquires the worker's completed job drop before the
        // producer writes the next job into the slot.
        self.0
            .compare_exchange(
                STATE_IDLE,
                STATE_WRITING,
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .is_ok()
    }

    fn publish_ready(&self) {
        // ORDERING: Release publishes the initialized job to the parked worker.
        self.0.store(STATE_READY, Ordering::Release);
    }

    fn try_claim_empty(&self) -> bool {
        // ORDERING: AcqRel pairs with `exit_releasing_stack`'s store of
        // `STATE_EMPTY` so the stack is completely unused before cloning.
        self.0
            .compare_exchange(
                STATE_EMPTY,
                STATE_STARTING,
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .is_ok()
    }

    fn abort_starting(&self) {
        // ORDERING: Release returns the unused stack slot after clone failure.
        self.0.store(STATE_EMPTY, Ordering::Release);
    }

    fn mark_running(&self) {
        // ORDERING: Release marks the job claimed and running.
        self.0.store(STATE_RUNNING, Ordering::Release);
    }

    fn park_idle(&self) {
        // ORDERING: Release publishes completion/drop of the prior job before
        // allowing a producer to claim `STATE_IDLE -> STATE_WRITING`.
        self.0.store(STATE_IDLE, Ordering::Release);
    }

    fn load(&self) -> u32 {
        self.0.load(Ordering::Acquire)
    }

    fn try_begin_exit(&self) -> bool {
        // ORDERING: AcqRel arbitrates exclusively against `try_claim_idle`.
        self.0
            .compare_exchange(
                STATE_IDLE,
                STATE_EXITING,
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .is_ok()
    }

    #[cfg(not(test))]
    fn as_atomic(&self) -> &AtomicU32 {
        &self.0
    }
}

#[repr(C, align(16))]
struct BulkReadStack([u8; BULK_READ_STACK_BYTES]);

struct BulkReadJobSlot(UnsafeCell<MaybeUninit<BulkReadJob>>);

// SAFETY: `BULK_READ_SLOT_STATE[i]` is the sole ownership gate for slot `i`.
// The receive owner writes `BULK_READ_JOBS[i]` only while holding exclusive
// `STATE_STARTING` or `STATE_WRITING` ownership, and the sole worker on stack
// `i` reads it only from `STATE_STARTING`/`STATE_READY` and drops it before
// publishing `STATE_IDLE` or releasing to `STATE_EMPTY`.
unsafe impl Sync for BulkReadJobSlot {}

static BULK_READ_SLOT_STATE: [BulkReadSlotState; BULK_READ_WORKERS] =
    [const { BulkReadSlotState::new() }; BULK_READ_WORKERS];
static BULK_READ_JOBS: [BulkReadJobSlot; BULK_READ_WORKERS] =
    [const { BulkReadJobSlot(UnsafeCell::new(MaybeUninit::uninit())) }; BULK_READ_WORKERS];
#[cfg(not(test))]
static mut BULK_READ_STACKS: [BulkReadStack; BULK_READ_WORKERS] =
    [const { BulkReadStack([0; BULK_READ_STACK_BYTES]) }; BULK_READ_WORKERS];

/// Detached source plan produced on the receive owner and consumed by one
/// transient worker thread.
pub(super) enum BulkReadSource {
    /// DVM/FAT file read through a detached `FileReadMap` source without
    /// holding `VFS_STORAGE` across device IPC.
    Mapped {
        mount_id: u64,
        read_map: Arc<FileReadMap>,
        source: Box<dyn ReadAt<Error = RangeError> + Send>,
        mount_generation: u64,
        storage_epoch: u64,
    },
    /// Immutable bootstrap-image file (lock-free read).
    EarlySystem { path: String },
    /// Pinned TmpFs node read in bounded chunks under brief storage holds.
    TmpfsNode { mount_id: u64, node: u64 },
    /// Path-backed fallback (e.g. `file_bytes_cache`, `SHARED_PATH_FILES`, or
    /// volumes without read-map support) read in bounded chunks.
    PathChunked {
        path: String,
        pid: u64,
        file_len: u64,
        mount_generation: u64,
    },
}

pub(super) struct BulkReadJob {
    pub(super) reply_cap: u64,
    pub(super) offset: u64,
    pub(super) count: usize,
    pub(super) file_len: u64,
    pub(super) source: BulkReadSource,
}

pub(super) enum BulkReadPlan {
    Eof { file_len: u64 },
    Job(BulkReadJob),
}

pub(super) fn reply_bulk_read_status(
    reply_cap: u64,
    status: i32,
    bytes_read: u64,
    file_len: u64,
) -> i64 {
    let response = VfsBulkReadResponse {
        version: VFS_IPC_ABI_VERSION,
        op: VFS_IPC_OP_BULK_READ,
        status,
        bytes_read,
        file_len,
        reserved0: 0,
    }
    .to_bytes();
    // SAFETY: `response` is an initialized 32-byte array live for the
    // synchronous reply copy.
    unsafe { ipc::reply(reply_cap, response.as_ptr(), response.len()) }
}

fn reply_bulk_read_memfd(reply_cap: u64, fd: i32, bytes_read: u64, file_len: u64) -> i64 {
    let response = VfsBulkReadResponse {
        version: VFS_IPC_ABI_VERSION,
        op: VFS_IPC_OP_BULK_READ,
        status: 0,
        bytes_read,
        file_len,
        reserved0: 0,
    }
    .to_bytes();
    let send_fd = fd as u64;
    let args = IpcReplyWithHandlesArgs {
        reply_cap,
        response_ptr: response.as_ptr() as u64,
        response_len: response.len() as u64,
        send_fds_ptr: (&send_fd as *const u64) as u64,
        send_fd_count: 1,
        reserved0: 0,
        reserved1: 0,
    };
    // SAFETY: `args`, `response`, and `send_fd` remain live for the synchronous
    // kernel reply copy; `fd` is owned by the caller until after this returns.
    unsafe {
        rustos_svc_runtime::syscall::syscall1(
            SYS_RUSTOS_IPC_REPLY_WITH_HANDLES,
            (&args as *const IpcReplyWithHandlesArgs) as u64,
        )
    }
}

/// Offloads a planned bulk read to a cached idle worker, or lazily clones a
/// worker onto an empty slot if none are currently parked.
///
/// If all worker slots are busy or the kernel task table cannot admit a new
/// worker right now, replies immediately with `EAGAIN` on the receive thread
/// so the caller falls back cleanly to `PREAD64`.
#[cfg(not(test))]
pub(super) fn enqueue_bulk_read(job: BulkReadJob) -> i64 {
    let reply_cap = job.reply_cap;
    // Fast path: reuse an already-running parked worker with zero `SYS_CLONE`.
    if let Some(index) = BULK_READ_SLOT_STATE
        .iter()
        .position(BulkReadSlotState::try_claim_idle)
    {
        // SAFETY: winning `STATE_IDLE -> STATE_WRITING` makes this thread the
        // sole owner of `BULK_READ_JOBS[index]` until `publish_ready()`.
        unsafe {
            (*BULK_READ_JOBS[index].0.get()).write(job);
        }
        BULK_READ_SLOT_STATE[index].publish_ready();
        futex_wake_slot(BULK_READ_SLOT_STATE[index].as_atomic());
        return 0;
    }

    // Cold path: claim an empty stack slot and clone one worker.
    let Some(index) = BULK_READ_SLOT_STATE
        .iter()
        .position(BulkReadSlotState::try_claim_empty)
    else {
        return reply_bulk_read_status(reply_cap, EAGAIN, 0, 0);
    };
    // SAFETY: winning `STATE_EMPTY -> STATE_STARTING` makes this thread the
    // sole owner of `BULK_READ_JOBS[index]` until the cloned worker starts.
    unsafe {
        (*BULK_READ_JOBS[index].0.get()).write(job);
    }
    // SAFETY: computes one-past-the-end of the claimed exclusive static stack
    // without dereferencing it.
    let stack_top = unsafe {
        core::ptr::addr_of_mut!(BULK_READ_STACKS[index])
            .cast::<u8>()
            .add(BULK_READ_STACK_BYTES) as u64
    };
    if super::snapshot_worker::spawn_thread_on_stack(stack_top, bulk_read_worker_entry).is_err() {
        // SAFETY: clone failed, so no worker was created and this thread still
        // exclusively owns the initialized job in `BULK_READ_JOBS[index]`.
        let dropped = unsafe { (*BULK_READ_JOBS[index].0.get()).assume_init_read() };
        drop(dropped);
        BULK_READ_SLOT_STATE[index].abort_starting();
        return reply_bulk_read_status(reply_cap, EAGAIN, 0, 0);
    }
    0
}

#[cfg(not(test))]
fn futex_wait_slot(state: &AtomicU32, expected: u32, timeout_ns: u64) -> i64 {
    let timeout = linux_abi::LinuxTimespec {
        tv_sec: (timeout_ns / 1_000_000_000) as i64,
        tv_nsec: (timeout_ns % 1_000_000_000) as i64,
    };
    // SAFETY: `state` is static shared process memory and `timeout` stays live
    // for the synchronous kernel futex wait.
    unsafe {
        rustos_svc_runtime::syscall::syscall6(
            linux_abi::SYS_FUTEX,
            state.as_ptr() as u64,
            (linux_abi::FUTEX_WAIT | linux_abi::FUTEX_PRIVATE_FLAG) as u64,
            u64::from(expected),
            (&timeout as *const linux_abi::LinuxTimespec) as u64,
            0,
            0,
        )
    }
}

#[cfg(not(test))]
fn futex_wake_slot(state: &AtomicU32) {
    // SAFETY: `state` is static shared process memory.
    let _ = unsafe {
        rustos_svc_runtime::syscall::syscall6(
            linux_abi::SYS_FUTEX,
            state.as_ptr() as u64,
            (linux_abi::FUTEX_WAKE | linux_abi::FUTEX_PRIVATE_FLAG) as u64,
            1,
            0,
            0,
            0,
        )
    };
}

#[cfg(test)]
pub(super) fn enqueue_bulk_read(job: BulkReadJob) -> i64 {
    reply_bulk_read_status(job.reply_cap, EAGAIN, 0, 0)
}

struct OwnedBulkMemfd(i32);

impl OwnedBulkMemfd {
    fn create() -> Result<Self, i32> {
        let name = CString::new("vfsd-bulk-read").map_err(|_| EINVAL)?;
        // SAFETY: `name` is a live NUL-terminated string for the synchronous
        // kernel-direct `memfd_create` syscall; the returned fd is private.
        let fd = unsafe {
            rustos_svc_runtime::syscall::syscall2(
                linux_abi::SYS_MEMFD_CREATE,
                name.as_ptr() as u64,
                linux_abi::MFD_CLOEXEC | linux_abi::MFD_ALLOW_SEALING,
            )
        };
        if fd < 0 {
            return Err((-fd) as i32);
        }
        match i32::try_from(fd) {
            Ok(fd) => Ok(Self(fd)),
            Err(_) => {
                // SAFETY: `fd` is the freshly created private descriptor.
                let _ = unsafe {
                    rustos_svc_runtime::syscall::syscall1(linux_abi::SYS_CLOSE, fd as u64)
                };
                Err(EOVERFLOW)
            }
        }
    }

    fn write_all(&mut self, mut bytes: &[u8]) -> Result<(), i32> {
        while !bytes.is_empty() {
            // SAFETY: `bytes` is a live readable slice and `self.0` is our
            // private memfd descriptor.
            let status = unsafe {
                rustos_svc_runtime::syscall::syscall3(
                    linux_abi::SYS_WRITE,
                    self.0 as u64,
                    bytes.as_ptr() as u64,
                    bytes.len() as u64,
                )
            };
            if status < 0 {
                return Err((-status) as i32);
            }
            let written = usize::try_from(status).map_err(|_| EOVERFLOW)?;
            if written == 0 || written > bytes.len() {
                return Err(EIO);
            }
            bytes = &bytes[written..];
        }
        Ok(())
    }

    fn seal(&mut self) -> Result<(), i32> {
        let seals = linux_abi::F_SEAL_WRITE
            | linux_abi::F_SEAL_GROW
            | linux_abi::F_SEAL_SHRINK
            | linux_abi::F_SEAL_SEAL;
        // SAFETY: `self.0` is still private and unmapped; integer-only fcntl.
        let status = unsafe {
            rustos_svc_runtime::syscall::syscall3(
                linux_abi::SYS_FCNTL,
                self.0 as u64,
                linux_abi::F_ADD_SEALS,
                seals,
            )
        };
        if status < 0 {
            return Err((-status) as i32);
        }
        Ok(())
    }
}

impl Drop for OwnedBulkMemfd {
    fn drop(&mut self) {
        if self.0 >= 0 {
            close_fd(self.0);
        }
    }
}

fn read_source_chunk(source: &mut BulkReadSource, pos: u64, out: &mut [u8]) -> Result<usize, i32> {
    match source {
        BulkReadSource::Mapped {
            mount_id,
            read_map,
            source,
            mount_generation,
            storage_epoch,
        } => {
            {
                let mut storage = lock_vfs_storage();
                storage.invalidate_caches_if_remounted();
                if storage.mount_generation != *mount_generation
                    || storage.dvm_storage_epoch != *storage_epoch
                {
                    return Err(EAGAIN);
                }
                let is_current = storage
                    .mounts
                    .by_id(*mount_id)
                    .and_then(|mount| mount.read_map_source.as_ref())
                    .is_some_and(|rms| rms.read_map_is_current(read_map));
                if !is_current {
                    return Err(EAGAIN);
                }
            }
            // Detached device read runs completely outside `VFS_STORAGE`.
            source.read_at(pos, out).map_err(map_range_error)
        }
        BulkReadSource::EarlySystem { path } => {
            early_system::read(path.as_str(), pos, out)?.ok_or(EIO)
        }
        BulkReadSource::TmpfsNode { mount_id, node } => {
            let want = out.len().min(BULK_READ_LOCKED_CHUNK_BYTES);
            lock_vfs_storage().read_node(*mount_id, *node, pos, &mut out[..want])
        }
        BulkReadSource::PathChunked {
            path,
            pid,
            file_len,
            mount_generation,
        } => {
            let want = out.len().min(BULK_READ_LOCKED_CHUNK_BYTES);
            let mut storage = lock_vfs_storage();
            storage.invalidate_caches_if_remounted();
            if storage.mount_generation != *mount_generation {
                return Err(EAGAIN);
            }
            if let Some(Ok(node)) = vfsd::proc_fs::lookup(path.as_str(), *pid) {
                if vfsd::proc_fs::is_virtual_file(node) {
                    return storage.read_proc_file_into(node, pos, &mut out[..want]);
                }
            }
            if let Some(file) = vfsd::proc_fs::sys_virtual_file(path.as_str()) {
                return storage.read_sys_file_into(file, pos, &mut out[..want]);
            }
            storage.read_file_slice_into(path.as_str(), *file_len, pos, &mut out[..want])
        }
    }
}

fn fill_bulk_memfd(job: &mut BulkReadJob) -> Result<(OwnedBulkMemfd, usize), i32> {
    let mut memfd = OwnedBulkMemfd::create()?;
    let window = job.count.min(BULK_READ_WINDOW_BYTES);
    let mut buf = Vec::new();
    buf.try_reserve_exact(window).map_err(|_| ENOMEM)?;
    buf.resize(window, 0);

    let mut written = 0usize;
    while written < job.count {
        let pos = job.offset.checked_add(written as u64).ok_or(EOVERFLOW)?;
        let want = (job.count - written).min(buf.len());
        let read = read_source_chunk(&mut job.source, pos, &mut buf[..want])?;
        if read == 0 {
            break;
        }
        if read > want {
            return Err(EIO);
        }
        memfd.write_all(&buf[..read])?;
        written += read;
        if read < want {
            break;
        }
    }
    if written > 0 {
        memfd.seal()?;
    }
    Ok((memfd, written))
}

/// Checks the 1-entry sealed immutable-volume snapshot cache before touching
/// `storaged` or FAT. Revalidates mount generation and DVM storage epoch under
/// one brief `VFS_STORAGE` hold, releases `VFS_STORAGE`, and returns an owned
/// duplicate fd on hit.
fn try_lookup_cached_snapshot(
    job: &BulkReadJob,
) -> Result<Option<snapshot_cache::CachedSnapshotFd>, i32> {
    match &job.source {
        BulkReadSource::Mapped {
            mount_id,
            read_map,
            mount_generation,
            storage_epoch,
            ..
        } => {
            let (cur_mount_gen, cur_epoch) = {
                let mut storage = lock_vfs_storage();
                storage.invalidate_caches_if_remounted();
                (storage.mount_generation, storage.dvm_storage_epoch)
            };
            snapshot_cache::invalidate_if_stale(cur_mount_gen, cur_epoch);
            if cur_mount_gen != *mount_generation || cur_epoch != *storage_epoch {
                return Err(EAGAIN);
            }
            Ok(snapshot_cache::lookup_mapped(
                *mount_id,
                read_map.identity(),
                job.file_len,
                *mount_generation,
                *storage_epoch,
            ))
        }
        BulkReadSource::PathChunked {
            path,
            file_len,
            mount_generation,
            ..
        } => {
            let (cur_mount_gen, cur_epoch) = {
                let mut storage = lock_vfs_storage();
                storage.invalidate_caches_if_remounted();
                (storage.mount_generation, storage.dvm_storage_epoch)
            };
            snapshot_cache::invalidate_if_stale(cur_mount_gen, cur_epoch);
            if cur_mount_gen != *mount_generation {
                return Err(EAGAIN);
            }
            if cur_epoch == 0 {
                return Ok(None);
            }
            Ok(snapshot_cache::lookup_path(
                path.as_str(),
                *file_len,
                *mount_generation,
                cur_epoch,
            ))
        }
        BulkReadSource::EarlySystem { .. } | BulkReadSource::TmpfsNode { .. } => Ok(None),
    }
}

/// Copies `[offset .. offset + count]` from an already-loaded sealed snapshot
/// memfd into a freshly sealed reply memfd via kernel-direct `SYS_PREAD64`
/// (zero `storaged` IPC, zero FAT reads, zero `VFS_STORAGE` holds).
fn fill_bulk_memfd_from_cached_snapshot(
    cached: &snapshot_cache::CachedSnapshotFd,
    offset: u64,
    count: usize,
) -> Result<(OwnedBulkMemfd, usize), i32> {
    let mut memfd = OwnedBulkMemfd::create()?;
    let window = count.min(BULK_READ_WINDOW_BYTES);
    let mut buf = Vec::new();
    buf.try_reserve_exact(window).map_err(|_| ENOMEM)?;
    buf.resize(window, 0);

    let mut written = 0usize;
    while written < count {
        let pos = offset.checked_add(written as u64).ok_or(EOVERFLOW)?;
        let want = (count - written).min(buf.len());
        let read = cached.pread_at(pos, &mut buf[..want])?;
        if read == 0 {
            break;
        }
        memfd.write_all(&buf[..read])?;
        written += read;
        if read < want {
            break;
        }
    }
    if written > 0 {
        memfd.seal()?;
    }
    Ok((memfd, written))
}

fn run_bulk_read_job(mut job: BulkReadJob) {
    let reply_cap = job.reply_cap;
    let file_len = job.file_len;
    match try_lookup_cached_snapshot(&job) {
        Ok(Some(cached)) => {
            if job.offset == 0 && (job.count as u64) == file_len {
                // Whole-file read of the exact sealed immutable snapshot:
                // transfer the cached sealed memfd handle directly with zero
                // copies and zero storage I/O.
                let _ = reply_bulk_read_memfd(reply_cap, cached.raw(), file_len, file_len);
                drop(cached);
                return;
            }
            let filled = fill_bulk_memfd_from_cached_snapshot(&cached, job.offset, job.count);
            drop(cached);
            match filled {
                Ok((memfd, written)) if written > 0 => {
                    let _ = reply_bulk_read_memfd(reply_cap, memfd.0, written as u64, file_len);
                    drop(memfd);
                }
                Ok((memfd, _)) => {
                    drop(memfd);
                    let _ = reply_bulk_read_status(reply_cap, 0, 0, file_len);
                }
                Err(errno) => {
                    let _ = reply_bulk_read_status(reply_cap, errno, 0, 0);
                }
            }
            return;
        }
        Ok(None) => {}
        Err(errno) => {
            let _ = reply_bulk_read_status(reply_cap, errno, 0, 0);
            return;
        }
    }
    match fill_bulk_memfd(&mut job) {
        Ok((memfd, written)) if written > 0 => {
            let _ = reply_bulk_read_memfd(reply_cap, memfd.0, written as u64, file_len);
            drop(memfd);
        }
        Ok((memfd, _)) => {
            drop(memfd);
            let _ = reply_bulk_read_status(reply_cap, 0, 0, file_len);
        }
        Err(errno) => {
            let _ = reply_bulk_read_status(reply_cap, errno, 0, 0);
        }
    }
}

#[cfg(not(test))]
fn current_bulk_read_index() -> usize {
    let sp: usize;
    // SAFETY: reads the current stack pointer register without memory access.
    unsafe {
        core::arch::asm!("mov {}, rsp", out(reg) sp, options(nomem, nostack, preserves_flags));
    }
    let base = core::ptr::addr_of!(BULK_READ_STACKS) as usize;
    (sp - base - 1) / BULK_READ_STACK_BYTES
}

/// Releases the static stack slot and terminates this worker in one asm block
/// with no stack access in between, matching `bulk_lanes::exit_releasing_stack`.
#[cfg(not(test))]
fn exit_releasing_stack(state: &AtomicU32) -> ! {
    // SAFETY: `state` is a static `AtomicU32`; on x86-64 a 32-bit store has
    // release semantics, and `SYS_EXIT` terminates only this worker thread
    // without touching the stack after the store.
    unsafe {
        core::arch::asm!(
            "mov dword ptr [{state}], {free}",
            "syscall",
            state = in(reg) state.as_ptr(),
            free = const STATE_EMPTY,
            in("rax") linux_abi::SYS_EXIT,
            in("rdi") 0_u64,
            options(noreturn),
        );
    }
}

#[cfg(not(test))]
extern "C" fn bulk_read_worker_entry() -> ! {
    let index = current_bulk_read_index();
    let slot = &BULK_READ_SLOT_STATE[index];
    slot.mark_running();
    loop {
        {
            // SAFETY: either the initial `STATE_STARTING` clone or a warm
            // `STATE_WRITING -> STATE_READY` publish handed exclusive ownership
            // of the initialized job in `BULK_READ_JOBS[index]` to this worker.
            let job = unsafe { (*BULK_READ_JOBS[index].0.get()).assume_init_read() };
            run_bulk_read_job(job);
        }
        slot.park_idle();
        let idle_deadline =
            rustos_svc_runtime::syscall::monotonic_nanos().saturating_add(WORKER_IDLE_RETIRE_NS);
        loop {
            match slot.load() {
                STATE_READY => {
                    slot.mark_running();
                    break;
                }
                STATE_WRITING => {
                    // Producer won `STATE_IDLE -> STATE_WRITING` and is copying
                    // the next `BulkReadJob` into the slot.
                    core::hint::spin_loop();
                }
                STATE_IDLE => {
                    let now = rustos_svc_runtime::syscall::monotonic_nanos();
                    if now >= idle_deadline {
                        if slot.try_begin_exit() {
                            exit_releasing_stack(slot.as_atomic());
                        }
                        continue;
                    }
                    let wait_ns = (idle_deadline - now).min(IDLE_FUTEX_STEP_NS);
                    let status = futex_wait_slot(slot.as_atomic(), STATE_IDLE, wait_ns);
                    // 0 (woken), -11 (EAGAIN), -4 (EINTR), -110 (ETIMEDOUT) are
                    // normal; any unsupported futex failure retires cleanly.
                    if !matches!(status, 0 | -11 | -4 | -110) && slot.try_begin_exit() {
                        exit_releasing_stack(slot.as_atomic());
                    }
                }
                _ => {
                    // Unexpected state: never spin forever; retire if idle.
                    if slot.try_begin_exit() {
                        exit_releasing_stack(slot.as_atomic());
                    }
                    core::hint::spin_loop();
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        BulkReadSlotState, STATE_EMPTY, STATE_EXITING, STATE_IDLE, STATE_READY, STATE_RUNNING,
        STATE_STARTING, STATE_WRITING,
    };

    #[test]
    fn slot_state_supports_cold_start_warm_reuse_and_idle_retirement() {
        let slot = BulkReadSlotState::new();
        assert_eq!(slot.load(), STATE_EMPTY);

        // Cold start: EMPTY -> STARTING -> RUNNING -> IDLE.
        assert!(!slot.try_claim_idle());
        assert!(slot.try_claim_empty());
        assert_eq!(slot.load(), STATE_STARTING);
        assert!(!slot.try_claim_empty());
        slot.mark_running();
        assert_eq!(slot.load(), STATE_RUNNING);
        slot.park_idle();
        assert_eq!(slot.load(), STATE_IDLE);

        // Warm reuse: IDLE -> WRITING -> READY -> RUNNING -> IDLE.
        assert!(slot.try_claim_idle());
        assert_eq!(slot.load(), STATE_WRITING);
        assert!(!slot.try_begin_exit());
        assert!(!slot.try_claim_idle());
        slot.publish_ready();
        assert_eq!(slot.load(), STATE_READY);
        assert!(!slot.try_begin_exit());
        slot.mark_running();
        assert_eq!(slot.load(), STATE_RUNNING);
        slot.park_idle();
        assert_eq!(slot.load(), STATE_IDLE);

        // Idle retirement: IDLE -> EXITING blocks concurrent producer claim.
        assert!(slot.try_begin_exit());
        assert_eq!(slot.load(), STATE_EXITING);
        assert!(!slot.try_claim_idle());
        assert!(!slot.try_claim_empty());
    }

    #[test]
    fn slot_state_clone_abort_returns_to_empty() {
        let slot = BulkReadSlotState::new();
        assert!(slot.try_claim_empty());
        assert_eq!(slot.load(), STATE_STARTING);
        slot.abort_starting();
        assert_eq!(slot.load(), STATE_EMPTY);
        assert!(slot.try_claim_empty());
    }
}
