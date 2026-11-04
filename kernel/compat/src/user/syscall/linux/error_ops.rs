use super::*;

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
