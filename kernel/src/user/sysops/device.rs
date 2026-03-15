use core::convert::TryFrom;

use x86_64::VirtAddr;
use x86_64::structures::paging::PageTableFlags;

use crate::io::device::{self as device_ns};
use crate::multitask;
use crate::paging;
use crate::user::abi::device::DisplayInfo;
use crate::user::handles::{DisplaySurfaceHandle, KernelHandle};
use crate::user::linux as linux_abi;
use crate::user::process_state::UserProcessState;
const PAGE_SIZE: u64 = 4096;

#[derive(Debug, Clone, Copy)]
pub(crate) enum DeviceSysopError {
    AddressSpace(paging::AddressSpaceError),
    BadFileDescriptor,
    Busy,
    InvalidArgument,
    DisplayUnavailable,
    NotFound,
    Unsupported,
}

impl From<paging::AddressSpaceError> for DeviceSysopError {
    fn from(value: paging::AddressSpaceError) -> Self {
        Self::AddressSpace(value)
    }
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct LegacySurfaceAllocation {
    pub address: u64,
    pub len: u64,
    pub width: u32,
    pub height: u32,
    pub stride_bytes: u32,
    pub bytes_per_pixel: u32,
    pub pixel_format: u32,
}

pub(crate) fn open_path_for_current_process(path: &str) -> Result<u64, DeviceSysopError> {
    let Some(result) = multitask::with_current_user_process_state_mut(|_, _, process_state| {
        let handle = device_ns::open(path)
            .map(KernelHandle::Device)
            .map_err(map_lookup_error)?;
        Ok(process_state.handles_mut().install(handle))
    }) else {
        return Err(DeviceSysopError::Unsupported);
    };

    result
}

pub(crate) fn close_current_process_handle(fd: u64) -> Result<(), DeviceSysopError> {
    let Some(result) = multitask::with_current_user_process_state_mut(|_, _, process_state| {
        process_state
            .handles_mut()
            .close(fd)
            .map(|_| ())
            .ok_or(DeviceSysopError::BadFileDescriptor)
    }) else {
        return Err(DeviceSysopError::Unsupported);
    };

    result
}

pub(crate) fn read_current_process_handle(
    fd: u64,
    user_ptr: u64,
    user_len: u64,
) -> Result<usize, DeviceSysopError> {
    let user_len = usize::try_from(user_len).map_err(|_| DeviceSysopError::InvalidArgument)?;
    if user_len == 0 {
        return Ok(0);
    }

    let Some(result) = multitask::with_current_user_process_state_mut(|_, _, process_state| {
        match process_state.handles().get(fd) {
            Some(KernelHandle::Device(device_handle)) => {
                device_ns::read_to_user(*device_handle, process_state, user_ptr, user_len)
                    .map_err(map_device_error)
            }
            Some(_) => Err(DeviceSysopError::Unsupported),
            None => Err(DeviceSysopError::BadFileDescriptor),
        }
    }) else {
        return Err(DeviceSysopError::Unsupported);
    };

    result
}

pub(crate) fn ioctl_current_process_handle(
    fd: u64,
    request: u64,
    arg: u64,
) -> Result<u64, DeviceSysopError> {
    let Some(result) = multitask::with_current_user_process_state_mut(|_, _, process_state| {
        match process_state.handles().get(fd) {
            Some(KernelHandle::Device(device_handle)) => {
                device_ns::ioctl_from_user(*device_handle, process_state, request, arg)
                    .map_err(map_device_error)
            }
            Some(_) => Err(DeviceSysopError::Unsupported),
            None => Err(DeviceSysopError::BadFileDescriptor),
        }
    }) else {
        return Err(DeviceSysopError::Unsupported);
    };

    result
}

pub(crate) fn mmap_current_process_handle(
    fd: u64,
    user_len: u64,
    prot: u64,
    flags: u64,
    offset: u64,
) -> Result<u64, DeviceSysopError> {
    if flags & linux_abi::MAP_SHARED == 0 || flags & linux_abi::MAP_ANONYMOUS != 0 {
        return Err(DeviceSysopError::InvalidArgument);
    }

    let Some(result) = multitask::with_current_user_process_state_mut(|_, _, process_state| {
        let mut surface = match process_state.handles().get(fd) {
            Some(KernelHandle::DisplaySurface(surface)) => *surface,
            Some(_) => return Err(DeviceSysopError::Unsupported),
            None => return Err(DeviceSysopError::BadFileDescriptor),
        };
        let mapped_addr = map_surface(process_state, &mut surface, user_len, prot, offset)?;
        let slot = process_state
            .handles_mut()
            .get_mut(fd)
            .ok_or(DeviceSysopError::BadFileDescriptor)?;
        *slot = KernelHandle::DisplaySurface(surface);
        Ok(mapped_addr)
    }) else {
        return Err(DeviceSysopError::Unsupported);
    };

    result
}

pub(crate) fn munmap_current_process_range(
    start: u64,
    user_len: u64,
) -> Result<(), DeviceSysopError> {
    let user_len = usize::try_from(user_len).map_err(|_| DeviceSysopError::InvalidArgument)?;
    if user_len == 0 {
        return Err(DeviceSysopError::InvalidArgument);
    }

    let Some(result) = multitask::with_current_user_process_state_mut(|_, _, process_state| {
        let unmapped_pages = process_state
            .address_space_mut()
            .unmap_user_bytes(VirtAddr::new(start), user_len)?;
        let unmapped_len = (unmapped_pages as u64)
            .checked_mul(PAGE_SIZE)
            .ok_or(DeviceSysopError::InvalidArgument)?;
        process_state
            .handles_mut()
            .clear_surface_mappings_in_range(start, unmapped_len);
        Ok(())
    }) else {
        return Err(DeviceSysopError::Unsupported);
    };

    result
}

pub(crate) fn allocate_legacy_surface_to_current_process(
    width: u64,
    height: u64,
    pixel_format: u64,
) -> Result<LegacySurfaceAllocation, DeviceSysopError> {
    let width = u32::try_from(width).map_err(|_| DeviceSysopError::InvalidArgument)?;
    let height = u32::try_from(height).map_err(|_| DeviceSysopError::InvalidArgument)?;
    let pixel_format =
        u32::try_from(pixel_format).map_err(|_| DeviceSysopError::InvalidArgument)?;
    let mut surface = DisplaySurfaceHandle::new(width, height, pixel_format)
        .ok_or(DeviceSysopError::InvalidArgument)?;

    let Some(result) = multitask::with_current_user_process_state_mut(|_, _, process_state| {
        let mapping_len = surface.mapping_len();
        let address = map_surface(
            process_state,
            &mut surface,
            mapping_len,
            linux_abi::PROT_READ | linux_abi::PROT_WRITE,
            0,
        )?;
        Ok(LegacySurfaceAllocation {
            address,
            len: surface.mapping_len(),
            width: surface.width(),
            height: surface.height(),
            stride_bytes: surface.stride_bytes(),
            bytes_per_pixel: surface.bytes_per_pixel(),
            pixel_format: surface.pixel_format(),
        })
    }) else {
        return Err(DeviceSysopError::Unsupported);
    };

    result
}

pub(crate) fn query_display_info() -> Result<DisplayInfo, DeviceSysopError> {
    device_ns::display_info().map_err(map_device_error)
}

pub(crate) fn present_legacy_frame(
    address_space: &paging::ProcessAddressSpace,
    user_ptr: u64,
    width: u64,
    height: u64,
    stride_bytes: u64,
    pixel_format: u64,
) -> Result<(), DeviceSysopError> {
    device_ns::present_frame_from_user(
        address_space,
        user_ptr,
        width,
        height,
        stride_bytes,
        pixel_format,
    )
    .map_err(map_device_error)
}

fn map_surface(
    process_state: &mut UserProcessState,
    surface: &mut DisplaySurfaceHandle,
    user_len: u64,
    prot: u64,
    offset: u64,
) -> Result<u64, DeviceSysopError> {
    let supported_prot = linux_abi::PROT_READ | linux_abi::PROT_WRITE;
    if prot & !supported_prot != 0 || prot & linux_abi::PROT_EXEC != 0 {
        return Err(DeviceSysopError::InvalidArgument);
    }
    if offset != 0 || user_len == 0 || user_len != surface.mapping_len() {
        return Err(DeviceSysopError::InvalidArgument);
    }

    if let Some(region) = surface.mapped_region() {
        return Ok(region.start.as_u64());
    }

    let page_flags = surface_page_flags(prot);
    let page_count = usize::try_from(surface.mapping_len() / PAGE_SIZE)
        .map_err(|_| DeviceSysopError::InvalidArgument)?;
    let region = process_state.map_zeroed_pages_from_mapping_cursor(page_count, page_flags)?;
    surface.set_mapped_region(region);
    Ok(region.start.as_u64())
}

fn surface_page_flags(prot: u64) -> PageTableFlags {
    let mut flags = PageTableFlags::NO_EXECUTE;
    if prot & linux_abi::PROT_WRITE != 0 {
        flags |= PageTableFlags::WRITABLE;
    }
    flags
}

fn map_lookup_error(err: device_ns::DeviceLookupError) -> DeviceSysopError {
    match err {
        device_ns::DeviceLookupError::InvalidPath => DeviceSysopError::InvalidArgument,
        device_ns::DeviceLookupError::NotFound => DeviceSysopError::NotFound,
    }
}

fn map_device_error(err: device_ns::DeviceError) -> DeviceSysopError {
    match err {
        device_ns::DeviceError::AddressSpace(err) => DeviceSysopError::AddressSpace(err),
        device_ns::DeviceError::Busy => DeviceSysopError::Busy,
        device_ns::DeviceError::DisplayUnavailable => DeviceSysopError::DisplayUnavailable,
        device_ns::DeviceError::InvalidArgument => DeviceSysopError::InvalidArgument,
        device_ns::DeviceError::NotFound => DeviceSysopError::NotFound,
        device_ns::DeviceError::Unsupported => DeviceSysopError::Unsupported,
    }
}
