//! Transient bulk-read workers for `VFS_IPC_OP_BULK_READ`.
//!
//! - **Owner:** `vfsd` owns the worker slots, static stacks, and private memfd
//!   creation/sealing; the caller (`mm_broker_ops`) receives ownership of the
//!   sealed memfd through handle-transfer reply.
//! - **Boundary:** Requests are validated and planned on the receive owner
//!   before a transient worker is spawned; workers never touch `VfsState`
//!   descriptor tables and never call `syscalld`.
//! - **Lifecycle:** One worker thread is cloned onto a claimed static stack for
//!   one bulk read, streams file bytes into a sealed memfd, replies directly to
//!   `reply_cap`, drops all heap state, and atomically releases its stack slot
//!   before `SYS_EXIT`. Zero resident threads are kept when idle.
//! - **Concurrency:** At most `BULK_READ_WORKERS` (2) transient workers run at
//!   once. Mapped DVM/FAT reads release `VFS_STORAGE` before every device read
//!   so `open`/`stat` never wait behind multi-megabyte transfers.
//! - **Failure:** Slot saturation or `clone` failure returns `EAGAIN` on the
//!   receive thread so callers fall back to `PREAD64`. Mid-transfer remounts,
//!   OOM, or I/O errors close the private memfd and reply with the exact errno.
//! - **Forbidden:** No resident threads, no `syscalld` IPC, no holding
//!   `VFS_STORAGE` across device I/O, and no stack use after releasing the
//!   stack slot.
//! - **Evidence:** `vfsd` unit tests for request bounds/proxy admission and
//!   `mm_broker_ops` bulk-read integration.

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

/// Maximum concurrent transient bulk-read workers (zero resident when idle).
const BULK_READ_WORKERS: usize = 2;
const BULK_READ_STACK_BYTES: usize = 128 * 1024;
/// Transfer window for detached DVM/FAT and early-system reads (512 KiB so
/// `BootBlockDevice` fans out across parallel `bulk_lanes`).
const BULK_READ_WINDOW_BYTES: usize = 512 * 1024;
/// Bounded per-lock chunk for sources that briefly acquire `VFS_STORAGE`.
const BULK_READ_LOCKED_CHUNK_BYTES: usize = 128 * 1024;

const STACK_FREE: u32 = 0;
const STACK_BUSY: u32 = 1;

#[repr(C, align(16))]
struct BulkReadStack([u8; BULK_READ_STACK_BYTES]);

struct BulkReadJobSlot(UnsafeCell<MaybeUninit<BulkReadJob>>);

// SAFETY: `BULK_READ_STACK_STATE[i]` is the sole ownership gate for slot `i`.
// The receive owner writes the job only after winning `STACK_FREE -> STACK_BUSY`
// and before cloning the worker; the sole worker takes the job, drops it before
// releasing the stack back to `STACK_FREE`.
unsafe impl Sync for BulkReadJobSlot {}

static BULK_READ_STACK_STATE: [AtomicU32; BULK_READ_WORKERS] =
    [const { AtomicU32::new(STACK_FREE) }; BULK_READ_WORKERS];
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

/// Offloads a planned bulk read to a transient worker thread.
///
/// If all worker slots are busy or the kernel task table cannot admit a
/// transient thread right now, replies immediately with `EAGAIN` on the
/// receive thread so the caller falls back cleanly to `PREAD64`.
#[cfg(not(test))]
pub(super) fn enqueue_bulk_read(job: BulkReadJob) -> i64 {
    let reply_cap = job.reply_cap;
    let Some(index) = BULK_READ_STACK_STATE.iter().position(|state| {
        // ORDERING: AcqRel claims exclusive ownership of slot `index` and its
        // static stack, pairing with the exiting worker's release store.
        state
            .compare_exchange(STACK_FREE, STACK_BUSY, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
    }) else {
        return reply_bulk_read_status(reply_cap, EAGAIN, 0, 0);
    };
    // SAFETY: winning `STACK_FREE -> STACK_BUSY` makes this thread the sole
    // owner of `BULK_READ_JOBS[index]` until the cloned worker starts.
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
        // ORDERING: Release returns the unused stack slot to future callers.
        BULK_READ_STACK_STATE[index].store(STACK_FREE, Ordering::Release);
        return reply_bulk_read_status(reply_cap, EAGAIN, 0, 0);
    }
    0
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

/// Releases the static stack slot and terminates this transient worker in one
/// asm block with no stack access in between, matching `bulk_lanes::exit_releasing_stack`.
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
            free = const STACK_FREE,
            in("rax") linux_abi::SYS_EXIT,
            in("rdi") 0_u64,
            options(noreturn),
        );
    }
}

#[cfg(not(test))]
extern "C" fn bulk_read_worker_entry() -> ! {
    let index = current_bulk_read_index();
    {
        // SAFETY: the receive owner wrote `BULK_READ_JOBS[index]` before
        // cloning this thread onto stack `index`, so this worker is the sole
        // consumer of the initialized job.
        let job = unsafe { (*BULK_READ_JOBS[index].0.get()).assume_init_read() };
        run_bulk_read_job(job);
    }
    exit_releasing_stack(&BULK_READ_STACK_STATE[index])
}
