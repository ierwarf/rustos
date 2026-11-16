//! Secondary Linux syscall dispatch: process-identity getters and the
//! kernel-local pipe/eventfd family. `dispatch_linux_syscall` falls through to
//! [`dispatch`] before reporting `ENOSYS`, which keeps the primary match under
//! its size budget without changing behaviour.
use super::*;

/// Handles the syscalls owned by this table; `None` means "not mine".
pub(super) fn dispatch(frame: &mut SyscallFrame) -> Option<u64> {
    Some(match frame.rax {
        linux_abi::SYS_PWRITE64 => {
            syscall_linux_vfs_pwrite64(frame.rdi, frame.rsi, frame.rdx, frame.r10)
        }
        linux_abi::SYS_READV => syscall_linux_vfs_readv(frame.rdi, frame.rsi, frame.rdx),
        linux_abi::SYS_PREADV => {
            syscall_linux_vfs_preadv(frame.rdi, frame.rsi, frame.rdx, frame.r10)
        }
        linux_abi::SYS_PWRITEV => {
            syscall_linux_vfs_pwritev(frame.rdi, frame.rsi, frame.rdx, frame.r10)
        }
        linux_abi::SYS_PREADV2 => syscall_linux_vfs_preadv2(
            frame.rdi, frame.rsi, frame.rdx, frame.r10, frame.r8, frame.r9,
        ),
        linux_abi::SYS_PWRITEV2 => syscall_linux_vfs_pwritev2(
            frame.rdi, frame.rsi, frame.rdx, frame.r10, frame.r8, frame.r9,
        ),
        linux_abi::SYS_FSYNC | linux_abi::SYS_FDATASYNC => syscall_linux_vfs_fsync(frame.rdi),
        linux_abi::SYS_SYNC => syscall_linux_vfs_sync(),
        linux_abi::SYS_SYNCFS => syscall_linux_vfs_syncfs(frame.rdi),
        linux_abi::SYS_FADVISE64 => {
            syscall_linux_vfs_fadvise64(frame.rdi, frame.rsi, frame.rdx, frame.r10)
        }
        linux_abi::SYS_CLOSE_RANGE => {
            syscall_linux_vfs_close_range(frame.rdi, frame.rsi, frame.rdx)
        }
        linux_abi::SYS_PAUSE => syscall_linux_pause(),
        linux_abi::SYS_RT_SIGPENDING => syscall_linux_rt_sigpending(frame.rdi, frame.rsi),
        linux_abi::SYS_RT_SIGSUSPEND => syscall_linux_rt_sigsuspend(frame),
        linux_abi::SYS_GETTIMEOFDAY => syscall_linux_gettimeofday(frame.rdi, frame.rsi),
        linux_abi::SYS_CLOCK_GETRES => syscall_linux_clock_getres(frame.rdi, frame.rsi),
        linux_abi::SYS_GETRUSAGE => syscall_linux_getrusage(frame.rdi as i64, frame.rsi),
        linux_abi::SYS_SYSINFO => syscall_linux_sysinfo(frame.rdi),
        linux_abi::SYS_TIMES => syscall_linux_times(frame.rdi),
        linux_abi::SYS_GETCPU => syscall_linux_getcpu(frame.rdi, frame.rsi),
        linux_abi::SYS_SCHED_GETSCHEDULER => syscall_linux_sched_getscheduler(frame.rdi as i64),
        linux_abi::SYS_SCHED_GETPARAM => syscall_linux_sched_getparam(frame.rdi as i64, frame.rsi),
        linux_abi::SYS_SCHED_GET_PRIORITY_MAX | linux_abi::SYS_SCHED_GET_PRIORITY_MIN => {
            syscall_linux_sched_get_priority_minmax(frame.rdi as i64)
        }
        linux_abi::SYS_SCHED_RR_GET_INTERVAL => {
            syscall_linux_sched_rr_get_interval(frame.rdi as i64, frame.rsi)
        }
        linux_abi::SYS_GETGROUPS => {
            syscall_linux_syscalld_getgroups(frame.rdi as i64, frame.rsi)
        }
        linux_abi::SYS_GETRESUID => syscall_linux_syscalld_getres_id(
            SYSCALL_OFFLOAD_OP_LINUX_GETRESUID,
            frame.rdi,
            frame.rsi,
            frame.rdx,
        ),
        linux_abi::SYS_GETRESGID => syscall_linux_syscalld_getres_id(
            SYSCALL_OFFLOAD_OP_LINUX_GETRESGID,
            frame.rdi,
            frame.rsi,
            frame.rdx,
        ),
        linux_abi::SYS_GETPRIORITY => syscall_linux_syscalld_getpriority(frame.rdi, frame.rsi),
        linux_abi::SYS_SETPRIORITY => {
            syscall_linux_syscalld_setpriority(frame.rdi, frame.rsi, frame.rdx as i64)
        }
        linux_abi::SYS_PIPE => syscall_linux_pipe(frame.rdi),
        linux_abi::SYS_PIPE2 => syscall_linux_pipe2(frame.rdi, frame.rsi),
        linux_abi::SYS_EVENTFD => syscall_linux_eventfd(frame.rdi),
        linux_abi::SYS_EVENTFD2 => syscall_linux_eventfd2(frame.rdi, frame.rsi),
        linux_abi::SYS_GETPPID => {
            syscall_linux_syscalld_u64_getter(SYSCALL_OFFLOAD_OP_LINUX_GETPPID, 0)
        }
        linux_abi::SYS_GETPGID => {
            syscall_linux_syscalld_u64_getter(SYSCALL_OFFLOAD_OP_LINUX_GETPGID, frame.rdi)
        }
        linux_abi::SYS_SETPGID => syscall_linux_syscalld_setpgid(frame.rdi, frame.rsi),
        linux_abi::SYS_GETSID => {
            syscall_linux_syscalld_u64_getter(SYSCALL_OFFLOAD_OP_LINUX_GETSID, frame.rdi)
        }
        linux_abi::SYS_SETSID => {
            syscall_linux_syscalld_u64_getter(SYSCALL_OFFLOAD_OP_LINUX_SETSID, 0)
        }
        _ => return None,
    })
}
