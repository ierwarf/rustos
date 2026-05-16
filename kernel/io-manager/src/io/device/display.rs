use core::convert::TryFrom;
#[cfg(rustos_debug_print_enabled)]
use core::sync::atomic::{AtomicUsize, Ordering};
#[cfg(rustos_debug_print_enabled)]
use x86_64::VirtAddr;

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

#[cfg(rustos_debug_print_enabled)]
const MAX_PRESENT_SURFACE_SAMPLE_LOGS: usize = 8;

#[cfg(rustos_debug_print_enabled)]
static PRESENT_SURFACE_SAMPLE_LOG_COUNT: AtomicUsize = AtomicUsize::new(0);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DisplayError {
    Unavailable,
    BufferTooSmall,
}

pub(crate) fn snapshot_info_local() -> Option<DisplayInfo> {
    let info = gui::display_info()?;
    Some(DisplayInfo::bgra8888(
        info.width,
        info.height,
        info.stride_bytes,
        info.bytes_per_pixel,
        info.generation,
        info.flags,
    ))
}

pub(crate) fn snapshot_info() -> Option<DisplayInfo> {
    snapshot_info_local()
}

pub(crate) fn query_info() -> Result<DisplayInfo, DeviceError> {
    snapshot_info().ok_or(DeviceError::DisplayUnavailable)
}

fn query_info_local() -> Result<DisplayInfo, DeviceError> {
    snapshot_info_local().ok_or(DeviceError::DisplayUnavailable)
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
            let display = snapshot_info().ok_or(DeviceError::DisplayUnavailable)?;
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
            let surface = create_surface_local(
                create.width,
                create.height,
                create.pixel_format,
                create.flags,
                display,
            )
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

fn present_bgra8888_from_surface(surface: DisplaySurfaceHandle) -> Result<(), DisplayError> {
    let (src_ptr, _) = surface_kernel_mapping(surface)?;
    let presented = gui::present_userspace_frame_from_kernel_bgra8888(
        src_ptr,
        surface.width() as usize,
        surface.height() as usize,
        surface.stride_bytes() as usize,
    );
    if !presented {
        return Err(DisplayError::Unavailable);
    }

    Ok(())
}

fn present_bgra8888_rect_from_surface(
    surface: DisplaySurfaceHandle,
    x: usize,
    y: usize,
    rect_width: usize,
    rect_height: usize,
) -> Result<(), DisplayError> {
    let (src_ptr, _) = surface_kernel_mapping(surface)?;
    let presented = gui::present_userspace_frame_rect_from_kernel_bgra8888(
        src_ptr,
        surface.width() as usize,
        surface.height() as usize,
        surface.stride_bytes() as usize,
        x,
        y,
        rect_width,
        rect_height,
    );
    if !presented {
        return Err(DisplayError::Unavailable);
    }

    Ok(())
}

fn surface_kernel_mapping(
    surface: DisplaySurfaceHandle,
) -> Result<(*const u8, usize), DisplayError> {
    // Prefer the cached pointer captured when the shared region was first
    // installed on the surface — `map_shared_region` would otherwise lock the
    // global IPC objects table (with interrupts disabled) on every frame.
    let (src_ptr, mapped_len) = surface
        .shared_region_kernel_mapping()
        .or_else(|| {
            surface
                .shared_region()
                .and_then(crate::ipc::map_shared_region)
        })
        .ok_or(DisplayError::BufferTooSmall)?;
    let frame_len =
        usize::try_from(surface.frame_len()).map_err(|_| DisplayError::BufferTooSmall)?;
    if src_ptr.is_null() || mapped_len < frame_len {
        return Err(DisplayError::BufferTooSmall);
    }

    Ok((src_ptr.cast_const(), mapped_len))
}

pub(crate) fn present_surface(
    address_space: &paging::ProcessAddressSpace,
    surface: DisplaySurfaceHandle,
) -> Result<(), DeviceError> {
    let display = query_info_local()?;
    validate_surface_for_present(surface, display)?;
    let region = surface
        .mapped_region()
        .ok_or(DeviceError::InvalidArgument)?;
    validate_surface_mapping(surface, region)?;
    log_present_surface_sample(address_space, surface, region.start.as_u64());
    present_bgra8888_from_surface(surface).map_err(map_display_error)?;
    Ok(())
}

#[cfg(rustos_debug_print_enabled)]
fn log_present_surface_sample(
    address_space: &paging::ProcessAddressSpace,
    surface: DisplaySurfaceHandle,
    user_ptr: u64,
) {
    let sample_index = PRESENT_SURFACE_SAMPLE_LOG_COUNT.fetch_add(1, Ordering::Relaxed);
    if sample_index >= MAX_PRESENT_SURFACE_SAMPLE_LOGS {
        return;
    }

    let mut bytes = [0_u8; 4];
    match address_space.copy_from_user(VirtAddr::new(user_ptr), &mut bytes) {
        Ok(()) => crate::debug::println!(
            "display present_surface sample #{} user_ptr={:#x} pixel0={:#010x} stride={} width={} height={}",
            sample_index + 1,
            user_ptr,
            u32::from_le_bytes(bytes),
            surface.stride_bytes(),
            surface.width(),
            surface.height(),
        ),
        Err(err) => crate::debug::println!(
            "display present_surface sample #{} user_ptr={:#x} copy_from_user failed: {:?}",
            sample_index + 1,
            user_ptr,
            err,
        ),
    }
}

#[cfg(not(rustos_debug_print_enabled))]
fn log_present_surface_sample(
    _address_space: &paging::ProcessAddressSpace,
    _surface: DisplaySurfaceHandle,
    _user_ptr: u64,
) {
}

pub(crate) fn present_surface_rect(
    _address_space: &paging::ProcessAddressSpace,
    surface: DisplaySurfaceHandle,
    x: usize,
    y: usize,
    width: usize,
    height: usize,
) -> Result<(), DeviceError> {
    let display = query_info_local()?;
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
    present_bgra8888_rect_from_surface(surface, x, y, width, height).map_err(map_display_error)
}

fn map_display_error(err: DisplayError) -> DeviceError {
    match err {
        DisplayError::Unavailable => DeviceError::DisplayUnavailable,
        DisplayError::BufferTooSmall => DeviceError::InvalidArgument,
    }
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

fn create_surface_local(
    width: u32,
    height: u32,
    pixel_format: u32,
    flags: u32,
    display: DisplayInfo,
) -> Option<DisplaySurfaceHandle> {
    if flags != 0
        || width != display.width
        || height != display.height
        || pixel_format != display.pixel_format
    {
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
