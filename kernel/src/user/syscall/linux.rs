use crate::user::linux as linux_abi;
use crate::user::sysops::linux as linux_ops;
use crate::{debug, paging};

use super::SyscallFrame;

const LINUX_EPERM: i64 = 1;
const LINUX_EACCES: i64 = 13;
const LINUX_ENOMEM: i64 = 12;
const LINUX_EAGAIN: i64 = 11;
const LINUX_EBADF: i64 = 9;
const LINUX_ENOENT: i64 = 2;
const LINUX_EBUSY: i64 = 16;
const LINUX_EFAULT: i64 = 14;
const LINUX_EINVAL: i64 = 22;
const LINUX_ENODEV: i64 = 19;
const LINUX_ENOTTY: i64 = 25;
const LINUX_EROFS: i64 = 30;
const LINUX_ESPIPE: i64 = 29;
const LINUX_ENOSYS: i64 = 38;

pub(super) fn dispatch_linux_syscall(frame: &SyscallFrame) -> u64 {
    if let Err(error) = syscall_check(frame) {
        return error;
    }

    match frame.rax {
        linux_abi::SYS_READ => syscall_linux_read(frame.rdi, frame.rsi, frame.rdx),
        linux_abi::SYS_WRITE => syscall_linux_write(frame.rdi, frame.rsi, frame.rdx),
        linux_abi::SYS_CLOSE => syscall_linux_close(frame.rdi),
        linux_abi::SYS_FSTAT => syscall_linux_fstat(frame.rdi, frame.rsi),
        linux_abi::SYS_LSEEK => syscall_linux_lseek(frame.rdi, frame.rsi as i64, frame.rdx),
        linux_abi::SYS_WRITEV => syscall_linux_writev(frame.rdi, frame.rsi, frame.rdx),
        linux_abi::SYS_ACCESS => syscall_linux_access(frame.rdi, frame.rsi),
        linux_abi::SYS_MMAP => syscall_linux_mmap(
            frame.rdi, frame.rsi, frame.rdx, frame.r10, frame.r8, frame.r9,
        ),
        linux_abi::SYS_MPROTECT => syscall_linux_mprotect(frame.rdi, frame.rsi, frame.rdx),
        linux_abi::SYS_MUNMAP => syscall_linux_munmap(frame.rdi, frame.rsi),
        linux_abi::SYS_BRK => syscall_linux_brk(frame.rdi),
        linux_abi::SYS_RT_SIGPROCMASK => {
            syscall_linux_rt_sigprocmask(frame.rdi, frame.rsi, frame.rdx, frame.r10)
        }
        linux_abi::SYS_IOCTL => syscall_linux_ioctl(frame.rdi, frame.rsi, frame.rdx),
        linux_abi::SYS_PREAD64 => syscall_linux_pread64(frame.rdi, frame.rsi, frame.rdx, frame.r10),
        linux_abi::SYS_NANOSLEEP => syscall_linux_nanosleep(frame.rdi, frame.rsi),
        linux_abi::SYS_GETPID => syscall_linux_getpid(),
        linux_abi::SYS_ARCH_PRCTL => syscall_linux_arch_prctl(frame.rdi, frame.rsi),
        linux_abi::SYS_SET_TID_ADDRESS => syscall_linux_set_tid_address(frame.rdi),
        linux_abi::SYS_CLOCK_GETTIME => syscall_linux_clock_gettime(frame.rdi, frame.rsi),
        linux_abi::SYS_CLOCK_NANOSLEEP => {
            syscall_linux_clock_nanosleep(frame.rdi, frame.rsi, frame.rdx, frame.r10)
        }
        linux_abi::SYS_OPENAT => syscall_linux_openat(frame.rdi, frame.rsi, frame.rdx, frame.r10),
        linux_abi::SYS_NEWFSTATAT => {
            syscall_linux_newfstatat(frame.rdi, frame.rsi, frame.rdx, frame.r10)
        }
        linux_abi::SYS_FACCESSAT => {
            syscall_linux_faccessat(frame.rdi, frame.rsi, frame.rdx, frame.r10)
        }
        linux_abi::SYS_SET_ROBUST_LIST => syscall_linux_set_robust_list(frame.rdi, frame.rsi),
        linux_abi::SYS_PRLIMIT64 => {
            syscall_linux_prlimit64(frame.rdi, frame.rsi, frame.rdx, frame.r10)
        }
        linux_abi::SYS_GETRANDOM => syscall_linux_getrandom(frame.rdi, frame.rsi, frame.rdx),
        linux_abi::SYS_RSEQ => syscall_linux_rseq(frame.rdi, frame.rsi, frame.rdx, frame.r10),
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
            | linux_abi::SYS_LSEEK
            | linux_abi::SYS_WRITEV
            | linux_abi::SYS_ACCESS
            | linux_abi::SYS_MMAP
            | linux_abi::SYS_MPROTECT
            | linux_abi::SYS_MUNMAP
            | linux_abi::SYS_BRK
            | linux_abi::SYS_RT_SIGPROCMASK
            | linux_abi::SYS_IOCTL
            | linux_abi::SYS_PREAD64
            | linux_abi::SYS_NANOSLEEP
            | linux_abi::SYS_GETPID
            | linux_abi::SYS_ARCH_PRCTL
            | linux_abi::SYS_SET_TID_ADDRESS
            | linux_abi::SYS_CLOCK_GETTIME
            | linux_abi::SYS_CLOCK_NANOSLEEP
            | linux_abi::SYS_OPENAT
            | linux_abi::SYS_NEWFSTATAT
            | linux_abi::SYS_FACCESSAT
            | linux_abi::SYS_SET_ROBUST_LIST
            | linux_abi::SYS_PRLIMIT64
            | linux_abi::SYS_GETRANDOM
            | linux_abi::SYS_RSEQ
            | linux_abi::SYS_EXIT
            | linux_abi::SYS_EXIT_GROUP
    )
}

fn syscall_process_exit(status: u64) -> u64 {
    linux_ops::exit_current_process(status)
}

fn syscall_linux_write(fd: u64, user_ptr: u64, user_len: u64) -> u64 {
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

fn syscall_linux_lseek(fd: u64, offset: i64, whence: u64) -> u64 {
    match linux_ops::lseek(fd, offset, whence) {
        Ok(position) => position,
        Err(err) => linux_errno(linux_sysop_error_to_errno(err)),
    }
}

fn syscall_linux_writev(fd: u64, iov_ptr: u64, iov_count: u64) -> u64 {
    match linux_ops::writev(fd, iov_ptr, iov_count) {
        Ok(written) => written as u64,
        Err(err) => linux_errno(linux_sysop_error_to_errno(err)),
    }
}

fn syscall_linux_access(path_ptr: u64, mode: u64) -> u64 {
    match linux_ops::access(path_ptr, mode) {
        Ok(()) => 0,
        Err(err) => {
            debug::println!(
                "linux access rejected: path_ptr={:#x} mode={:#x} err={:?}",
                path_ptr,
                mode,
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
                "linux openat rejected: dirfd={} path_ptr={:#x} flags={:#x} mode={:#x} err={:?}",
                dirfd,
                path_ptr,
                flags,
                mode,
                err,
            );
            linux_errno(linux_sysop_error_to_errno(err))
        }
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
                "linux newfstatat rejected: dirfd={} path_ptr={:#x} stat_ptr={:#x} flags={:#x} err={:?}",
                dirfd,
                path_ptr,
                stat_ptr,
                flags,
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
                "linux faccessat rejected: dirfd={} path_ptr={:#x} mode={:#x} flags={:#x} err={:?}",
                dirfd,
                path_ptr,
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
        Err(err) => linux_errno(linux_sysop_error_to_errno(err)),
    }
}

fn syscall_linux_getpid() -> u64 {
    linux_ops::getpid()
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
        linux_ops::LinuxSysopError::NotTty => LINUX_ENOTTY,
        linux_ops::LinuxSysopError::PermissionDenied => LINUX_EACCES,
        linux_ops::LinuxSysopError::ReadOnlyFilesystem => LINUX_EROFS,
        linux_ops::LinuxSysopError::Unsupported => LINUX_ENOSYS,
    }
}
