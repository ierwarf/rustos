use super::*;

use core::mem::size_of;

use crate::user::sysops::device::{self, DeviceSysopError};
use kernel_object::api::device::{DeviceAccessKind, DeviceId};
use kernel_object::api::handle::{DeviceHandleRights, HandleRights};
use rustos_user_abi::syscall::{
    DEVMGRD_DEVICE_ACCESS_EVDEV, DEVMGRD_DEVICE_ACCESS_NATIVE, DEVMGRD_DEVICE_ID_CONSOLE,
    DEVMGRD_DEVICE_ID_DISPLAY, DEVMGRD_DEVICE_ID_INPUT, DEVMGRD_DEVICE_RIGHT_ADMIN,
    DEVMGRD_DEVICE_RIGHT_IOCTL, DEVMGRD_DEVICE_RIGHT_MAP, DEVMGRD_DEVICE_RIGHT_READ,
    DEVMGRD_DEVICE_RIGHT_TRANSFER, DEVMGRD_DEVICE_RIGHT_WRITE, DEVMGRD_IOCTL_ROUTE_SESSIOND_COMMIT,
    DEVMGRD_IPC_ABI_VERSION, DEVMGRD_IPC_OP_IOCTL_ROUTE, DevmgrdDeviceIoctlRequest,
    DevmgrdDeviceIoctlResponse, IPC_SERVICE_CAP_DEVICE_POLICY, IPC_SERVICE_CAP_SESSION_POLICY,
    IPC_SERVICE_DEVMGRD, RustosDeviceIoctlBrokerArgs, RustosDeviceOpenBrokerArgs,
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
    let Some(rights) = device_handle_rights(args.rights) else {
        return linux_errno(LINUX_EINVAL);
    };
    if (args.open_flags & linux_abi::O_ACCMODE) != linux_abi::O_RDONLY && !rights.allows_write() {
        return linux_errno(LINUX_EACCES);
    }

    let device_handle = crate::io::device::DeviceHandle::with_access(device_id, access);
    let input_token = if device_id == DeviceId::Input {
        let input_access = match access {
            DeviceAccessKind::Native => INPUTD_ACCESS_NATIVE,
            DeviceAccessKind::Evdev => INPUTD_ACCESS_EVDEV,
        };
        if let Err(errno) = waitset_broker_ops::register_input_open_description(
            device_handle.token_id(),
            input_access,
        ) {
            return linux_errno(errno);
        }
        Some(device_handle.token_id())
    } else {
        None
    };
    let handle = multitask::KernelHandle::Device(device_handle);
    let installed = multitask::with_current_user_process_state_mut(|_, _, process_state| {
        process_state
            .handles_mut()
            .install_with_open_flags_and_rights(handle, args.open_flags, rights)
    });
    match installed {
        Some(Some(fd)) => fd,
        Some(None) => {
            if let Some(token) = input_token {
                let _ = waitset_broker_ops::release_input_open_description(token);
            }
            linux_errno(LINUX_EMFILE)
        }
        None => {
            if let Some(token) = input_token {
                let _ = waitset_broker_ops::release_input_open_description(token);
            }
            linux_errno(LINUX_EINVAL)
        }
    }
}

pub(super) fn syscall_linux_rustos_device_ioctl_broker(args_ptr: u64) -> u64 {
    let has_device_policy =
        ipc_ops::current_process_has_service_capability(IPC_SERVICE_CAP_DEVICE_POLICY);
    let has_session_policy =
        ipc_ops::current_process_has_service_capability(IPC_SERVICE_CAP_SESSION_POLICY);
    if !has_device_policy && !has_session_policy {
        return linux_errno(LINUX_EPERM);
    }

    let args = match usermem::read_current_user_struct::<RustosDeviceIoctlBrokerArgs>(args_ptr) {
        Ok(args) => args,
        Err(err) => return linux_errno(address_space_error_to_linux_errno(err)),
    };
    if args.reserved0 != 0 || (args.process_id == 0 && !has_session_policy) {
        return linux_errno(LINUX_EINVAL);
    }
    if !has_device_policy && !session_policy_device_ioctl_allowed(&args) {
        return linux_errno(LINUX_EPERM);
    }
    if !has_device_policy {
        return match device::ioctl_current_process_fd(args.fd, args.request, args.arg) {
            Ok(value) => value,
            Err(err) => linux_errno(device_sysop_error_to_linux_errno(err)),
        };
    }

    match device::ioctl_process_device_handle(args.process_id, args.fd, args.request, args.arg) {
        Ok(value) => value,
        Err(err) => linux_errno(device_sysop_error_to_linux_errno(err)),
    }
}

// RING3-MIGRATION-REFERENCE START: capability-broker exception: sessiond owns
// session ioctl commit policy. Ring0 keeps capability-gated native ioctl commit
// substrate.
fn session_policy_device_ioctl_allowed(args: &RustosDeviceIoctlBrokerArgs) -> bool {
    if args.process_id != 0
        && !multitask::current_user_process_id().is_some_and(|pid| pid == args.process_id)
    {
        return false;
    }
    match session_ioctl_route_via_devmgrd(args.request) {
        Ok(route) => route == DEVMGRD_IOCTL_ROUTE_SESSIOND_COMMIT,
        Err(_) => false,
    }
}

fn session_ioctl_route_via_devmgrd(request_number: u64) -> Result<u64, i64> {
    let mut request = DevmgrdDeviceIoctlRequest {
        version: DEVMGRD_IPC_ABI_VERSION,
        op: DEVMGRD_IPC_OP_IOCTL_ROUTE,
        request: request_number,
        ..DevmgrdDeviceIoctlRequest::default()
    };
    if let Some(snapshot) = multitask::current_user_snapshot() {
        request.pid = snapshot.process_id();
        request.tid = snapshot.thread_id();
        request.session_handle = snapshot.console_session().raw();
    }
    if request.pid == 0 || request.tid == 0 {
        return Err(LINUX_EINVAL);
    }
    let response = ipc_ops::call_service_endpoint_with_class(
        IPC_SERVICE_DEVMGRD,
        as_bytes(&request),
        ipc_ops::ServiceIpcClass::InteractiveControl,
    )?;
    if response.len() != size_of::<DevmgrdDeviceIoctlResponse>() {
        return Err(LINUX_EINVAL);
    }
    let response = read_unaligned::<DevmgrdDeviceIoctlResponse>(response.as_slice());
    if response.version != DEVMGRD_IPC_ABI_VERSION
        || response.op != DEVMGRD_IPC_OP_IOCTL_ROUTE
        || response.payload_len != 0
        || response.reserved1 != 0
        || response.reserved0 != 0
    {
        return Err(LINUX_EINVAL);
    }
    if response.status != 0 {
        return Err(response.status.unsigned_abs() as i64);
    }
    Ok(response.value)
}
// RING3-MIGRATION-REFERENCE END: sessiond-owned ioctl commit substrate exception.

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
        DeviceSysopError::TryAgain => LINUX_EAGAIN,
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
