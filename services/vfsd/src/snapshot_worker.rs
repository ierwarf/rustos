//! Bounded executable-snapshot worker owned independently from the VFS receive loop.

use alloc::format;
#[cfg(not(test))]
use core::arch::asm;
use core::cell::UnsafeCell;
use core::mem::{size_of, MaybeUninit};
use core::sync::atomic::{AtomicBool, Ordering};

use rustos_svc_runtime::ipc;
use rustos_user_abi::linux as linux_abi;
use rustos_user_abi::syscall::{
    VfsExecutableSnapshotRequest, VfsExecutableSnapshotResponse,
    VFS_EXECUTABLE_SNAPSHOT_ABI_VERSION, VFS_EXECUTABLE_SNAPSHOT_OP_OPEN,
};
use vfsd::SnapshotWorkerAdmission;

#[cfg(test)]
use super::ENOSYS;
use super::{reply_executable_snapshot, EAGAIN};

const SNAPSHOT_WORKER_STACK_BYTES: usize = 128 * 1024;
const SYS_CLONE: u64 = 56;
const CLONE_VM: u64 = 0x0000_0100;
const CLONE_FS: u64 = 0x0000_0200;
const CLONE_FILES: u64 = 0x0000_0400;
const CLONE_SIGHAND: u64 = 0x0000_0800;
const CLONE_THREAD: u64 = 0x0001_0000;

#[derive(Clone, Copy)]
struct SnapshotJob {
    reply_cap: u64,
    sender_pid: u64,
    sender_tid: u64,
    request: VfsExecutableSnapshotRequest,
}

struct SnapshotJobSlot(UnsafeCell<MaybeUninit<SnapshotJob>>);

// SAFETY: the matching `SNAPSHOT_JOB_ADMISSION` entry is the publication
// authority for its slot. The receive owner writes only in WRITING and
// publishes READY with Release; one worker claims READY with Acquire, copies
// the fixed job, and returns IDLE. Slots are never shared between admissions.
unsafe impl Sync for SnapshotJobSlot {}

#[repr(C, align(16))]
struct SnapshotWorkerStack([u8; SNAPSHOT_WORKER_STACK_BYTES]);

/// Bounded snapshot concurrency.
///
/// Two, not more, and the bound is a consequence rather than a preference.
/// Every snapshot read serializes on the single storage owner, so a third
/// worker could only queue there; what a second worker buys is that one
/// snapshot can allocate, seal, and reply while another holds storage, and that
/// a second concurrent request is served instead of being rejected as overload.
///
/// This is ownership structure, not a latency fix: a single caller issuing
/// snapshots one at a time sees no difference. The per-snapshot phase report in
/// `storage_owner` is what says where a slow snapshot's time actually goes.
const SNAPSHOT_WORKERS: usize = 2;

static SNAPSHOT_JOB_ADMISSION: [SnapshotWorkerAdmission; SNAPSHOT_WORKERS] =
    [const { SnapshotWorkerAdmission::new() }; SNAPSHOT_WORKERS];
static SNAPSHOT_WORKERS_STARTED: AtomicBool = AtomicBool::new(false);
static SNAPSHOT_JOB: [SnapshotJobSlot; SNAPSHOT_WORKERS] =
    [const { SnapshotJobSlot(UnsafeCell::new(MaybeUninit::uninit())) }; SNAPSHOT_WORKERS];
#[cfg(not(test))]
static mut SNAPSHOT_WORKER_STACKS: [SnapshotWorkerStack; SNAPSHOT_WORKERS] =
    [const { SnapshotWorkerStack([0; SNAPSHOT_WORKER_STACK_BYTES]) }; SNAPSHOT_WORKERS];

pub(super) fn demote_current_thread_or_exit(role: &str) {
    // SAFETY: the syscall takes no pointers and changes only the calling
    // thread's scheduler class.
    let status = unsafe {
        rustos_svc_runtime::syscall::syscall0(
            rustos_user_abi::syscall::SYS_RUSTOS_SCHED_DEMOTE_SELF,
        )
    };
    if status == 0 {
        ipc::debug_line(&format!("vfsd: role={role} scheduling-class=user"));
        return;
    }
    ipc::debug_line(&format!(
        "vfsd: fatal role={role} scheduling demotion failed"
    ));
    // SAFETY: the syscall terminates only the calling worker and consumes no
    // userspace pointers.
    unsafe {
        rustos_svc_runtime::syscall::syscall1(linux_abi::SYS_EXIT, 134);
    }
    loop {
        core::hint::spin_loop();
    }
}

fn reply_snapshot_overload(reply_cap: u64) -> i64 {
    let response = VfsExecutableSnapshotResponse {
        version: VFS_EXECUTABLE_SNAPSHOT_ABI_VERSION,
        op: VFS_EXECUTABLE_SNAPSHOT_OP_OPEN,
        status: EAGAIN,
        ..VfsExecutableSnapshotResponse::default()
    };
    // SAFETY: response is an initialized fixed-layout wire value and remains
    // live for the synchronous reply copy.
    unsafe {
        ipc::reply(
            reply_cap,
            (&response as *const VfsExecutableSnapshotResponse).cast::<u8>(),
            size_of::<VfsExecutableSnapshotResponse>(),
        )
    }
}

pub(super) fn enqueue_executable_snapshot(
    reply_cap: u64,
    sender_pid: u64,
    sender_tid: u64,
    request: VfsExecutableSnapshotRequest,
) -> i64 {
    if ensure_snapshot_worker().is_err() {
        return reply_snapshot_overload(reply_cap);
    }
    let Some(index) = SNAPSHOT_JOB_ADMISSION
        .iter()
        .position(SnapshotWorkerAdmission::try_reserve)
    else {
        // Every worker is busy. The caller is told the provider is saturated
        // rather than being left to time out, so it can distinguish "no
        // capacity right now" from "this open failed".
        return reply_snapshot_overload(reply_cap);
    };
    // SAFETY: WRITING grants the receive owner exclusive access to this exact
    // slot; no other admission can be in WRITING for the same index.
    unsafe {
        (*SNAPSHOT_JOB[index].0.get()).write(SnapshotJob {
            reply_cap,
            sender_pid,
            sender_tid,
            request,
        });
    }
    SNAPSHOT_JOB_ADMISSION[index].publish_ready();
    0
}

/// Claims any published job, returning the slot the caller now owns.
///
/// Workers are interchangeable: a worker owns a slot for the duration of one
/// job rather than for its lifetime, so the pool needs no per-worker identity
/// and every worker can run the same entry.
#[cfg(not(test))]
fn claim_any_ready_job() -> Option<usize> {
    SNAPSHOT_JOB_ADMISSION
        .iter()
        .position(SnapshotWorkerAdmission::try_claim)
}

pub(super) fn ensure_snapshot_worker() -> Result<(), i64> {
    if SNAPSHOT_WORKERS_STARTED.load(Ordering::Acquire) {
        return Ok(());
    }
    if SNAPSHOT_WORKERS_STARTED
        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
        .is_err()
    {
        return Ok(());
    }
    for index in 0..SNAPSHOT_WORKERS {
        if let Err(errno) = spawn_snapshot_worker(index) {
            // A partially started pool is still a correct pool: the workers
            // that did start claim any slot. Only a pool with no worker at all
            // is a failure, because then nothing would ever claim a job.
            if index == 0 {
                SNAPSHOT_WORKERS_STARTED.store(false, Ordering::Release);
                return Err(errno);
            }
            break;
        }
    }
    Ok(())
}

#[cfg(not(test))]
fn spawn_snapshot_worker(index: usize) -> Result<(), i64> {
    // SAFETY: this computes one exclusive static stack's one-past-the-end
    // address without dereferencing it. `ensure_snapshot_worker` runs the loop
    // once, so each index is handed out to exactly one worker.
    let stack_top = unsafe {
        core::ptr::addr_of_mut!(SNAPSHOT_WORKER_STACKS[index])
            .cast::<u8>()
            .add(SNAPSHOT_WORKER_STACK_BYTES) as u64
    };
    let flags = CLONE_VM | CLONE_FS | CLONE_FILES | CLONE_SIGHAND | CLONE_THREAD;
    let result: i64;
    // SAFETY: the clone ABI receives the aligned exclusive stack top; the
    // child branches directly to the non-returning worker entry.
    unsafe {
        asm!(
            "syscall",
            "test rax, rax",
            "jnz 2f",
            "call {entry}",
            "ud2",
            "2:",
            entry = sym snapshot_worker_entry,
            inlateout("rax") SYS_CLONE as i64 => result,
            in("rdi") flags,
            in("rsi") stack_top,
            in("rdx") 0_u64,
            in("r10") 0_u64,
            in("r8") 0_u64,
            lateout("rcx") _,
            lateout("r11") _,
        );
    }
    if result <= 0 {
        Err(if result < 0 { -result } else { EAGAIN as i64 })
    } else {
        Ok(())
    }
}

#[cfg(test)]
fn spawn_snapshot_worker(_index: usize) -> Result<(), i64> {
    Err(ENOSYS as i64)
}

#[cfg(not(test))]
extern "C" fn snapshot_worker_entry() -> ! {
    loop {
        let Some(index) = claim_any_ready_job() else {
            rustos_svc_runtime::syscall::sleep_millis(1);
            continue;
        };
        // SAFETY: Acquire of READY observes the complete fixed job, and BUSY
        // excludes the receive owner from this exact slot until the copy below
        // has completed.
        let job = unsafe { (*SNAPSHOT_JOB[index].0.get()).assume_init_read() };
        // Storage is acquired inside, per phase, never held across the bulk
        // read.
        let _ =
            reply_executable_snapshot(job.reply_cap, job.sender_pid, job.sender_tid, &job.request);
        SNAPSHOT_JOB_ADMISSION[index].release();
    }
}
