use super::*;

use crate::user::sysops::device::{self, DeviceSysopError};
use rustos_user_abi::syscall::{IPC_SERVICE_CAP_DEVICE_POLICY, RustosDeviceIoctlBrokerArgs};

pub(super) fn syscall_linux_rustos_device_ioctl_broker(args_ptr: u64) -> u64 {
    if !ipc_ops::current_process_has_service_capability(IPC_SERVICE_CAP_DEVICE_POLICY) {
        return linux_errno(LINUX_EPERM);
    }

    let args = match usermem::read_current_user_struct::<RustosDeviceIoctlBrokerArgs>(args_ptr) {
        Ok(args) => args,
        Err(err) => return linux_errno(address_space_error_to_linux_errno(err)),
    };
    if args.process_id == 0 || args.reserved0 != 0 {
        return linux_errno(LINUX_EINVAL);
    }

    match device::ioctl_process_device_handle(args.process_id, args.fd, args.request, args.arg) {
        Ok(value) => value,
        Err(err) => linux_errno(device_sysop_error_to_linux_errno(err)),
    }
}

pub(in crate::user::syscall::linux) fn device_sysop_error_to_linux_errno(
    err: DeviceSysopError,
) -> i64 {
    match err {
        DeviceSysopError::AddressSpace(err) => address_space_error_to_linux_errno(err),
        DeviceSysopError::BadFileDescriptor => LINUX_EBADF,
        DeviceSysopError::InvalidArgument => LINUX_EINVAL,
        DeviceSysopError::DisplayUnavailable => LINUX_ENODEV,
        DeviceSysopError::NotFound => LINUX_ENOENT,
        DeviceSysopError::StaleSurface => LINUX_ESTALE,
        DeviceSysopError::Unsupported => LINUX_ENOSYS,
    }
}
