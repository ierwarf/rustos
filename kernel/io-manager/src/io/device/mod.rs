pub(crate) mod console;
pub(crate) mod debug;
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
    Debug,
    Display,
    Input,
}

impl DeviceId {
    pub const fn path(self) -> &'static str {
        match self {
            Self::Console => console_abi::CONSOLE_PATH,
            Self::Debug => diag_abi::DIAG_DEVICE_PATH,
            Self::Display => device_abi::DISPLAY_PATH,
            Self::Input => device_abi::INPUT_PATH,
        }
    }
}

impl From<DeviceId> for kernel_object::api::device::DeviceId {
    fn from(value: DeviceId) -> Self {
        match value {
            DeviceId::Console => Self::Console,
            DeviceId::Debug => Self::Debug,
            DeviceId::Display => Self::Display,
            DeviceId::Input => Self::Input,
        }
    }
}

impl From<kernel_object::api::device::DeviceId> for DeviceId {
    fn from(value: kernel_object::api::device::DeviceId) -> Self {
        match value {
            kernel_object::api::device::DeviceId::Console => Self::Console,
            kernel_object::api::device::DeviceId::Debug => Self::Debug,
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

    pub(crate) const fn with_access(device_id: DeviceId, access_kind: DeviceAccessKind) -> Self {
        Self::from_parts(device_id, access_kind)
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
                DeviceId::Debug => kernel_object::api::device::DeviceId::Debug,
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
                kernel_object::api::device::DeviceId::Debug => DeviceId::Debug,
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

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct DeviceDescriptor {
    pub(crate) id: DeviceId,
    pub(crate) path: &'static str,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DeviceLookupError {
    InvalidPath,
    NotFound,
}

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

const DEVICE_DESCRIPTORS: [DeviceDescriptor; 4] = [
    DeviceDescriptor {
        id: DeviceId::Console,
        path: DeviceId::Console.path(),
    },
    DeviceDescriptor {
        id: DeviceId::Debug,
        path: DeviceId::Debug.path(),
    },
    DeviceDescriptor {
        id: DeviceId::Display,
        path: DeviceId::Display.path(),
    },
    DeviceDescriptor {
        id: DeviceId::Input,
        path: DeviceId::Input.path(),
    },
];

pub(crate) fn descriptors() -> &'static [DeviceDescriptor] {
    &DEVICE_DESCRIPTORS
}

pub fn lookup(path: &str) -> Result<DeviceDescriptor, DeviceLookupError> {
    Ok(normalize_device_path(path)?.descriptor)
}

pub fn open(path: &str) -> Result<DeviceHandle, DeviceLookupError> {
    let normalized = normalize_device_path(path)?;
    Ok(DeviceHandle::with_access(
        normalized.descriptor.id,
        normalized.access_kind,
    ))
}

pub fn read_to_user(
    handle: DeviceHandle,
    process_state: &mut UserProcessState,
    user_ptr: u64,
    user_len: usize,
) -> Result<usize, DeviceError> {
    match handle.device_id() {
        DeviceId::Debug => debug::read_to_user(process_state, user_ptr, user_len),
        DeviceId::Input => match handle.access_kind() {
            DeviceAccessKind::Native => input::read_to_user(process_state, user_ptr, user_len),
            DeviceAccessKind::Evdev => input::read_evdev_to_user(process_state, user_ptr, user_len),
        },
        DeviceId::Console | DeviceId::Display => Err(DeviceError::Unsupported),
    }
}

pub fn read_to_current_user(
    handle: DeviceHandle,
    user_ptr: u64,
    user_len: usize,
) -> Result<usize, DeviceError> {
    match handle.device_id() {
        DeviceId::Debug => debug::read_to_current_user(user_ptr, user_len),
        DeviceId::Input => match handle.access_kind() {
            DeviceAccessKind::Native => input::read_to_current_user(user_ptr, user_len),
            DeviceAccessKind::Evdev => input::read_evdev_to_current_user(user_ptr, user_len),
        },
        DeviceId::Console | DeviceId::Display => Err(DeviceError::Unsupported),
    }
}

pub fn ioctl_from_user(
    handle: DeviceHandle,
    process_state: &mut UserProcessState,
    request: u64,
    arg: u64,
) -> Result<u64, DeviceError> {
    match handle.device_id() {
        DeviceId::Console => console::ioctl(process_state, request, arg),
        DeviceId::Debug => debug::ioctl(process_state, request, arg),
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

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct NormalizedDevicePath {
    descriptor: DeviceDescriptor,
    access_kind: DeviceAccessKind,
}

fn normalize_device_path(path: &str) -> Result<NormalizedDevicePath, DeviceLookupError> {
    if !path.starts_with('/') {
        return Err(DeviceLookupError::InvalidPath);
    }

    let components = path
        .split('/')
        .filter(|component| !component.is_empty() && *component != ".");
    let parts = components.collect::<alloc::vec::Vec<_>>();
    let Some(root) = parts.first().copied() else {
        return Err(DeviceLookupError::InvalidPath);
    };
    if root != "dev" {
        return Err(DeviceLookupError::NotFound);
    }

    match parts.as_slice() {
        ["dev", "console0"] => Ok(NormalizedDevicePath {
            descriptor: DEVICE_DESCRIPTORS[0],
            access_kind: DeviceAccessKind::Native,
        }),
        ["dev", "debug0"] => Ok(NormalizedDevicePath {
            descriptor: DEVICE_DESCRIPTORS[1],
            access_kind: DeviceAccessKind::Native,
        }),
        ["dev", "display0"] | ["dev", "dri", "card0"] => Ok(NormalizedDevicePath {
            descriptor: DEVICE_DESCRIPTORS[2],
            access_kind: DeviceAccessKind::Native,
        }),
        ["dev", "input0"] => Ok(NormalizedDevicePath {
            descriptor: DEVICE_DESCRIPTORS[3],
            access_kind: DeviceAccessKind::Native,
        }),
        ["dev", "input", "event0"] => Ok(NormalizedDevicePath {
            descriptor: DEVICE_DESCRIPTORS[3],
            access_kind: DeviceAccessKind::Evdev,
        }),
        _ => Err(DeviceLookupError::NotFound),
    }
}

#[cfg(test)]
mod tests {
    use super::{DeviceAccessKind, DeviceId, DeviceLookupError, lookup, open};

    #[test]
    fn lookup_accepts_registered_device_paths() {
        assert_eq!(lookup("/dev/console0").unwrap().id, DeviceId::Console);
        assert_eq!(lookup("/dev/debug0").unwrap().id, DeviceId::Debug);
        assert_eq!(lookup("/dev/display0").unwrap().id, DeviceId::Display);
        assert_eq!(lookup("/dev/input0").unwrap().id, DeviceId::Input);
        assert_eq!(lookup("/dev/dri/card0").unwrap().id, DeviceId::Display);
        assert_eq!(lookup("/dev/input/event0").unwrap().id, DeviceId::Input);
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
        assert_eq!(open("/dev/input0").unwrap().device_id(), DeviceId::Input);
        assert_eq!(
            open("/dev/input/event0").unwrap().access_kind(),
            DeviceAccessKind::Evdev
        );
    }
}
