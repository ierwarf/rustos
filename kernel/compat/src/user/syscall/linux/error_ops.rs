use super::*;
use crate::user::sysops::linux as linux_ops;

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

pub(super) fn linux_sysop_error_to_errno(err: linux_ops::LinuxSysopError) -> i64 {
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
        linux_ops::LinuxSysopError::NetworkUnreachable => LINUX_ENETUNREACH,
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
