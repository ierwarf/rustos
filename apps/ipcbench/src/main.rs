//! Ring3 syscall, scheduler, and IPC round-trip cost harness.
//!
//! - **Owner:** this app owns only measurement and reporting; every probe uses
//!   an already-published ABI and no privileged or bench-only kernel path.
//! - **Boundary:** results are advisory measurements, never a policy input.
//! - **Lifecycle:** calibrate the TSC, run each probe warm, report, exit.
//! - **Failure:** a probe that cannot run reports `skip` and the harness
//!   continues, so one unavailable service cannot void the whole run.
//! - **Forbidden:** no incidental formatting or logging inside a measured
//!   interval; samples are collected into preallocated storage. Lifecycle
//!   probes deliberately include allocations performed by the operation they
//!   name (thread creation, fork, exec, and anonymous mapping).

use std::arch::asm;
use std::ffi::CString;
use std::mem::size_of;
use std::ptr;
use std::thread;
use std::time::Duration;

use rustos_user_abi::syscall::{SYS_RUSTOS_DEBUG_PRINT, SYS_RUSTOS_PHASE_PROFILE_DRAIN};

mod cow_probe;
mod ipc_probes;
mod scheduling_context_probe;
use cow_probe::probe_fork_cow_private_write;

const SYS_LINUX_GETPID: u64 = 39;
/// Offloaded to `syscalld`, so one call is one complete cross-process IPC
/// round trip over the ordinary application path. The gap against
/// `SYS_LINUX_GETPID`, which the kernel answers locally, is that round trip.
const SYS_LINUX_GETUID: u64 = 102;
const SYS_LINUX_SCHED_YIELD: u64 = 24;
const SYS_LINUX_CLOCK_GETTIME: u64 = 228;
const CLOCK_MONOTONIC: u64 = 1;

/// Warm the branch predictors, the handle-table cache lines, and the receiver
/// state machine before sampling. A cold first call is a boot-path cost, not
/// the steady-state cost this harness reports.
const WARMUP: usize = 2_000;
const SYSCALL_ITERS: usize = 50_000;
const IPC_ITERS: usize = 20_000;
const LIFECYCLE_ITERS: usize = 128;
const LIFECYCLE_WARMUP: usize = 8;
const EXEC_REPLACE_ITERS: usize = 32;
const EXEC_REPLACE_WARMUP: usize = 2;

/// `--isolate-probe` wants every phase-profile sample charged while this
/// probe runs to belong to it alone. The rest of the session catalog starts
/// at roughly the same wall-clock moment this program does, so without a
/// settle the one-time startup burst of every other session program lands
/// inside the measured window just from launch order, not from anything the
/// isolated probe did.
const ISOLATE_SETTLE: Duration = Duration::from_secs(15);

// ---------------------------------------------------------------- syscalls

#[inline(always)]
unsafe fn syscall0(n: u64) -> i64 {
    let ret: i64;
    unsafe {
        asm!(
            "syscall",
            inlateout("rax") n => ret,
            lateout("rcx") _,
            lateout("r11") _,
            options(nostack),
        );
    }
    ret
}

#[inline(always)]
unsafe fn syscall1(n: u64, a0: u64) -> i64 {
    let ret: i64;
    unsafe {
        asm!(
            "syscall",
            inlateout("rax") n => ret,
            in("rdi") a0,
            lateout("rcx") _,
            lateout("r11") _,
            options(nostack),
        );
    }
    ret
}

#[inline(always)]
unsafe fn syscall2(n: u64, a0: u64, a1: u64) -> i64 {
    let ret: i64;
    unsafe {
        asm!(
            "syscall",
            inlateout("rax") n => ret,
            in("rdi") a0,
            in("rsi") a1,
            lateout("rcx") _,
            lateout("r11") _,
            options(nostack),
        );
    }
    ret
}

#[inline(always)]
unsafe fn syscall3(n: u64, a0: u64, a1: u64, a2: u64) -> i64 {
    let ret: i64;
    unsafe {
        asm!(
            "syscall",
            inlateout("rax") n => ret,
            in("rdi") a0,
            in("rsi") a1,
            in("rdx") a2,
            lateout("rcx") _,
            lateout("r11") _,
            options(nostack),
        );
    }
    ret
}

#[inline(always)]
unsafe fn syscall4(n: u64, a0: u64, a1: u64, a2: u64, a3: u64) -> i64 {
    let ret: i64;
    unsafe {
        asm!(
            "syscall",
            inlateout("rax") n => ret,
            in("rdi") a0,
            in("rsi") a1,
            in("rdx") a2,
            in("r10") a3,
            lateout("rcx") _,
            lateout("r11") _,
            options(nostack),
        );
    }
    ret
}

#[inline(always)]
unsafe fn syscall5(n: u64, a0: u64, a1: u64, a2: u64, a3: u64, a4: u64) -> i64 {
    let ret: i64;
    unsafe {
        asm!(
            "syscall",
            inlateout("rax") n => ret,
            in("rdi") a0,
            in("rsi") a1,
            in("rdx") a2,
            in("r10") a3,
            in("r8") a4,
            lateout("rcx") _,
            lateout("r11") _,
            options(nostack),
        );
    }
    ret
}

#[inline(always)]
unsafe fn syscall6(n: u64, a0: u64, a1: u64, a2: u64, a3: u64, a4: u64, a5: u64) -> i64 {
    let ret: i64;
    unsafe {
        asm!(
            "syscall",
            inlateout("rax") n => ret,
            in("rdi") a0,
            in("rsi") a1,
            in("rdx") a2,
            in("r10") a3,
            in("r8") a4,
            in("r9") a5,
            lateout("rcx") _,
            lateout("r11") _,
            options(nostack),
        );
    }
    ret
}

/// See `SYS_RUSTOS_PHASE_PROFILE_DRAIN`: flushes the kernel's
/// `ipc-call-phase-*`/`usermem-phase-*` counters immediately rather than
/// waiting for their ordinary once-per-second housekeeping drain.
fn force_drain_phase_profiles() {
    unsafe {
        syscall0(SYS_RUSTOS_PHASE_PROFILE_DRAIN);
    }
}

fn debug_line(message: &str) {
    let mut line = Vec::with_capacity(message.len() + 1);
    line.extend_from_slice(message.as_bytes());
    line.push(b'\n');
    unsafe {
        syscall2(
            SYS_RUSTOS_DEBUG_PRINT,
            line.as_ptr() as u64,
            line.len() as u64,
        );
    }
}

// -------------------------------------------------------------------- TSC

/// `lfence` before the read keeps earlier work from drifting past the sample.
/// The measured regions are all syscalls, which serialize on their own, so a
/// full `cpuid` barrier would only add its own cost to every sample.
#[inline(always)]
fn tsc() -> u64 {
    let lo: u32;
    let hi: u32;
    unsafe {
        asm!(
            "lfence",
            "rdtsc",
            out("eax") lo,
            out("edx") hi,
            options(nomem, nostack),
        );
    }
    ((hi as u64) << 32) | (lo as u64)
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct KTimespec {
    tv_sec: i64,
    tv_nsec: i64,
}

fn monotonic_nanos() -> u64 {
    let mut ts = KTimespec::default();
    let status = unsafe {
        syscall2(
            SYS_LINUX_CLOCK_GETTIME,
            CLOCK_MONOTONIC,
            (&mut ts as *mut KTimespec) as u64,
        )
    };
    if status < 0 {
        return 0;
    }
    let seconds = u64::try_from(ts.tv_sec).unwrap_or(0);
    let nanos = u64::try_from(ts.tv_nsec).unwrap_or(0);
    seconds.saturating_mul(1_000_000_000).saturating_add(nanos)
}

/// Returns TSC kHz, or 0 when the monotonic clock is unavailable. A zero
/// result degrades the report to cycles only rather than inventing a scale.
fn calibrate_tsc_khz() -> u64 {
    let t0 = monotonic_nanos();
    if t0 == 0 {
        return 0;
    }
    let c0 = tsc();
    thread::sleep(Duration::from_millis(300));
    let c1 = tsc();
    let t1 = monotonic_nanos();
    if t1 <= t0 || c1 <= c0 {
        return 0;
    }
    let cycles = c1 - c0;
    let nanos = t1 - t0;
    // cycles/ns * 1e6 = kHz
    cycles.saturating_mul(1_000_000) / nanos
}

// ------------------------------------------------------------------ stats

struct Stats {
    iters: usize,
    min: u64,
    p50: u64,
    p90: u64,
    p99: u64,
    max: u64,
    mean: u64,
}

fn summarize(samples: &mut [u64]) -> Stats {
    samples.sort_unstable();
    let len = samples.len();
    let sum: u128 = samples.iter().map(|value| u128::from(*value)).sum();
    let pick = |q: usize| samples[(len - 1).min(len * q / 100)];
    Stats {
        iters: len,
        min: samples[0],
        p50: pick(50),
        p90: pick(90),
        p99: pick(99),
        max: samples[len - 1],
        mean: (sum / len as u128) as u64,
    }
}

fn report(name: &str, stats: &Stats, tsc_khz: u64) {
    let ns = |cycles: u64| -> u64 {
        if tsc_khz == 0 {
            0
        } else {
            cycles.saturating_mul(1_000_000) / tsc_khz
        }
    };
    debug_line(&format!(
        "ipcbench: result name={name} iters={} min={} p50={} p90={} p99={} max={} mean={} \
         min_ns={} p50_ns={} mean_ns={}",
        stats.iters,
        stats.min,
        stats.p50,
        stats.p90,
        stats.p99,
        stats.max,
        stats.mean,
        ns(stats.min),
        ns(stats.p50),
        ns(stats.mean),
    ));
}

fn skip(name: &str, reason: &str) {
    debug_line(&format!("ipcbench: skip name={name} reason={reason}"));
}

/// Runs `op` `iters` times, sampling each call. `op` returns false to abort the
/// probe. Sample storage is allocated up front so no measured interval can
/// contain an allocation.
fn measure<F>(iters: usize, warmup: usize, mut op: F) -> Option<Vec<u64>>
where
    F: FnMut() -> bool,
{
    for _ in 0..warmup {
        if !op() {
            return None;
        }
    }
    let mut samples = Vec::with_capacity(iters);
    for _ in 0..iters {
        let start = tsc();
        let ok = op();
        let end = tsc();
        if !ok {
            return None;
        }
        samples.push(end.wrapping_sub(start));
    }
    Some(samples)
}

/// Variant for probes whose authoritative timestamp is produced by the child
/// at a lifecycle boundary.  The parent still owns all setup and cleanup, but
/// recording the child's stamped interval prevents scheduler delay after the
/// observed transition from being charged as though it occurred before it.
fn measure_stamped<F>(iters: usize, warmup: usize, mut op: F) -> Option<Vec<u64>>
where
    F: FnMut() -> Option<u64>,
{
    for _ in 0..warmup {
        op()?;
    }
    let mut samples = Vec::with_capacity(iters);
    for _ in 0..iters {
        samples.push(op()?);
    }
    Some(samples)
}

// ------------------------------------------------------------------ probes

/// The cost of the measurement itself. Every other number in the report
/// includes this, so the reader needs it to interpret the small ones.
fn probe_tsc_overhead(tsc_khz: u64) {
    let mut samples = Vec::with_capacity(SYSCALL_ITERS);
    for _ in 0..WARMUP {
        let _ = tsc();
    }
    for _ in 0..SYSCALL_ITERS {
        let start = tsc();
        let end = tsc();
        samples.push(end.wrapping_sub(start));
    }
    report("tsc_overhead", &summarize(&mut samples), tsc_khz);
}

fn probe_null_syscall(tsc_khz: u64) {
    let result = measure(SYSCALL_ITERS, WARMUP, || unsafe {
        syscall0(SYS_LINUX_GETPID) >= 0
    });
    match result {
        Some(mut samples) => report("null_syscall_getpid", &summarize(&mut samples), tsc_khz),
        None => skip("null_syscall_getpid", "getpid-failed"),
    }
}

/// `cpuid` is unconditionally intercepted by the hypervisor, so this measures
/// one VM exit and nothing else. Every other probe runs in the same guest, so
/// this is the constant that says whether an unexplained cost *could* be
/// hypervisor exits and how many would be needed to account for it.
fn probe_vmexit_cpuid(tsc_khz: u64) {
    let result = measure(SYSCALL_ITERS, WARMUP, || {
        // The intrinsic is used rather than raw `asm!` because LLVM reserves
        // `rbx`. Leaf 0 is architecturally available at any privilege level and
        // only reads vendor/leaf identification.
        let leaf = core::arch::x86_64::__cpuid(0);
        core::hint::black_box(leaf.eax);
        true
    });
    match result {
        Some(mut samples) => report("vmexit_cpuid", &summarize(&mut samples), tsc_khz),
        None => skip("vmexit_cpuid", "cpuid-failed"),
    }
}

fn probe_sched_yield(tsc_khz: u64) {
    let result = measure(SYSCALL_ITERS, WARMUP, || unsafe {
        syscall0(SYS_LINUX_SCHED_YIELD) >= 0
    });
    match result {
        Some(mut samples) => report("sched_yield", &summarize(&mut samples), tsc_khz),
        None => skip("sched_yield", "sched-yield-failed"),
    }
}

// ------------------------------------------------------- lifecycle / memory

fn child_exited_successfully(status: i32) -> bool {
    libc::WIFEXITED(status) && libc::WEXITSTATUS(status) == 0
}

/// Invoke the libc fork contract, including its clone-style child-tid fields.
/// This keeps the benchmark on the ABI used by ordinary dynamically linked
/// applications rather than a narrower raw `SYS_fork` special case.
unsafe fn linux_fork_syscall() -> libc::pid_t {
    unsafe { libc::fork() }
}

unsafe fn wait_child(pid: libc::pid_t, status: &mut i32) -> Result<(), i32> {
    loop {
        if unsafe { libc::waitpid(pid, status, 0) } == pid {
            return Ok(());
        }
        let errno = std::io::Error::last_os_error().raw_os_error().unwrap_or(-1);
        if errno != libc::EINTR {
            return Err(errno);
        }
    }
}

/// One explicit child-to-parent lifecycle result backing. This is deliberately
/// a memfd rather than anonymous memory: an exec replacement cannot retain an
/// old mapping, and a positional read/write keeps the acknowledgement tied to
/// the same published backing across both address-space generations.
struct LifecycleStampMapping {
    fd: libc::c_int,
}

impl LifecycleStampMapping {
    unsafe fn new() -> Result<Self, i32> {
        let name =
            CString::new("ipcbench-lifecycle").expect("fixed lifecycle memfd name contains no NUL");
        // No MFD_CLOEXEC: the exec replacement child remaps this exact fd.
        let fd = unsafe { libc::memfd_create(name.as_ptr(), 0) };
        if fd < 0 {
            return Err(last_errno());
        }
        let length = size_of::<[u64; 2]>();
        if unsafe { libc::ftruncate(fd, length as libc::off_t) } != 0 {
            let errno = last_errno();
            unsafe { libc::close(fd) };
            return Err(errno);
        }
        Ok(Self { fd })
    }

    fn read(&self) -> Result<(u64, u64), i32> {
        let mut bytes = [0_u8; size_of::<[u64; 2]>()];
        let read = unsafe {
            libc::pread(
                self.fd,
                bytes.as_mut_ptr().cast::<libc::c_void>(),
                bytes.len(),
                0,
            )
        };
        if read != bytes.len() as isize {
            return Err(last_errno());
        }
        let before = u64::from_le_bytes(bytes[..8].try_into().expect("fixed stamp width"));
        let after = u64::from_le_bytes(bytes[8..].try_into().expect("fixed stamp width"));
        Ok((before, after))
    }

    fn rewind(&self) -> Result<(), i32> {
        (unsafe { libc::lseek(self.fd, 0, libc::SEEK_SET) } == 0)
            .then_some(())
            .ok_or_else(last_errno)
    }
}

impl Drop for LifecycleStampMapping {
    fn drop(&mut self) {
        unsafe {
            libc::close(self.fd);
        }
    }
}

fn last_errno() -> i32 {
    std::io::Error::last_os_error().raw_os_error().unwrap_or(-1)
}

unsafe fn write_lifecycle_stamp(fd: libc::c_int, before: u64, after: u64) -> Result<(), i32> {
    let mut bytes = [0_u8; size_of::<[u64; 2]>()];
    bytes[..8].copy_from_slice(&before.to_le_bytes());
    bytes[8..].copy_from_slice(&after.to_le_bytes());
    // RustOS publishes regular `write` for local memfd descriptions; the
    // parent reads position-independently through `pread64`, so the child's
    // inherited description offset cannot alter the observation boundary.
    let written = unsafe { libc::write(fd, bytes.as_ptr().cast::<libc::c_void>(), bytes.len()) };
    (written == bytes.len() as isize)
        .then_some(())
        .ok_or_else(last_errno)
}

/// Measures the complete published Linux lifecycle rather than a private
/// kernel helper: procd authorizes fork, the child retires, wait observes the
/// exact exit status, and the process becomes eligible for the kernel reaper.
fn probe_fork_exit_wait(tsc_khz: u64) {
    let mut failure_errno = 0;
    let mut failure_stage = "unknown";
    let mut completed = 0_usize;
    let result = measure(LIFECYCLE_ITERS, LIFECYCLE_WARMUP, || unsafe {
        let pid = linux_fork_syscall();
        if pid == 0 {
            libc::_exit(0);
        }
        if pid < 0 {
            failure_stage = "fork";
            failure_errno = std::io::Error::last_os_error().raw_os_error().unwrap_or(-1);
            return false;
        }
        let mut status = 0;
        if let Err(errno) = wait_child(pid, &mut status) {
            failure_stage = "wait";
            failure_errno = errno;
            return false;
        }
        if !child_exited_successfully(status) {
            failure_stage = "child-status";
            failure_errno = -2;
            return false;
        }
        completed += 1;
        true
    });
    match result {
        Some(mut samples) => report("fork_exit_wait", &summarize(&mut samples), tsc_khz),
        None => skip(
            "fork_exit_wait",
            &format!("{failure_stage}-failed-errno-{failure_errno}-after-{completed}"),
        ),
    }
}

/// Includes loaderd/vfsd/procd policy and the kernel's staged exec ownership
/// transfer. The child mode exits before reading the benchmark probe contract,
/// preventing recursive benchmark execution after replacement.
fn probe_fork_exec_exit_wait(tsc_khz: u64) {
    let path = CString::new("apps/ipcbench/ipcbench.elf").expect("fixed exec path");
    let child_arg = CString::new("--lifecycle-child").expect("fixed child argument");
    let argv = [path.as_ptr(), child_arg.as_ptr(), ptr::null()];
    let envp = [ptr::null()];
    let mut failure_stage = "unknown";
    let mut failure_detail = 0;
    let mut completed = 0_usize;
    let result = measure(32, 2, || unsafe {
        let pid = linux_fork_syscall();
        if pid == 0 {
            libc::execve(path.as_ptr(), argv.as_ptr(), envp.as_ptr());
            let errno = std::io::Error::last_os_error()
                .raw_os_error()
                .unwrap_or(127);
            libc::_exit(errno.clamp(1, 125));
        }
        if pid < 0 {
            failure_stage = "fork";
            failure_detail = std::io::Error::last_os_error().raw_os_error().unwrap_or(-1);
            return false;
        }
        let mut status = 0;
        if let Err(errno) = wait_child(pid, &mut status) {
            failure_stage = "wait";
            failure_detail = errno;
            return false;
        }
        if !child_exited_successfully(status) {
            failure_stage = "child-status";
            failure_detail = status;
            return false;
        }
        completed += 1;
        true
    });
    match result {
        Some(mut samples) => report("fork_exec_exit_wait", &summarize(&mut samples), tsc_khz),
        None => skip(
            "fork_exec_exit_wait",
            &format!("{failure_stage}-{failure_detail}-after-{completed}"),
        ),
    }
}

fn probe_thread_clone_exit_join(tsc_khz: u64) {
    let mut completed = 0_usize;
    let mut failure = "unknown";
    let result = measure(64, 4, || {
        let handle = match thread::Builder::new().spawn(|| 0_u64) {
            Ok(handle) => handle,
            Err(_) => {
                failure = "spawn";
                return false;
            }
        };
        if handle.join().is_err() {
            failure = "join";
            return false;
        }
        completed += 1;
        true
    });
    match result {
        Some(mut samples) => report("thread_clone_exit_join", &summarize(&mut samples), tsc_khz),
        None => skip(
            "thread_clone_exit_join",
            &format!("{failure}-failed-after-{completed}"),
        ),
    }
}

/// Measures from the parent publishing a fork request until the child has
/// executed its first ordinary user instruction and acknowledged it through a
/// shared memfd. The final wait keeps every iteration self-contained, but is
/// intentionally outside the reported child-first-turn interval.
fn probe_spawn_activation_to_first_turn(tsc_khz: u64) {
    let mapping = match unsafe { LifecycleStampMapping::new() } {
        Ok(mapping) => mapping,
        Err(errno) => {
            skip(
                "spawn_activation_to_first_turn",
                &format!("memfd-errno-{errno}"),
            );
            return;
        }
    };
    let mut failure = "unknown";
    let result = measure_stamped(LIFECYCLE_ITERS, LIFECYCLE_WARMUP, || unsafe {
        if mapping.rewind().is_err() {
            failure = "rewind-stamp";
            return None;
        }
        let start = tsc();
        let pid = linux_fork_syscall();
        if pid == 0 {
            let after = tsc();
            let status = write_lifecycle_stamp(mapping.fd, 0, after)
                .map(|_| 0)
                .unwrap_or(125);
            libc::_exit(status);
        }
        if pid < 0 {
            failure = "fork";
            return None;
        }
        let mut status = 0;
        if let Err(_) = wait_child(pid, &mut status) {
            failure = "wait";
            return None;
        }
        let (_, after) = match mapping.read() {
            Ok(stamp) => stamp,
            Err(_) => {
                failure = "read-stamp";
                return None;
            }
        };
        if !child_exited_successfully(status) || after < start {
            failure = "child-status-or-tsc-order";
            return None;
        }
        Some(after - start)
    });
    match result {
        Some(mut samples) => report(
            "spawn_activation_to_first_turn",
            &summarize(&mut samples),
            tsc_khz,
        ),
        None => skip(
            "spawn_activation_to_first_turn",
            &format!("{failure}-failed"),
        ),
    }
}

/// Measures the published child terminal path from the child's final user
/// instruction through exit retirement, wait status publication, and the
/// parent-triggered reap opportunity.  No private reaper syscall is used.
fn probe_exit_retire_to_reap(tsc_khz: u64) {
    let mapping = match unsafe { LifecycleStampMapping::new() } {
        Ok(mapping) => mapping,
        Err(errno) => {
            skip("exit_retire_to_reap", &format!("memfd-errno-{errno}"));
            return;
        }
    };
    let mut failure = "unknown";
    let result = measure_stamped(LIFECYCLE_ITERS, LIFECYCLE_WARMUP, || unsafe {
        if mapping.rewind().is_err() {
            failure = "rewind-stamp";
            return None;
        }
        let pid = linux_fork_syscall();
        if pid == 0 {
            let exit_started = tsc();
            let status = write_lifecycle_stamp(mapping.fd, 0, exit_started)
                .map(|_| 0)
                .unwrap_or(125);
            libc::_exit(status);
        }
        if pid < 0 {
            failure = "fork";
            return None;
        }
        let mut status = 0;
        if let Err(_) = wait_child(pid, &mut status) {
            failure = "wait";
            return None;
        }
        let reaped_at = tsc();
        let (_, exit_started) = match mapping.read() {
            Ok(stamp) => stamp,
            Err(_) => {
                failure = "read-stamp";
                return None;
            }
        };
        if !child_exited_successfully(status) || exit_started == 0 || reaped_at < exit_started {
            failure = "child-status-or-tsc-order";
            return None;
        }
        Some(reaped_at - exit_started)
    });
    match result {
        Some(mut samples) => report("exit_retire_to_reap", &summarize(&mut samples), tsc_khz),
        None => skip("exit_retire_to_reap", &format!("{failure}-failed")),
    }
}

/// A separate exec probe from `fork_exec_exit_wait`: the latter measures the
/// complete child lifecycle, while this one timestamps exactly the pre-exec
/// handoff and first instruction of the replacement image. The replacement
/// writes the inherited pre-created memfd only after it has entered the new
/// address space, so its acknowledgement cannot come from the pre-exec image.
fn probe_exec_replace_single_thread(tsc_khz: u64) {
    let mapping = match unsafe { LifecycleStampMapping::new() } {
        Ok(mapping) => mapping,
        Err(errno) => {
            skip(
                "exec_replace_single_thread",
                &format!("memfd-errno-{errno}"),
            );
            return;
        }
    };
    let path = CString::new("apps/ipcbench/ipcbench.elf").expect("fixed exec path");
    let child_mode = CString::new("--exec-replace-child").expect("fixed child mode");
    let envp = [ptr::null()];
    let mut failure = "unknown";
    let result = measure_stamped(EXEC_REPLACE_ITERS, EXEC_REPLACE_WARMUP, || unsafe {
        if mapping.rewind().is_err() {
            failure = "rewind-stamp";
            return None;
        }
        let pid = linux_fork_syscall();
        if pid == 0 {
            let before = tsc();
            let fd = CString::new(mapping.fd.to_string()).expect("fd decimal has no NUL");
            let before_arg = CString::new(before.to_string()).expect("tsc decimal has no NUL");
            let argv = [
                path.as_ptr(),
                child_mode.as_ptr(),
                fd.as_ptr(),
                before_arg.as_ptr(),
                ptr::null(),
            ];
            libc::execve(path.as_ptr(), argv.as_ptr(), envp.as_ptr());
            libc::_exit(last_errno().clamp(1, 125));
        }
        if pid < 0 {
            failure = "fork";
            return None;
        }
        let mut status = 0;
        if let Err(_) = wait_child(pid, &mut status) {
            failure = "wait";
            return None;
        }
        let (before, after) = match mapping.read() {
            Ok(stamp) => stamp,
            Err(_) => {
                failure = "read-stamp";
                return None;
            }
        };
        if !child_exited_successfully(status) || before == 0 || after < before {
            failure = "child-status-or-tsc-order";
            return None;
        }
        Some(after - before)
    });
    match result {
        Some(mut samples) => report(
            "exec_replace_single_thread",
            &summarize(&mut samples),
            tsc_khz,
        ),
        None => skip("exec_replace_single_thread", &format!("{failure}-failed")),
    }
}

#[derive(Clone, Copy)]
enum MappingTouch {
    None,
    Read,
    /// One byte at offset zero. On a demand-paged mapping this faults in
    /// exactly one page no matter how many were reserved, so a multi-page
    /// probe using this measures reservation and teardown, not fault cost.
    Write,
    /// One byte in every 4 KiB page, so the whole reservation is resident
    /// before it is torn down. This is the only touch that makes an unmap
    /// walk a fully present range.
    WriteEveryPage,
}

fn measure_anonymous_mapping(
    name: &str,
    pages: usize,
    touch: MappingTouch,
    iters: usize,
    tsc_khz: u64,
) {
    let Some(len) = pages.checked_mul(4096) else {
        skip(name, "mapping-length-overflow");
        return;
    };
    let result = measure(iters, 8, || unsafe {
        let mapping = libc::mmap(
            ptr::null_mut(),
            len,
            libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_PRIVATE | libc::MAP_ANONYMOUS,
            -1,
            0,
        );
        if mapping == libc::MAP_FAILED {
            return false;
        }
        match touch {
            MappingTouch::None => {}
            MappingTouch::Read => {
                ptr::read_volatile(mapping.cast::<u8>());
            }
            MappingTouch::Write => {
                ptr::write_volatile(mapping.cast::<u8>(), 0xa5);
            }
            MappingTouch::WriteEveryPage => {
                let base = mapping.cast::<u8>();
                for page in 0..pages {
                    ptr::write_volatile(base.add(page * 4096), 0xa5);
                }
            }
        }
        libc::munmap(mapping, len) == 0
    });
    match result {
        Some(mut samples) => report(name, &summarize(&mut samples), tsc_khz),
        None => skip(name, "mmap-touch-or-munmap-failed"),
    }
}

/// Faults in a multi-page anonymous mapping, unmaps its middle, then touches
/// what `munmap(2)` says must survive on either side.
///
/// This is a correctness probe wearing a benchmark's clothes, and it is the
/// only one that runs the ring0/pagerd map agreement end to end on real
/// hardware. Ring0's VMA table preserves the left and right remainders of an
/// interior unmap; pagerd used to delete every overlapping region, so the
/// touch after the unmap found no pager region, took a fault nothing could
/// resolve, and killed this process. A unit test on either side alone cannot
/// see that, because each side is self-consistent.
///
/// The measured interval is deliberately the whole cycle: reserve, fault the
/// range in, split it, and re-fault both remainders. A regression that breaks
/// agreement does not show up as a slow number here - it shows up as a `skip`
/// line or a dead process, which is exactly the signal wanted.
fn measure_split_mapping_survives(name: &str, pages: usize, iters: usize, tsc_khz: u64) {
    // Three whole pages minimum: one to keep on each side of the hole, and at
    // least one to remove. Fewer cannot produce an interior split at all.
    if pages < 3 {
        skip(name, "split-needs-at-least-three-pages");
        return;
    }
    let Some(len) = pages.checked_mul(4096) else {
        skip(name, "mapping-length-overflow");
        return;
    };
    let hole_page = pages / 2;
    let result = measure(iters, 4, || unsafe {
        let mapping = libc::mmap(
            ptr::null_mut(),
            len,
            libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_PRIVATE | libc::MAP_ANONYMOUS,
            -1,
            0,
        );
        if mapping == libc::MAP_FAILED {
            return false;
        }
        let base = mapping.cast::<u8>();
        for page in 0..pages {
            ptr::write_volatile(base.add(page * 4096), 0xa5);
        }
        // Unmap one interior page. Both remainders must stay mapped.
        if libc::munmap(base.add(hole_page * 4096).cast(), 4096) != 0 {
            let _ = libc::munmap(mapping, len);
            return false;
        }
        // Re-touch both remainders. Each already has a resident page, so this
        // takes no fault; a write that faults here means ring0 dropped a
        // mapping it was supposed to keep.
        for page in (0..pages).filter(|page| *page != hole_page) {
            ptr::write_volatile(base.add(page * 4096), 0x5a);
            if ptr::read_volatile(base.add(page * 4096)) != 0x5a {
                return false;
            }
        }
        // Free the remainders as two separate ranges, which is what ring0 and
        // pagerd must both now be holding.
        let head = hole_page * 4096;
        let tail_start = (hole_page + 1) * 4096;
        let head_freed = head == 0 || libc::munmap(mapping, head) == 0;
        let tail_len = len - tail_start;
        let tail_freed = tail_len == 0 || libc::munmap(base.add(tail_start).cast(), tail_len) == 0;
        head_freed && tail_freed
    });
    match result {
        Some(mut samples) => report(name, &summarize(&mut samples), tsc_khz),
        None => skip(name, "split-remainder-touch-or-unmap-failed"),
    }
}

// ------------------------------------------------------------ probe filter

/// Every `ipc-call-phase-*`/`usermem-phase-*` counter this harness's probes
/// charge is process-wide and drains on a wall-clock window that has nothing
/// to do with probe boundaries, so a phase total charged by more than one
/// probe in the same boot cannot be divided into either probe's round-trip
/// count. `cargo xtask bench --isolate-probe <name>` writes this contract to
/// the private per-run KVM disk so exactly one probe runs, and every counter
/// in the boot belongs to it. Reading it directly, the same way `uiserver`
/// reads its own acceptance contract, keeps this a two-file change: no
/// service mediates it.
const IPCBENCH_PROBE_CONTRACT_PATH: &str = "/system/registry/system/ipcbench-probe-v1.env";
const IPCBENCH_PROBE_CONTRACT_MAX_BYTES: usize = 256;

fn probe_filter() -> Option<String> {
    let contents = runtime_control::read_bounded_config_snapshot(
        IPCBENCH_PROBE_CONTRACT_PATH,
        IPCBENCH_PROBE_CONTRACT_MAX_BYTES,
    )
    .ok()?;
    parse_ipcbench_probe_contract(&contents)
}

fn parse_ipcbench_probe_contract(contents: &str) -> Option<String> {
    let mut contract = false;
    let mut probe = None;
    for line in contents.lines() {
        if !contract && line == "contract=rustos-ipcbench-probe-v1" {
            contract = true;
            continue;
        }
        match line.strip_prefix("probe=") {
            Some(name) if probe.is_none() && !name.is_empty() => probe = Some(name.to_owned()),
            _ => return None,
        }
    }
    probe.filter(|_| contract)
}

fn run_all_probes(tsc_khz: u64) {
    probe_tsc_overhead(tsc_khz);
    probe_null_syscall(tsc_khz);
    probe_vmexit_cpuid(tsc_khz);
    probe_sched_yield(tsc_khz);
    ipc_probes::probe_ipc_mechanism_only(tsc_khz);
    ipc_probes::probe_ipc_intra_process(tsc_khz);
    ipc_probes::probe_ipc_intra_process_reply_recv(tsc_khz);
    ipc_probes::probe_syscall_offload(tsc_khz);
    // This is a capability probe, not just a timing sample: a failure means
    // fork returned a non-private mapping or COW teardown lost ownership.
    probe_fork_cow_private_write(tsc_khz, 8, 1);
    // The mapping probes are otherwise isolate-only, because their numbers are
    // measurements. This one is here for its *outcome*: it is the only
    // end-to-end check that ring0 and pagerd still agree about what an
    // interior `munmap` leaves behind, and a disagreement kills the process
    // rather than producing a slow number. Three pages and 64 iterations, so
    // the default lane pays a few hundred microseconds for it.
    measure_split_mapping_survives("mmap_split_survives_3_pages", 3, 64, tsc_khz);
}

/// A closed vocabulary rather than a dispatch table keyed by the report name:
/// an unrecognized filter must not silently fall back to running everything,
/// which would defeat the isolation this exists for.
fn run_single_probe(name: &str, tsc_khz: u64) {
    match name {
        "tsc_overhead" => probe_tsc_overhead(tsc_khz),
        "null_syscall_getpid" => probe_null_syscall(tsc_khz),
        "vmexit_cpuid" => probe_vmexit_cpuid(tsc_khz),
        "sched_yield" => probe_sched_yield(tsc_khz),
        "ipc_try_recv_empty" => ipc_probes::probe_ipc_mechanism_only(tsc_khz),
        "ipc_rt_intra_process" => ipc_probes::probe_ipc_intra_process(tsc_khz),
        "ipc_rt_intra_process_reply_recv" => {
            ipc_probes::probe_ipc_intra_process_reply_recv(tsc_khz)
        }
        "ipc_rt_cross_process_syscalld_getuid" => ipc_probes::probe_syscall_offload(tsc_khz),
        "fork_exit_wait" => probe_fork_exit_wait(tsc_khz),
        "fork_cow_private_write" => probe_fork_cow_private_write(tsc_khz, 64, 4),
        "fork_exec_exit_wait" => probe_fork_exec_exit_wait(tsc_khz),
        "thread_clone_exit_join" => probe_thread_clone_exit_join(tsc_khz),
        "exec_replace_single_thread" => probe_exec_replace_single_thread(tsc_khz),
        "spawn_activation_to_first_turn" => probe_spawn_activation_to_first_turn(tsc_khz),
        "exit_retire_to_reap" => probe_exit_retire_to_reap(tsc_khz),
        "anon_mmap_reserve" => {
            measure_anonymous_mapping(name, 1, MappingTouch::None, 2_000, tsc_khz)
        }
        "anon_first_read_fault" => {
            measure_anonymous_mapping(name, 1, MappingTouch::Read, 2_000, tsc_khz)
        }
        "anon_first_write_fault" | "mmap_unmap_1" => {
            measure_anonymous_mapping(name, 1, MappingTouch::Write, 2_000, tsc_khz)
        }
        "mmap_unmap_64" => measure_anonymous_mapping(name, 64, MappingTouch::Write, 256, tsc_khz),
        // Reserve 1024 pages, fault exactly one, then unmap. This is the
        // reservation-and-teardown cost of a large mapping: the unmap walks a
        // range holding one resident page. It is *not* a fault-path or a
        // fully-present-unmap measurement, despite the page count in its name.
        "mmap_unmap_1024_pages" => {
            measure_anonymous_mapping(name, 1024, MappingTouch::Write, 32, tsc_khz)
        }
        // The same reservation with every page faulted in before teardown.
        // This is the probe that actually exercises the demand-fault path and
        // the fully-present unmap, so it is the only one whose numbers may be
        // used to justify a change to either. Iterations are lower because one
        // iteration costs 1024 fault round trips, not one.
        "mmap_unmap_1024_faulted_pages" => {
            measure_anonymous_mapping(name, 1024, MappingTouch::WriteEveryPage, 16, tsc_khz)
        }
        // Interior unmap, then touch what must survive on either side. This is
        // the only probe that runs ring0/pagerd map agreement end to end; a
        // `skip` line from it means the two replicas disagree about the map.
        "mmap_split_survives_3_pages" => measure_split_mapping_survives(name, 3, 512, tsc_khz),
        "mmap_split_survives_64_pages" => measure_split_mapping_survives(name, 64, 64, tsc_khz),
        "ipc_nested_passive_server" => {
            scheduling_context_probe::probe_nested_passive_server(tsc_khz)
        }
        "scheduling_budget_exhaust_refill" => {
            scheduling_context_probe::probe_budget_exhaust_refill(tsc_khz)
        }
        other => skip(other, "unrecognized-probe-filter"),
    }
}

/// Keep the hardware-only CPUID anchor in every isolated report. It performs
/// no RustOS syscall and therefore cannot contaminate kernel phase or frame
/// counters, while making a same-session target-only A-B-A comparison capable
/// of rejecting host clock drift instead of trusting equal-looking TSC rates.
fn run_isolated_probe(name: &str, tsc_khz: u64) {
    if name != "vmexit_cpuid" {
        probe_vmexit_cpuid(tsc_khz);
    }
    run_single_probe(name, tsc_khz);
}

#[cfg(test)]
mod probe_filter_tests {
    use super::parse_ipcbench_probe_contract;

    #[test]
    fn exact_contract_with_a_probe_name_parses() {
        assert_eq!(
            parse_ipcbench_probe_contract(
                "contract=rustos-ipcbench-probe-v1\nprobe=ipc_rt_intra_process\n"
            ),
            Some("ipc_rt_intra_process".to_owned())
        );
    }

    #[test]
    fn a_missing_contract_line_is_rejected() {
        assert_eq!(
            parse_ipcbench_probe_contract("probe=ipc_rt_intra_process\n"),
            None
        );
    }

    #[test]
    fn an_empty_probe_name_is_rejected() {
        assert_eq!(
            parse_ipcbench_probe_contract("contract=rustos-ipcbench-probe-v1\nprobe=\n"),
            None
        );
    }

    #[test]
    fn a_duplicated_probe_line_is_rejected() {
        assert_eq!(
            parse_ipcbench_probe_contract("contract=rustos-ipcbench-probe-v1\nprobe=a\nprobe=b\n"),
            None
        );
    }

    #[test]
    fn an_unrecognized_line_is_rejected() {
        assert_eq!(
            parse_ipcbench_probe_contract("contract=rustos-ipcbench-probe-v1\nprobe=a\nextra=1\n"),
            None
        );
    }
}

fn run_exec_replace_child(fd_arg: &str, before_arg: &str) -> ! {
    let fd = match fd_arg.parse::<libc::c_int>() {
        Ok(fd) if fd >= 0 => fd,
        _ => unsafe { libc::_exit(126) },
    };
    let before = match before_arg.parse::<u64>() {
        Ok(before) if before != 0 => before,
        _ => unsafe { libc::_exit(126) },
    };
    let after = tsc();
    let status = unsafe { write_lifecycle_stamp(fd, before, after) }
        .map(|_| 0)
        .unwrap_or(126);
    unsafe {
        libc::close(fd);
        libc::_exit(status);
    }
}

fn main() {
    let mut args = std::env::args();
    let _program = args.next();
    match args.next().as_deref() {
        Some("--lifecycle-child") => return,
        Some("--exec-replace-child") => {
            let (Some(fd), Some(before)) = (args.next(), args.next()) else {
                std::process::exit(126);
            };
            run_exec_replace_child(&fd, &before);
        }
        _ => {}
    }
    debug_line("ipcbench: begin");

    let filter = probe_filter();
    if filter.is_some() {
        // `--isolate-probe` wants every `ipc-call-phase-*`/`usermem-phase-*`
        // sample in the window to belong to this probe alone, but every
        // session-startup program launches around the same moment: uiserver's
        // first scene compile, wayclick's first frame, and netprobe's
        // self-test are a one-time burst that would otherwise land inside the
        // measured window just by starting at the same time as this probe
        // does. Letting that burst pass before calibrating or charging
        // anything is cheap; a contaminated sample is not recoverable after
        // the fact.
        debug_line("ipcbench: isolate settle begin");
        thread::sleep(ISOLATE_SETTLE);
        debug_line("ipcbench: isolate settle done");
        // The ordinary housekeeping drain runs on its own once-per-second
        // cadence, decoupled from this boundary, so whatever it has not yet
        // flushed from boot and the settle sleep would otherwise leak into
        // the measured window the moment it next fires.
        force_drain_phase_profiles();
    }

    let tsc_khz = calibrate_tsc_khz();
    debug_line(&format!("ipcbench: tsc_khz={tsc_khz}"));

    match filter {
        Some(name) => {
            run_isolated_probe(&name, tsc_khz);
            // Flush the probe's own tail charges before the log capture can
            // see "end": the ordinary once-per-second drain cadence is not
            // synchronized to a probe's own finish time, so whatever
            // accumulated since its last window closed would otherwise sit in
            // the live counters, undrained and invisible to the log.
            force_drain_phase_profiles();
        }
        None => run_all_probes(tsc_khz),
    }

    debug_line("ipcbench: end");
}
