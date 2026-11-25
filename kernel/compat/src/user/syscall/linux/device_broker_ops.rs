//! Device-policy broker admission and capability installation boundary.
//!
//! - **Owner:** `devmgrd` and `sessiond` own device/open/ioctl policy; Compat
//!   installs only their typed, capability-authorized decisions.
//! - **Boundary:** Caller identity, ABI version, device/access identifiers,
//!   rights, flags, ioctl routes, and reply envelopes are untrusted.
//! - **Lifecycle:** Validate complete request, reserve any input description,
//!   install one handle, or release the reservation on every failed branch.
//! - **Concurrency:** No service call runs while a process handle-table
//!   mutation is held; installation rechecks the current process generation.
//! - **Failure:** Malformed, stale, over-authorized, or unrouteable requests
//!   fail without leaking an input registration or partial descriptor.
//! - **Forbidden:** Ring0 must not invent device policy, widen returned rights,
//!   or route to a weaker provider when the named owner is unavailable.
//! - **Evidence:** `commercial-envelope`, `service-call-authority`.
use super::*;

use crate::user::sysops::device::{self, DeviceSysopError};
use kernel_object::api::device::{DeviceAccessKind, DeviceId};
use kernel_object::api::handle::{DeviceHandleRights, HandleRights};
use rustos_user_abi::syscall::{ServiceCapability,
    DEVMGRD_DEVICE_ACCESS_EVDEV, DEVMGRD_DEVICE_ACCESS_NATIVE, DEVMGRD_DEVICE_ID_CONSOLE,
    DEVMGRD_DEVICE_ID_DISPLAY, DEVMGRD_DEVICE_ID_INPUT, DEVMGRD_DEVICE_RIGHT_ADMIN,
    DEVMGRD_DEVICE_RIGHT_IOCTL, DEVMGRD_DEVICE_RIGHT_MAP, DEVMGRD_DEVICE_RIGHT_READ,
    DEVMGRD_DEVICE_RIGHT_TRANSFER, DEVMGRD_DEVICE_RIGHT_WRITE, DEVMGRD_IOCTL_ROUTE_SESSIOND_COMMIT,
    DEVMGRD_IPC_ABI_VERSION, DEVMGRD_IPC_OP_IOCTL_ROUTE, RustosDeviceIoctlBrokerArgs,
    RustosDeviceOpenBrokerArgs,
};

pub(super) fn syscall_linux_rustos_device_open_broker(args_ptr: u64) -> u64 {
    linux_result((|| {
        let args: RustosDeviceOpenBrokerArgs =
            require_capability_and_read_args(ServiceCapability::DevicePolicy, args_ptr)?;
        if args.abi_version != DEVMGRD_IPC_ABI_VERSION
            || args.reserved0 != 0
            || args.reserved1 != 0
            || args.rights == 0
            || args.rights & !allowed_device_rights_mask() != 0
        {
            return Err(LINUX_EINVAL);
        }

        let device_id = map_device_id(args.device_id).ok_or(LINUX_EINVAL)?;
        let access = map_device_access(args.access).ok_or(LINUX_EINVAL)?;
        let rights = device_handle_rights(args.rights).ok_or(LINUX_EINVAL)?;
        if (args.open_flags & linux_abi::O_ACCMODE) != linux_abi::O_RDONLY
            && !rights.allows_write()
        {
            return Err(LINUX_EACCES);
        }

        let device_handle = crate::io::device::DeviceHandle::with_access(device_id, access);
        let input_token = if device_id == DeviceId::Input {
            let input_access = match access {
                DeviceAccessKind::Native => INPUTD_ACCESS_NATIVE,
                DeviceAccessKind::Evdev => INPUTD_ACCESS_EVDEV,
            };
            waitset_broker_ops::register_input_open_description(
                device_handle.token_id(),
                input_access,
            )?;
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
            Some(Some(fd)) => Ok(fd),
            other => {
                if let Some(token) = input_token {
                    let _ = waitset_broker_ops::release_input_open_description(token);
                }
                Err(if other.is_none() { LINUX_EINVAL } else { LINUX_EMFILE })
            }
        }
    })())
}

pub(super) fn syscall_linux_rustos_device_ioctl_broker(args_ptr: u64) -> u64 {
    linux_result((|| {
        let has_device_policy =
            ipc_ops::current_process_has_service_capability(ServiceCapability::DevicePolicy);
        let has_session_policy =
            ipc_ops::current_process_has_service_capability(ServiceCapability::SessionPolicy);
        if !has_device_policy && !has_session_policy {
            return Err(LINUX_EPERM);
        }

        let args = read_user_struct::<RustosDeviceIoctlBrokerArgs>(args_ptr)?;
        if args.reserved0 != 0 || (args.process_id == 0 && !has_session_policy) {
            return Err(LINUX_EINVAL);
        }
        if !has_device_policy && !session_policy_device_ioctl_allowed(&args) {
            return Err(LINUX_EPERM);
        }
        let outcome = if !has_device_policy {
            device::ioctl_current_process_fd(args.fd, args.request, args.arg)
        } else {
            device::ioctl_process_device_handle(args.process_id, args.fd, args.request, args.arg)
        };
        outcome.map_err(device_sysop_error_to_linux_errno)
    })())
}

// RING3-MIGRATION-REFERENCE START: capability-broker exception: sessiond owns
// session ioctl commit policy. Ring0 keeps capability-gated native ioctl commit
// substrate.
fn session_policy_device_ioctl_allowed(args: &RustosDeviceIoctlBrokerArgs) -> bool {
    if args.process_id != 0
        && multitask::current_user_process_id().is_none_or(|pid| pid != args.process_id)
    {
        return false;
    }
    match session_ioctl_route_via_devmgrd(args.request) {
        Ok(route) => route == DEVMGRD_IOCTL_ROUTE_SESSIOND_COMMIT,
        Err(_) => false,
    }
}

fn session_ioctl_route_via_devmgrd(request_number: u64) -> Result<u64, i64> {
    let response = super::service_ops::ipc_helpers::call_devmgrd_ioctl(
        0,
        DEVMGRD_IPC_OP_IOCTL_ROUTE,
        request_number,
        0,
        true,
        false,
        false,
        |_| Ok(()),
    )?;
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
