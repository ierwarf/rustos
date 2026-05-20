use super::*;

use crate::user::sysops::device::{self, DeviceSysopError};
use kernel_object::api::device::DeviceAccessKind;
use kernel_object::api::handle::{DeviceHandleRights, HandleRights};
use rustos_user_abi::syscall::{
    DEVMGRD_DEVICE_ACCESS_EVDEV, DEVMGRD_DEVICE_ACCESS_NATIVE, DEVMGRD_DEVICE_ID_CONSOLE,
    DEVMGRD_DEVICE_ID_DISPLAY, DEVMGRD_DEVICE_ID_INPUT, DEVMGRD_DEVICE_RIGHT_ADMIN,
    DEVMGRD_DEVICE_RIGHT_IOCTL, DEVMGRD_DEVICE_RIGHT_MAP, DEVMGRD_DEVICE_RIGHT_READ,
    DEVMGRD_DEVICE_RIGHT_TRANSFER, DEVMGRD_DEVICE_RIGHT_WRITE, IPC_SERVICE_CAP_DEVICE_POLICY,
    RustosDeviceIoctlBrokerArgs, RustosDeviceOpenBrokerArgs,
};

pub(super) fn syscall_linux_rustos_device_open_broker(args_ptr: u64) -> u64 {
    if !ipc_ops::current_process_has_service_capability(IPC_SERVICE_CAP_DEVICE_POLICY) {
        return linux_errno(LINUX_EPERM);
    }

    let args = match usermem::read_current_user_struct::<RustosDeviceOpenBrokerArgs>(args_ptr) {
        Ok(args) => args,
        Err(err) => return linux_errno(address_space_error_to_linux_errno(err)),
    };
    if args.abi_version != rustos_user_abi::syscall::DEVMGRD_IPC_ABI_VERSION
        || args.reserved0 != 0
        || args.reserved1 != 0
        || args.rights == 0
        || args.rights & !allowed_device_rights_mask() != 0
    {
        return linux_errno(LINUX_EINVAL);
    }

    let Some(device_id) = map_device_id(args.device_id) else {
        return linux_errno(LINUX_EINVAL);
    };
    let Some(access) = map_device_access(args.access) else {
        return linux_errno(LINUX_EINVAL);
    };
    if matches!(device_id, crate::io::device::DeviceId::Input)
        && matches!(access, DeviceAccessKind::Evdev)
        && args.rights & (DEVMGRD_DEVICE_RIGHT_WRITE | DEVMGRD_DEVICE_RIGHT_ADMIN) != 0
    {
        return linux_errno(LINUX_EPERM);
    }
    let Some(rights) = device_handle_rights(args.rights) else {
        return linux_errno(LINUX_EINVAL);
    };
    if (args.open_flags & linux_abi::O_ACCMODE) != linux_abi::O_RDONLY && !rights.allows_write() {
        return linux_errno(LINUX_EACCES);
    }

    let handle = multitask::KernelHandle::Device(crate::io::device::DeviceHandle::with_access(
        device_id, access,
    ));
    match multitask::with_current_user_process_state_mut(|_, _, process_state| {
        process_state
            .handles_mut()
            .install_with_open_flags_and_rights(handle, args.open_flags, rights)
    }) {
        Some(fd) => fd,
        None => linux_errno(LINUX_EINVAL),
    }
}

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

fn allowed_device_rights_mask() -> u64 {
    DEVMGRD_DEVICE_RIGHT_READ
        | DEVMGRD_DEVICE_RIGHT_WRITE
        | DEVMGRD_DEVICE_RIGHT_IOCTL
        | DEVMGRD_DEVICE_RIGHT_ADMIN
        | DEVMGRD_DEVICE_RIGHT_MAP
        | DEVMGRD_DEVICE_RIGHT_TRANSFER
}

fn device_handle_rights(mask: u64) -> Option<HandleRights> {
    if mask == 0 || mask & !allowed_device_rights_mask() != 0 {
        return None;
    }
    let mut rights = DeviceHandleRights::empty();
    if mask & DEVMGRD_DEVICE_RIGHT_READ != 0 {
        rights = rights.union(DeviceHandleRights::READ);
    }
    if mask & DEVMGRD_DEVICE_RIGHT_WRITE != 0 {
        rights = rights.union(DeviceHandleRights::WRITE);
    }
    if mask & DEVMGRD_DEVICE_RIGHT_IOCTL != 0 {
        rights = rights.union(DeviceHandleRights::IOCTL);
    }
    if mask & DEVMGRD_DEVICE_RIGHT_ADMIN != 0 {
        rights = rights.union(DeviceHandleRights::ADMIN);
    }
    if mask & DEVMGRD_DEVICE_RIGHT_MAP != 0 {
        rights = rights.union(DeviceHandleRights::MAP);
    }
    if mask & DEVMGRD_DEVICE_RIGHT_TRANSFER != 0 {
        rights = rights.union(DeviceHandleRights::TRANSFER);
    }
    Some(HandleRights::Device(rights))
}

fn map_device_id(id: u16) -> Option<crate::io::device::DeviceId> {
    match id {
        DEVMGRD_DEVICE_ID_CONSOLE => Some(crate::io::device::DeviceId::Console),
        DEVMGRD_DEVICE_ID_DISPLAY => Some(crate::io::device::DeviceId::Display),
        DEVMGRD_DEVICE_ID_INPUT => Some(crate::io::device::DeviceId::Input),
        _ => None,
    }
}

fn map_device_access(access: u16) -> Option<DeviceAccessKind> {
    match access {
        DEVMGRD_DEVICE_ACCESS_NATIVE => Some(DeviceAccessKind::Native),
        DEVMGRD_DEVICE_ACCESS_EVDEV => Some(DeviceAccessKind::Evdev),
        _ => None,
    }
}
