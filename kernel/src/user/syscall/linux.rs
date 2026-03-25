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

const LINUX_EPERM: i64 = 1;
const LINUX_EACCES: i64 = 13;
const LINUX_ENOMEM: i64 = 12;
const LINUX_EAGAIN: i64 = 11;
const LINUX_EBADF: i64 = 9;
const LINUX_ENOENT: i64 = 2;
const LINUX_ESRCH: i64 = 3;
const LINUX_EBUSY: i64 = 16;
const LINUX_EFAULT: i64 = 14;
const LINUX_EINVAL: i64 = 22;
const LINUX_ENODEV: i64 = 19;
const LINUX_ENOTDIR: i64 = 20;
const LINUX_ENOTTY: i64 = 25;
const LINUX_EROFS: i64 = 30;
const LINUX_ESPIPE: i64 = 29;
const LINUX_ESTALE: i64 = 116;
const LINUX_ENOSYS: i64 = 38;
const SECONDARY_LINUX_SYSCALL_DEBUG_LIMIT: usize = 0;

static SECONDARY_LINUX_SYSCALL_DEBUG_LOGS: AtomicUsize = AtomicUsize::new(0);

fn debug_log_secondary_linux_syscall(message: impl FnOnce() -> alloc::string::String) {
    if multitask::current_console_session().is_system() {
        return;
    }

    if SECONDARY_LINUX_SYSCALL_DEBUG_LOGS.fetch_add(1, Ordering::Relaxed)
        >= SECONDARY_LINUX_SYSCALL_DEBUG_LIMIT
    {
        return;
    }

    let pid = multitask::current_user_id().unwrap_or(0);
    let session = multitask::current_console_session();
    debug::println!(
        "secondary linux syscall: pid={} session={} {}",
        pid,
        session.raw(),
        message(),
    );
}

pub(super) fn dispatch_linux_syscall(frame: &SyscallFrame) -> u64 {
    if let Err(error) = syscall_check(frame) {
        return error;
    }

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

    match frame.rax {
        linux_abi::SYS_READ => syscall_linux_read(frame.rdi, frame.rsi, frame.rdx),
        linux_abi::SYS_WRITE => syscall_linux_write(frame.rdi, frame.rsi, frame.rdx),
        linux_abi::SYS_CLOSE => syscall_linux_close(frame.rdi),
        linux_abi::SYS_FSTAT => syscall_linux_fstat(frame.rdi, frame.rsi),
        linux_abi::SYS_POLL => syscall_linux_poll(frame.rdi, frame.rsi, (frame.rdx as u32) as i32),
        linux_abi::SYS_DUP => syscall_linux_dup(frame.rdi),
        linux_abi::SYS_DUP2 => syscall_linux_dup2(frame.rdi, frame.rsi),
        linux_abi::SYS_LSEEK => syscall_linux_lseek(frame.rdi, frame.rsi as i64, frame.rdx),
        linux_abi::SYS_WRITEV => syscall_linux_writev(frame.rdi, frame.rsi, frame.rdx),
        linux_abi::SYS_ACCESS => syscall_linux_access(frame.rdi, frame.rsi),
        linux_abi::SYS_SCHED_YIELD => syscall_linux_sched_yield(),
        linux_abi::SYS_GETCWD => syscall_linux_getcwd(frame.rdi, frame.rsi),
        linux_abi::SYS_MMAP => syscall_linux_mmap(
            frame.rdi, frame.rsi, frame.rdx, frame.r10, frame.r8, frame.r9,
        ),
        linux_abi::SYS_MPROTECT => syscall_linux_mprotect(frame.rdi, frame.rsi, frame.rdx),
        linux_abi::SYS_MUNMAP => syscall_linux_munmap(frame.rdi, frame.rsi),
        linux_abi::SYS_SIGALTSTACK => syscall_linux_sigaltstack(frame.rdi, frame.rsi),
        linux_abi::SYS_BRK => syscall_linux_brk(frame.rdi),
        linux_abi::SYS_RT_SIGACTION => {
            syscall_linux_rt_sigaction(frame.rdi, frame.rsi, frame.rdx, frame.r10)
        }
        linux_abi::SYS_RT_SIGPROCMASK => {
            syscall_linux_rt_sigprocmask(frame.rdi, frame.rsi, frame.rdx, frame.r10)
        }
        linux_abi::SYS_IOCTL => syscall_linux_ioctl(frame.rdi, frame.rsi, frame.rdx),
        linux_abi::SYS_PREAD64 => syscall_linux_pread64(frame.rdi, frame.rsi, frame.rdx, frame.r10),
        linux_abi::SYS_NANOSLEEP => syscall_linux_nanosleep(frame.rdi, frame.rsi),
        linux_abi::SYS_GETPID => syscall_linux_getpid(),
        linux_abi::SYS_CLONE => syscall_linux_clone(frame),
        linux_abi::SYS_UNAME => syscall_linux_uname(frame.rdi),
        linux_abi::SYS_GETTID => syscall_linux_gettid(),
        linux_abi::SYS_SCHED_GETAFFINITY => {
            syscall_linux_sched_getaffinity(frame.rdi, frame.rsi, frame.rdx)
        }
        linux_abi::SYS_GETUID => syscall_linux_getuid(),
        linux_abi::SYS_GETGID => syscall_linux_getgid(),
        linux_abi::SYS_GETEUID => syscall_linux_geteuid(),
        linux_abi::SYS_GETEGID => syscall_linux_getegid(),
        linux_abi::SYS_FCNTL => syscall_linux_fcntl(frame.rdi, frame.rsi, frame.rdx),
        linux_abi::SYS_READLINK => syscall_linux_readlink(frame.rdi, frame.rsi, frame.rdx),
        linux_abi::SYS_FUTEX => syscall_linux_futex(
            frame, frame.rdi, frame.rsi, frame.rdx, frame.r10, frame.r8, frame.r9,
        ),
        linux_abi::SYS_ARCH_PRCTL => syscall_linux_arch_prctl(frame.rdi, frame.rsi),
        linux_abi::SYS_SET_TID_ADDRESS => syscall_linux_set_tid_address(frame.rdi),
        linux_abi::SYS_CLOCK_GETTIME => syscall_linux_clock_gettime(frame.rdi, frame.rsi),
        linux_abi::SYS_CLOCK_NANOSLEEP => {
            syscall_linux_clock_nanosleep(frame.rdi, frame.rsi, frame.rdx, frame.r10)
        }
        linux_abi::SYS_TGKILL => syscall_linux_tgkill(frame.rdi, frame.rsi, frame.rdx),
        linux_abi::SYS_OPENAT => syscall_linux_openat(frame.rdi, frame.rsi, frame.rdx, frame.r10),
        linux_abi::SYS_NEWFSTATAT => {
            syscall_linux_newfstatat(frame.rdi, frame.rsi, frame.rdx, frame.r10)
        }
        linux_abi::SYS_READLINKAT => {
            syscall_linux_readlinkat(frame.rdi, frame.rsi, frame.rdx, frame.r10)
        }
        linux_abi::SYS_FACCESSAT => {
            syscall_linux_faccessat(frame.rdi, frame.rsi, frame.rdx, frame.r10)
        }
        linux_abi::SYS_SET_ROBUST_LIST => syscall_linux_set_robust_list(frame.rdi, frame.rsi),
        linux_abi::SYS_DUP3 => syscall_linux_dup3(frame.rdi, frame.rsi, frame.rdx),
        linux_abi::SYS_PRLIMIT64 => {
            syscall_linux_prlimit64(frame.rdi, frame.rsi, frame.rdx, frame.r10)
        }
        linux_abi::SYS_GETRANDOM => syscall_linux_getrandom(frame.rdi, frame.rsi, frame.rdx),
        linux_abi::SYS_STATX => {
            syscall_linux_statx(frame.rdi, frame.rsi, frame.rdx, frame.r10, frame.r8)
        }
        linux_abi::SYS_RSEQ => syscall_linux_rseq(frame.rdi, frame.rsi, frame.rdx, frame.r10),
        linux_abi::SYS_CLONE3 => syscall_linux_clone3(frame),
        linux_abi::SYS_EXIT | linux_abi::SYS_EXIT_GROUP => syscall_process_exit(frame.rdi),
        _ => unreachable!("linux syscall_check allowed an unknown syscall"),
    }
}

fn syscall_check(frame: &SyscallFrame) -> Result<(), u64> {
    if !linux_syscall_number_supported(frame.rax) {
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
    }

    if !super::syscall_frame_security_check(frame) {
        debug::println!(
            "rejected unsafe linux syscall: nr={} rip={:#x} rsp={:#x} rflags={:#x}",
            frame.rax,
            frame.user_rip,
            frame.user_rsp,
            frame.user_rflags,
        );
        return Err(linux_errno(LINUX_EPERM));
    }

    Ok(())
}

fn linux_syscall_number_supported(syscall_number: u64) -> bool {
    matches!(
        syscall_number,
        linux_abi::SYS_READ
            | linux_abi::SYS_WRITE
            | linux_abi::SYS_CLOSE
            | linux_abi::SYS_FSTAT
            | linux_abi::SYS_POLL
            | linux_abi::SYS_DUP
            | linux_abi::SYS_DUP2
            | linux_abi::SYS_LSEEK
            | linux_abi::SYS_WRITEV
            | linux_abi::SYS_ACCESS
            | linux_abi::SYS_SCHED_YIELD
            | linux_abi::SYS_GETCWD
            | linux_abi::SYS_MMAP
            | linux_abi::SYS_MPROTECT
            | linux_abi::SYS_MUNMAP
            | linux_abi::SYS_SIGALTSTACK
            | linux_abi::SYS_BRK
            | linux_abi::SYS_RT_SIGACTION
            | linux_abi::SYS_RT_SIGPROCMASK
            | linux_abi::SYS_IOCTL
            | linux_abi::SYS_PREAD64
            | linux_abi::SYS_NANOSLEEP
            | linux_abi::SYS_GETPID
            | linux_abi::SYS_CLONE
            | linux_abi::SYS_UNAME
            | linux_abi::SYS_GETTID
            | linux_abi::SYS_SCHED_GETAFFINITY
            | linux_abi::SYS_GETUID
            | linux_abi::SYS_GETGID
            | linux_abi::SYS_GETEUID
            | linux_abi::SYS_GETEGID
            | linux_abi::SYS_FCNTL
            | linux_abi::SYS_READLINK
            | linux_abi::SYS_FUTEX
            | linux_abi::SYS_ARCH_PRCTL
            | linux_abi::SYS_SET_TID_ADDRESS
            | linux_abi::SYS_CLOCK_GETTIME
            | linux_abi::SYS_CLOCK_NANOSLEEP
            | linux_abi::SYS_TGKILL
            | linux_abi::SYS_OPENAT
            | linux_abi::SYS_NEWFSTATAT
            | linux_abi::SYS_READLINKAT
            | linux_abi::SYS_FACCESSAT
            | linux_abi::SYS_SET_ROBUST_LIST
            | linux_abi::SYS_DUP3
            | linux_abi::SYS_PRLIMIT64
            | linux_abi::SYS_GETRANDOM
            | linux_abi::SYS_STATX
            | linux_abi::SYS_RSEQ
            | linux_abi::SYS_CLONE3
            | linux_abi::SYS_EXIT
            | linux_abi::SYS_EXIT_GROUP
    )
}

fn syscall_process_exit(status: u64) -> u64 {
    linux_ops::exit_current_process(status)
}

fn syscall_linux_write(fd: u64, user_ptr: u64, user_len: u64) -> u64 {
    debug_log_secondary_linux_syscall(|| {
        alloc::format!("write fd={} ptr={:#x} len={}", fd, user_ptr, user_len)
    });
    match linux_ops::write(fd, user_ptr, user_len) {
        Ok(written) => written as u64,
        Err(err) => {
            debug::println!(
                "linux write rejected: fd={} user_ptr={:#x} len={} err={:?}",
                fd,
                user_ptr,
                user_len,
                err,
            );
            linux_errno(linux_sysop_error_to_errno(err))
        }
    }
}

fn syscall_linux_read(fd: u64, user_ptr: u64, user_len: u64) -> u64 {
    debug_log_secondary_linux_syscall(|| {
        alloc::format!("read fd={} ptr={:#x} len={}", fd, user_ptr, user_len)
    });
    match linux_ops::read(fd, user_ptr, user_len) {
        Ok(read) => read as u64,
        Err(err) => {
            debug::println!(
                "linux read rejected: fd={} user_ptr={:#x} len={} err={:?}",
                fd,
                user_ptr,
                user_len,
                err,
            );
            linux_errno(linux_sysop_error_to_errno(err))
        }
    }
}

fn syscall_linux_close(fd: u64) -> u64 {
    match linux_ops::close(fd) {
        Ok(()) => 0,
        Err(err) => linux_errno(linux_sysop_error_to_errno(err)),
    }
}

fn syscall_linux_fstat(fd: u64, stat_ptr: u64) -> u64 {
    match linux_ops::fstat(fd, stat_ptr) {
        Ok(()) => 0,
        Err(err) => linux_errno(linux_sysop_error_to_errno(err)),
    }
}

fn syscall_linux_poll(pollfds_ptr: u64, nfds: u64, timeout_millis: i32) -> u64 {
    debug_log_secondary_linux_syscall(|| {
        alloc::format!(
            "poll pollfds={:#x} nfds={} timeout_ms={}",
            pollfds_ptr,
            nfds,
            timeout_millis
        )
    });
    match linux_ops::poll(pollfds_ptr, nfds, timeout_millis) {
        Ok(ready) => ready,
        Err(err) => linux_errno(linux_sysop_error_to_errno(err)),
    }
}

fn syscall_linux_dup(fd: u64) -> u64 {
    match linux_ops::dup(fd) {
        Ok(new_fd) => new_fd,
        Err(err) => linux_errno(linux_sysop_error_to_errno(err)),
    }
}

fn syscall_linux_dup2(oldfd: u64, newfd: u64) -> u64 {
    match linux_ops::dup2(oldfd, newfd) {
        Ok(fd) => fd,
        Err(err) => linux_errno(linux_sysop_error_to_errno(err)),
    }
}

fn syscall_linux_lseek(fd: u64, offset: i64, whence: u64) -> u64 {
    match linux_ops::lseek(fd, offset, whence) {
        Ok(position) => position,
        Err(err) => linux_errno(linux_sysop_error_to_errno(err)),
    }
}

fn syscall_linux_writev(fd: u64, iov_ptr: u64, iov_count: u64) -> u64 {
    debug_log_secondary_linux_syscall(|| {
        alloc::format!("writev fd={} iov={:#x} iovcnt={}", fd, iov_ptr, iov_count)
    });
    match linux_ops::writev(fd, iov_ptr, iov_count) {
        Ok(written) => written as u64,
        Err(err) => {
            debug::println!(
                "linux writev rejected: fd={} iov_ptr={:#x} iov_count={} err={:?}",
                fd,
                iov_ptr,
                iov_count,
                err,
            );
            linux_errno(linux_sysop_error_to_errno(err))
        }
    }
}

fn syscall_linux_access(path_ptr: u64, mode: u64) -> u64 {
    match linux_ops::access(path_ptr, mode) {
        Ok(()) => 0,
        Err(err) => {
            debug::println!(
                "linux access rejected: path_ptr={:#x} path={} mode={:#x} err={:?}",
                path_ptr,
                debug_user_path(path_ptr),
                mode,
                err,
            );
            linux_errno(linux_sysop_error_to_errno(err))
        }
    }
}

fn syscall_linux_sched_yield() -> u64 {
    linux_ops::sched_yield()
}

fn syscall_linux_rt_sigaction(
    signal: u64,
    action_ptr: u64,
    old_action_ptr: u64,
    sigset_size: u64,
) -> u64 {
    match linux_ops::rt_sigaction(signal, action_ptr, old_action_ptr, sigset_size) {
        Ok(()) => 0,
        Err(err) => {
            debug::println!(
                "linux rt_sigaction rejected: signal={} action_ptr={:#x} old_action_ptr={:#x} sigset_size={} err={:?}",
                signal,
                action_ptr,
                old_action_ptr,
                sigset_size,
                err,
            );
            linux_errno(linux_sysop_error_to_errno(err))
        }
    }
}

fn syscall_linux_openat(dirfd: u64, path_ptr: u64, flags: u64, mode: u64) -> u64 {
    match linux_ops::openat(dirfd, path_ptr, flags, mode) {
        Ok(fd) => fd,
        Err(err) => {
            debug::println!(
                "linux openat rejected: dirfd={} path_ptr={:#x} path={} flags={:#x} mode={:#x} err={:?}",
                dirfd,
                path_ptr,
                debug_user_path(path_ptr),
                flags,
                mode,
                err,
            );
            linux_errno(linux_sysop_error_to_errno(err))
        }
    }
}

fn syscall_linux_getcwd(user_ptr: u64, user_len: u64) -> u64 {
    match linux_ops::getcwd(user_ptr, user_len) {
        Ok(read) => read as u64,
        Err(err) => {
            debug::println!(
                "linux getcwd rejected: user_ptr={:#x} len={} err={:?}",
                user_ptr,
                user_len,
                err,
            );
            linux_errno(linux_sysop_error_to_errno(err))
        }
    }
}

fn syscall_linux_fcntl(fd: u64, cmd: u64, arg: u64) -> u64 {
    match linux_ops::fcntl(fd, cmd, arg) {
        Ok(result) => result,
        Err(err) => linux_errno(linux_sysop_error_to_errno(err)),
    }
}

fn syscall_linux_pread64(fd: u64, user_ptr: u64, user_len: u64, offset: u64) -> u64 {
    match linux_ops::pread64(fd, user_ptr, user_len, offset) {
        Ok(read) => read as u64,
        Err(err) => linux_errno(linux_sysop_error_to_errno(err)),
    }
}

fn syscall_linux_newfstatat(dirfd: u64, path_ptr: u64, stat_ptr: u64, flags: u64) -> u64 {
    match linux_ops::newfstatat(dirfd, path_ptr, stat_ptr, flags) {
        Ok(()) => 0,
        Err(err) => {
            debug::println!(
                "linux newfstatat rejected: dirfd={} path_ptr={:#x} path={} stat_ptr={:#x} flags={:#x} err={:?}",
                dirfd,
                path_ptr,
                debug_user_path(path_ptr),
                stat_ptr,
                flags,
                err,
            );
            linux_errno(linux_sysop_error_to_errno(err))
        }
    }
}

fn syscall_linux_readlink(path_ptr: u64, user_ptr: u64, user_len: u64) -> u64 {
    match linux_ops::readlink(path_ptr, user_ptr, user_len) {
        Ok(read) => read as u64,
        Err(err) => {
            debug::println!(
                "linux readlink rejected: path_ptr={:#x} path={} user_ptr={:#x} len={} err={:?}",
                path_ptr,
                debug_user_path(path_ptr),
                user_ptr,
                user_len,
                err,
            );
            linux_errno(linux_sysop_error_to_errno(err))
        }
    }
}

fn syscall_linux_readlinkat(dirfd: u64, path_ptr: u64, user_ptr: u64, user_len: u64) -> u64 {
    match linux_ops::readlinkat(dirfd, path_ptr, user_ptr, user_len) {
        Ok(read) => read as u64,
        Err(err) => {
            debug::println!(
                "linux readlinkat rejected: dirfd={} path_ptr={:#x} path={} user_ptr={:#x} len={} err={:?}",
                dirfd,
                path_ptr,
                debug_user_path(path_ptr),
                user_ptr,
                user_len,
                err,
            );
            linux_errno(linux_sysop_error_to_errno(err))
        }
    }
}

fn syscall_linux_faccessat(dirfd: u64, path_ptr: u64, mode: u64, flags: u64) -> u64 {
    match linux_ops::faccessat(dirfd, path_ptr, mode, flags) {
        Ok(()) => 0,
        Err(err) => {
            debug::println!(
                "linux faccessat rejected: dirfd={} path_ptr={:#x} path={} mode={:#x} flags={:#x} err={:?}",
                dirfd,
                path_ptr,
                debug_user_path(path_ptr),
                mode,
                flags,
                err,
            );
            linux_errno(linux_sysop_error_to_errno(err))
        }
    }
}

fn syscall_linux_mmap(
    requested_addr: u64,
    user_len: u64,
    prot: u64,
    flags: u64,
    fd: u64,
    offset: u64,
) -> u64 {
    match linux_ops::mmap(requested_addr, user_len, prot, flags, fd, offset) {
        Ok(addr) => addr,
        Err(err) => {
            debug::println!(
                "linux mmap rejected: addr={:#x} len={} prot={:#x} flags={:#x} fd={} offset={:#x} err={:?}",
                requested_addr,
                user_len,
                prot,
                flags,
                fd,
                offset,
                err,
            );
            linux_errno(linux_sysop_error_to_errno(err))
        }
    }
}

fn syscall_linux_mprotect(start: u64, user_len: u64, prot: u64) -> u64 {
    match linux_ops::mprotect(start, user_len, prot) {
        Ok(()) => 0,
        Err(err) => {
            debug::println!(
                "linux mprotect rejected: start={:#x} len={} prot={:#x} err={:?}",
                start,
                user_len,
                prot,
                err,
            );
            linux_errno(linux_sysop_error_to_errno(err))
        }
    }
}

fn syscall_linux_munmap(start: u64, user_len: u64) -> u64 {
    match linux_ops::munmap(start, user_len) {
        Ok(()) => 0,
        Err(err) => {
            debug::println!(
                "linux munmap rejected: start={:#x} len={} err={:?}",
                start,
                user_len,
                err,
            );
            linux_errno(linux_sysop_error_to_errno(err))
        }
    }
}

fn syscall_linux_sigaltstack(stack_ptr: u64, old_stack_ptr: u64) -> u64 {
    match linux_ops::sigaltstack(stack_ptr, old_stack_ptr) {
        Ok(()) => 0,
        Err(err) => {
            debug::println!(
                "linux sigaltstack rejected: stack_ptr={:#x} old_stack_ptr={:#x} err={:?}",
                stack_ptr,
                old_stack_ptr,
                err,
            );
            linux_errno(linux_sysop_error_to_errno(err))
        }
    }
}

fn syscall_linux_brk(addr: u64) -> u64 {
    linux_ops::brk(addr)
}

fn syscall_linux_rt_sigprocmask(how: u64, set_ptr: u64, oldset_ptr: u64, sigset_size: u64) -> u64 {
    match linux_ops::rt_sigprocmask(how, set_ptr, oldset_ptr, sigset_size) {
        Ok(()) => 0,
        Err(err) => linux_errno(linux_sysop_error_to_errno(err)),
    }
}

fn syscall_linux_ioctl(fd: u64, _request: u64, _arg: u64) -> u64 {
    match linux_ops::ioctl(fd, _request, _arg) {
        Ok(value) => value,
        Err(err) => {
            debug::println!(
                "linux ioctl rejected: fd={} request={:#x} arg={:#x} err={:?}",
                fd,
                _request,
                _arg,
                err,
            );
            linux_errno(linux_sysop_error_to_errno(err))
        }
    }
}

fn syscall_linux_getpid() -> u64 {
    linux_ops::getpid()
}

fn syscall_linux_clone(frame: &SyscallFrame) -> u64 {
    let clone_frame = linux_ops::LinuxCloneFrame {
        user_rip: frame.user_rip,
        user_rflags: frame.user_rflags,
        registers: crate::multitask::UserTaskRegisters {
            rax: frame.rax,
            rbx: frame.rbx,
            rcx: frame.user_rip,
            rdx: frame.rdx,
            rsi: frame.rsi,
            rdi: frame.rdi,
            rbp: frame.rbp,
            r8: frame.r8,
            r9: frame.r9,
            r10: frame.r10,
            r11: frame.user_rflags,
            r12: frame.r12,
            r13: frame.r13,
            r14: frame.r14,
            r15: frame.r15,
        },
    };
    match linux_ops::clone(
        clone_frame,
        frame.rdi,
        frame.rsi,
        frame.rdx,
        frame.r10,
        frame.r8,
    ) {
        Ok(tid) => tid,
        Err(err) => {
            debug::println!(
                "linux clone rejected: flags={:#x} child_stack={:#x} parent_tid={:#x} child_tid={:#x} tls={:#x} err={:?}",
                frame.rdi,
                frame.rsi,
                frame.rdx,
                frame.r10,
                frame.r8,
                err,
            );
            linux_errno(linux_sysop_error_to_errno(err))
        }
    }
}

fn syscall_linux_clone3(frame: &SyscallFrame) -> u64 {
    let clone_frame = linux_ops::LinuxCloneFrame {
        user_rip: frame.user_rip,
        user_rflags: frame.user_rflags,
        registers: crate::multitask::UserTaskRegisters {
            rax: frame.rax,
            rbx: frame.rbx,
            rcx: frame.user_rip,
            rdx: frame.rdx,
            rsi: frame.rsi,
            rdi: frame.rdi,
            rbp: frame.rbp,
            r8: frame.r8,
            r9: frame.r9,
            r10: frame.r10,
            r11: frame.user_rflags,
            r12: frame.r12,
            r13: frame.r13,
            r14: frame.r14,
            r15: frame.r15,
        },
    };
    match linux_ops::clone3(clone_frame, frame.rdi, frame.rsi) {
        Ok(tid) => tid,
        Err(err) => {
            debug::println!(
                "linux clone3 rejected: args_ptr={:#x} size={} err={:?}",
                frame.rdi,
                frame.rsi,
                err,
            );
            linux_errno(linux_sysop_error_to_errno(err))
        }
    }
}

fn syscall_linux_uname(buf_ptr: u64) -> u64 {
    match linux_ops::uname(buf_ptr) {
        Ok(()) => 0,
        Err(err) => linux_errno(linux_sysop_error_to_errno(err)),
    }
}

fn syscall_linux_gettid() -> u64 {
    linux_ops::gettid()
}

fn syscall_linux_sched_getaffinity(pid: u64, cpusetsize: u64, mask_ptr: u64) -> u64 {
    match linux_ops::sched_getaffinity(pid, cpusetsize, mask_ptr) {
        Ok(len) => len,
        Err(err) => linux_errno(linux_sysop_error_to_errno(err)),
    }
}

fn syscall_linux_getuid() -> u64 {
    linux_ops::getuid()
}

fn syscall_linux_getgid() -> u64 {
    linux_ops::getgid()
}

fn syscall_linux_geteuid() -> u64 {
    linux_ops::geteuid()
}

fn syscall_linux_getegid() -> u64 {
    linux_ops::getegid()
}

fn syscall_linux_futex(
    frame: &SyscallFrame,
    uaddr: u64,
    op: u64,
    val: u64,
    timeout_ptr: u64,
    uaddr2: u64,
    val3: u64,
) -> u64 {
    if !multitask::current_console_session().is_system() {
        let user_rsp = frame.user_rsp;
        let return_rip = if user_rsp != 0 {
            let mut bytes = [0_u8; 8];
            match usermem::copy_from_current_user_exact(user_rsp, &mut bytes) {
                Ok(()) => u64::from_le_bytes(bytes),
                Err(_) => 0,
            }
        } else {
            0
        };
        debug_log_secondary_linux_syscall(|| {
            alloc::format!(
                "futex entry uaddr={:#x} op={:#x} val={:#x} timeout_ptr={:#x} uaddr2={:#x} val3={:#x} user_rsp={:#x} return_rip={:#x}",
                uaddr,
                op,
                val,
                timeout_ptr,
                uaddr2,
                val3,
                user_rsp,
                return_rip
            )
        });
    }

    match linux_ops::futex(uaddr, op, val, timeout_ptr, uaddr2, val3) {
        Ok(value) => value,
        Err(err) => linux_errno(linux_sysop_error_to_errno(err)),
    }
}

fn syscall_linux_arch_prctl(code: u64, arg: u64) -> u64 {
    match linux_ops::arch_prctl(code, arg) {
        Ok(value) => value,
        Err(err) => linux_errno(linux_sysop_error_to_errno(err)),
    }
}

fn syscall_linux_set_tid_address(user_ptr: u64) -> u64 {
    match linux_ops::set_tid_address(user_ptr) {
        Ok(pid) => pid,
        Err(err) => linux_errno(linux_sysop_error_to_errno(err)),
    }
}

fn syscall_linux_clock_gettime(clock_id: u64, timespec_ptr: u64) -> u64 {
    match linux_ops::clock_gettime(clock_id, timespec_ptr) {
        Ok(()) => 0,
        Err(err) => {
            debug::println!(
                "linux clock_gettime rejected: clock_id={} timespec_ptr={:#x} err={:?}",
                clock_id,
                timespec_ptr,
                err,
            );
            linux_errno(linux_sysop_error_to_errno(err))
        }
    }
}

fn syscall_linux_clock_nanosleep(
    clock_id: u64,
    flags: u64,
    request_ptr: u64,
    remaining_ptr: u64,
) -> u64 {
    match linux_ops::clock_nanosleep(clock_id, flags, request_ptr, remaining_ptr) {
        Ok(()) => 0,
        Err(err) => linux_errno(linux_sysop_error_to_errno(err)),
    }
}

fn syscall_linux_tgkill(tgid: u64, tid: u64, signal: u64) -> u64 {
    match linux_ops::tgkill(tgid, tid, signal) {
        Ok(()) => 0,
        Err(err) => {
            debug::println!(
                "linux tgkill rejected: tgid={} tid={} signal={} err={:?}",
                tgid,
                tid,
                signal,
                err,
            );
            linux_errno(linux_sysop_error_to_errno(err))
        }
    }
}

fn syscall_linux_set_robust_list(head_ptr: u64, len: u64) -> u64 {
    match linux_ops::set_robust_list(head_ptr, len) {
        Ok(()) => 0,
        Err(err) => {
            debug::println!(
                "linux set_robust_list rejected: head_ptr={:#x} len={} err={:?}",
                head_ptr,
                len,
                err,
            );
            linux_errno(linux_sysop_error_to_errno(err))
        }
    }
}

fn syscall_linux_dup3(oldfd: u64, newfd: u64, flags: u64) -> u64 {
    match linux_ops::dup3(oldfd, newfd, flags) {
        Ok(fd) => fd,
        Err(err) => linux_errno(linux_sysop_error_to_errno(err)),
    }
}

fn syscall_linux_prlimit64(pid: u64, resource: u64, new_limit_ptr: u64, old_limit_ptr: u64) -> u64 {
    match linux_ops::prlimit64(pid, resource, new_limit_ptr, old_limit_ptr) {
        Ok(()) => 0,
        Err(err) => {
            debug::println!(
                "linux prlimit64 rejected: pid={} resource={} new_limit_ptr={:#x} old_limit_ptr={:#x} err={:?}",
                pid,
                resource,
                new_limit_ptr,
                old_limit_ptr,
                err,
            );
            linux_errno(linux_sysop_error_to_errno(err))
        }
    }
}

fn syscall_linux_getrandom(user_ptr: u64, user_len: u64, flags: u64) -> u64 {
    match linux_ops::getrandom(user_ptr, user_len, flags) {
        Ok(read) => read as u64,
        Err(err) => {
            debug::println!(
                "linux getrandom rejected: user_ptr={:#x} len={} flags={:#x} err={:?}",
                user_ptr,
                user_len,
                flags,
                err,
            );
            linux_errno(linux_sysop_error_to_errno(err))
        }
    }
}

fn syscall_linux_statx(dirfd: u64, path_ptr: u64, flags: u64, mask: u64, statx_ptr: u64) -> u64 {
    match linux_ops::statx(
        dirfd,
        path_ptr,
        flags,
        u32::try_from(mask).unwrap_or(u32::MAX),
        statx_ptr,
    ) {
        Ok(()) => 0,
        Err(err) => {
            debug::println!(
                "linux statx rejected: dirfd={} path_ptr={:#x} path={} flags={:#x} mask={:#x} statx_ptr={:#x} err={:?}",
                dirfd,
                path_ptr,
                debug_user_path(path_ptr),
                flags,
                mask,
                statx_ptr,
                err,
            );
            linux_errno(linux_sysop_error_to_errno(err))
        }
    }
}

fn syscall_linux_rseq(area_ptr: u64, len: u64, flags: u64, signature: u64) -> u64 {
    match linux_ops::rseq(area_ptr, len, flags, signature) {
        Ok(()) => 0,
        Err(err) => {
            debug::println!(
                "linux rseq rejected: area_ptr={:#x} len={} flags={:#x} signature={:#x} err={:?}",
                area_ptr,
                len,
                flags,
                signature,
                err,
            );
            linux_errno(linux_sysop_error_to_errno(err))
        }
    }
}

fn syscall_linux_nanosleep(request_ptr: u64, remaining_ptr: u64) -> u64 {
    match linux_ops::nanosleep(request_ptr, remaining_ptr) {
        Ok(()) => 0,
        Err(err) => linux_errno(linux_sysop_error_to_errno(err)),
    }
}

fn linux_errno(errno: i64) -> u64 {
    (-errno) as u64
}

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
        | paging::AddressSpaceError::HugePageConflict => LINUX_EFAULT,
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
        linux_ops::LinuxSysopError::BadFileDescriptor => LINUX_EBADF,
        linux_ops::LinuxSysopError::Busy => LINUX_EBUSY,
        linux_ops::LinuxSysopError::DisplayUnavailable => LINUX_ENODEV,
        linux_ops::LinuxSysopError::IllegalSeek => LINUX_ESPIPE,
        linux_ops::LinuxSysopError::InvalidArgument => LINUX_EINVAL,
        linux_ops::LinuxSysopError::NoMemory => LINUX_ENOMEM,
        linux_ops::LinuxSysopError::NotFound => LINUX_ENOENT,
        linux_ops::LinuxSysopError::NotDirectory => LINUX_ENOTDIR,
        linux_ops::LinuxSysopError::NoSuchProcess => LINUX_ESRCH,
        linux_ops::LinuxSysopError::NotTty => LINUX_ENOTTY,
        linux_ops::LinuxSysopError::PermissionDenied => LINUX_EACCES,
        linux_ops::LinuxSysopError::ReadOnlyFilesystem => LINUX_EROFS,
        linux_ops::LinuxSysopError::Stale => LINUX_ESTALE,
        linux_ops::LinuxSysopError::TryAgain => LINUX_EAGAIN,
        linux_ops::LinuxSysopError::Unsupported => LINUX_ENOSYS,
    }
}
