pub(crate) mod display;
pub(crate) mod input;
pub(crate) mod runtime;

use core::mem::size_of;
use core::slice;

use x86_64::VirtAddr;

use crate::paging;
use crate::user::abi::{device as device_abi, runtime as runtime_abi};
use crate::user::process_state::UserProcessState;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum DeviceId {
    Display,
    Input,
    Runtime,
}

impl DeviceId {
    pub(crate) const fn path(self) -> &'static str {
        match self {
            Self::Display => device_abi::DISPLAY_PATH,
            Self::Input => device_abi::INPUT_PATH,
            Self::Runtime => runtime_abi::RUNTIME_PATH,
        }
    }

    pub(crate) const fn supports_read(self) -> bool {
        matches!(self, Self::Input)
    }

    pub(crate) const fn supports_ioctl(self) -> bool {
        matches!(self, Self::Display | Self::Runtime)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct DeviceHandle {
    device_id: DeviceId,
}

impl DeviceHandle {
    pub(crate) const fn new(device_id: DeviceId) -> Self {
        Self { device_id }
    }

    pub(crate) const fn device_id(self) -> DeviceId {
        self.device_id
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct DeviceDescriptor {
    pub(crate) id: DeviceId,
    pub(crate) path: &'static str,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum DeviceLookupError {
    InvalidPath,
    NotFound,
}

#[derive(Debug, Clone, Copy)]
pub(crate) enum DeviceError {
    AddressSpace(paging::AddressSpaceError),
    Busy,
    DisplayUnavailable,
    InvalidArgument,
    NotFound,
    Unsupported,
}

impl From<paging::AddressSpaceError> for DeviceError {
    fn from(value: paging::AddressSpaceError) -> Self {
        Self::AddressSpace(value)
    }
}

const DEVICE_DESCRIPTORS: [DeviceDescriptor; 3] = [
    DeviceDescriptor {
        id: DeviceId::Display,
        path: DeviceId::Display.path(),
    },
    DeviceDescriptor {
        id: DeviceId::Input,
        path: DeviceId::Input.path(),
    },
    DeviceDescriptor {
        id: DeviceId::Runtime,
        path: DeviceId::Runtime.path(),
    },
];

pub(crate) fn lookup(path: &str) -> Result<DeviceDescriptor, DeviceLookupError> {
    let normalized = normalize_device_path(path)?;
    DEVICE_DESCRIPTORS
        .into_iter()
        .find(|descriptor| descriptor.path == normalized)
        .ok_or(DeviceLookupError::NotFound)
}

pub(crate) fn open(path: &str) -> Result<DeviceHandle, DeviceLookupError> {
    let descriptor = lookup(path)?;
    Ok(DeviceHandle::new(descriptor.id))
}

pub(crate) fn read_to_user(
    handle: DeviceHandle,
    process_state: &mut UserProcessState,
    user_ptr: u64,
    user_len: usize,
) -> Result<usize, DeviceError> {
    match handle.device_id() {
        DeviceId::Input => input::read_to_user(process_state, user_ptr, user_len),
        DeviceId::Display | DeviceId::Runtime => Err(DeviceError::Unsupported),
    }
}

pub(crate) fn ioctl_from_user(
    handle: DeviceHandle,
    process_state: &mut UserProcessState,
    request: u64,
    arg: u64,
) -> Result<u64, DeviceError> {
    match handle.device_id() {
        DeviceId::Display => display::ioctl(process_state, request, arg),
        DeviceId::Runtime => runtime::ioctl(process_state, request, arg),
        DeviceId::Input => Err(DeviceError::Unsupported),
    }
}

pub(crate) fn display_info() -> Result<device_abi::DisplayInfo, DeviceError> {
    display::query_info()
}

pub(crate) fn present_frame_from_user(
    address_space: &paging::ProcessAddressSpace,
    user_ptr: u64,
    width: u64,
    height: u64,
    stride_bytes: u64,
    pixel_format: u64,
) -> Result<(), DeviceError> {
    display::present_legacy_frame(
        address_space,
        user_ptr,
        width,
        height,
        stride_bytes,
        pixel_format,
    )
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

fn normalize_device_path(path: &str) -> Result<&str, DeviceLookupError> {
    if !path.starts_with('/') {
        return Err(DeviceLookupError::InvalidPath);
    }

    let mut components = path
        .split('/')
        .filter(|component| !component.is_empty() && *component != ".");
    let Some(root) = components.next() else {
        return Err(DeviceLookupError::InvalidPath);
    };
    if root != "dev" {
        return Err(DeviceLookupError::NotFound);
    }

    let Some(name) = components.next() else {
        return Err(DeviceLookupError::InvalidPath);
    };
    if components.next().is_some() {
        return Err(DeviceLookupError::NotFound);
    }

    Ok(match name {
        "display0" => DeviceId::Display.path(),
        "input0" => DeviceId::Input.path(),
        "runtime0" => DeviceId::Runtime.path(),
        _ => return Err(DeviceLookupError::NotFound),
    })
}

#[cfg(test)]
mod tests {
    use super::{DeviceId, DeviceLookupError, lookup, open};

    #[test]
    fn lookup_accepts_registered_device_paths() {
        assert_eq!(lookup("/dev/display0").unwrap().id, DeviceId::Display);
        assert_eq!(lookup("/dev/input0").unwrap().id, DeviceId::Input);
        assert_eq!(lookup("/dev/runtime0").unwrap().id, DeviceId::Runtime);
    }

    #[test]
    fn lookup_normalizes_redundant_separators() {
        assert_eq!(lookup("//dev///display0").unwrap().id, DeviceId::Display);
    }

    #[test]
    fn lookup_rejects_invalid_namespaces() {
        assert_eq!(lookup("/tmp/display0"), Err(DeviceLookupError::NotFound));
        assert_eq!(lookup("display0"), Err(DeviceLookupError::InvalidPath));
        assert_eq!(
            lookup("/dev/display0/child"),
            Err(DeviceLookupError::NotFound)
        );
    }

    #[test]
    fn open_returns_device_handle() {
        assert_eq!(
            open("/dev/runtime0").unwrap().device_id(),
            DeviceId::Runtime
        );
    }
}
