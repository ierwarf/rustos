//! Linux syscall numbers and flags for the memory-introspection, scheduling
//! policy, signal-wait, rlimit and multi-message families.
//!
//! Numbers come from the pinned `linux-raw-sys`; flag values that the crate
//! does not export are spelled from the UAPI headers and pinned by the tests
//! below. This module owns no policy or mutable state.

use super::linux;

pub const SYS_MREMAP: u64 = linux::__NR_mremap as u64;
pub const SYS_MINCORE: u64 = linux::__NR_mincore as u64;
pub const SYS_MLOCK: u64 = linux::__NR_mlock as u64;
pub const SYS_MUNLOCK: u64 = linux::__NR_munlock as u64;
pub const SYS_MLOCKALL: u64 = linux::__NR_mlockall as u64;
pub const SYS_MUNLOCKALL: u64 = linux::__NR_munlockall as u64;
pub const SYS_MLOCK2: u64 = linux::__NR_mlock2 as u64;
pub const SYS_MSYNC: u64 = linux::__NR_msync as u64;
pub const SYS_MEMBARRIER: u64 = linux::__NR_membarrier as u64;
pub const SYS_RT_SIGTIMEDWAIT: u64 = linux::__NR_rt_sigtimedwait as u64;
pub const SYS_EPOLL_PWAIT2: u64 = linux::__NR_epoll_pwait2 as u64;
pub const SYS_GETRLIMIT: u64 = linux::__NR_getrlimit as u64;
pub const SYS_SETRLIMIT: u64 = linux::__NR_setrlimit as u64;
pub const SYS_SCHED_SETSCHEDULER: u64 = linux::__NR_sched_setscheduler as u64;
pub const SYS_SCHED_SETPARAM: u64 = linux::__NR_sched_setparam as u64;
pub const SYS_SENDMMSG: u64 = linux::__NR_sendmmsg as u64;
pub const SYS_RECVMMSG: u64 = linux::__NR_recvmmsg as u64;

pub const MREMAP_MAYMOVE: u64 = linux::MREMAP_MAYMOVE as u64;
pub const MREMAP_FIXED: u64 = linux::MREMAP_FIXED as u64;
pub const MREMAP_DONTUNMAP: u64 = linux::MREMAP_DONTUNMAP as u64;

pub const MS_ASYNC: u64 = linux::MS_ASYNC as u64;
pub const MS_INVALIDATE: u64 = linux::MS_INVALIDATE as u64;
pub const MS_SYNC: u64 = linux::MS_SYNC as u64;

pub const MCL_CURRENT: u64 = linux::MCL_CURRENT as u64;
pub const MCL_FUTURE: u64 = linux::MCL_FUTURE as u64;
pub const MCL_ONFAULT: u64 = linux::MCL_ONFAULT as u64;
pub const MLOCK_ONFAULT: u64 = linux::MLOCK_ONFAULT as u64;

/// `membarrier(2)`: `MEMBARRIER_CMD_QUERY` is the only command RustOS answers
/// (with an empty supported mask); every other command is `EINVAL`.
pub const MEMBARRIER_CMD_QUERY: u64 = 0;

pub const SCHED_OTHER: i64 = 0;
pub const SCHED_FIFO: i64 = linux::SCHED_FIFO as i64;
pub const SCHED_RR: i64 = linux::SCHED_RR as i64;
pub const SCHED_BATCH: i64 = linux::SCHED_BATCH as i64;
pub const SCHED_IDLE: i64 = linux::SCHED_IDLE as i64;
/// Priority range of the POSIX real-time classes on Linux.
pub const SCHED_RT_PRIORITY_MIN: u64 = 1;
pub const SCHED_RT_PRIORITY_MAX: u64 = 99;

/// `recvmmsg(2)` flag: block for the first message only.
pub const MSG_WAITFORONE: u64 = 0x1_0000;
/// `UIO_MAXIOV`: the longest `sendmmsg`/`recvmmsg` vector Linux accepts.
pub const UIO_MAXIOV: u64 = 1024;
/// `sizeof(struct mmsghdr)`: a 56-byte `msghdr` plus `msg_len` and padding.
pub const MMSGHDR_SIZE: usize = 64;
/// Offset of `msg_len` inside `struct mmsghdr`.
pub const MMSGHDR_MSG_LEN_OFFSET: usize = 56;

/// `O_PATH` and the `O_TMPFILE` directory bit (`__O_TMPFILE`).
pub const O_PATH: u64 = linux::O_PATH as u64;
pub const O_TMPFILE: u64 = linux::O_TMPFILE as u64;
pub const O_NOFOLLOW: u64 = linux::O_NOFOLLOW as u64;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn x86_64_numbers_match_the_kernel_table() {
        assert_eq!(SYS_MREMAP, 25);
        assert_eq!(SYS_MSYNC, 26);
        assert_eq!(SYS_MINCORE, 27);
        assert_eq!(SYS_GETRLIMIT, 97);
        assert_eq!(SYS_RT_SIGTIMEDWAIT, 128);
        assert_eq!(SYS_SCHED_SETPARAM, 142);
        assert_eq!(SYS_SCHED_SETSCHEDULER, 144);
        assert_eq!(SYS_MLOCK, 149);
        assert_eq!(SYS_MUNLOCK, 150);
        assert_eq!(SYS_MLOCKALL, 151);
        assert_eq!(SYS_MUNLOCKALL, 152);
        assert_eq!(SYS_SETRLIMIT, 160);
        assert_eq!(SYS_RECVMMSG, 299);
        assert_eq!(SYS_SENDMMSG, 307);
        assert_eq!(SYS_MEMBARRIER, 324);
        assert_eq!(SYS_MLOCK2, 325);
        assert_eq!(SYS_EPOLL_PWAIT2, 441);
    }

    #[test]
    fn flag_values_match_the_uapi() {
        assert_eq!((MREMAP_MAYMOVE, MREMAP_FIXED, MREMAP_DONTUNMAP), (1, 2, 4));
        assert_eq!((MS_ASYNC, MS_INVALIDATE, MS_SYNC), (1, 2, 4));
        assert_eq!((MCL_CURRENT, MCL_FUTURE, MCL_ONFAULT), (1, 2, 4));
        assert_eq!(MLOCK_ONFAULT, 1);
        assert_eq!(MSG_WAITFORONE, 0x10000);
        assert_eq!(O_PATH, 0o10000000);
        assert_eq!(O_NOFOLLOW, 0o400000);
        assert_eq!(O_TMPFILE, 0o20000000 | 0o200000);
        assert_eq!(MMSGHDR_SIZE, 64);
    }
}
