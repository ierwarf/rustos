// RING3-MIGRATION-REFERENCE START: devmgrd should own device namespace, open,
// and ioctl policy. Ring0 keeps native device handles and current-process
// user-copy substrate.
pub(crate) mod display;
pub(crate) mod input;

use core::mem::size_of;
use core::slice;

use x86_64::VirtAddr;

use crate::memory::paging;
use crate::user::abi::{console as console_abi, device as device_abi};
use crate::user::process_state::UserProcessState;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DeviceId {
    Console,
    Display,
    Input,
}

impl DeviceId {
    pub const fn path(self) -> &'static str {
        match self {
            Self::Console => console_abi::CONSOLE_PATH,
            Self::Display => device_abi::DISPLAY_PATH,
            Self::Input => device_abi::INPUT_PATH,
        }
    }
}

impl From<DeviceId> for kernel_object::api::device::DeviceId {
    fn from(value: DeviceId) -> Self {
        match value {
            DeviceId::Console => Self::Console,
            DeviceId::Display => Self::Display,
            DeviceId::Input => Self::Input,
        }
    }
}

impl From<kernel_object::api::device::DeviceId> for DeviceId {
    fn from(value: kernel_object::api::device::DeviceId) -> Self {
        match value {
            kernel_object::api::device::DeviceId::Console => Self::Console,
            kernel_object::api::device::DeviceId::Display => Self::Display,
            kernel_object::api::device::DeviceId::Input => Self::Input,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DeviceAccessKind {
    Native,
    Evdev,
}

impl From<DeviceAccessKind> for kernel_object::api::device::DeviceAccessKind {
    fn from(value: DeviceAccessKind) -> Self {
        match value {
            DeviceAccessKind::Native => Self::Native,
            DeviceAccessKind::Evdev => Self::Evdev,
        }
    }
}

impl From<kernel_object::api::device::DeviceAccessKind> for DeviceAccessKind {
    fn from(value: kernel_object::api::device::DeviceAccessKind) -> Self {
        match value {
            kernel_object::api::device::DeviceAccessKind::Native => Self::Native,
            kernel_object::api::device::DeviceAccessKind::Evdev => Self::Evdev,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DeviceHandle {
    device_id: DeviceId,
    access_kind: DeviceAccessKind,
}

impl DeviceHandle {
    pub const fn from_parts(device_id: DeviceId, access_kind: DeviceAccessKind) -> Self {
        Self {
            device_id,
            access_kind,
        }
    }

    pub const fn device_id(self) -> DeviceId {
        self.device_id
    }

    pub const fn access_kind(self) -> DeviceAccessKind {
        self.access_kind
    }

    pub const fn into_object_handle(self) -> kernel_object::api::device::DeviceHandle {
        kernel_object::api::device::DeviceHandle::with_access(
            match self.device_id {
                DeviceId::Console => kernel_object::api::device::DeviceId::Console,
                DeviceId::Display => kernel_object::api::device::DeviceId::Display,
                DeviceId::Input => kernel_object::api::device::DeviceId::Input,
            },
            match self.access_kind {
                DeviceAccessKind::Native => kernel_object::api::device::DeviceAccessKind::Native,
                DeviceAccessKind::Evdev => kernel_object::api::device::DeviceAccessKind::Evdev,
            },
        )
    }

    pub const fn from_object_handle(handle: kernel_object::api::device::DeviceHandle) -> Self {
        Self::from_parts(
            match handle.device_id() {
                kernel_object::api::device::DeviceId::Console => DeviceId::Console,
                kernel_object::api::device::DeviceId::Display => DeviceId::Display,
                kernel_object::api::device::DeviceId::Input => DeviceId::Input,
            },
            match handle.access_kind() {
                kernel_object::api::device::DeviceAccessKind::Native => DeviceAccessKind::Native,
                kernel_object::api::device::DeviceAccessKind::Evdev => DeviceAccessKind::Evdev,
            },
        )
    }
}

impl From<DeviceHandle> for kernel_object::api::device::DeviceHandle {
    fn from(value: DeviceHandle) -> Self {
        value.into_object_handle()
    }
}

impl From<kernel_object::api::device::DeviceHandle> for DeviceHandle {
    fn from(value: kernel_object::api::device::DeviceHandle) -> Self {
        Self::from_object_handle(value)
    }
}

// `/dev` namespace lookup is owned by `devmgrd`; the kernel only keeps typed
// device IO brokers (read/ioctl) below.
#[derive(Debug, Clone, Copy)]
pub enum DeviceError {
    AddressSpace(paging::AddressSpaceError),
    DisplayUnavailable,
    InvalidArgument,
    NotFound,
    StaleSurface,
    Unsupported,
}

impl From<paging::AddressSpaceError> for DeviceError {
    fn from(value: paging::AddressSpaceError) -> Self {
        Self::AddressSpace(value)
    }
}

pub fn ioctl_from_user(
    handle: DeviceHandle,
    process_state: &mut UserProcessState,
    request: u64,
    arg: u64,
) -> Result<u64, DeviceError> {
    match handle.device_id() {
        DeviceId::Console => Err(DeviceError::Unsupported),
        DeviceId::Display => display::ioctl(process_state, request, arg),
        DeviceId::Input => Err(DeviceError::Unsupported),
    }
}

pub(super) fn read_user_struct<T: Copy>(
    address_space: &paging::ProcessAddressSpace,
    user_ptr: u64,
) -> Result<T, DeviceError> {
    let mut value = core::mem::MaybeUninit::<T>::uninit();
    let bytes =
        unsafe { slice::from_raw_parts_mut(value.as_mut_ptr().cast::<u8>(), size_of::<T>()) };
    address_space.validate_user_read_buffer(VirtAddr::new(user_ptr), bytes.len())?;
    address_space.copy_from_user(VirtAddr::new(user_ptr), bytes)?;
    Ok(unsafe { value.assume_init() })
}

pub(super) fn write_user_struct<T: Copy>(
    address_space: &paging::ProcessAddressSpace,
    user_ptr: u64,
    value: &T,
) -> Result<(), DeviceError> {
    let bytes = unsafe { slice::from_raw_parts((value as *const T).cast::<u8>(), size_of::<T>()) };
    address_space.validate_user_write_buffer(VirtAddr::new(user_ptr), bytes.len())?;
    address_space.copy_into_user(VirtAddr::new(user_ptr), bytes)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{DeviceAccessKind, DeviceHandle, DeviceId};

    #[test]
    fn device_handle_round_trip_keeps_id_and_access() {
        let handle = DeviceHandle::with_access(DeviceId::Input, DeviceAccessKind::Evdev);
        let object = handle.into_object_handle();
        let restored = DeviceHandle::from_object_handle(object);
        assert_eq!(restored.device_id(), DeviceId::Input);
        assert_eq!(restored.access_kind(), DeviceAccessKind::Evdev);
    }
}
// RING3-MIGRATION-REFERENCE END: devmgrd-owned device namespace policy.
