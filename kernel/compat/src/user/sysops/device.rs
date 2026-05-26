use core::convert::TryFrom;
use core::mem::size_of;
use core::slice;

use x86_64::structures::paging::PageTableFlags;
use x86_64::VirtAddr;

use crate::io::device::{self as device_ns};
use crate::memory::paging;
use crate::multitask;
use crate::user::handles::{DisplaySurfaceHandle, KernelHandle, RemoteVfsHandleKind};
use crate::user::linux as linux_abi;
use crate::user::process_state::UserProcessState;
use rustos_user_abi::device as device_abi;

const PAGE_SIZE: u64 = 4096;

#[derive(Debug, Clone, Copy)]
pub(crate) enum DeviceSysopError {
    AddressSpace(paging::AddressSpaceError),
    BadFileDescriptor,
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

pub(crate) fn ioctl_process_device_handle(
    process_id: u64,
    fd: u64,
    request: u64,
    arg: u64,
) -> Result<u64, DeviceSysopError> {
    let Some(result) = multitask::with_process_state_by_pid_mut(process_id, |process_state| {
        ioctl_via_process_state(process_state, fd, request, arg)
    }) else {
        return Err(DeviceSysopError::Unsupported);
    };

    result
}

/// Direct, no-IPC ioctl entry for the currently running user process. Used by the
/// kernel-side fast path so userspace `ioctl(2)` does not have to round-trip through
/// the devmgrd policy service for data-path operations (e.g. display present) where
/// no policy decision is actually performed today.
pub(crate) fn ioctl_current_process_fd(
    fd: u64,
    request: u64,
    arg: u64,
) -> Result<u64, DeviceSysopError> {
    let Some(result) = multitask::with_current_user_process_state_mut(|_, _, process_state| {
        ioctl_via_process_state(process_state, fd, request, arg)
    }) else {
        return Err(DeviceSysopError::Unsupported);
    };

    result
}

fn ioctl_via_process_state(
    process_state: &mut UserProcessState,
    fd: u64,
    request: u64,
    arg: u64,
) -> Result<u64, DeviceSysopError> {
    let Some(entry) = process_state.handles().get_entry(fd) else {
        return Err(DeviceSysopError::BadFileDescriptor);
    };
    match entry.handle() {
        KernelHandle::Device(device_handle) => {
            if !entry.rights().allows_device_ioctl() {
                return Err(DeviceSysopError::Unsupported);
            }
            device_ns::ioctl_from_user(*device_handle, process_state, request, arg)
                .map_err(map_device_error)
        }
        KernelHandle::RemoteVfs(remote) if remote.kind() == RemoteVfsHandleKind::Device => {
            let path = remote.path();
            ioctl_remote_device(process_state, path.as_str(), request, arg)
        }
        _ => Err(DeviceSysopError::Unsupported),
    }
}

fn ioctl_remote_device(
    process_state: &mut UserProcessState,
    path: &str,
    request: u64,
    arg: u64,
) -> Result<u64, DeviceSysopError> {
    if is_display_device_path(path) {
        return ioctl_display_device(process_state, request, arg);
    }
    Err(DeviceSysopError::Unsupported)
}

fn is_display_device_path(path: &str) -> bool {
    matches!(path, "/dev/display0" | "/dev/dri/card0")
}

fn ioctl_display_device(
    process_state: &mut UserProcessState,
    request: u64,
    arg: u64,
) -> Result<u64, DeviceSysopError> {
    match request {
        device_abi::DISPLAY_IOCTL_PRESENT => ioctl_display_present(process_state, arg),
        device_abi::DISPLAY_IOCTL_PRESENT_RECT => ioctl_display_present_rect(process_state, arg),
        _ => Err(DeviceSysopError::Unsupported),
    }
}

fn ioctl_display_present(
    process_state: &mut UserProcessState,
    arg: u64,
) -> Result<u64, DeviceSysopError> {
    let request = read_process_struct::<device_abi::DisplayPresentRequest>(process_state, arg)?;
    if request.reserved != 0 {
        return Err(DeviceSysopError::InvalidArgument);
    }
    let surface = process_surface(process_state, request.surface_handle)?;
    present_surface(surface, None)?;
    Ok(0)
}

fn ioctl_display_present_rect(
    process_state: &mut UserProcessState,
    arg: u64,
) -> Result<u64, DeviceSysopError> {
    let request = read_process_struct::<device_abi::DisplayPresentRectRequest>(process_state, arg)?;
    if request.reserved != 0 || request.width == 0 || request.height == 0 {
        return Err(DeviceSysopError::InvalidArgument);
    }
    let surface = process_surface(process_state, request.surface_handle)?;
    let x_end = request
        .x
        .checked_add(request.width)
        .ok_or(DeviceSysopError::InvalidArgument)?;
    let y_end = request
        .y
        .checked_add(request.height)
        .ok_or(DeviceSysopError::InvalidArgument)?;
    if x_end > surface.width() || y_end > surface.height() {
        return Err(DeviceSysopError::InvalidArgument);
    }
    present_surface(
        surface,
        Some((request.x, request.y, request.width, request.height)),
    )?;
    Ok(0)
}

fn process_surface(
    process_state: &UserProcessState,
    surface_fd: u32,
) -> Result<DisplaySurfaceHandle, DeviceSysopError> {
    match process_state.handles().get(u64::from(surface_fd)) {
        Some(KernelHandle::DisplaySurface(surface)) => Ok(*surface),
        Some(_) => Err(DeviceSysopError::Unsupported),
        None => Err(DeviceSysopError::BadFileDescriptor),
    }
}

fn present_surface(
    surface: DisplaySurfaceHandle,
    rect: Option<(u32, u32, u32, u32)>,
) -> Result<(), DeviceSysopError> {
    // Fast path: use the cached kernel pointer stashed when the shared region
    // was first installed. This avoids contending for the global IPC objects
    // mutex (which also disables interrupts) on every frame.
    let (ptr, len) = surface
        .shared_region_kernel_mapping()
        .or_else(|| {
            surface
                .shared_region()
                .and_then(crate::ipc::map_shared_region)
        })
        .ok_or(DeviceSysopError::InvalidArgument)?;
    let frame_len =
        usize::try_from(surface.frame_len()).map_err(|_| DeviceSysopError::InvalidArgument)?;
    if len < frame_len {
        return Err(DeviceSysopError::InvalidArgument);
    }
    let width = surface.width() as usize;
    let height = surface.height() as usize;
    let stride = surface.stride_bytes() as usize;
    let presented = match rect {
        Some((x, y, rect_width, rect_height)) => {
            kernel_io_manager::api::io::gui::present_userspace_frame_rect_from_kernel_bgra8888(
                ptr.cast_const(),
                width,
                height,
                stride,
                x as usize,
                y as usize,
                rect_width as usize,
                rect_height as usize,
            )
        }
        None => kernel_io_manager::api::io::gui::present_userspace_frame_from_kernel_bgra8888(
            ptr.cast_const(),
            width,
            height,
            stride,
        ),
    };
    if presented {
        Ok(())
    } else {
        Err(DeviceSysopError::DisplayUnavailable)
    }
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
        let end = start
            .checked_add(user_len as u64)
            .ok_or(DeviceSysopError::InvalidArgument)?;
        let shared_surface_segments = process_state.handles().surface_overlap_segments(start, end);
        if shared_surface_segments.is_empty() {
            let unmapped_pages = process_state
                .address_space_mut()
                .unmap_user_bytes(VirtAddr::new(start), user_len)?;
            let unmapped_len = (unmapped_pages as u64)
                .checked_mul(PAGE_SIZE)
                .ok_or(DeviceSysopError::InvalidArgument)?;
            process_state
                .handles_mut()
                .clear_surface_mappings_in_range(start, unmapped_len);
            return Ok(());
        }

        for (segment_start, segment_len) in &shared_surface_segments {
            let segment_len =
                usize::try_from(*segment_len).map_err(|_| DeviceSysopError::InvalidArgument)?;
            let page_count = segment_len.div_ceil(PAGE_SIZE as usize);
            process_state
                .address_space_mut()
                .unmap_user_pages_without_free_at(VirtAddr::new(*segment_start), page_count)?;
        }
        process_state
            .handles_mut()
            .clear_surface_mappings_in_range(start, user_len as u64);
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

    let shared_region = surface
        .shared_region()
        .ok_or(DeviceSysopError::InvalidArgument)?;
    let frames =
        crate::ipc::shared_region_frames(shared_region).ok_or(DeviceSysopError::InvalidArgument)?;
    let page_flags = surface_page_flags(prot);
    let expected_page_count = usize::try_from(surface.mapping_len() / PAGE_SIZE)
        .map_err(|_| DeviceSysopError::InvalidArgument)?;
    if frames.len() != expected_page_count {
        return Err(DeviceSysopError::InvalidArgument);
    }
    let region = process_state.map_existing_pages_from_mapping_cursor(&frames, page_flags)?;
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

fn read_process_struct<T: Copy + Default>(
    process_state: &UserProcessState,
    ptr: u64,
) -> Result<T, DeviceSysopError> {
    if ptr == 0 {
        return Err(DeviceSysopError::InvalidArgument);
    }
    let mut value = T::default();
    let bytes =
        unsafe { slice::from_raw_parts_mut((&mut value as *mut T).cast::<u8>(), size_of::<T>()) };
    process_state
        .address_space()
        .validate_user_read_buffer(VirtAddr::new(ptr), bytes.len())?;
    process_state
        .address_space()
        .copy_from_user(VirtAddr::new(ptr), bytes)?;
    Ok(value)
}

fn map_device_error(err: device_ns::DeviceError) -> DeviceSysopError {
    match err {
        device_ns::DeviceError::AddressSpace(err) => DeviceSysopError::AddressSpace(err),
        device_ns::DeviceError::DisplayUnavailable => DeviceSysopError::DisplayUnavailable,
        device_ns::DeviceError::InvalidArgument => DeviceSysopError::InvalidArgument,
        device_ns::DeviceError::NotFound => DeviceSysopError::NotFound,
        device_ns::DeviceError::StaleSurface => DeviceSysopError::StaleSurface,
        device_ns::DeviceError::Unsupported => DeviceSysopError::Unsupported,
    }
}
