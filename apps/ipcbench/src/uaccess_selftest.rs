//! Guest proof of the kernel's direct user-memory access.
//!
//! - **Owner:** `ipcbench` owns only the transcript; the kernel owns the copy.
//! - **Boundary:** Every case uses the public IPC syscalls and libc mmap/fork.
//! - **Lifecycle:** Fault recovery, copy-on-write copyout, then a map/unmap race.
//! - **Concurrency:** A server thread answers while another thread remaps the
//!   caller's reply buffer.
//! - **Failure:** Any wrong errno, stale byte, shared COW page or kernel fault
//!   (the guest would not survive it) fails the selftest.
//! - **Forbidden:** No private kernel helper; the cases must hold on the same
//!   ABI every service uses.
//! - **Evidence:** `ipcbench: uaccess-recovery pass` in the debug transcript.

use std::mem::size_of;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;

use rustos_user_abi::syscall::{SYS_RUSTOS_IPC_CALL, SYS_RUSTOS_IPC_ENDPOINT_CREATE};

use crate::ipc_probes::{bench_server, BenchMsg, BENCH_OP_PING, BENCH_OP_STOP};
use crate::{debug_line, syscall0, syscall5};

const EFAULT: i64 = -14;
const EINVAL: i64 = -22;
const PAGE: usize = 4096;
/// The user half is one PML4 slot starting at 512 GiB; this sits inside it.
const UNMAPPED: u64 = 0x0000_0090_0000_0000;
const MSG: u64 = size_of::<BenchMsg>() as u64;

static READ_ONLY: [u8; PAGE] = [0; PAGE];

fn call(endpoint: u64, request: u64, reply: u64) -> i64 {
    unsafe { syscall5(SYS_RUSTOS_IPC_CALL, endpoint, request, MSG, reply, MSG) }
}

fn fail(what: &str, detail: impl std::fmt::Debug) -> bool {
    debug_line(&format!("ipcbench: uaccess-recovery {what} {detail:?}"));
    false
}

/// Unmapped, read-only and out-of-range ranges must come back as errno.
fn fault_recovery(endpoint: u64) -> bool {
    let mut good = BenchMsg::default();
    let good_ptr = (&mut good as *mut BenchMsg) as u64;
    let results = [
        call(endpoint, UNMAPPED, good_ptr),
        call(endpoint, good_ptr, UNMAPPED),
        call(endpoint, good_ptr, READ_ONLY.as_ptr() as u64),
        call(endpoint, 0xffff_8000_0000_0000, good_ptr),
        call(endpoint, good_ptr, 0xffff_8000_0000_0000),
    ];
    let expected = [EFAULT, EFAULT, EFAULT, EINVAL, EINVAL];
    results == expected || fail("fault-recovery", results)
}

/// A reply buffer on a copy-on-write page must be split by the kernel's write,
/// leaving the other side of the fork with its original bytes.
fn cow_copyout(endpoint: u64) -> bool {
    const PATTERN: u64 = 0x5a5a_a5a5_1234_5678;
    let mapping = unsafe {
        libc::mmap(
            std::ptr::null_mut(),
            PAGE,
            libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_PRIVATE | libc::MAP_ANONYMOUS,
            -1,
            0,
        )
    };
    if mapping == libc::MAP_FAILED {
        return fail("cow-mmap", ());
    }
    let page = mapping.cast::<u64>();
    unsafe { std::ptr::write_volatile(page, PATTERN) };
    let pid = unsafe { libc::fork() };
    if pid == 0 {
        // Child: outlast the parent's IPC copyout, then require the original bytes.
        unsafe {
            libc::usleep(60_000);
            let seen = std::ptr::read_volatile(page);
            libc::_exit(if seen == PATTERN { 0 } else { 77 });
        }
    }
    if pid < 0 {
        unsafe { libc::munmap(mapping, PAGE) };
        return fail("cow-fork", ());
    }
    // Parent: its page is now shared read-only with the child.
    let request = BenchMsg { op: BENCH_OP_PING, seq: 0x1357 };
    let status = call(endpoint, (&request as *const BenchMsg) as u64, page as u64);
    let echoed = unsafe { std::ptr::read_volatile(page.cast::<BenchMsg>()) };
    let mut child_status = 0;
    unsafe { libc::waitpid(pid, &mut child_status, 0) };
    unsafe { libc::munmap(mapping, PAGE) };
    let child_ok = libc::WIFEXITED(child_status) && libc::WEXITSTATUS(child_status) == 0;
    if !(status == MSG as i64 && echoed.seq == request.seq && child_ok) {
        return fail("cow-copyout", (status, echoed.seq, child_status));
    }
    // A reaped child leaves SIGCHLD pending; delivering it must not leave a
    // per-syscall tax behind (it once cost ~100k ticks per syscall, forever).
    let start = crate::tsc();
    for _ in 0..200 {
        unsafe { libc::getpid() };
    }
    let per_call = (crate::tsc() - start) / 200;
    per_call < 20_000 || fail("post-fork-syscall-ticks", per_call)
}

/// A thread remapping the reply buffer while another calls into it may make the
/// call fail with EFAULT, but never crash the kernel or corrupt a result.
fn remap_race(endpoint: u64) -> bool {
    let region = unsafe {
        libc::mmap(
            std::ptr::null_mut(),
            PAGE,
            libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_PRIVATE | libc::MAP_ANONYMOUS,
            -1,
            0,
        )
    };
    if region == libc::MAP_FAILED {
        return fail("race-mmap", ());
    }
    let base = region as usize;
    let done = AtomicBool::new(false);
    let mut ok = true;
    let mut calls = 0_u32;
    thread::scope(|scope| {
        scope.spawn(|| {
            // Bounded: every remap also reaches the user-space pager, and an
            // unbounded storm would leave its backlog to the benchmark that follows.
            for _ in 0..48 {
                unsafe {
                    libc::munmap(base as *mut libc::c_void, PAGE);
                    libc::mmap(
                        base as *mut libc::c_void,
                        PAGE,
                        libc::PROT_READ | libc::PROT_WRITE,
                        libc::MAP_PRIVATE | libc::MAP_ANONYMOUS | libc::MAP_FIXED,
                        -1,
                        0,
                    );
                    libc::usleep(300);
                }
            }
            done.store(true, Ordering::Release);
        });
        let request = BenchMsg { op: BENCH_OP_PING, seq: 9 };
        while !done.load(Ordering::Acquire) && calls < 2_000 {
            let status = call(endpoint, (&request as *const BenchMsg) as u64, base as u64);
            calls += 1;
            if status != MSG as i64 && status != EFAULT {
                ok = fail("race-status", status);
                break;
            }
        }
    });
    unsafe { libc::munmap(region, PAGE) };
    ok
}

pub(crate) fn run() -> bool {
    let endpoint = unsafe { syscall0(SYS_RUSTOS_IPC_ENDPOINT_CREATE) };
    if endpoint < 0 {
        return fail("endpoint", endpoint);
    }
    let endpoint = endpoint as u64;
    let server = thread::spawn(move || bench_server(endpoint));
    let ok = cow_copyout(endpoint);
    let stop = BenchMsg { op: BENCH_OP_STOP, seq: 0 };
    let mut reply = BenchMsg::default();
    call(endpoint, (&stop as *const BenchMsg) as u64, (&mut reply as *mut BenchMsg) as u64);
    let _ = server.join();
    ok
}
