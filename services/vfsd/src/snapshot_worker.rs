//! Bounded executable-snapshot worker owned independently from the VFS receive loop.

use alloc::{format, sync::Arc};
#[cfg(not(test))]
use core::arch::asm;
use core::cell::UnsafeCell;
use core::mem::{size_of, MaybeUninit};
use core::sync::atomic::{AtomicBool, AtomicU32, Ordering};

use rustos_svc_runtime::ipc;
use rustos_user_abi::linux as linux_abi;
use rustos_user_abi::syscall::{
    VfsExecutableSnapshotRequest, VfsExecutableSnapshotResponse,
    VFS_EXECUTABLE_SNAPSHOT_ABI_VERSION, VFS_EXECUTABLE_SNAPSHOT_OP_OPEN,
    VFS_EXECUTABLE_SNAPSHOT_OP_POLL, VFS_EXECUTABLE_SNAPSHOT_OP_CANCEL,
    VFS_EXECUTABLE_SNAPSHOT_PENDING, VFS_EXECUTABLE_SNAPSHOT_MAX_LEASE_MS,
};
use vfsd::{SnapshotWorkerAdmission, SnapshotWorkerSignal};
use vfsd::snapshot_sessions::{SnapshotControl, SnapshotSessions, SessionError, SessionPoll};

#[cfg(test)]
use super::ENOSYS;
use super::{reply_executable_snapshot, ExecutableSnapshotOpen, EAGAIN, EINVAL, EACCES, ETIMEDOUT, EOVERFLOW, ENOENT};

const SNAPSHOT_WORKER_STACK_BYTES: usize = 128 * 1024;
const SYS_CLONE: u64 = 56;
const CLONE_VM: u64 = 0x0000_0100;
const CLONE_FS: u64 = 0x0000_0200;
const CLONE_FILES: u64 = 0x0000_0400;
const CLONE_SIGHAND: u64 = 0x0000_0800;
const CLONE_THREAD: u64 = 0x0001_0000;

struct SnapshotJob {
    id: u64,
    control: Arc<SnapshotControl>,
    request: VfsExecutableSnapshotRequest,
}

pub(super) struct CompletedSnapshot {
    pub snapshot: ExecutableSnapshotOpen,
    pub request: VfsExecutableSnapshotRequest,
}

static SESSIONS: spin::Mutex<SnapshotSessions<CompletedSnapshot, SNAPSHOT_WORKERS>> =
    spin::Mutex::new(SnapshotSessions::new());

struct SnapshotJobSlot(UnsafeCell<MaybeUninit<SnapshotJob>>);

// SAFETY: the matching `SNAPSHOT_JOB_ADMISSION` entry is the publication
// authority for its slot. The receive owner writes only in WRITING and
// publishes READY with Release; one worker claims READY with Acquire, moves
// the initialized job, and returns IDLE after completion. Slots are never shared between admissions.
unsafe impl Sync for SnapshotJobSlot {}

#[repr(C, align(16))]
struct SnapshotWorkerStack([u8; SNAPSHOT_WORKER_STACK_BYTES]);

/// Bounded snapshot concurrency.
///
/// Two, not more, and the bound is a consequence rather than a preference.
/// The underlying device is serialized, but detached data reads do not retain
/// namespace ownership. A second worker can hash/seal/reap while its peer reads
/// and can serve a second admitted lease instead of rejecting all overlap.
/// Final-frame byte admission independently bounds their combined payloads.
///
/// This is ownership structure, not a latency fix: a single caller issuing
/// snapshots one at a time sees no difference. The per-snapshot phase report in
/// `storage_owner` is what says where a slow snapshot's time actually goes.
const SNAPSHOT_WORKERS: usize = 2;

static SNAPSHOT_JOB_ADMISSION: [SnapshotWorkerAdmission; SNAPSHOT_WORKERS] =
    [const { SnapshotWorkerAdmission::new() }; SNAPSHOT_WORKERS];
static SNAPSHOT_WORKERS_STARTED: AtomicBool = AtomicBool::new(false);
static SNAPSHOT_WORK_SIGNAL: SnapshotWorkerSignal = SnapshotWorkerSignal::new();
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

pub(super) fn reply_snapshot_status(reply_cap: u64, op: u16, status: i32) -> i64 {
    let response = VfsExecutableSnapshotResponse {
        version: VFS_EXECUTABLE_SNAPSHOT_ABI_VERSION,
        op,
        status,
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
    let now = rustos_svc_runtime::syscall::monotonic_nanos();
    if request.job_id != 0 || request.deadline_ns <= now {
        return reply_snapshot_status(reply_cap, request.op, ETIMEDOUT);
    }
    if request.deadline_ns - now > VFS_EXECUTABLE_SNAPSHOT_MAX_LEASE_MS * 1_000_000 {
        return reply_snapshot_status(reply_cap, request.op, EINVAL);
    }
    if ensure_snapshot_worker().is_err() {
        return reply_snapshot_status(reply_cap, request.op, EAGAIN);
    }
    let owner = (sender_pid, sender_tid);
    let admission = { SESSIONS.lock().begin(owner, request.deadline_ns, now) };
    let (id, control) = match admission {
        Ok(admission) => admission,
        Err(error) => return reply_snapshot_status(reply_cap, request.op, session_errno(error)),
    };
    let Some(index) = SNAPSHOT_JOB_ADMISSION
        .iter()
        .position(SnapshotWorkerAdmission::try_reserve)
    else {
        // Every worker is busy. The caller is told the provider is saturated
        // rather than being left to time out, so it can distinguish "no
        // capacity right now" from "this open failed".
        let rejected = { SESSIONS.lock().finish(id, Err(EAGAIN), now) };
        drop(rejected);
        let _ = SESSIONS.lock().cancel(id, owner, request.deadline_ns);
        wake_snapshot_workers(SNAPSHOT_WORK_SIGNAL.publish());
        return reply_snapshot_status(reply_cap, request.op, EAGAIN);
    };
    // SAFETY: WRITING grants the receive owner exclusive access to this exact
    // slot; no other admission can be in WRITING for the same index.
    unsafe {
        (*SNAPSHOT_JOB[index].0.get()).write(SnapshotJob {
            id,
            control,
            request,
        });
    }
    SNAPSHOT_JOB_ADMISSION[index].publish_ready();
    wake_snapshot_workers(SNAPSHOT_WORK_SIGNAL.publish());
    let response = VfsExecutableSnapshotResponse {
        version: VFS_EXECUTABLE_SNAPSHOT_ABI_VERSION, op: request.op,
        status: VFS_EXECUTABLE_SNAPSHOT_PENDING, job_id: id,
        ..VfsExecutableSnapshotResponse::default()
    };
    let reply = reply_control(reply_cap, &response);
    if reply < 0 {
        // OPEN reply loss cannot orphan work or retain a reply capability.
        let _ = SESSIONS.lock().cancel(id, owner, request.deadline_ns);
        wake_snapshot_workers(SNAPSHOT_WORK_SIGNAL.publish());
    }
    reply
}

fn session_errno(error: SessionError) -> i32 {
    match error {
        SessionError::Full => EAGAIN, SessionError::Exhausted => EOVERFLOW,
        SessionError::Unknown => ENOENT, SessionError::Owner => EACCES,
        SessionError::Lease => ETIMEDOUT,
    }
}
fn reply_control(reply_cap: u64, response: &VfsExecutableSnapshotResponse) -> i64 {
    // SAFETY: the initialized wire value stays live during the reply copy.
    unsafe { ipc::reply(reply_cap, (response as *const VfsExecutableSnapshotResponse).cast(), size_of::<VfsExecutableSnapshotResponse>()) }
}

pub(super) fn continue_executable_snapshot(reply_cap: u64, sender_pid: u64, sender_tid: u64,
    request: &VfsExecutableSnapshotRequest) -> i64 {
    if request.version != VFS_EXECUTABLE_SNAPSHOT_ABI_VERSION || request.flags != 0
        || request.reserved0 != 0 || request.job_id == 0 || request.path_len != 0
        || request.target_pid != 0 || request.dirfd != 0 || request.max_bytes != 0
        || !matches!(request.op, VFS_EXECUTABLE_SNAPSHOT_OP_POLL | VFS_EXECUTABLE_SNAPSHOT_OP_CANCEL)
    { return reply_snapshot_status(reply_cap, request.op, EINVAL); }
    let owner = (sender_pid, sender_tid);
    if request.op == VFS_EXECUTABLE_SNAPSHOT_OP_CANCEL {
        let result = { SESSIONS.lock().cancel(request.job_id, owner, request.deadline_ns) };
        wake_snapshot_workers(SNAPSHOT_WORK_SIGNAL.publish());
        return reply_snapshot_status(reply_cap, request.op, result.map_or_else(session_errno, |_| 0));
    }
    let outcome = { SESSIONS.lock().poll(request.job_id, owner, request.deadline_ns,
        rustos_svc_runtime::syscall::monotonic_nanos()) };
    match outcome {
        Err(error) => reply_snapshot_status(reply_cap, request.op, session_errno(error)),
        Ok(SessionPoll::Ready(Err(errno))) => reply_snapshot_status(reply_cap, request.op, errno),
        Ok(SessionPoll::Ready(Ok(complete))) => reply_executable_snapshot(reply_cap, request.job_id, complete),
        Ok(SessionPoll::Pending { completed, total }) => reply_control(reply_cap, &VfsExecutableSnapshotResponse {
            version: VFS_EXECUTABLE_SNAPSHOT_ABI_VERSION, op: request.op,
            status: VFS_EXECUTABLE_SNAPSHOT_PENDING, job_id: request.job_id,
            completed_bytes: completed, file_bytes: total,
            ..VfsExecutableSnapshotResponse::default()
        }),
    }
}

fn reap_expired_sessions() {
    loop {
        let expired = { SESSIONS.lock().expire_one(rustos_svc_runtime::syscall::monotonic_nanos()) };
        match expired { Some(payload) => drop(payload), None => break }
    }
}

fn wake_snapshot_workers(word: &AtomicU32) {
    // SAFETY: the aligned atomic word is static shared process memory. The
    // wake is only a hint: READY remains the sole admission authority, and a
    // timed wait preserves progress even when a wake is refused.
    let _ = unsafe {
        rustos_svc_runtime::syscall::syscall6(
            linux_abi::SYS_FUTEX,
            word.as_ptr() as u64,
            (linux_abi::FUTEX_WAKE | linux_abi::FUTEX_PRIVATE_FLAG) as u64,
            SNAPSHOT_WORKERS as u64,
            0,
            0,
            0,
        )
    };
}

#[cfg(not(test))]
fn wait_snapshot_work(word: &AtomicU32, expected: u32) {
    reap_expired_sessions();
    let timeout = linux_abi::LinuxTimespec {
        tv_sec: 0,
        tv_nsec: 1_000_000,
    };
    // SAFETY: the static word and initialized timeout outlive the synchronous
    // syscall. WAIT compares and registers atomically with respect to WAKE;
    // publication after an empty scan cannot slip through that boundary.
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
    // Success, stale comparison, interruption and timeout all re-scan READY.
    // Unsupported/refused waiter admission must not turn into a busy loop.
    if !matches!(status, 0 | -11 | -4 | -110) {
        rustos_svc_runtime::syscall::sleep_millis(1);
    }
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
    let mut demoted = false;
    loop {
        reap_expired_sessions();
        let Some(index) =
            SNAPSHOT_WORK_SIGNAL.claim_or_wait(claim_any_ready_job, wait_snapshot_work)
        else {
            continue;
        };
        if !demoted && super::SNAPSHOT_WORKER_DEMOTED.load(Ordering::Acquire) {
            demote_current_thread_or_exit("snapshot-worker");
            demoted = true;
        }
        // SAFETY: Acquire of READY observes the complete fixed job, and BUSY
        // excludes the receive owner from this exact slot until the move below
        // has completed.
        let job = unsafe { (*SNAPSHOT_JOB[index].0.get()).assume_init_read() };
        // Storage is acquired inside, per phase, never held across the bulk
        // read.
        let result = super::open_executable_snapshot(&job.request, &job.control)
            .map(|snapshot| CompletedSnapshot { snapshot, request: job.request });
        if let Err(errno) = &result {
            ipc::debug_line(&format!("vfsd: executable snapshot rejected stage=open errno={errno} completed={} total={}",
                job.control.completed_bytes(), job.control.total_bytes()));
        }
        let rejected = { SESSIONS.lock().finish(job.id, result,
            rustos_svc_runtime::syscall::monotonic_nanos()) };
        // Frame release and budget refunds happen outside every table/storage
        // lock, on a worker, including cancellation and an abandoned client.
        drop(rejected);
        SNAPSHOT_JOB_ADMISSION[index].release();
    }
}
