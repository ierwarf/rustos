//! Minimal Linux errno set used by syscalld policies. Vendored so the service
//! can stay `no_std` and drop the `libc` dependency that ties bootstrap
//! services to glibc startup.

pub const EPERM: i32 = 1;
pub const ESRCH: i32 = 3;
pub const ENOMEM: i32 = 12;
pub const EACCES: i32 = 13;
pub const EINVAL: i32 = 22;
pub const ENOSYS: i32 = 38;

/// Linux `RLIMIT_*` resource codes we care about.
pub const RLIMIT_STACK: u64 = 3;

// Linux x86_64 syscall numbers issued directly by syscalld (the ones the
// kernel does NOT offload back to us — see the bootstrap fallbacks in
// `kernel/compat/src/user/syscall/linux/memory_ops.rs`).
const SYS_NANOSLEEP: u64 = 35;
const SYS_CLOCK_GETTIME: u64 = 228;

pub const CLOCK_REALTIME: i32 = 0;
pub const CLOCK_MONOTONIC: i32 = 1;

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct KTimespec {
    pub tv_sec: i64,
    pub tv_nsec: i64,
}

/// Sleep at least `millis` milliseconds via the raw Linux `nanosleep` syscall.
/// Used as the receive loop back-off (replaces `std::thread::sleep`).
pub fn sleep_millis(millis: u64) {
    let ts = KTimespec {
        tv_sec: (millis / 1000) as i64,
        tv_nsec: ((millis % 1000) * 1_000_000) as i64,
    };
    unsafe {
        let _ = rustos_svc_runtime::syscall::syscall2(
            SYS_NANOSLEEP,
            (&ts as *const KTimespec) as u64,
            0,
        );
    }
}

/// Sleep at least `req` nanoseconds. Mirrors `nanosleep(2)` semantics
/// (no remainder reporting — callers that need it must marshal the remainder
/// payload themselves).
pub fn nanosleep(req: &KTimespec) -> i64 {
    unsafe {
        rustos_svc_runtime::syscall::syscall2(SYS_NANOSLEEP, (req as *const KTimespec) as u64, 0)
    }
}

/// Read the kernel clock identified by `clock_id` into `out`. Returns 0 on
/// success or `-errno`.
pub fn clock_gettime(clock_id: i32, out: &mut KTimespec) -> i64 {
    unsafe {
        rustos_svc_runtime::syscall::syscall2(
            SYS_CLOCK_GETTIME,
            clock_id as u64,
            (out as *mut KTimespec) as u64,
        )
    }
}
