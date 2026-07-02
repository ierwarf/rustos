// RING3-MIGRATION-REFERENCE START: hot-path exception: devmgrd/uiserver own
// display ioctl admission, surface policy, and presentation routing. Ring0
// keeps framebuffer mapping, current-process user-copy, display surface handles,
// and hot present substrate.
use core::convert::TryFrom;

use crate::io::gui;
use crate::memory::paging;
use crate::user::abi::device::{
    self, DisplayInfo, DisplayPresentRectRequest, DisplayPresentRequest, DisplaySurfaceCreate,
    PIXEL_FORMAT_BGRA8888,
};
use crate::user::handles::{DisplaySurfaceHandle, KernelHandle};
use crate::user::process_state::UserProcessState;

use super::{DeviceError, read_user_struct, write_user_struct};

const MAX_DISPLAY_SURFACES_PER_PROCESS: usize = 4;

fn display_info() -> Result<DisplayInfo, DeviceError> {
    let info = gui::display_info().ok_or(DeviceError::DisplayUnavailable)?;
    Ok(DisplayInfo::bgra8888(
        info.width,
        info.height,
        info.stride_bytes,
        info.bytes_per_pixel,
        info.generation,
        info.flags,
    ))
}

pub(crate) fn ioctl(
    process_state: &mut UserProcessState,
    request: u64,
    arg: u64,
) -> Result<u64, DeviceError> {
    match request {
        device::DISPLAY_IOCTL_GET_INFO => {
            let info = display_info()?;
            write_user_struct(process_state.address_space(), arg, &info)?;
            Ok(0)
        }
        device::DISPLAY_IOCTL_CREATE_SURFACE => {
            let mut create =
                read_user_struct::<DisplaySurfaceCreate>(process_state.address_space(), arg)?;
            let display = display_info()?;
            if !display.is_primary_provider() {
                return Err(DeviceError::DisplayUnavailable);
            }
            if create.width != display.width
                || create.height != display.height
                || create.pixel_format != PIXEL_FORMAT_BGRA8888
                || create.flags != 0
                || create.reserved != 0
            {
                return Err(DeviceError::InvalidArgument);
            }
            if process_state.handles().display_surface_count() >= MAX_DISPLAY_SURFACES_PER_PROCESS {
                return Err(DeviceError::InvalidArgument);
            }
            let surface = create_surface(create.width, create.height, create.pixel_format, display)
                .ok_or(DeviceError::InvalidArgument)?;
            let handle = process_state
                .handles_mut()
                .install(KernelHandle::DisplaySurface(surface));
            create.handle = u32::try_from(handle).map_err(|_| DeviceError::InvalidArgument)?;
            create.bytes_per_pixel = surface.bytes_per_pixel();
            create.stride_bytes = surface.stride_bytes();
            create.mapping_len = surface.mapping_len();
            create.generation = surface.generation();
            write_user_struct(process_state.address_space(), arg, &create)?;
            Ok(0)
        }
        device::DISPLAY_IOCTL_PRESENT => {
            let request =
                read_user_struct::<DisplayPresentRequest>(process_state.address_space(), arg)?;
            if request.reserved != 0 {
                return Err(DeviceError::InvalidArgument);
            }
            let surface_fd = u64::from(request.surface_handle);
            let surface = match process_state.handles().get(surface_fd) {
                Some(KernelHandle::DisplaySurface(surface)) => *surface,
                Some(_) | None => return Err(DeviceError::InvalidArgument),
            };
            present_surface(process_state.address_space(), surface)?;
            Ok(0)
        }
        device::DISPLAY_IOCTL_PRESENT_RECT => {
            let request =
                read_user_struct::<DisplayPresentRectRequest>(process_state.address_space(), arg)?;
            if request.reserved != 0 {
                return Err(DeviceError::InvalidArgument);
            }
            let surface_fd = u64::from(request.surface_handle);
            let surface = match process_state.handles().get(surface_fd) {
                Some(KernelHandle::DisplaySurface(surface)) => *surface,
                Some(_) | None => return Err(DeviceError::InvalidArgument),
            };
            present_surface_rect(
                surface,
                request.x as usize,
                request.y as usize,
                request.width as usize,
                request.height as usize,
            )?;
            Ok(0)
        }
        _ => Err(DeviceError::Unsupported),
    }
}

fn surface_kernel_ptr(surface: DisplaySurfaceHandle) -> Result<(*const u8, usize), DeviceError> {
    let (src_ptr, mapped_len) = surface
        .shared_region_kernel_mapping()
        .or_else(|| {
            surface
                .shared_region()
                .and_then(crate::ipc::map_shared_region)
        })
        .ok_or(DeviceError::InvalidArgument)?;
    let frame_len =
        usize::try_from(surface.frame_len()).map_err(|_| DeviceError::InvalidArgument)?;
    if src_ptr.is_null() || mapped_len < frame_len {
        return Err(DeviceError::InvalidArgument);
    }
    Ok((src_ptr.cast_const(), mapped_len))
}

pub(crate) fn present_surface(
    address_space: &paging::ProcessAddressSpace,
    surface: DisplaySurfaceHandle,
) -> Result<(), DeviceError> {
    let display = display_info()?;
    validate_surface_for_present(surface, display)?;
    let region = surface
        .mapped_region()
        .ok_or(DeviceError::InvalidArgument)?;
    validate_surface_mapping(surface, region)?;
    let _ = address_space;
    let (ptr, _) = surface_kernel_ptr(surface)?;
    let presented = gui::present_userspace_frame_from_kernel_bgra8888(
        ptr,
        surface.width() as usize,
        surface.height() as usize,
        surface.stride_bytes() as usize,
    );
    if !presented {
        return Err(DeviceError::DisplayUnavailable);
    }
    Ok(())
}

pub(crate) fn present_surface_rect(
    surface: DisplaySurfaceHandle,
    x: usize,
    y: usize,
    width: usize,
    height: usize,
) -> Result<(), DeviceError> {
    let display = display_info()?;
    validate_surface_for_present(surface, display)?;
    if width == 0 || height == 0 {
        return Ok(());
    }
    let x_end = x.checked_add(width).ok_or(DeviceError::InvalidArgument)?;
    let y_end = y.checked_add(height).ok_or(DeviceError::InvalidArgument)?;
    if x >= surface.width() as usize
        || y >= surface.height() as usize
        || x_end > surface.width() as usize
        || y_end > surface.height() as usize
    {
        return Err(DeviceError::InvalidArgument);
    }
    let region = surface
        .mapped_region()
        .ok_or(DeviceError::InvalidArgument)?;
    validate_surface_mapping(surface, region)?;
    let (ptr, _) = surface_kernel_ptr(surface)?;
    let presented = gui::present_userspace_frame_rect_from_kernel_bgra8888(
        ptr,
        surface.width() as usize,
        surface.height() as usize,
        surface.stride_bytes() as usize,
        x,
        y,
        width,
        height,
    );
    if !presented {
        return Err(DeviceError::DisplayUnavailable);
    }
    Ok(())
}

fn align_up_u64(value: u64, align: u64) -> Option<u64> {
    if align == 0 {
        return None;
    }
    let remainder = value % align;
    if remainder == 0 {
        Some(value)
    } else {
        value.checked_add(align - remainder)
    }
}

fn create_surface(
    width: u32,
    height: u32,
    pixel_format: u32,
    display: DisplayInfo,
) -> Option<DisplaySurfaceHandle> {
    if width != display.width || height != display.height || pixel_format != display.pixel_format {
        return None;
    }
    let mut surface = DisplaySurfaceHandle::new(width, height, pixel_format, display.generation)?;
    let region = crate::ipc::create_shared_region(surface.mapping_len() as usize).ok()?;
    surface.set_shared_region(region);
    Some(surface)
}

fn validate_surface_for_present(
    surface: DisplaySurfaceHandle,
    display: DisplayInfo,
) -> Result<(), DeviceError> {
    if surface.generation() != display.generation {
        return Err(DeviceError::StaleSurface);
    }
    if surface.width() != display.width
        || surface.height() != display.height
        || surface.bytes_per_pixel() != display.bytes_per_pixel
        || surface.stride_bytes() != display.stride_bytes
        || surface.pixel_format() != display.pixel_format
    {
        return Err(DeviceError::InvalidArgument);
    }
    let expected_frame_len = u64::from(display.stride_bytes)
        .checked_mul(u64::from(display.height))
        .ok_or(DeviceError::InvalidArgument)?;
    if surface.frame_len() != expected_frame_len {
        return Err(DeviceError::InvalidArgument);
    }
    let expected_mapping_len =
        align_up_u64(expected_frame_len, 4096).ok_or(DeviceError::InvalidArgument)?;
    if surface.mapping_len() != expected_mapping_len {
        return Err(DeviceError::InvalidArgument);
    }
    Ok(())
}

fn validate_surface_mapping(
    surface: DisplaySurfaceHandle,
    region: paging::UserRegion,
) -> Result<(), DeviceError> {
    let mapped_len = (region.page_count as u64)
        .checked_mul(4096)
        .ok_or(DeviceError::InvalidArgument)?;
    if mapped_len < surface.mapping_len() {
        return Err(DeviceError::InvalidArgument);
    }
    region
        .start
        .as_u64()
        .checked_add(surface.mapping_len().saturating_sub(1))
        .ok_or(DeviceError::InvalidArgument)?;
    Ok(())
}
// RING3-MIGRATION-REFERENCE END: devmgrd/uiserver-owned display ioctl substrate exception.
