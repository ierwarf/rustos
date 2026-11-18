//! Secondary Linux syscall dispatch: process-identity getters and the
//! kernel-local pipe/eventfd family. `dispatch_linux_syscall` falls through to
//! [`dispatch`] before reporting `ENOSYS`, which keeps the primary match under
//! its size budget without changing behaviour.
use super::*;

/// Handles the syscalls owned by this table; `None` means "not mine".
pub(super) fn dispatch(frame: &mut SyscallFrame) -> Option<u64> {
    Some(match frame.rax {
        linux_abi::SYS_FLOCK => super::file_lock_ops::syscall_linux_flock(frame.rdi, frame.rsi),
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
        linux_abi::SYS_FSYNC => syscall_linux_vfs_fsync_mode(frame.rdi, false),
        linux_abi::SYS_FDATASYNC => syscall_linux_vfs_fsync_mode(frame.rdi, true),
        linux_abi::SYS_SYNC => syscall_linux_vfs_sync(),
        linux_abi::SYS_SYNCFS => syscall_linux_vfs_syncfs(frame.rdi),
        linux_abi::SYS_TRUNCATE => syscall_linux_vfs_truncate(frame.rdi, frame.rsi),
        linux_abi::SYS_MKDIRAT => syscall_linux_vfs_mkdir(frame.rdi, frame.rsi, frame.rdx),
        linux_abi::SYS_RMDIR => syscall_linux_vfs_unlinkat(
            linux_abi::AT_FDCWD as u64,
            frame.rdi,
            linux_abi::AT_REMOVEDIR as u64,
        ),
        linux_abi::SYS_RENAME => syscall_linux_vfs_renameat2(
            linux_abi::AT_FDCWD as u64,
            frame.rdi,
            linux_abi::AT_FDCWD as u64,
            frame.rsi,
            0,
        ),
        linux_abi::SYS_RENAMEAT => {
            syscall_linux_vfs_renameat2(frame.rdi, frame.rsi, frame.rdx, frame.r10, 0)
        }
        linux_abi::SYS_RENAMEAT2 => {
            syscall_linux_vfs_renameat2(frame.rdi, frame.rsi, frame.rdx, frame.r10, frame.r8)
        }
        linux_abi::SYS_SYMLINK => {
            syscall_linux_vfs_symlinkat(frame.rdi, linux_abi::AT_FDCWD as u64, frame.rsi)
        }
        linux_abi::SYS_SYMLINKAT => {
            syscall_linux_vfs_symlinkat(frame.rdi, frame.rsi, frame.rdx)
        }
        linux_abi::SYS_STATFS => syscall_linux_vfs_statfs(frame.rdi, frame.rsi),
        linux_abi::SYS_FSTATFS => syscall_linux_vfs_fstatfs(frame.rdi, frame.rsi),
        linux_abi::SYS_FADVISE64 => {
            syscall_linux_vfs_fadvise64(frame.rdi, frame.rsi, frame.rdx, frame.r10)
        }
        linux_abi::SYS_FALLOCATE => {
            syscall_linux_vfs_fallocate(frame.rdi, frame.rsi, frame.rdx, frame.r10)
        }
        linux_abi::SYS_FCHMOD => syscall_linux_vfs_fchmod(frame.rdi, frame.rsi),
        linux_abi::SYS_CHMOD => syscall_linux_vfs_fchmodat(
            linux_abi::AT_FDCWD as u64,
            frame.rdi,
            frame.rsi,
            0,
        ),
        linux_abi::SYS_FCHMODAT => {
            syscall_linux_vfs_fchmodat(frame.rdi, frame.rsi, frame.rdx, 0)
        }
        linux_abi::SYS_FCHMODAT2 => {
            syscall_linux_vfs_fchmodat(frame.rdi, frame.rsi, frame.rdx, frame.r10)
        }
        linux_abi::SYS_CHOWN => syscall_linux_vfs_fchownat(
            linux_abi::AT_FDCWD as u64,
            frame.rdi,
            frame.rsi,
            frame.rdx,
            0,
        ),
        linux_abi::SYS_LCHOWN => syscall_linux_vfs_fchownat(
            linux_abi::AT_FDCWD as u64,
            frame.rdi,
            frame.rsi,
            frame.rdx,
            linux_abi::AT_SYMLINK_NOFOLLOW,
        ),
        linux_abi::SYS_FCHOWN => syscall_linux_vfs_fchown(frame.rdi, frame.rsi, frame.rdx),
        linux_abi::SYS_FCHOWNAT => {
            syscall_linux_vfs_fchownat(frame.rdi, frame.rsi, frame.rdx, frame.r10, frame.r8)
        }
        linux_abi::SYS_MKNOD => syscall_linux_vfs_mknodat(
            linux_abi::AT_FDCWD as u64,
            frame.rdi,
            frame.rsi,
            frame.rdx,
        ),
        linux_abi::SYS_MKNODAT => {
            syscall_linux_vfs_mknodat(frame.rdi, frame.rsi, frame.rdx, frame.r10)
        }
        linux_abi::SYS_LINK => syscall_linux_vfs_linkat(
            linux_abi::AT_FDCWD as u64,
            frame.rdi,
            linux_abi::AT_FDCWD as u64,
            frame.rsi,
            0,
        ),
        linux_abi::SYS_LINKAT => {
            syscall_linux_vfs_linkat(frame.rdi, frame.rsi, frame.rdx, frame.r10, frame.r8)
        }
        linux_abi::SYS_UTIME => syscall_linux_vfs_utime(frame.rdi, frame.rsi),
        linux_abi::SYS_UTIMES => {
            syscall_linux_vfs_futimesat(linux_abi::AT_FDCWD as u64, frame.rdi, frame.rsi)
        }
        linux_abi::SYS_FUTIMESAT => {
            syscall_linux_vfs_futimesat(frame.rdi, frame.rsi, frame.rdx)
        }
        linux_abi::SYS_UTIMENSAT => {
            syscall_linux_vfs_utimensat(frame.rdi, frame.rsi, frame.rdx, frame.r10)
        }
        linux_abi::SYS_SENDFILE => {
            syscall_linux_vfs_sendfile(frame.rdi, frame.rsi, frame.rdx, frame.r10)
        }
        linux_abi::SYS_COPY_FILE_RANGE => syscall_linux_vfs_copy_file_range(
            frame.rdi, frame.rsi, frame.rdx, frame.r10, frame.r8, frame.r9,
        ),
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
        linux_abi::SYS_SCHED_GET_PRIORITY_MAX => {
            syscall_linux_sched_get_priority(frame.rdi as i64, true)
        }
        linux_abi::SYS_SCHED_GET_PRIORITY_MIN => {
            syscall_linux_sched_get_priority(frame.rdi as i64, false)
        }
        linux_abi::SYS_SCHED_SETSCHEDULER => {
            syscall_linux_sched_setscheduler(frame.rdi as i64, frame.rsi as i64, frame.rdx)
        }
        linux_abi::SYS_SCHED_SETPARAM => syscall_linux_sched_setparam(frame.rdi as i64, frame.rsi),
        linux_abi::SYS_GETRLIMIT => syscall_linux_getrlimit(frame.rdi, frame.rsi),
        linux_abi::SYS_SETRLIMIT => syscall_linux_setrlimit(frame.rdi, frame.rsi),
        linux_abi::SYS_RT_SIGTIMEDWAIT => {
            syscall_linux_rt_sigtimedwait(frame.rdi, frame.rsi, frame.rdx, frame.r10)
        }
        linux_abi::SYS_EPOLL_PWAIT2 => syscall_linux_epoll_pwait2(
            frame.rdi, frame.rsi, frame.rdx, frame.r10, frame.r8, frame.r9,
        ),
        linux_abi::SYS_SENDMMSG => {
            syscall_linux_sendmmsg(frame.rdi, frame.rsi, frame.rdx, frame.r10)
        }
        linux_abi::SYS_RECVMMSG => {
            syscall_linux_recvmmsg(frame.rdi, frame.rsi, frame.rdx, frame.r10, frame.r8)
        }
        linux_abi::SYS_MREMAP => {
            syscall_linux_mremap(frame.rdi, frame.rsi, frame.rdx, frame.r10, frame.r8)
        }
        linux_abi::SYS_MINCORE => syscall_linux_mincore(frame.rdi, frame.rsi, frame.rdx),
        linux_abi::SYS_MLOCK => syscall_linux_mlock(frame.rdi, frame.rsi),
        linux_abi::SYS_MLOCK2 => syscall_linux_mlock2(frame.rdi, frame.rsi, frame.rdx),
        linux_abi::SYS_MUNLOCK => syscall_linux_munlock(frame.rdi, frame.rsi),
        linux_abi::SYS_MLOCKALL => syscall_linux_mlockall(frame.rdi),
        linux_abi::SYS_MUNLOCKALL => syscall_linux_munlockall(),
        linux_abi::SYS_MSYNC => syscall_linux_msync(frame.rdi, frame.rsi, frame.rdx),
        linux_abi::SYS_MEMBARRIER => syscall_linux_membarrier(frame.rdi, frame.rsi),
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
