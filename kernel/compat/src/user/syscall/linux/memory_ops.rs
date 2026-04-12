use super::*;

pub(super) fn syscall_linux_mmap(
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

pub(super) fn syscall_linux_mprotect(start: u64, user_len: u64, prot: u64) -> u64 {
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

pub(super) fn syscall_linux_munmap(start: u64, user_len: u64) -> u64 {
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

pub(super) fn syscall_linux_madvise(start: u64, user_len: u64, advice: u64) -> u64 {
    match linux_ops::madvise(start, user_len, advice) {
        Ok(()) => 0,
        Err(err) => {
            debug::println!(
                "linux madvise rejected: start={:#x} len={} advice={} err={:?}",
                start,
                user_len,
                advice,
                err,
            );
            linux_errno(linux_sysop_error_to_errno(err))
        }
    }
}

pub(super) fn syscall_linux_sigaltstack(stack_ptr: u64, old_stack_ptr: u64) -> u64 {
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

pub(super) fn syscall_linux_brk(addr: u64) -> u64 {
    linux_ops::brk(addr)
}
