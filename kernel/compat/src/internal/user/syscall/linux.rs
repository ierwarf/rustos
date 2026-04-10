mod fs_ops;
mod memory_ops;
mod network_ops;
mod process_ops;

use alloc::string::String;
use core::convert::TryFrom;
use core::sync::atomic::{AtomicUsize, Ordering};

use crate::debug;
use crate::memory::paging;
use crate::multitask;
use crate::user::linux as linux_abi;
use crate::user::sysops::linux as linux_ops;
use crate::user::sysops::usermem;

use super::SyscallFrame;

const LINUX_E2BIG: i64 = 7;
const LINUX_ENOEXEC: i64 = 8;
const LINUX_EINTR: i64 = 4;
const LINUX_EACCES: i64 = 13;
const LINUX_ENOMEM: i64 = 12;
const LINUX_ECHILD: i64 = 10;
const LINUX_EAGAIN: i64 = 11;
const LINUX_EBADF: i64 = 9;
const LINUX_ENOENT: i64 = 2;
const LINUX_ESRCH: i64 = 3;
const LINUX_EBUSY: i64 = 16;
const LINUX_EFAULT: i64 = 14;
const LINUX_EINVAL: i64 = 22;
const LINUX_ENODEV: i64 = 19;
const LINUX_ENOTDIR: i64 = 20;
const LINUX_ENOTSOCK: i64 = 88;
const LINUX_ENOTTY: i64 = 25;
const LINUX_EOPNOTSUPP: i64 = 95;
const LINUX_EAFNOSUPPORT: i64 = 97;
const LINUX_EADDRINUSE: i64 = 98;
const LINUX_EISCONN: i64 = 106;
const LINUX_ENOTCONN: i64 = 107;
const LINUX_ECONNREFUSED: i64 = 111;
const LINUX_EPIPE: i64 = 32;
const LINUX_EROFS: i64 = 30;
const LINUX_ESPIPE: i64 = 29;
const LINUX_ESTALE: i64 = 116;
const LINUX_ENOSYS: i64 = 38;
const SECONDARY_LINUX_SYSCALL_DEBUG_LIMIT: usize = 0;
const MAX_RUSTOS_DEBUG_PRINT_BYTES: usize = 2048;

static SECONDARY_LINUX_SYSCALL_DEBUG_LOGS: AtomicUsize = AtomicUsize::new(0);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[allow(dead_code)]
enum LinuxSyscallSupport {
    Native,
    Partial,
    Stub,
}

#[cfg_attr(not(rustos_debug_print_enabled), allow(unused_variables))]
fn debug_log_secondary_linux_syscall(message: impl FnOnce() -> alloc::string::String) {
    if multitask::current_console_session().is_system() {
        return;
    }

    if SECONDARY_LINUX_SYSCALL_DEBUG_LOGS.fetch_add(1, Ordering::Relaxed)
        >= SECONDARY_LINUX_SYSCALL_DEBUG_LIMIT
    {
        return;
    }

    let snapshot = multitask::current_user_snapshot();
    let pid = snapshot.map(|user| user.thread_id()).unwrap_or(0);
    let session = snapshot
        .map(|user| user.console_session())
        .unwrap_or_else(multitask::current_console_session);
    debug::println!(
        "secondary linux syscall: pid={} session={} {}",
        pid,
        session.raw(),
        message(),
    );
}

pub(super) fn dispatch_linux_syscall(frame: &mut SyscallFrame) -> u64 {
    if let Err(error) = syscall_check(frame) {
        return error;
    }

    linux_ops::deliver_pending_signals_for_current_thread();

    debug_log_secondary_linux_syscall(|| {
        alloc::format!(
            "nr={} rip={:#x} rdi={:#x} rsi={:#x} rdx={:#x} r10={:#x}",
            frame.rax,
            frame.user_rip,
            frame.rdi,
            frame.rsi,
            frame.rdx,
            frame.r10,
        )
    });

    let result = match frame.rax {
        linux_abi::SYS_READ => fs_ops::syscall_linux_read(frame.rdi, frame.rsi, frame.rdx),
        linux_abi::SYS_WRITE => fs_ops::syscall_linux_write(frame.rdi, frame.rsi, frame.rdx),
        linux_abi::SYS_UNLINK => fs_ops::syscall_linux_unlink(frame.rdi),
        linux_abi::SYS_RUSTOS_DEBUG_PRINT => syscall_linux_rustos_debug_print(frame.rdi, frame.rsi),
        linux_abi::SYS_RUSTOS_SPAWN_EXEC => process_ops::syscall_linux_rustos_spawn_exec(
            frame.rdi,
            frame.rsi,
            frame.rdx,
            frame.r10,
            frame.r8,
            frame.r9,
        ),
        linux_abi::SYS_CLOSE => fs_ops::syscall_linux_close(frame.rdi),
        linux_abi::SYS_SOCKET => network_ops::syscall_linux_socket(frame.rdi, frame.rsi, frame.rdx),
        linux_abi::SYS_SENDTO => network_ops::syscall_linux_sendto(
            frame.rdi, frame.rsi, frame.rdx, frame.r10, frame.r8, frame.r9,
        ),
        linux_abi::SYS_ACCEPT => network_ops::syscall_linux_accept(frame.rdi, frame.rsi, frame.rdx),
        linux_abi::SYS_FTRUNCATE => fs_ops::syscall_linux_ftruncate(frame.rdi, frame.rsi),
        linux_abi::SYS_FSTAT => fs_ops::syscall_linux_fstat(frame.rdi, frame.rsi),
        linux_abi::SYS_POLL => {
            fs_ops::syscall_linux_poll(frame.rdi, frame.rsi, (frame.rdx as u32) as i32)
        }
        linux_abi::SYS_PPOLL => {
            fs_ops::syscall_linux_ppoll(frame.rdi, frame.rsi, frame.rdx, frame.r10, frame.r8)
        }
        linux_abi::SYS_EPOLL_WAIT => fs_ops::syscall_linux_epoll_wait(
            frame.rdi,
            frame.rsi,
            frame.rdx,
            (frame.r10 as u32) as i32,
        ),
        linux_abi::SYS_EPOLL_PWAIT => fs_ops::syscall_linux_epoll_pwait(
            frame.rdi,
            frame.rsi,
            frame.rdx,
            (frame.r10 as u32) as i32,
            frame.r8,
            frame.r9,
        ),
        linux_abi::SYS_EPOLL_CTL => {
            fs_ops::syscall_linux_epoll_ctl(frame.rdi, frame.rsi, frame.rdx, frame.r10)
        }
        linux_abi::SYS_DUP => fs_ops::syscall_linux_dup(frame.rdi),
        linux_abi::SYS_DUP2 => fs_ops::syscall_linux_dup2(frame.rdi, frame.rsi),
        linux_abi::SYS_LSEEK => fs_ops::syscall_linux_lseek(frame.rdi, frame.rsi as i64, frame.rdx),
        linux_abi::SYS_WRITEV => fs_ops::syscall_linux_writev(frame.rdi, frame.rsi, frame.rdx),
        linux_abi::SYS_ACCESS => fs_ops::syscall_linux_access(frame.rdi, frame.rsi),
        linux_abi::SYS_SCHED_YIELD => process_ops::syscall_linux_sched_yield(),
        linux_abi::SYS_MOUNT => {
            fs_ops::syscall_linux_mount(frame.rdi, frame.rsi, frame.rdx, frame.r10, frame.r8)
        }
        linux_abi::SYS_GETCWD => fs_ops::syscall_linux_getcwd(frame.rdi, frame.rsi),
        linux_abi::SYS_MKDIR => fs_ops::syscall_linux_mkdir(frame.rdi, frame.rsi),
        linux_abi::SYS_CHDIR => fs_ops::syscall_linux_chdir(frame.rdi),
        linux_abi::SYS_MMAP => memory_ops::syscall_linux_mmap(
            frame.rdi, frame.rsi, frame.rdx, frame.r10, frame.r8, frame.r9,
        ),
        linux_abi::SYS_MPROTECT => {
            memory_ops::syscall_linux_mprotect(frame.rdi, frame.rsi, frame.rdx)
        }
        linux_abi::SYS_MUNMAP => memory_ops::syscall_linux_munmap(frame.rdi, frame.rsi),
        linux_abi::SYS_MADVISE => {
            memory_ops::syscall_linux_madvise(frame.rdi, frame.rsi, frame.rdx)
        }
        linux_abi::SYS_SIGALTSTACK => memory_ops::syscall_linux_sigaltstack(frame.rdi, frame.rsi),
        linux_abi::SYS_BRK => memory_ops::syscall_linux_brk(frame.rdi),
        linux_abi::SYS_RT_SIGACTION => {
            process_ops::syscall_linux_rt_sigaction(frame.rdi, frame.rsi, frame.rdx, frame.r10)
        }
        linux_abi::SYS_RT_SIGPROCMASK => {
            process_ops::syscall_linux_rt_sigprocmask(frame.rdi, frame.rsi, frame.rdx, frame.r10)
        }
        linux_abi::SYS_IOCTL => fs_ops::syscall_linux_ioctl(frame.rdi, frame.rsi, frame.rdx),
        linux_abi::SYS_PREAD64 => {
            fs_ops::syscall_linux_pread64(frame.rdi, frame.rsi, frame.rdx, frame.r10)
        }
        linux_abi::SYS_NANOSLEEP => process_ops::syscall_linux_nanosleep(frame.rdi, frame.rsi),
        linux_abi::SYS_GETPID => process_ops::syscall_linux_getpid(),
        linux_abi::SYS_FORK => process_ops::syscall_linux_fork(frame),
        linux_abi::SYS_WAIT4 => {
            process_ops::syscall_linux_wait4(frame.rdi as i64, frame.rsi, frame.rdx, frame.r10)
        }
        linux_abi::SYS_CLONE => process_ops::syscall_linux_clone(frame),
        linux_abi::SYS_UNAME => process_ops::syscall_linux_uname(frame.rdi),
        linux_abi::SYS_GETTID => process_ops::syscall_linux_gettid(),
        linux_abi::SYS_SCHED_GETAFFINITY => {
            process_ops::syscall_linux_sched_getaffinity(frame.rdi, frame.rsi, frame.rdx)
        }
        linux_abi::SYS_GETUID => process_ops::syscall_linux_getuid(),
        linux_abi::SYS_GETGID => process_ops::syscall_linux_getgid(),
        linux_abi::SYS_GETEUID => process_ops::syscall_linux_geteuid(),
        linux_abi::SYS_GETEGID => process_ops::syscall_linux_getegid(),
        linux_abi::SYS_SETUID => process_ops::syscall_linux_setuid(frame.rdi),
        linux_abi::SYS_SETGID => process_ops::syscall_linux_setgid(frame.rdi),
        linux_abi::SYS_FCNTL => fs_ops::syscall_linux_fcntl(frame.rdi, frame.rsi, frame.rdx),
        linux_abi::SYS_READLINK => fs_ops::syscall_linux_readlink(frame.rdi, frame.rsi, frame.rdx),
        linux_abi::SYS_FUTEX => process_ops::syscall_linux_futex(
            frame, frame.rdi, frame.rsi, frame.rdx, frame.r10, frame.r8, frame.r9,
        ),
        linux_abi::SYS_ARCH_PRCTL => process_ops::syscall_linux_arch_prctl(frame.rdi, frame.rsi),
        linux_abi::SYS_SET_TID_ADDRESS => process_ops::syscall_linux_set_tid_address(frame.rdi),
        linux_abi::SYS_CLOCK_GETTIME => {
            process_ops::syscall_linux_clock_gettime(frame.rdi, frame.rsi)
        }
        linux_abi::SYS_CLOCK_NANOSLEEP => {
            process_ops::syscall_linux_clock_nanosleep(frame.rdi, frame.rsi, frame.rdx, frame.r10)
        }
        linux_abi::SYS_EXECVE => process_ops::syscall_linux_execve(frame),
        linux_abi::SYS_TGKILL => process_ops::syscall_linux_tgkill(frame.rdi, frame.rsi, frame.rdx),
        linux_abi::SYS_OPENAT => {
            fs_ops::syscall_linux_openat(frame.rdi, frame.rsi, frame.rdx, frame.r10)
        }
        linux_abi::SYS_UNLINKAT => fs_ops::syscall_linux_unlinkat(frame.rdi, frame.rsi, frame.rdx),
        linux_abi::SYS_SOCKETPAIR => {
            network_ops::syscall_linux_socketpair(frame.rdi, frame.rsi, frame.rdx, frame.r10)
        }
        linux_abi::SYS_BIND => network_ops::syscall_linux_bind(frame.rdi, frame.rsi, frame.rdx),
        linux_abi::SYS_CONNECT => {
            network_ops::syscall_linux_connect(frame.rdi, frame.rsi, frame.rdx)
        }
        linux_abi::SYS_LISTEN => network_ops::syscall_linux_listen(frame.rdi, frame.rsi),
        linux_abi::SYS_ACCEPT4 => {
            network_ops::syscall_linux_accept4(frame.rdi, frame.rsi, frame.rdx, frame.r10)
        }
        linux_abi::SYS_GETSOCKNAME => {
            network_ops::syscall_linux_getsockname(frame.rdi, frame.rsi, frame.rdx)
        }
        linux_abi::SYS_GETPEERNAME => {
            network_ops::syscall_linux_getpeername(frame.rdi, frame.rsi, frame.rdx)
        }
        linux_abi::SYS_SETSOCKOPT => network_ops::syscall_linux_setsockopt(
            frame.rdi, frame.rsi, frame.rdx, frame.r10, frame.r8,
        ),
        linux_abi::SYS_GETSOCKOPT => network_ops::syscall_linux_getsockopt(
            frame.rdi, frame.rsi, frame.rdx, frame.r10, frame.r8,
        ),
        linux_abi::SYS_SHUTDOWN => network_ops::syscall_linux_shutdown(frame.rdi, frame.rsi),
        linux_abi::SYS_SENDMSG => {
            network_ops::syscall_linux_sendmsg(frame.rdi, frame.rsi, frame.rdx)
        }
        linux_abi::SYS_RECVFROM => network_ops::syscall_linux_recvfrom(
            frame.rdi, frame.rsi, frame.rdx, frame.r10, frame.r8, frame.r9,
        ),
        linux_abi::SYS_RECVMSG => {
            network_ops::syscall_linux_recvmsg(frame.rdi, frame.rsi, frame.rdx)
        }
        linux_abi::SYS_GETDENTS64 => {
            fs_ops::syscall_linux_getdents64(frame.rdi, frame.rsi, frame.rdx)
        }
        linux_abi::SYS_EXECVEAT => process_ops::syscall_linux_execveat(frame),
        linux_abi::SYS_NEWFSTATAT => {
            fs_ops::syscall_linux_newfstatat(frame.rdi, frame.rsi, frame.rdx, frame.r10)
        }
        linux_abi::SYS_UMOUNT2 => fs_ops::syscall_linux_umount2(frame.rdi, frame.rsi),
        linux_abi::SYS_READLINKAT => {
            fs_ops::syscall_linux_readlinkat(frame.rdi, frame.rsi, frame.rdx, frame.r10)
        }
        linux_abi::SYS_FACCESSAT => {
            fs_ops::syscall_linux_faccessat(frame.rdi, frame.rsi, frame.rdx, frame.r10)
        }
        linux_abi::SYS_SET_ROBUST_LIST => {
            process_ops::syscall_linux_set_robust_list(frame.rdi, frame.rsi)
        }
        linux_abi::SYS_GET_ROBUST_LIST => {
            process_ops::syscall_linux_get_robust_list(frame.rdi, frame.rsi, frame.rdx)
        }
        linux_abi::SYS_DUP3 => fs_ops::syscall_linux_dup3(frame.rdi, frame.rsi, frame.rdx),
        linux_abi::SYS_EPOLL_CREATE1 => fs_ops::syscall_linux_epoll_create1(frame.rdi),
        linux_abi::SYS_PRLIMIT64 => {
            process_ops::syscall_linux_prlimit64(frame.rdi, frame.rsi, frame.rdx, frame.r10)
        }
        linux_abi::SYS_GETRANDOM => {
            process_ops::syscall_linux_getrandom(frame.rdi, frame.rsi, frame.rdx)
        }
        linux_abi::SYS_STATX => {
            process_ops::syscall_linux_statx(frame.rdi, frame.rsi, frame.rdx, frame.r10, frame.r8)
        }
        linux_abi::SYS_RSEQ => {
            process_ops::syscall_linux_rseq(frame.rdi, frame.rsi, frame.rdx, frame.r10)
        }
        linux_abi::SYS_CLONE3 => process_ops::syscall_linux_clone3(frame),
        linux_abi::SYS_MEMFD_CREATE => {
            process_ops::syscall_linux_memfd_create(frame.rdi, frame.rsi)
        }
        linux_abi::SYS_EXIT | linux_abi::SYS_EXIT_GROUP => syscall_process_exit(frame.rdi),
        _ => unreachable!("linux syscall_check allowed an unknown syscall"),
    };
    linux_ops::deliver_pending_signals_for_current_thread();
    result
}

fn syscall_check(frame: &SyscallFrame) -> Result<(), u64> {
    let Some(support) = linux_syscall_support_level(frame.rax) else {
        debug::println!(
            "unsupported linux syscall: nr={} rip={:#x} rdi={:#x} rsi={:#x} rdx={:#x} r10={:#x}",
            frame.rax,
            frame.user_rip,
            frame.rdi,
            frame.rsi,
            frame.rdx,
            frame.r10,
        );
        return Err(linux_errno(LINUX_ENOSYS));
    };

    if !super::syscall_frame_security_check(frame) {
        super::validate_syscall_entry_or_terminate(frame);
    }

    if support == LinuxSyscallSupport::Stub {
        return Err(linux_errno(LINUX_ENOSYS));
    }

    Ok(())
}

fn linux_syscall_support_level(syscall_number: u64) -> Option<LinuxSyscallSupport> {
    if !linux_syscall_number_supported(syscall_number) {
        return None;
    }

    Some(match syscall_number {
        linux_abi::SYS_PPOLL
        | linux_abi::SYS_EPOLL_PWAIT
        | linux_abi::SYS_SIGALTSTACK
        | linux_abi::SYS_RT_SIGACTION
        | linux_abi::SYS_RT_SIGPROCMASK
        | linux_abi::SYS_GET_ROBUST_LIST
        | linux_abi::SYS_FORK
        | linux_abi::SYS_CLONE
        | linux_abi::SYS_FUTEX
        | linux_abi::SYS_TGKILL
        | linux_abi::SYS_SET_ROBUST_LIST
        | linux_abi::SYS_RSEQ
        | linux_abi::SYS_WAIT4
        | linux_abi::SYS_CLONE3 => LinuxSyscallSupport::Partial,
        linux_abi::SYS_RT_SIGRETURN => LinuxSyscallSupport::Stub,
        _ => LinuxSyscallSupport::Native,
    })
}

fn linux_syscall_number_supported(syscall_number: u64) -> bool {
    matches!(
        syscall_number,
        linux_abi::SYS_READ
            | linux_abi::SYS_WRITE
            | linux_abi::SYS_CLOSE
            | linux_abi::SYS_UNLINK
            | linux_abi::SYS_SOCKET
            | linux_abi::SYS_SENDTO
            | linux_abi::SYS_ACCEPT
            | linux_abi::SYS_FTRUNCATE
            | linux_abi::SYS_FSTAT
            | linux_abi::SYS_POLL
            | linux_abi::SYS_PPOLL
            | linux_abi::SYS_EPOLL_WAIT
            | linux_abi::SYS_EPOLL_PWAIT
            | linux_abi::SYS_EPOLL_CTL
            | linux_abi::SYS_DUP
            | linux_abi::SYS_DUP2
            | linux_abi::SYS_LSEEK
            | linux_abi::SYS_WRITEV
            | linux_abi::SYS_ACCESS
            | linux_abi::SYS_SCHED_YIELD
            | linux_abi::SYS_MOUNT
            | linux_abi::SYS_GETCWD
            | linux_abi::SYS_MKDIR
            | linux_abi::SYS_CHDIR
            | linux_abi::SYS_MMAP
            | linux_abi::SYS_MPROTECT
            | linux_abi::SYS_MUNMAP
            | linux_abi::SYS_MADVISE
            | linux_abi::SYS_SIGALTSTACK
            | linux_abi::SYS_BRK
            | linux_abi::SYS_RT_SIGACTION
            | linux_abi::SYS_RT_SIGPROCMASK
            | linux_abi::SYS_RT_SIGRETURN
            | linux_abi::SYS_IOCTL
            | linux_abi::SYS_PREAD64
            | linux_abi::SYS_NANOSLEEP
            | linux_abi::SYS_GETPID
            | linux_abi::SYS_FORK
            | linux_abi::SYS_WAIT4
            | linux_abi::SYS_CLONE
            | linux_abi::SYS_UNAME
            | linux_abi::SYS_GETTID
            | linux_abi::SYS_SCHED_GETAFFINITY
            | linux_abi::SYS_GETUID
            | linux_abi::SYS_GETGID
            | linux_abi::SYS_GETEUID
            | linux_abi::SYS_GETEGID
            | linux_abi::SYS_SETUID
            | linux_abi::SYS_SETGID
            | linux_abi::SYS_FCNTL
            | linux_abi::SYS_READLINK
            | linux_abi::SYS_FUTEX
            | linux_abi::SYS_ARCH_PRCTL
            | linux_abi::SYS_EXECVE
            | linux_abi::SYS_SET_TID_ADDRESS
            | linux_abi::SYS_CLOCK_GETTIME
            | linux_abi::SYS_CLOCK_NANOSLEEP
            | linux_abi::SYS_TGKILL
            | linux_abi::SYS_OPENAT
            | linux_abi::SYS_UNLINKAT
            | linux_abi::SYS_SOCKETPAIR
            | linux_abi::SYS_BIND
            | linux_abi::SYS_CONNECT
            | linux_abi::SYS_LISTEN
            | linux_abi::SYS_ACCEPT4
            | linux_abi::SYS_GETSOCKNAME
            | linux_abi::SYS_GETPEERNAME
            | linux_abi::SYS_SETSOCKOPT
            | linux_abi::SYS_GETSOCKOPT
            | linux_abi::SYS_SHUTDOWN
            | linux_abi::SYS_SENDMSG
            | linux_abi::SYS_RECVFROM
            | linux_abi::SYS_RECVMSG
            | linux_abi::SYS_GETDENTS64
            | linux_abi::SYS_EXECVEAT
            | linux_abi::SYS_NEWFSTATAT
            | linux_abi::SYS_UMOUNT2
            | linux_abi::SYS_READLINKAT
            | linux_abi::SYS_FACCESSAT
            | linux_abi::SYS_SET_ROBUST_LIST
            | linux_abi::SYS_GET_ROBUST_LIST
            | linux_abi::SYS_DUP3
            | linux_abi::SYS_EPOLL_CREATE1
            | linux_abi::SYS_PRLIMIT64
            | linux_abi::SYS_GETRANDOM
            | linux_abi::SYS_STATX
            | linux_abi::SYS_RSEQ
            | linux_abi::SYS_CLONE3
            | linux_abi::SYS_MEMFD_CREATE
            | linux_abi::SYS_RUSTOS_DEBUG_PRINT
            | linux_abi::SYS_RUSTOS_SPAWN_EXEC
            | linux_abi::SYS_EXIT
            | linux_abi::SYS_EXIT_GROUP
    )
}

fn syscall_process_exit(status: u64) -> u64 {
    linux_ops::exit_current_process(status)
}

fn syscall_linux_rustos_debug_print(user_ptr: u64, user_len: u64) -> u64 {
    let requested_len = match usize::try_from(user_len) {
        Ok(len) => len,
        Err(_) => return linux_errno(LINUX_EINVAL),
    };
    if requested_len == 0 {
        return 0;
    }

    let len = requested_len.min(MAX_RUSTOS_DEBUG_PRINT_BYTES);
    let mut written = 0usize;
    let mut chunk = [0_u8; 256];
    while written < len {
        let chunk_len = (len - written).min(chunk.len());
        let ptr = match user_ptr.checked_add(written as u64) {
            Some(ptr) => ptr,
            None => return linux_errno(LINUX_EINVAL),
        };
        if let Err(err) = usermem::copy_from_current_user_exact(ptr, &mut chunk[..chunk_len]) {
            return linux_errno(address_space_error_to_linux_errno(err));
        }
        debug::write_bytes(&chunk[..chunk_len]);
        written += chunk_len;
    }
    written as u64
}

fn linux_errno(errno: i64) -> u64 {
    (-errno) as u64
}

#[cfg_attr(not(rustos_debug_print_enabled), allow(dead_code))]
fn debug_user_path(path_ptr: u64) -> String {
    match usermem::read_current_user_c_string(path_ptr, 256) {
        Ok(path) => path,
        Err(_) => String::from("<invalid>"),
    }
}

fn address_space_error_to_linux_errno(err: paging::AddressSpaceError) -> i64 {
    match err {
        paging::AddressSpaceError::ProtectionViolation
        | paging::AddressSpaceError::NotMapped
        | paging::AddressSpaceError::HugePageConflict
        | paging::AddressSpaceError::InvalidFrameOwnership => LINUX_EFAULT,
        paging::AddressSpaceError::ZeroSizedAllocation
        | paging::AddressSpaceError::AddressOverflow
        | paging::AddressSpaceError::AddressOutOfRange
        | paging::AddressSpaceError::AddressNotPageAligned
        | paging::AddressSpaceError::AlreadyMapped => LINUX_EINVAL,
        paging::AddressSpaceError::OutOfFrames => LINUX_ENOMEM,
    }
}

fn linux_sysop_error_to_errno(err: linux_ops::LinuxSysopError) -> i64 {
    match err {
        linux_ops::LinuxSysopError::AddressSpace(err) => address_space_error_to_linux_errno(err),
        linux_ops::LinuxSysopError::AddressFamilyNotSupported => LINUX_EAFNOSUPPORT,
        linux_ops::LinuxSysopError::AddressInUse => LINUX_EADDRINUSE,
        linux_ops::LinuxSysopError::AlreadyConnected => LINUX_EISCONN,
        linux_ops::LinuxSysopError::BadFileDescriptor => LINUX_EBADF,
        linux_ops::LinuxSysopError::Busy => LINUX_EBUSY,
        linux_ops::LinuxSysopError::BrokenPipe => LINUX_EPIPE,
        linux_ops::LinuxSysopError::ConnectionRefused => LINUX_ECONNREFUSED,
        linux_ops::LinuxSysopError::DisplayUnavailable => LINUX_ENODEV,
        linux_ops::LinuxSysopError::ExecFormat => LINUX_ENOEXEC,
        linux_ops::LinuxSysopError::IllegalSeek => LINUX_ESPIPE,
        linux_ops::LinuxSysopError::Interrupted => LINUX_EINTR,
        linux_ops::LinuxSysopError::InvalidArgument => LINUX_EINVAL,
        linux_ops::LinuxSysopError::NoMemory => LINUX_ENOMEM,
        linux_ops::LinuxSysopError::NotFound => LINUX_ENOENT,
        linux_ops::LinuxSysopError::NotDirectory => LINUX_ENOTDIR,
        linux_ops::LinuxSysopError::NotConnected => LINUX_ENOTCONN,
        linux_ops::LinuxSysopError::NotSocket => LINUX_ENOTSOCK,
        linux_ops::LinuxSysopError::NoSuchProcess => LINUX_ESRCH,
        linux_ops::LinuxSysopError::NotTty => LINUX_ENOTTY,
        linux_ops::LinuxSysopError::OperationNotSupported => LINUX_EOPNOTSUPP,
        linux_ops::LinuxSysopError::PermissionDenied => LINUX_EACCES,
        linux_ops::LinuxSysopError::ReadOnlyFilesystem => LINUX_EROFS,
        linux_ops::LinuxSysopError::Stale => LINUX_ESTALE,
        linux_ops::LinuxSysopError::TooBig => LINUX_E2BIG,
        linux_ops::LinuxSysopError::TryAgain => LINUX_EAGAIN,
        linux_ops::LinuxSysopError::Unsupported => LINUX_ENOSYS,
    }
}

#[cfg(test)]
mod tests {
    use super::{LinuxSyscallSupport, linux_syscall_support_level};
    use crate::user::linux as linux_abi;

    #[test]
    fn syscall_support_matrix_marks_runtime_fragile_calls_as_partial() {
        assert_eq!(
            linux_syscall_support_level(linux_abi::SYS_FUTEX),
            Some(LinuxSyscallSupport::Partial)
        );
        assert_eq!(
            linux_syscall_support_level(linux_abi::SYS_GET_ROBUST_LIST),
            Some(LinuxSyscallSupport::Partial)
        );
        assert_eq!(
            linux_syscall_support_level(linux_abi::SYS_CLONE3),
            Some(LinuxSyscallSupport::Partial)
        );
        assert_eq!(
            linux_syscall_support_level(linux_abi::SYS_RT_SIGRETURN),
            Some(LinuxSyscallSupport::Stub)
        );
        assert_eq!(
            linux_syscall_support_level(linux_abi::SYS_READ),
            Some(LinuxSyscallSupport::Native)
        );
        assert_eq!(
            linux_syscall_support_level(linux_abi::SYS_RECVFROM),
            Some(LinuxSyscallSupport::Native)
        );
        assert_eq!(linux_syscall_support_level(u64::MAX), None);
    }
}
