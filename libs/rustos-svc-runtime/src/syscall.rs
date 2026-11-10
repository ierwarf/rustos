//! Raw `syscall` instruction wrappers. Bypass libc entirely so a static-PIE
//! service can issue RustOS-native syscalls (`SYS_RUSTOS_IPC_*`) without
//! depending on glibc/musl startup. IPC data operations share the opt-in fastcall
//! boundary; large/bounded/handle traffic keeps the original buffer handlers.
//! Linux syscalls and IPC control operations retain the ordinary entry ABI.

use core::arch::asm;

/// Shared IPC-only bridge for services that retain libc/native wrappers for
/// their other syscalls. Returns raw `-errno`; never reads/writes libc errno.
pub use rustos_user_abi::syscall::ipc_register_buffer_syscall as try_fast_ipc;

#[inline]
pub unsafe fn syscall0(number: u64) -> i64 {
    if let Some(result) =
        unsafe { rustos_user_abi::syscall::ipc_register_buffer_syscall(number, [0; 6]) }
    {
        return result;
    }
    let ret: i64;
    asm!(
        "syscall",
        inlateout("rax") number as i64 => ret,
        lateout("rcx") _,
        lateout("r11") _,
        options(nostack, preserves_flags),
    );
    ret
}

#[inline]
pub unsafe fn syscall1(number: u64, a0: u64) -> i64 {
    if let Some(result) = unsafe {
        rustos_user_abi::syscall::ipc_register_buffer_syscall(number, [a0, 0, 0, 0, 0, 0])
    } {
        return result;
    }
    let ret: i64;
    asm!(
        "syscall",
        inlateout("rax") number as i64 => ret,
        in("rdi") a0,
        lateout("rcx") _,
        lateout("r11") _,
        options(nostack, preserves_flags),
    );
    ret
}

#[inline]
pub unsafe fn syscall2(number: u64, a0: u64, a1: u64) -> i64 {
    if let Some(result) = unsafe {
        rustos_user_abi::syscall::ipc_register_buffer_syscall(number, [a0, a1, 0, 0, 0, 0])
    } {
        return result;
    }
    let ret: i64;
    asm!(
        "syscall",
        inlateout("rax") number as i64 => ret,
        in("rdi") a0,
        in("rsi") a1,
        lateout("rcx") _,
        lateout("r11") _,
        options(nostack, preserves_flags),
    );
    ret
}

#[inline]
pub unsafe fn syscall3(number: u64, a0: u64, a1: u64, a2: u64) -> i64 {
    if let Some(result) = unsafe {
        rustos_user_abi::syscall::ipc_register_buffer_syscall(number, [a0, a1, a2, 0, 0, 0])
    } {
        return result;
    }
    let ret: i64;
    asm!(
        "syscall",
        inlateout("rax") number as i64 => ret,
        in("rdi") a0,
        in("rsi") a1,
        in("rdx") a2,
        lateout("rcx") _,
        lateout("r11") _,
        options(nostack, preserves_flags),
    );
    ret
}

#[inline]
pub unsafe fn syscall4(number: u64, a0: u64, a1: u64, a2: u64, a3: u64) -> i64 {
    if let Some(result) = unsafe {
        rustos_user_abi::syscall::ipc_register_buffer_syscall(number, [a0, a1, a2, a3, 0, 0])
    } {
        return result;
    }
    let ret: i64;
    asm!(
        "syscall",
        inlateout("rax") number as i64 => ret,
        in("rdi") a0,
        in("rsi") a1,
        in("rdx") a2,
        in("r10") a3,
        lateout("rcx") _,
        lateout("r11") _,
        options(nostack, preserves_flags),
    );
    ret
}

#[inline]
pub unsafe fn syscall5(number: u64, a0: u64, a1: u64, a2: u64, a3: u64, a4: u64) -> i64 {
    if let Some(result) = unsafe {
        rustos_user_abi::syscall::ipc_register_buffer_syscall(number, [a0, a1, a2, a3, a4, 0])
    } {
        return result;
    }
    let ret: i64;
    asm!(
        "syscall",
        inlateout("rax") number as i64 => ret,
        in("rdi") a0,
        in("rsi") a1,
        in("rdx") a2,
        in("r10") a3,
        in("r8") a4,
        lateout("rcx") _,
        lateout("r11") _,
        options(nostack, preserves_flags),
    );
    ret
}

#[inline]
pub unsafe fn syscall6(number: u64, a0: u64, a1: u64, a2: u64, a3: u64, a4: u64, a5: u64) -> i64 {
    if let Some(result) = unsafe {
        rustos_user_abi::syscall::ipc_register_buffer_syscall(number, [a0, a1, a2, a3, a4, a5])
    } {
        return result;
    }
    let ret: i64;
    asm!(
        "syscall",
        inlateout("rax") number as i64 => ret,
        in("rdi") a0,
        in("rsi") a1,
        in("rdx") a2,
        in("r10") a3,
        in("r8") a4,
        in("r9") a5,
        lateout("rcx") _,
        lateout("r11") _,
        options(nostack, preserves_flags),
    );
    ret
}

// Linux x86_64 syscall numbers used by the runtime itself.
pub const SYS_MMAP: u64 = 9;
pub const SYS_SCHED_YIELD: u64 = 24;
pub const SYS_FUTEX: u64 = 202;
pub const SYS_NANOSLEEP: u64 = 35;
pub const SYS_EXIT_GROUP: u64 = 231;
pub const SYS_CLOCK_GETTIME: u64 = 228;
pub const CLOCK_MONOTONIC: u64 = 1;

// Linux mmap flag bits we need for the allocator's lazy heap-growth path.
pub const PROT_READ: u64 = 0x1;
pub const PROT_WRITE: u64 = 0x2;
pub const MAP_PRIVATE: u64 = 0x02;
pub const MAP_ANONYMOUS: u64 = 0x20;

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct KTimespec {
    pub tv_sec: i64,
    pub tv_nsec: i64,
}

#[inline]
pub fn exit_group(code: i32) -> ! {
    unsafe {
        syscall1(SYS_EXIT_GROUP, code as u64);
    }
    // Should never return. If it does, spin halt.
    loop {
        unsafe { asm!("hlt", options(nomem, nostack)) }
    }
}

const FUTEX_WAIT_PRIVATE: u64 = 128;
const FUTEX_WAKE_PRIVATE: u64 = 129;

/// Sleeps on a process-private futex word while it still equals `expected`.
/// Returns the raw kernel result; callers re-check the word, so a spurious
/// wakeup, `EAGAIN` (the word already changed) and `EINTR` are all retried.
///
/// # Safety
/// `word` must point to a live, aligned `u32` for the duration of the call.
#[inline]
pub unsafe fn futex_wait_private(word: *const u32, expected: u32) -> i64 {
    // SAFETY: the caller guarantees `word`; FUTEX_WAIT only reads it.
    syscall4(
        SYS_FUTEX,
        word as u64,
        FUTEX_WAIT_PRIVATE,
        u64::from(expected),
        0,
    )
}

/// Wakes up to `count` waiters of a process-private futex word.
///
/// # Safety
/// `word` must point to a live, aligned `u32` for the duration of the call.
#[inline]
pub unsafe fn futex_wake_private(word: *const u32, count: u32) -> i64 {
    // SAFETY: the caller guarantees `word`; FUTEX_WAKE never writes it.
    syscall3(SYS_FUTEX, word as u64, FUTEX_WAKE_PRIVATE, u64::from(count))
}

/// Issue an anonymous private mmap. Returns the mapped base or a negative
/// errno encoded as `i64`. The kernel bootstrap fallback in
/// `kernel/compat/src/user/syscall/linux/memory_ops.rs` accepts this even
/// before syscalld is registered.
#[inline]
pub unsafe fn mmap_anonymous(len: usize) -> i64 {
    syscall6(
        SYS_MMAP,
        0,
        len as u64,
        PROT_READ | PROT_WRITE,
        MAP_PRIVATE | MAP_ANONYMOUS,
        u64::MAX, // fd = -1
        0,
    )
}

/// Reads `CLOCK_MONOTONIC` in nanoseconds.
///
/// This is the only time source a transaction deadline may be built from. The
/// wall clock is not ordering authority: it can step, and a stepped budget
/// either fails a healthy transaction or lets an expired one continue.
///
/// Fallible callers may use [`try_monotonic_nanos`]. A service that requires a
/// clock to uphold its authority leases must fail-stop on a broken clock, not
/// fabricate zero (which turns finite deadlines into unlimited authority).
#[inline]
pub fn monotonic_nanos() -> u64 {
    match try_monotonic_nanos() {
        Ok(now) => now,
        Err(_) => {
            crate::ipc::debug_bytes(b"service: fatal monotonic clock unavailable\n");
            exit_group(125)
        }
    }
}

/// Read the kernel-wide CLOCK_MONOTONIC nanosecond domain without fallback.
#[inline]
pub fn try_monotonic_nanos() -> Result<u64, rustos_user_abi::deadline::MonotonicClockError> {
    let mut ts = KTimespec::default();
    // SAFETY: the syscall writes exactly one `KTimespec` through this exclusive
    // borrow and consumes no other pointer.
    let status = unsafe {
        syscall2(
            SYS_CLOCK_GETTIME,
            CLOCK_MONOTONIC,
            (&mut ts as *mut KTimespec) as u64,
        )
    };
    decode_monotonic_read(status, ts)
}

fn decode_monotonic_read(
    status: i64,
    ts: KTimespec,
) -> Result<u64, rustos_user_abi::deadline::MonotonicClockError> {
    if status != 0 {
        return Err(rustos_user_abi::deadline::MonotonicClockError::Syscall(status));
    }
    rustos_user_abi::deadline::checked_monotonic_nanos(ts.tv_sec, ts.tv_nsec)
}

#[cfg(test)]
mod clock_tests {
    use super::*;
    use rustos_user_abi::deadline::MonotonicClockError;

    #[test]
    fn failed_clock_read_is_an_error_not_an_immortal_deadline() {
        assert_eq!(decode_monotonic_read(-14, KTimespec::default()), Err(MonotonicClockError::Syscall(-14)));
        assert_eq!(decode_monotonic_read(1, KTimespec::default()), Err(MonotonicClockError::Syscall(1)));
        assert_eq!(decode_monotonic_read(0, KTimespec { tv_sec: 0, tv_nsec: -1 }), Err(MonotonicClockError::InvalidTimespec));
        assert_eq!(decode_monotonic_read(0, KTimespec { tv_sec: 2, tv_nsec: 3 }), Ok(2_000_000_003));
    }
}

#[inline]
pub fn sleep_millis(millis: u64) {
    let ts = KTimespec {
        tv_sec: (millis / 1000) as i64,
        tv_nsec: ((millis % 1000) * 1_000_000) as i64,
    };
    unsafe {
        let _ = syscall2(SYS_NANOSLEEP, (&ts as *const KTimespec) as u64, 0);
    }
}
