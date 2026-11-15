//! Secondary Linux syscall dispatch: process-identity getters and the
//! kernel-local pipe/eventfd family. `dispatch_linux_syscall` falls through to
//! [`dispatch`] before reporting `ENOSYS`, which keeps the primary match under
//! its size budget without changing behaviour.
use super::*;

/// Handles the syscalls owned by this table; `None` means "not mine".
pub(super) fn dispatch(frame: &SyscallFrame) -> Option<u64> {
    Some(match frame.rax {
        linux_abi::SYS_READV => syscall_linux_vfs_readv(frame.rdi, frame.rsi, frame.rdx),
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
