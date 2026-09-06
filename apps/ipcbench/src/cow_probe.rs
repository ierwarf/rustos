//! End-to-end Linux private-anonymous COW capability probe.
//!
//! - **Owner:** `ipcbench` owns only measurement and result verification.
//! - **Boundary:** The probe uses public libc mmap/fork/wait/munmap operations.
//! - **Lifecycle:** Map resident pages, fork, diverge, verify, reap, then unmap.
//! - **Concurrency:** Parent and child write independently after activation.
//! - **Failure:** Any syscall, child status, or value mismatch reports `skip`.
//! - **Forbidden:** No private kernel helper or shared test page is used.
//! - **Evidence:** `fork_cow_private_write` is the KVM COW acceptance result.

use super::*;

/// Proves private anonymous COW through the public Linux ABI.
///
/// The page is resident before fork. After the child becomes runnable, both
/// sides write different values. Whichever side writes first must copy while
/// the remaining side may promote the sole old-frame mapping in place. The
/// memfd stamp is outside the private mapping, so the parent can verify both
/// the child's inherited value and its post-write value without sharing the
/// page under test. A second untouched COW page receives kernel copyout via
/// pread: direct user stores must not hide a missing syscall-write split.
pub(super) fn probe_fork_cow_private_write(tsc_khz: u64, iters: usize, warmup: usize) {
    const INITIAL: u64 = 0x1122_3344_5566_7788;
    const PARENT: u64 = 0xa1a2_a3a4_a5a6_a7a8;
    const CHILD: u64 = 0xb1b2_b3b4_b5b6_b7b8;

    let mapping = unsafe {
        libc::mmap(
            ptr::null_mut(),
            8192,
            libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_PRIVATE | libc::MAP_ANONYMOUS,
            -1,
            0,
        )
    };
    if mapping == libc::MAP_FAILED {
        skip("fork_cow_private_write", "mmap-failed");
        return;
    }
    let stamp = match unsafe { LifecycleStampMapping::new() } {
        Ok(stamp) => stamp,
        Err(errno) => {
            unsafe { libc::munmap(mapping, 8192) };
            skip("fork_cow_private_write", &format!("memfd-errno-{errno}"));
            return;
        }
    };
    let page = mapping.cast::<u64>();
    let copyout_page = unsafe { page.add(4096 / size_of::<u64>()) };
    let mut failure = "unknown";
    let mut failure_detail = 0;
    let mut completed = 0_usize;
    let result = measure(iters, warmup, || unsafe {
        if stamp.rewind().is_err() {
            failure = "rewind-stamp";
            return false;
        }
        if write_lifecycle_stamp(stamp.fd, CHILD, PARENT).is_err() {
            failure = "seed-copyout";
            return false;
        }
        ptr::write_volatile(page, INITIAL);
        ptr::write_volatile(copyout_page, INITIAL);
        ptr::write_volatile(copyout_page.add(1), INITIAL);
        let pid = linux_fork_syscall();
        if pid == 0 {
            let before = ptr::read_volatile(page);
            let copied = libc::pread(stamp.fd, copyout_page.cast(), 16, 0);
            if copied != 16 {
                libc::_exit(100 + last_errno());
            }
            if ptr::read_volatile(copyout_page) != CHILD
                || ptr::read_volatile(copyout_page.add(1)) != PARENT
            {
                libc::_exit(123);
            }
            ptr::write_volatile(page, CHILD);
            let after = ptr::read_volatile(page);
            let status = if before == INITIAL && after == CHILD {
                stamp
                    .rewind()
                    .and_then(|_| write_lifecycle_stamp(stamp.fd, before, after))
                    .map(|_| 0)
                    .unwrap_or(125)
            } else {
                124
            };
            libc::_exit(status);
        }
        if pid < 0 {
            failure = "fork";
            failure_detail = last_errno();
            return false;
        }

        // Parent and child are now independently runnable. This write must not
        // change either value the child reports through the separate memfd.
        ptr::write_volatile(page, PARENT);
        let mut status = 0;
        if let Err(errno) = wait_child(pid, &mut status) {
            failure = "wait";
            failure_detail = errno;
            return false;
        }
        let (child_before, child_after) = match stamp.read() {
            Ok(values) => values,
            Err(errno) => {
                failure = "read-stamp";
                failure_detail = errno;
                return false;
            }
        };
        if !child_exited_successfully(status)
            || ptr::read_volatile(page) != PARENT
            || ptr::read_volatile(copyout_page) != INITIAL
            || ptr::read_volatile(copyout_page.add(1)) != INITIAL
            || child_before != INITIAL
            || child_after != CHILD
        {
            failure = "private-values";
            failure_detail = status;
            return false;
        }
        completed += 1;
        true
    });
    unsafe { libc::munmap(mapping, 8192) };

    match result {
        Some(mut samples) => report("fork_cow_private_write", &summarize(&mut samples), tsc_khz),
        None => skip(
            "fork_cow_private_write",
            &format!("{failure}-{failure_detail}-after-{completed}"),
        ),
    }
}
