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

// SAFETY: `SNAPSHOT_JOB_ADMISSION` is the publication authority. The receive
// owner writes only in WRITING and publishes READY with Release; the sole
// worker claims READY with Acquire, copies the fixed job, and returns IDLE.
unsafe impl Sync for SnapshotJobSlot {}

#[repr(C, align(16))]
struct SnapshotWorkerStack([u8; SNAPSHOT_WORKER_STACK_BYTES]);

static SNAPSHOT_JOB_ADMISSION: SnapshotWorkerAdmission = SnapshotWorkerAdmission::new();
static SNAPSHOT_WORKER_STARTED: AtomicBool = AtomicBool::new(false);
static SNAPSHOT_JOB: SnapshotJobSlot = SnapshotJobSlot(UnsafeCell::new(MaybeUninit::uninit()));
#[cfg(not(test))]
static mut SNAPSHOT_WORKER_STACK: SnapshotWorkerStack =
    SnapshotWorkerStack([0; SNAPSHOT_WORKER_STACK_BYTES]);

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
    if !SNAPSHOT_JOB_ADMISSION.try_reserve() {
        return reply_snapshot_overload(reply_cap);
    }
    // SAFETY: WRITING grants the sole receive owner exclusive slot access.
    unsafe {
        (*SNAPSHOT_JOB.0.get()).write(SnapshotJob {
            reply_cap,
            sender_pid,
            sender_tid,
            request,
        });
    }
    SNAPSHOT_JOB_ADMISSION.publish_ready();
    0
}

pub(super) fn ensure_snapshot_worker() -> Result<(), i64> {
    if SNAPSHOT_WORKER_STARTED.load(Ordering::Acquire) {
        return Ok(());
    }
    if SNAPSHOT_WORKER_STARTED
        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
        .is_err()
    {
        return Ok(());
    }
    if let Err(errno) = spawn_snapshot_worker() {
        SNAPSHOT_WORKER_STARTED.store(false, Ordering::Release);
        return Err(errno);
    }
    Ok(())
}

#[cfg(not(test))]
fn spawn_snapshot_worker() -> Result<(), i64> {
    // SAFETY: this computes the exclusive static stack's one-past-the-end
    // address without dereferencing it; admission starts the worker once.
    let stack_top = unsafe {
        core::ptr::addr_of_mut!(SNAPSHOT_WORKER_STACK.0)
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
fn spawn_snapshot_worker() -> Result<(), i64> {
    Err(ENOSYS as i64)
}

#[cfg(not(test))]
extern "C" fn snapshot_worker_entry() -> ! {
    loop {
        if !SNAPSHOT_JOB_ADMISSION.try_claim() {
            rustos_svc_runtime::syscall::sleep_millis(1);
            continue;
        }
        // SAFETY: Acquire of READY observes the complete fixed job, and BUSY
        // excludes the receive owner until this copy has completed.
        let job = unsafe { (*SNAPSHOT_JOB.0.get()).assume_init_read() };
        // Storage is acquired inside, per phase and per chunk, never held
        // across the bulk read.
        let _ = reply_executable_snapshot(
            job.reply_cap,
            job.sender_pid,
            job.sender_tid,
            &job.request,
        );
        SNAPSHOT_JOB_ADMISSION.release();
    }
}
