use core::convert::TryFrom;

use crate::io::gui;
use crate::paging;
use crate::user::abi::device::{
    self, DisplayInfo, DisplayPresentRectRequest, DisplayPresentRequest, DisplaySurfaceCreate,
    PIXEL_FORMAT_BGRA8888,
};
use crate::user::handles::{DisplaySurfaceHandle, KernelHandle};
use crate::user::process_state::UserProcessState;

use super::{read_user_struct, write_user_struct, DeviceError};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DisplayError {
    Unavailable,
    InvalidDimensions,
    InvalidStride,
    BufferTooSmall,
}

pub(crate) fn snapshot_info() -> Option<DisplayInfo> {
    let info = gui::display_info()?;
    Some(DisplayInfo {
        width: info.width,
        height: info.height,
        stride_bytes: info.stride_bytes,
        bytes_per_pixel: info.bytes_per_pixel,
        pixel_format: PIXEL_FORMAT_BGRA8888,
        reserved: 0,
    })
}

pub(crate) fn query_info() -> Result<DisplayInfo, DeviceError> {
    snapshot_info().ok_or(DeviceError::DisplayUnavailable)
}

pub(crate) fn ioctl(
    process_state: &mut UserProcessState,
    request: u64,
    arg: u64,
) -> Result<u64, DeviceError> {
    match request {
        device::DISPLAY_IOCTL_GET_INFO => {
            let info = query_info()?;
            write_user_struct(process_state.address_space(), arg, &info)?;
            Ok(0)
        }
        device::DISPLAY_IOCTL_CREATE_SURFACE => {
            let mut create =
                read_user_struct::<DisplaySurfaceCreate>(process_state.address_space(), arg)?;
            let surface =
                DisplaySurfaceHandle::new(create.width, create.height, create.pixel_format)
                    .ok_or(DeviceError::InvalidArgument)?;
            let handle = process_state
                .handles_mut()
                .install(KernelHandle::DisplaySurface(surface));
            create.handle = u32::try_from(handle).map_err(|_| DeviceError::InvalidArgument)?;
            create.bytes_per_pixel = surface.bytes_per_pixel();
            create.stride_bytes = surface.stride_bytes();
            create.mapping_len = surface.mapping_len();
            write_user_struct(process_state.address_space(), arg, &create)?;
            Ok(0)
        }
        device::DISPLAY_IOCTL_PRESENT => {
            let request =
                read_user_struct::<DisplayPresentRequest>(process_state.address_space(), arg)?;
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
            let surface_fd = u64::from(request.surface_handle);
            let surface = match process_state.handles().get(surface_fd) {
                Some(KernelHandle::DisplaySurface(surface)) => *surface,
                Some(_) | None => return Err(DeviceError::InvalidArgument),
            };
            present_surface_rect(
                process_state.address_space(),
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

pub(crate) fn present_bgra8888(
    width: usize,
    height: usize,
    stride_bytes: usize,
    bytes: &[u8],
) -> Result<(), DisplayError> {
    let display = snapshot_info().ok_or(DisplayError::Unavailable)?;
    if width != display.width as usize || height != display.height as usize {
        return Err(DisplayError::InvalidDimensions);
    }

    let min_stride = width
        .checked_mul(display.bytes_per_pixel as usize)
        .ok_or(DisplayError::InvalidStride)?;
    if stride_bytes < min_stride {
        return Err(DisplayError::InvalidStride);
    }

    let required_len = stride_bytes
        .checked_mul(height)
        .ok_or(DisplayError::BufferTooSmall)?;
    if bytes.len() < required_len {
        return Err(DisplayError::BufferTooSmall);
    }

    if !gui::present_userspace_frame_bgra8888(width, height, stride_bytes, bytes) {
        return Err(DisplayError::Unavailable);
    }

    Ok(())
}

pub(crate) fn present_bgra8888_from_user(
    address_space: &paging::ProcessAddressSpace,
    user_ptr: u64,
    width: usize,
    height: usize,
    stride_bytes: usize,
) -> Result<(), DisplayError> {
    let display = snapshot_info().ok_or(DisplayError::Unavailable)?;
    if width != display.width as usize || height != display.height as usize {
        return Err(DisplayError::InvalidDimensions);
    }

    let min_stride = width
        .checked_mul(display.bytes_per_pixel as usize)
        .ok_or(DisplayError::InvalidStride)?;
    if stride_bytes < min_stride {
        return Err(DisplayError::InvalidStride);
    }

    let presented = gui::present_userspace_frame_from_user_bgra8888(
        address_space,
        user_ptr,
        width,
        height,
        stride_bytes,
    )
    .map_err(|_| DisplayError::BufferTooSmall)?;
    if !presented {
        return Err(DisplayError::Unavailable);
    }

    Ok(())
}

pub(crate) fn present_bgra8888_rect_from_user(
    address_space: &paging::ProcessAddressSpace,
    user_ptr: u64,
    width: usize,
    height: usize,
    stride_bytes: usize,
    x: usize,
    y: usize,
    rect_width: usize,
    rect_height: usize,
) -> Result<(), DisplayError> {
    let display = snapshot_info().ok_or(DisplayError::Unavailable)?;
    if width != display.width as usize || height != display.height as usize {
        return Err(DisplayError::InvalidDimensions);
    }

    let min_stride = width
        .checked_mul(display.bytes_per_pixel as usize)
        .ok_or(DisplayError::InvalidStride)?;
    if stride_bytes < min_stride {
        return Err(DisplayError::InvalidStride);
    }

    let presented = gui::present_userspace_frame_rect_from_user_bgra8888(
        address_space,
        user_ptr,
        width,
        height,
        stride_bytes,
        x,
        y,
        rect_width,
        rect_height,
    )
    .map_err(|_| DisplayError::BufferTooSmall)?;
    if !presented {
        return Err(DisplayError::Unavailable);
    }

    Ok(())
}

pub(crate) fn present_surface(
    address_space: &paging::ProcessAddressSpace,
    surface: DisplaySurfaceHandle,
) -> Result<(), DeviceError> {
    let region = surface
        .mapped_region()
        .ok_or(DeviceError::InvalidArgument)?;
    present_bgra8888_from_user(
        address_space,
        region.start.as_u64(),
        surface.width() as usize,
        surface.height() as usize,
        surface.stride_bytes() as usize,
    )
    .map_err(map_display_error)
}

pub(crate) fn present_surface_rect(
    address_space: &paging::ProcessAddressSpace,
    surface: DisplaySurfaceHandle,
    x: usize,
    y: usize,
    width: usize,
    height: usize,
) -> Result<(), DeviceError> {
    let region = surface
        .mapped_region()
        .ok_or(DeviceError::InvalidArgument)?;
    present_bgra8888_rect_from_user(
        address_space,
        region.start.as_u64(),
        surface.width() as usize,
        surface.height() as usize,
        surface.stride_bytes() as usize,
        x,
        y,
        width,
        height,
    )
    .map_err(map_display_error)
}

fn map_display_error(err: DisplayError) -> DeviceError {
    match err {
        DisplayError::Unavailable => DeviceError::DisplayUnavailable,
        DisplayError::InvalidDimensions
        | DisplayError::InvalidStride
        | DisplayError::BufferTooSmall => DeviceError::InvalidArgument,
    }
}
