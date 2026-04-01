use core::convert::TryFrom;

use x86_64::structures::paging::PageTableFlags;
use x86_64::VirtAddr;

use crate::io::device::{self as device_ns};
use crate::memory::paging;
use crate::multitask;
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
    StaleSurface,
    Unsupported,
}

impl From<paging::AddressSpaceError> for DeviceSysopError {
    fn from(value: paging::AddressSpaceError) -> Self {
        Self::AddressSpace(value)
    }
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

#[cfg_attr(not(rustos_debug_print_enabled), allow(unused_variables))]
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
                let device_id = device_handle.device_id();
                let result =
                    device_ns::read_to_user(*device_handle, process_state, user_ptr, user_len)
                        .map_err(map_device_error);
                if matches!(result, Err(DeviceSysopError::Unsupported)) {
                    crate::debug::println!(
                        "device read unsupported: fd={} device={:?} user_ptr={:#x} len={}",
                        fd,
                        device_id,
                        user_ptr,
                        user_len,
                    );
                }
                result
            }
            Some(KernelHandle::Console(_)) => {
                crate::debug::println!(
                    "device read wrong-handle: fd={} handle=console user_ptr={:#x} len={}",
                    fd,
                    user_ptr,
                    user_len,
                );
                Err(DeviceSysopError::Unsupported)
            }
            Some(KernelHandle::VfsFile(_)) => {
                crate::debug::println!(
                    "device read wrong-handle: fd={} handle=vfs-file user_ptr={:#x} len={}",
                    fd,
                    user_ptr,
                    user_len,
                );
                Err(DeviceSysopError::Unsupported)
            }
            Some(KernelHandle::VfsDirectory(_)) => {
                crate::debug::println!(
                    "device read wrong-handle: fd={} handle=vfs-dir user_ptr={:#x} len={}",
                    fd,
                    user_ptr,
                    user_len,
                );
                Err(DeviceSysopError::Unsupported)
            }
            Some(KernelHandle::DisplaySurface(_)) => {
                crate::debug::println!(
                    "device read wrong-handle: fd={} handle=display-surface user_ptr={:#x} len={}",
                    fd,
                    user_ptr,
                    user_len,
                );
                Err(DeviceSysopError::Unsupported)
            }
            None => Err(DeviceSysopError::BadFileDescriptor),
        }
    }) else {
        return Err(DeviceSysopError::Unsupported);
    };

    result
}

pub(crate) fn ioctl_current_process_device_handle(
    device_handle: device_ns::DeviceHandle,
    request: u64,
    arg: u64,
) -> Result<u64, DeviceSysopError> {
    let Some(result) = multitask::with_current_user_process_state_mut(|_, _, process_state| {
        device_ns::ioctl_from_user(device_handle, process_state, request, arg)
            .map_err(map_device_error)
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

fn map_device_error(err: device_ns::DeviceError) -> DeviceSysopError {
    match err {
        device_ns::DeviceError::AddressSpace(err) => DeviceSysopError::AddressSpace(err),
        device_ns::DeviceError::Busy => DeviceSysopError::Busy,
        device_ns::DeviceError::DisplayUnavailable => DeviceSysopError::DisplayUnavailable,
        device_ns::DeviceError::InvalidArgument => DeviceSysopError::InvalidArgument,
        device_ns::DeviceError::NotFound => DeviceSysopError::NotFound,
        device_ns::DeviceError::StaleSurface => DeviceSysopError::StaleSurface,
        device_ns::DeviceError::Unsupported => DeviceSysopError::Unsupported,
    }
}
