pub(crate) mod display;

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
    token: u64,
}

impl DeviceHandle {
    pub fn from_parts(device_id: DeviceId, access_kind: DeviceAccessKind) -> Self {
        Self::from_object_handle(kernel_object::api::device::DeviceHandle::with_access(
            device_id.into(),
            access_kind.into(),
        ))
    }

    pub const fn device_id(self) -> DeviceId {
        self.device_id
    }

    pub const fn access_kind(self) -> DeviceAccessKind {
        self.access_kind
    }

    pub const fn token_id(self) -> u64 {
        self.token
    }

    pub fn into_object_handle(self) -> kernel_object::api::device::DeviceHandle {
        kernel_object::api::device::DeviceHandle::from_parts_with_token(
            match self.device_id {
                DeviceId::Console => kernel_object::api::device::DeviceId::Console,
                DeviceId::Display => kernel_object::api::device::DeviceId::Display,
                DeviceId::Input => kernel_object::api::device::DeviceId::Input,
            },
            match self.access_kind {
                DeviceAccessKind::Native => kernel_object::api::device::DeviceAccessKind::Native,
                DeviceAccessKind::Evdev => kernel_object::api::device::DeviceAccessKind::Evdev,
            },
            self.token,
        )
    }

    pub const fn from_object_handle(handle: kernel_object::api::device::DeviceHandle) -> Self {
        Self {
            device_id: match handle.device_id() {
                kernel_object::api::device::DeviceId::Console => DeviceId::Console,
                kernel_object::api::device::DeviceId::Display => DeviceId::Display,
                kernel_object::api::device::DeviceId::Input => DeviceId::Input,
            },
            access_kind: match handle.access_kind() {
                kernel_object::api::device::DeviceAccessKind::Native => DeviceAccessKind::Native,
                kernel_object::api::device::DeviceAccessKind::Evdev => DeviceAccessKind::Evdev,
            },
            token: handle.token_id(),
        }
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
    TryAgain,
    Unsupported,
}

impl From<paging::AddressSpaceError> for DeviceError {
    fn from(value: paging::AddressSpaceError) -> Self {
        Self::AddressSpace(value)
    }
}

pub fn ioctl_from_user(
    handle: DeviceHandle,
    process_id: u64,
    process_state: &mut UserProcessState,
    request: u64,
    arg: u64,
) -> Result<u64, DeviceError> {
    match handle.device_id() {
        DeviceId::Console => Err(DeviceError::Unsupported),
        DeviceId::Display => display::ioctl(process_id, process_state, request, arg),
        DeviceId::Input => Err(DeviceError::Unsupported),
    }
}

// SAFETY: Implementors must be repr(C), contain only fields for which every
// bit pattern is valid, and have no padding (including nested fields). This
// permits both arbitrary user bytes on input and bytewise reads on output.
unsafe trait UserDevicePod: Copy {}

// These ABI records contain only u32, u64 and byte-array fields. The size
// assertions additionally reject padding changes to their present layouts.
macro_rules! user_device_pod {
    ($($ty:ty => $size:expr),+ $(,)?) => {$ (
        const _: () = assert!(size_of::<$ty>() == $size);
        // SAFETY: see the field and layout requirements above.
        unsafe impl UserDevicePod for $ty {}
    )+};
}
user_device_pod! {
    device_abi::DisplayInfo => 32,
    device_abi::DisplaySurfaceCreate => 48,
    device_abi::DisplayPresentRequest => 8,
    device_abi::DisplayPresentRectRequest => 24,
    device_abi::DisplayGpuInfo => 64,
    device_abi::DisplayGpuDamage => 16,
    device_abi::DisplayGpuSubmitRequest => 40,
    device_abi::DisplayGpuCompletionQuery => 264,
}

fn read_user_struct<T: UserDevicePod>(
    address_space: &paging::ProcessAddressSpace,
    user_ptr: u64,
) -> Result<T, DeviceError> {
    // SAFETY: UserDevicePod permits the all-zero bit pattern.
    let mut value = unsafe { core::mem::zeroed::<T>() };
    let bytes = unsafe { slice::from_raw_parts_mut((&mut value as *mut T).cast::<u8>(), size_of::<T>()) };
    address_space.validate_user_read_buffer(VirtAddr::new(user_ptr), bytes.len())?;
    address_space.copy_from_user(VirtAddr::new(user_ptr), bytes)?;
    Ok(value)
}

fn write_user_struct<T: UserDevicePod>(
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
    fn device_pod_records_accept_all_ones_and_round_trip_bytes() {
        fn check<T: super::UserDevicePod>() {
            let mut value = unsafe { core::mem::zeroed::<T>() };
            let bytes = unsafe {
                core::slice::from_raw_parts_mut(
                    (&mut value as *mut T).cast::<u8>(), core::mem::size_of::<T>(),
                )
            };
            bytes.fill(0xff);
            let output = unsafe {
                core::slice::from_raw_parts(
                    (&value as *const T).cast::<u8>(), core::mem::size_of::<T>(),
                )
            };
            assert!(output.iter().all(|byte| *byte == 0xff));
        }
        check::<super::device_abi::DisplayInfo>();
        check::<super::device_abi::DisplaySurfaceCreate>();
        check::<super::device_abi::DisplayPresentRequest>();
        check::<super::device_abi::DisplayPresentRectRequest>();
        check::<super::device_abi::DisplayGpuInfo>();
        check::<super::device_abi::DisplayGpuDamage>();
        check::<super::device_abi::DisplayGpuSubmitRequest>();
        check::<super::device_abi::DisplayGpuCompletionQuery>();
    }

    #[test]
    fn device_handle_round_trip_keeps_id_and_access() {
        let handle = DeviceHandle::from_parts(DeviceId::Input, DeviceAccessKind::Evdev);
        let object = handle.into_object_handle();
        let restored = DeviceHandle::from_object_handle(object);
        assert_eq!(restored.device_id(), DeviceId::Input);
        assert_eq!(restored.access_kind(), DeviceAccessKind::Evdev);
        assert_eq!(restored.token_id(), handle.token_id());
    }
}
