//! Secondary Linux syscall dispatch: process-identity getters and the
//! kernel-local pipe/eventfd family. `dispatch_linux_syscall` falls through to
//! [`dispatch`] before reporting `ENOSYS`, which keeps the primary match under
//! its size budget without changing behaviour.
use super::*;

/// Handles the syscalls owned by this table; `None` means "not mine".
pub(super) fn dispatch(
    frame: &mut SyscallFrame,
    [rdi, rsi, rdx, r10, r8, r9]: [u64; 6],
) -> Option<u64> {
    Some(match frame.rax {
        linux_abi::SYS_PTRACE => {
            super::ptrace_refusal::syscall_linux_ptrace(rdi, rsi, rdx, r10)
        }
        linux_abi::SYS_FLOCK => super::file_lock_ops::syscall_linux_flock(rdi, rsi),
        linux_abi::SYS_PWRITE64 => {
            syscall_linux_vfs_pwrite64(rdi, rsi, rdx, r10)
        }
        linux_abi::SYS_READV => syscall_linux_vfs_readv(rdi, rsi, rdx),
        linux_abi::SYS_PREADV => {
            syscall_linux_vfs_preadv(rdi, rsi, rdx, r10)
        }
        linux_abi::SYS_PWRITEV => {
            syscall_linux_vfs_pwritev(rdi, rsi, rdx, r10)
        }
        linux_abi::SYS_PREADV2 => syscall_linux_vfs_preadv2(
            rdi, rsi, rdx, r10, r8, r9,
        ),
        linux_abi::SYS_PWRITEV2 => syscall_linux_vfs_pwritev2(
            rdi, rsi, rdx, r10, r8, r9,
        ),
        linux_abi::SYS_FSYNC => syscall_linux_vfs_fsync_mode(rdi, false),
        linux_abi::SYS_FDATASYNC => syscall_linux_vfs_fsync_mode(rdi, true),
        linux_abi::SYS_SYNC => syscall_linux_vfs_sync(),
        linux_abi::SYS_SYNCFS => syscall_linux_vfs_syncfs(rdi),
        linux_abi::SYS_TRUNCATE => syscall_linux_vfs_truncate(rdi, rsi),
        linux_abi::SYS_MKDIRAT => syscall_linux_vfs_mkdir(rdi, rsi, rdx),
        linux_abi::SYS_RMDIR => syscall_linux_vfs_unlinkat(
            linux_abi::AT_FDCWD as u64,
            rdi,
            linux_abi::AT_REMOVEDIR as u64,
        ),
        linux_abi::SYS_RENAME => syscall_linux_vfs_renameat2(
            linux_abi::AT_FDCWD as u64,
            rdi,
            linux_abi::AT_FDCWD as u64,
            rsi,
            0,
        ),
        linux_abi::SYS_RENAMEAT => {
            syscall_linux_vfs_renameat2(rdi, rsi, rdx, r10, 0)
        }
        linux_abi::SYS_RENAMEAT2 => {
            syscall_linux_vfs_renameat2(rdi, rsi, rdx, r10, r8)
        }
        linux_abi::SYS_SYMLINK => {
            syscall_linux_vfs_symlinkat(rdi, linux_abi::AT_FDCWD as u64, rsi)
        }
        linux_abi::SYS_SYMLINKAT => {
            syscall_linux_vfs_symlinkat(rdi, rsi, rdx)
        }
        linux_abi::SYS_STATFS => syscall_linux_vfs_statfs(rdi, rsi),
        linux_abi::SYS_FSTATFS => syscall_linux_vfs_fstatfs(rdi, rsi),
        linux_abi::SYS_FADVISE64 => {
            syscall_linux_vfs_fadvise64(rdi, rsi, rdx, r10)
        }
        linux_abi::SYS_FALLOCATE => {
            syscall_linux_vfs_fallocate(rdi, rsi, rdx, r10)
        }
        linux_abi::SYS_FCHMOD => syscall_linux_vfs_fchmod(rdi, rsi),
        linux_abi::SYS_CHMOD => syscall_linux_vfs_fchmodat(
            linux_abi::AT_FDCWD as u64,
            rdi,
            rsi,
            0,
        ),
        linux_abi::SYS_FCHMODAT => {
            syscall_linux_vfs_fchmodat(rdi, rsi, rdx, 0)
        }
        linux_abi::SYS_FCHMODAT2 => {
            syscall_linux_vfs_fchmodat(rdi, rsi, rdx, r10)
        }
        linux_abi::SYS_CHOWN => syscall_linux_vfs_fchownat(
            linux_abi::AT_FDCWD as u64,
            rdi,
            rsi,
            rdx,
            0,
        ),
        linux_abi::SYS_LCHOWN => syscall_linux_vfs_fchownat(
            linux_abi::AT_FDCWD as u64,
            rdi,
            rsi,
            rdx,
            linux_abi::AT_SYMLINK_NOFOLLOW,
        ),
        linux_abi::SYS_FCHOWN => syscall_linux_vfs_fchown(rdi, rsi, rdx),
        linux_abi::SYS_FCHOWNAT => {
            syscall_linux_vfs_fchownat(rdi, rsi, rdx, r10, r8)
        }
        linux_abi::SYS_MKNOD => syscall_linux_vfs_mknodat(
            linux_abi::AT_FDCWD as u64,
            rdi,
            rsi,
            rdx,
        ),
        linux_abi::SYS_MKNODAT => {
            syscall_linux_vfs_mknodat(rdi, rsi, rdx, r10)
        }
        linux_abi::SYS_LINK => syscall_linux_vfs_linkat(
            linux_abi::AT_FDCWD as u64,
            rdi,
            linux_abi::AT_FDCWD as u64,
            rsi,
            0,
        ),
        linux_abi::SYS_LINKAT => {
            syscall_linux_vfs_linkat(rdi, rsi, rdx, r10, r8)
        }
        linux_abi::SYS_UTIME => syscall_linux_vfs_utime(rdi, rsi),
        linux_abi::SYS_UTIMES => {
            syscall_linux_vfs_futimesat(linux_abi::AT_FDCWD as u64, rdi, rsi)
        }
        linux_abi::SYS_FUTIMESAT => {
            syscall_linux_vfs_futimesat(rdi, rsi, rdx)
        }
        linux_abi::SYS_UTIMENSAT => {
            syscall_linux_vfs_utimensat(rdi, rsi, rdx, r10)
        }
        linux_abi::SYS_SENDFILE => {
            syscall_linux_vfs_sendfile(rdi, rsi, rdx, r10)
        }
        linux_abi::SYS_COPY_FILE_RANGE => syscall_linux_vfs_copy_file_range(
            rdi, rsi, rdx, r10, r8, r9,
        ),
        linux_abi::SYS_CLOSE_RANGE => {
            syscall_linux_vfs_close_range(rdi, rsi, rdx)
        }
        linux_abi::SYS_PAUSE => syscall_linux_pause(),
        linux_abi::SYS_RT_SIGPENDING => syscall_linux_rt_sigpending(rdi, rsi),
        linux_abi::SYS_RT_SIGSUSPEND => syscall_linux_rt_sigsuspend(frame),
        linux_abi::SYS_GETTIMEOFDAY => syscall_linux_gettimeofday(rdi, rsi),
        linux_abi::SYS_CLOCK_GETRES => syscall_linux_clock_getres(rdi, rsi),
        linux_abi::SYS_GETRUSAGE => syscall_linux_getrusage(rdi as i64, rsi),
        linux_abi::SYS_SYSINFO => syscall_linux_sysinfo(rdi),
        linux_abi::SYS_TIMES => syscall_linux_times(rdi),
        linux_abi::SYS_GETCPU => syscall_linux_getcpu(rdi, rsi),
        linux_abi::SYS_SCHED_GETSCHEDULER => syscall_linux_sched_getscheduler(rdi as i64),
        linux_abi::SYS_SCHED_GETPARAM => syscall_linux_sched_getparam(rdi as i64, rsi),
        linux_abi::SYS_SCHED_GET_PRIORITY_MAX => {
            syscall_linux_sched_get_priority(rdi as i64, true)
        }
        linux_abi::SYS_SCHED_GET_PRIORITY_MIN => {
            syscall_linux_sched_get_priority(rdi as i64, false)
        }
        linux_abi::SYS_SCHED_SETSCHEDULER => {
            syscall_linux_sched_setscheduler(rdi as i64, rsi as i64, rdx)
        }
        linux_abi::SYS_SCHED_SETPARAM => syscall_linux_sched_setparam(rdi as i64, rsi),
        linux_abi::SYS_GETRLIMIT => syscall_linux_getrlimit(rdi, rsi),
        linux_abi::SYS_SETRLIMIT => syscall_linux_setrlimit(rdi, rsi),
        linux_abi::SYS_RT_SIGTIMEDWAIT => {
            syscall_linux_rt_sigtimedwait(rdi, rsi, rdx, r10)
        }
        linux_abi::SYS_EPOLL_PWAIT2 => {
            let (epfd, events, max, timeout, mask, size) =
                (rdi, rsi, rdx, r10, r8, r9);
            syscall_linux_epoll_pwait2(frame, epfd, events, max, timeout, mask, size)
        }
        linux_abi::SYS_SENDMMSG => {
            syscall_linux_sendmmsg(rdi, rsi, rdx, r10)
        }
        linux_abi::SYS_RECVMMSG => {
            syscall_linux_recvmmsg(rdi, rsi, rdx, r10, r8)
        }
        linux_abi::SYS_MREMAP => {
            syscall_linux_mremap(rdi, rsi, rdx, r10, r8)
        }
        linux_abi::SYS_MINCORE => syscall_linux_mincore(rdi, rsi, rdx),
        linux_abi::SYS_MLOCK => syscall_linux_mlock(rdi, rsi),
        linux_abi::SYS_MLOCK2 => syscall_linux_mlock2(rdi, rsi, rdx),
        linux_abi::SYS_MUNLOCK => syscall_linux_munlock(rdi, rsi),
        linux_abi::SYS_MLOCKALL => syscall_linux_mlockall(rdi),
        linux_abi::SYS_MUNLOCKALL => syscall_linux_munlockall(),
        linux_abi::SYS_MSYNC => syscall_linux_msync(rdi, rsi, rdx),
        linux_abi::SYS_MEMBARRIER => syscall_linux_membarrier(rdi, rsi),
        linux_abi::SYS_SCHED_RR_GET_INTERVAL => {
            syscall_linux_sched_rr_get_interval(rdi as i64, rsi)
        }
        linux_abi::SYS_SETRESUID => syscall_linux_syscalld_set_ids(
            SYSCALL_OFFLOAD_OP_LINUX_SETRESUID,
            [rdi, rsi, rdx],
        ),
        linux_abi::SYS_SETRESGID => syscall_linux_syscalld_set_ids(
            SYSCALL_OFFLOAD_OP_LINUX_SETRESGID,
            [rdi, rsi, rdx],
        ),
        linux_abi::SYS_SETREUID => {
            syscall_linux_syscalld_set_ids(SYSCALL_OFFLOAD_OP_LINUX_SETREUID, [rdi, rsi, 0])
        }
        linux_abi::SYS_SETREGID => {
            syscall_linux_syscalld_set_ids(SYSCALL_OFFLOAD_OP_LINUX_SETREGID, [rdi, rsi, 0])
        }
        linux_abi::SYS_GETGROUPS => {
            syscall_linux_syscalld_getgroups(rdi as i64, rsi)
        }
        linux_abi::SYS_GETRESUID => syscall_linux_syscalld_getres_id(
            SYSCALL_OFFLOAD_OP_LINUX_GETRESUID,
            rdi,
            rsi,
            rdx,
        ),
        linux_abi::SYS_GETRESGID => syscall_linux_syscalld_getres_id(
            SYSCALL_OFFLOAD_OP_LINUX_GETRESGID,
            rdi,
            rsi,
            rdx,
        ),
        linux_abi::SYS_GETPRIORITY => syscall_linux_syscalld_getpriority(rdi, rsi),
        linux_abi::SYS_SETPRIORITY => {
            syscall_linux_syscalld_setpriority(rdi, rsi, rdx as i64)
        }
        linux_abi::SYS_PIPE => syscall_linux_pipe(rdi),
        linux_abi::SYS_PIPE2 => syscall_linux_pipe2(rdi, rsi),
        linux_abi::SYS_EVENTFD => syscall_linux_eventfd(rdi),
        linux_abi::SYS_EVENTFD2 => syscall_linux_eventfd2(rdi, rsi),
        linux_abi::SYS_TIMERFD_CREATE => syscall_linux_timerfd_create(rdi, rsi),
        linux_abi::SYS_TIMERFD_SETTIME => {
            syscall_linux_timerfd_settime(rdi, rsi, rdx, r10)
        }
        linux_abi::SYS_TIMERFD_GETTIME => syscall_linux_timerfd_gettime(rdi, rsi),
        linux_abi::SYS_NAME_TO_HANDLE_AT => {
            syscall_linux_name_to_handle_at(rdi, rsi, rdx, r10, r8)
        }
        linux_abi::SYS_PKEY_MPROTECT => {
            syscall_linux_pkey_mprotect(rdi, rsi, rdx, r10)
        }
        linux_abi::SYS_PKEY_ALLOC => syscall_linux_pkey_alloc(rdi, rsi),
        linux_abi::SYS_PKEY_FREE => syscall_linux_pkey_free(rdi),
        linux_abi::SYS_GETPPID => {
            if let Some(ppid) = multitask::current_user_process_id()
                .and_then(multitask::parent_process_id_of)
                .filter(|&ppid| ppid != 0)
            {
                ppid
            } else {
                syscall_linux_syscalld_u64_getter(SYSCALL_OFFLOAD_OP_LINUX_GETPPID, 0)
            }
        }
        linux_abi::SYS_GETPGRP => {
            syscall_linux_syscalld_u64_getter(SYSCALL_OFFLOAD_OP_LINUX_GETPGID, 0)
        }
        linux_abi::SYS_FACCESSAT2 => {
            const KNOWN: u64 = linux_abi::AT_EACCESS as u64
                | linux_abi::AT_SYMLINK_NOFOLLOW as u64
                | linux_abi::AT_EMPTY_PATH as u64;
            if r10 & !KNOWN != 0 {
                linux_errno(LINUX_EINVAL)
            } else {
                syscall_linux_vfs_access(rdi, rsi, rdx, r10)
            }
        }
        linux_abi::SYS_GETPGID => {
            syscall_linux_syscalld_u64_getter(SYSCALL_OFFLOAD_OP_LINUX_GETPGID, rdi)
        }
        linux_abi::SYS_SETPGID => syscall_linux_syscalld_setpgid(rdi, rsi),
        linux_abi::SYS_GETSID => {
            syscall_linux_syscalld_u64_getter(SYSCALL_OFFLOAD_OP_LINUX_GETSID, rdi)
        }
        linux_abi::SYS_SETSID => {
            syscall_linux_syscalld_u64_getter(SYSCALL_OFFLOAD_OP_LINUX_SETSID, 0)
        }
        _ => return None,
    })
}
