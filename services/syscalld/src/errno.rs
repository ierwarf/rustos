//! Minimal Linux errno set used by syscalld policies. Vendored so the service
//! can stay `no_std` and drop the `libc` dependency that ties bootstrap
//! services to glibc startup.

pub const EPERM: i32 = 1;
pub const ESRCH: i32 = 3;
pub const ENOMEM: i32 = 12;
pub const EACCES: i32 = 13;
pub const EINVAL: i32 = 22;
pub const EOVERFLOW: i32 = 75;
pub const ENOSYS: i32 = 38;

/// Linux `RLIMIT_*` resource codes we care about.
pub const RLIMIT_STACK: u64 = 3;

// Linux x86_64 syscall number used for syscalld's own IPC receive backoff.
// Offloaded guest time syscalls must not be proxied through syscalld and then
// reissued as the same Linux syscall.
const SYS_NANOSLEEP: u64 = 35;

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
struct KTimespec {
    tv_sec: i64,
    tv_nsec: i64,
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
