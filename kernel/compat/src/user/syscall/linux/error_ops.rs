use super::*;

pub(super) const LINUX_EPERM: i64 = 1;
pub(super) const LINUX_ENOENT: i64 = 2;
pub(super) const LINUX_ESRCH: i64 = 3;
pub(super) const LINUX_EINTR: i64 = 4;
pub(super) const LINUX_EIO: i64 = 5;
pub(super) const LINUX_E2BIG: i64 = 7;
pub(super) const LINUX_ENOEXEC: i64 = 8;
pub(super) const LINUX_EBADF: i64 = 9;
pub(super) const LINUX_ECHILD: i64 = 10;
pub(super) const LINUX_EAGAIN: i64 = 11;
pub(super) const LINUX_ENOMEM: i64 = 12;
pub(super) const LINUX_EACCES: i64 = 13;
pub(super) const LINUX_EFAULT: i64 = 14;
pub(super) const LINUX_EBUSY: i64 = 16;
pub(super) const LINUX_EEXIST: i64 = 17;
pub(super) const LINUX_ENODEV: i64 = 19;
pub(super) const LINUX_ENOTDIR: i64 = 20;
pub(super) const LINUX_EINVAL: i64 = 22;
pub(super) const LINUX_EMFILE: i64 = 24;
pub(super) const LINUX_ENOTTY: i64 = 25;
pub(super) const LINUX_ENOSPC: i64 = 28;
pub(super) const LINUX_EROFS: i64 = 30;
pub(super) const LINUX_EPIPE: i64 = 32;
pub(super) const LINUX_ERANGE: i64 = 34;
pub(super) const LINUX_ENOSYS: i64 = 38;
pub(super) const LINUX_EOVERFLOW: i64 = 75;
pub(super) const LINUX_ENOTSOCK: i64 = 88;
pub(super) const LINUX_EMSGSIZE: i64 = 90;
pub(super) const LINUX_EOPNOTSUPP: i64 = 95;
pub(super) const LINUX_ETIMEDOUT: i64 = 110;
pub(super) const LINUX_ECANCELED: i64 = 125;
pub(super) const LINUX_ESTALE: i64 = 116;

/// Diagnose each unsupported standard Linux syscall once, without copying user
/// data or logging on the successful syscall hot path. Numeric arguments are
/// retained to distinguish feature probes from required ABI operations.
pub(super) fn unimplemented_linux_syscall(frame: &SyscallFrame) -> u64 {
    use core::sync::atomic::{AtomicU64, Ordering};
    static SEEN: [AtomicU64; 8] = [const { AtomicU64::new(0) }; 8];
    static OUT_OF_RANGE: AtomicU64 = AtomicU64::new(0);
    let first = if frame.rax < 512 {
        let bit = 1_u64 << (frame.rax % 64);
        SEEN[frame.rax as usize / 64].fetch_or(bit, Ordering::Relaxed) & bit == 0
    } else {
        let count = OUT_OF_RANGE.fetch_add(1, Ordering::Relaxed) + 1;
        count <= 4 || count.is_power_of_two()
    };
    if first {
        let identity = multitask::current_user_snapshot()
            .map(|s| (s.process_id() << 32) | (s.thread_id() & 0xffff_ffff))
            .unwrap_or(0);
        use nucleus_core::debug::{record_milestone, LogCategory};
        record_milestone(LogCategory::Compat, "linux-syscall-unimplemented", frame.rax, identity);
        record_milestone(LogCategory::Compat, "linux-syscall-unimplemented-args01", frame.rdi, frame.rsi);
        record_milestone(LogCategory::Compat, "linux-syscall-unimplemented-args23", frame.rdx, frame.r10);
    }
    linux_errno(LINUX_ENOSYS)
}

pub(super) fn record_busy_linux_syscall(frame: &SyscallFrame) {
    use core::sync::atomic::{AtomicU64, Ordering};
    static COUNT: AtomicU64 = AtomicU64::new(0);
    let count = COUNT.fetch_add(1, Ordering::Relaxed) + 1;
    if count <= 16 || count.is_power_of_two() {
        nucleus_core::debug::record_milestone(nucleus_core::debug::LogCategory::Compat,
            "linux-syscall-busy", frame.rax, count);
        nucleus_core::debug::record_milestone(nucleus_core::debug::LogCategory::Compat,
            "linux-syscall-busy-args01", frame.rdi, frame.rsi);
    }
}

/// Errno for a failed user-pointer validation or copy.
///
/// Linux reports every unusable user address as `EFAULT`, including a null or
/// out-of-range pointer; `address_space_error_to_linux_errno` maps those to
/// `EINVAL`, which is right for mapping requests (mmap/munmap) but not for a
/// buffer argument. Use this for buffer pointers only.
pub(super) fn user_pointer_error_to_linux_errno(err: paging::AddressSpaceError) -> i64 {
    match err {
        paging::AddressSpaceError::OutOfFrames => LINUX_ENOMEM,
        _ => LINUX_EFAULT,
    }
}

pub(super) fn address_space_error_to_linux_errno(err: paging::AddressSpaceError) -> i64 {
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

#[cfg(test)]
mod user_pointer_errno_tests {
    use super::*;

    #[test]
    fn unusable_buffer_addresses_are_efault_but_mapping_requests_keep_einval() {
        for err in [
            paging::AddressSpaceError::AddressOutOfRange,
            paging::AddressSpaceError::AddressOverflow,
            paging::AddressSpaceError::NotMapped,
            paging::AddressSpaceError::ProtectionViolation,
        ] {
            assert_eq!(user_pointer_error_to_linux_errno(err), LINUX_EFAULT);
        }
        assert_eq!(
            user_pointer_error_to_linux_errno(paging::AddressSpaceError::OutOfFrames),
            LINUX_ENOMEM
        );
        assert_eq!(
            address_space_error_to_linux_errno(paging::AddressSpaceError::AddressOutOfRange),
            LINUX_EINVAL
        );
    }
}
