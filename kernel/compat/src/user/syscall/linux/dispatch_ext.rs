//! Secondary Linux syscall dispatch: process-identity getters and the
//! kernel-local pipe/eventfd family. `dispatch_linux_syscall` falls through to
//! [`dispatch`] before reporting `ENOSYS`, which keeps the primary match under
//! its size budget without changing behaviour.
use super::*;

/// Handles the syscalls owned by this table; `None` means "not mine".
pub(super) fn dispatch(frame: &SyscallFrame) -> Option<u64> {
    Some(match frame.rax {
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
