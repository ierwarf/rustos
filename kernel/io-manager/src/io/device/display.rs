// RING3-MIGRATION-REFERENCE START: hot-path exception: devmgrd/uiserver own
// display ioctl admission, surface policy, and presentation routing. Ring0
// keeps framebuffer mapping, current-process user-copy, display surface handles,
// and hot present substrate.
use alloc::vec;
use alloc::vec::Vec;
use core::convert::TryFrom;

use x86_64::VirtAddr;

use driver_domain_protocol::{
    DVM_GPU_RENDER_MAX_BATCH_BYTES, DVM_GPU_RENDER_MAX_COMMANDS, DVM_GPU_RENDER_MAX_IN_FLIGHT,
};

use crate::io::gui;
use crate::memory::paging;
use crate::user::abi::device::{
    self, DISPLAY_GPU_ABI_VERSION, DISPLAY_GPU_INFO_FLAG_DIRECT_DMABUF,
    DISPLAY_GPU_INFO_FLAG_STAGED_COPY, DISPLAY_GPU_SUBMIT_FLAG_STAGED_COPY,
    DISPLAY_INFO_FLAG_GPU_COMPOSITOR, DISPLAY_SURFACE_FLAG_GPU_ATLAS, DisplayGpuCompletionQuery,
    DisplayGpuDamage, DisplayGpuInfo, DisplayGpuSubmitRequest, DisplayInfo,
    DisplayPresentRectRequest, DisplayPresentRequest, DisplaySurfaceCreate, PIXEL_FORMAT_BGRA8888,
};
use crate::user::handles::{DisplaySurfaceHandle, KernelHandle};
use crate::user::process_state::UserProcessState;

use super::{DeviceError, read_user_struct, write_user_struct};

const MAX_DISPLAY_SURFACES_PER_PROCESS: usize = 4;

pub(crate) fn prepare_ioctl(request: u64, gpu_atlas_create_slot: Option<u32>) {
    if matches!(
        request,
        device::DISPLAY_IOCTL_GET_INFO
            | device::DISPLAY_IOCTL_CREATE_SURFACE
            | device::DISPLAY_IOCTL_PRESENT
            | device::DISPLAY_IOCTL_PRESENT_RECT
            | device::DISPLAY_IOCTL_GPU_GET_INFO
            | device::DISPLAY_IOCTL_GPU_SUBMIT
            | device::DISPLAY_IOCTL_GPU_QUERY_COMPLETION
    ) {
        // Transport discovery and MMIO publication can take the sleepable
        // MMIO registry. Run it before compat pins the process handle table;
        // the ioctl itself revalidates the fd and all generation-stamped
        // objects after this phase.
        let _ = gui::display_info();
        let _ = crate::io::dvm_display::gpu_atlas_info();
        if request == device::DISPLAY_IOCTL_CREATE_SURFACE
            && let Some(slot) = gpu_atlas_create_slot
        {
            // The argument and exact next-free slot were captured under the
            // process owner before this sleepable preflight. Map only that
            // slot; mapping all three 16 MiB apertures on the first CREATE
            // serialized UI bootstrap for many seconds under KVM.
            let _ = crate::io::dvm_display::gpu_atlas_slot_mapping(slot);
        }
    }
}

pub(crate) fn gpu_atlas_create_slot_from_user(
    process_state: &UserProcessState,
    request: u64,
    arg: u64,
) -> Option<u32> {
    if request != device::DISPLAY_IOCTL_CREATE_SURFACE {
        return None;
    }
    let create =
        read_user_struct::<DisplaySurfaceCreate>(process_state.address_space(), arg).ok()?;
    if create.flags != DISPLAY_SURFACE_FLAG_GPU_ATLAS {
        return None;
    }
    let gpu = crate::io::dvm_display::gpu_atlas_info_snapshot()?;
    (0..gpu.slot_count).find(|slot| !process_state.handles().gpu_atlas_slot_in_use(*slot))
}

fn display_info() -> Result<DisplayInfo, DeviceError> {
    let info = gui::display_info_snapshot().ok_or(DeviceError::DisplayUnavailable)?;
    let flags = if crate::io::dvm_display::gpu_atlas_info_snapshot().is_some() {
        info.flags | DISPLAY_INFO_FLAG_GPU_COMPOSITOR
    } else {
        info.flags
    };
    Ok(DisplayInfo::bgra8888(
        info.width,
        info.height,
        info.stride_bytes,
        info.bytes_per_pixel,
        info.generation,
        flags,
    ))
}

pub(crate) fn ioctl(
    process_id: u64,
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
            if create.pixel_format != PIXEL_FORMAT_BGRA8888 || create.reserved != 0 {
                return Err(DeviceError::InvalidArgument);
            }
            if process_state.handles().display_surface_count() >= MAX_DISPLAY_SURFACES_PER_PROCESS {
                return Err(DeviceError::InvalidArgument);
            }
            let surface = if create.flags == 0 {
                if create.width != display.width || create.height != display.height {
                    return Err(DeviceError::InvalidArgument);
                }
                create_surface(
                    process_id,
                    create.width,
                    create.height,
                    create.pixel_format,
                    display,
                )
                .ok_or(DeviceError::InvalidArgument)?
            } else if create.flags == DISPLAY_SURFACE_FLAG_GPU_ATLAS {
                let gpu = crate::io::dvm_display::gpu_atlas_info_snapshot()
                    .ok_or(DeviceError::DisplayUnavailable)?;
                if create.width != gpu.width || create.height != gpu.height {
                    return Err(DeviceError::InvalidArgument);
                }
                let slot = (0..gpu.slot_count)
                    .find(|slot| !process_state.handles().gpu_atlas_slot_in_use(*slot))
                    .ok_or(DeviceError::TryAgain)?;
                create_gpu_atlas_surface(
                    create.width,
                    create.height,
                    create.pixel_format,
                    display,
                    slot,
                )
                .ok_or(DeviceError::InvalidArgument)?
            } else {
                return Err(DeviceError::InvalidArgument);
            };
            let Some(handle) = process_state
                .handles_mut()
                .install(KernelHandle::DisplaySurface(surface))
            else {
                if let Some(region) = surface.shared_region() {
                    crate::ipc::release_shared_region_descriptor(region);
                }
                return Err(DeviceError::TryAgain);
            };
            create.handle = u32::try_from(handle).map_err(|_| DeviceError::InvalidArgument)?;
            create.bytes_per_pixel = surface.bytes_per_pixel();
            create.stride_bytes = surface.stride_bytes();
            create.reserved = surface.binding_slot().unwrap_or(0);
            create.mapping_len = surface.mapping_len();
            create.generation = surface.generation();
            if let Err(error) = write_user_struct(process_state.address_space(), arg, &create) {
                let _ = process_state.handles_mut().close(handle);
                return Err(error);
            }
            Ok(0)
        }
        device::DISPLAY_IOCTL_GPU_GET_INFO => {
            let display = display_info()?;
            let gpu = crate::io::dvm_display::gpu_atlas_info_snapshot()
                .ok_or(DeviceError::DisplayUnavailable)?;
            let info = DisplayGpuInfo {
                version: DISPLAY_GPU_ABI_VERSION,
                flags: match gpu.submit_flags {
                    driver_domain_protocol::DVM_GPU_ATLAS_SUBMIT_FLAG_STAGED_COPY => {
                        DISPLAY_GPU_INFO_FLAG_STAGED_COPY
                    }
                    driver_domain_protocol::DVM_GPU_ATLAS_SUBMIT_FLAG_DIRECT_DMABUF => {
                        DISPLAY_GPU_INFO_FLAG_DIRECT_DMABUF
                    }
                    _ => return Err(DeviceError::DisplayUnavailable),
                },
                atlas_width: gpu.width,
                atlas_height: gpu.height,
                atlas_stride_bytes: gpu.stride_bytes,
                slot_count: gpu.slot_count,
                max_commands: DVM_GPU_RENDER_MAX_COMMANDS,
                max_batch_bytes: DVM_GPU_RENDER_MAX_BATCH_BYTES as u32,
                generation: display.generation,
                context_id: gpu.context_id,
                context_epoch: gpu.context_epoch,
                prime_fence_value: gpu.prime_fence_value,
                prime_duration_ns: gpu.prime_duration_ns,
            };
            if info.slot_count != DVM_GPU_RENDER_MAX_IN_FLIGHT {
                return Err(DeviceError::DisplayUnavailable);
            }
            write_user_struct(process_state.address_space(), arg, &info)?;
            Ok(0)
        }
        device::DISPLAY_IOCTL_GPU_SUBMIT => {
            let request =
                read_user_struct::<DisplayGpuSubmitRequest>(process_state.address_space(), arg)?;
            if request.flags != DISPLAY_GPU_SUBMIT_FLAG_STAGED_COPY
                || request.reserved != 0
                || request.batch_ptr == 0
                || request.batch_len == 0
                || request.batch_len as usize > DVM_GPU_RENDER_MAX_BATCH_BYTES
                || request.damage_count > driver_domain_protocol::DVM_GPU_ATLAS_MAX_DAMAGE_RECTS
                || (request.damage_count != 0) != (request.damage_ptr != 0)
            {
                return Err(DeviceError::InvalidArgument);
            }
            let surface = gpu_atlas_surface(process_state, request.surface_handle)?;
            let region = surface
                .mapped_region()
                .ok_or(DeviceError::InvalidArgument)?;
            validate_surface_mapping(surface, region)?;
            let batch_len = request.batch_len as usize;
            let mut batch = vec![0_u8; batch_len];
            process_state
                .address_space()
                .validate_user_read_buffer(VirtAddr::new(request.batch_ptr), batch_len)?;
            process_state
                .address_space()
                .copy_from_user(VirtAddr::new(request.batch_ptr), &mut batch)?;
            let mut damage = Vec::with_capacity(request.damage_count as usize);
            for index in 0..request.damage_count {
                let offset = u64::from(index)
                    .checked_mul(core::mem::size_of::<DisplayGpuDamage>() as u64)
                    .and_then(|offset| request.damage_ptr.checked_add(offset))
                    .ok_or(DeviceError::InvalidArgument)?;
                let rect =
                    read_user_struct::<DisplayGpuDamage>(process_state.address_space(), offset)?;
                damage.push(driver_domain_protocol::DvmGpuAtlasDamage {
                    x: rect.x,
                    y: rect.y,
                    width: rect.width,
                    height: rect.height,
                });
            }
            let slot = surface.binding_slot().ok_or(DeviceError::InvalidArgument)?;
            match crate::io::dvm_display::try_submit_gpu_atlas(
                u64::from(request.surface_handle),
                slot,
                surface.width(),
                surface.height(),
                surface.stride_bytes(),
                &damage,
                &batch,
            ) {
                crate::io::dvm_display::DvmGpuSubmitOutcome::Submitted => Ok(0),
                crate::io::dvm_display::DvmGpuSubmitOutcome::Backpressured => {
                    Err(DeviceError::TryAgain)
                }
                crate::io::dvm_display::DvmGpuSubmitOutcome::Unavailable => {
                    Err(DeviceError::DisplayUnavailable)
                }
                crate::io::dvm_display::DvmGpuSubmitOutcome::Invalid => {
                    Err(DeviceError::InvalidArgument)
                }
            }
        }
        device::DISPLAY_IOCTL_GPU_QUERY_COMPLETION => {
            let mut query =
                read_user_struct::<DisplayGpuCompletionQuery>(process_state.address_space(), arg)?;
            if query.reserved != 0 || query.completion.iter().any(|byte| *byte != 0) {
                return Err(DeviceError::InvalidArgument);
            }
            let surface = gpu_atlas_surface(process_state, query.surface_handle)?;
            let slot = surface.binding_slot().ok_or(DeviceError::InvalidArgument)?;
            match crate::io::dvm_display::query_gpu_atlas_completion(slot, &mut query.completion) {
                crate::io::dvm_display::DvmGpuCompletionOutcome::Completed => {
                    write_user_struct(process_state.address_space(), arg, &query)?;
                    Ok(0)
                }
                crate::io::dvm_display::DvmGpuCompletionOutcome::Pending => {
                    Err(DeviceError::TryAgain)
                }
                crate::io::dvm_display::DvmGpuCompletionOutcome::Unavailable => {
                    Err(DeviceError::DisplayUnavailable)
                }
                crate::io::dvm_display::DvmGpuCompletionOutcome::Invalid => {
                    Err(DeviceError::InvalidArgument)
                }
            }
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

fn gpu_atlas_surface(
    process_state: &UserProcessState,
    surface_handle: u32,
) -> Result<DisplaySurfaceHandle, DeviceError> {
    let surface = match process_state.handles().get(u64::from(surface_handle)) {
        Some(KernelHandle::DisplaySurface(surface)) => *surface,
        Some(_) | None => return Err(DeviceError::InvalidArgument),
    };
    let display = display_info()?;
    let gpu =
        crate::io::dvm_display::gpu_atlas_info_snapshot().ok_or(DeviceError::DisplayUnavailable)?;
    if !surface.is_gpu_atlas()
        || surface.generation() != display.generation
        || surface.width() != gpu.width
        || surface.height() != gpu.height
        || surface.stride_bytes() != gpu.stride_bytes
        || surface.pixel_format() != PIXEL_FORMAT_BGRA8888
        || surface
            .binding_slot()
            .is_none_or(|slot| slot >= gpu.slot_count)
    {
        return Err(DeviceError::InvalidArgument);
    }
    Ok(surface)
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
    match gui::present_userspace_frame_from_kernel_bgra8888(
        ptr,
        surface.width() as usize,
        surface.height() as usize,
        surface.stride_bytes() as usize,
    ) {
        gui::GuiPresentOutcome::Presented => Ok(()),
        gui::GuiPresentOutcome::Backpressured => Err(DeviceError::TryAgain),
        gui::GuiPresentOutcome::Unavailable => Err(DeviceError::DisplayUnavailable),
    }
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
    match gui::present_userspace_frame_rect_from_kernel_bgra8888(
        gui::KernelBgraFrame {
            src_ptr: ptr,
            width: surface.width() as usize,
            height: surface.height() as usize,
            stride_bytes: surface.stride_bytes() as usize,
        },
        gui::GuiDamageRect {
            x,
            y,
            width,
            height,
        },
    ) {
        gui::GuiPresentOutcome::Presented => Ok(()),
        gui::GuiPresentOutcome::Backpressured => Err(DeviceError::TryAgain),
        gui::GuiPresentOutcome::Unavailable => Err(DeviceError::DisplayUnavailable),
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

fn create_surface(
    process_id: u64,
    width: u32,
    height: u32,
    pixel_format: u32,
    display: DisplayInfo,
) -> Option<DisplaySurfaceHandle> {
    if width != display.width || height != display.height || pixel_format != display.pixel_format {
        return None;
    }
    let mut surface = DisplaySurfaceHandle::new_with_stride(
        width,
        height,
        display.stride_bytes,
        pixel_format,
        display.generation,
    )?;
    let region =
        crate::ipc::create_shared_region_for_process(process_id, surface.mapping_len() as usize)
            .ok()?;
    surface.set_shared_region(region);
    Some(surface)
}

fn create_gpu_atlas_surface(
    width: u32,
    height: u32,
    pixel_format: u32,
    display: DisplayInfo,
    binding_slot: u32,
) -> Option<DisplaySurfaceHandle> {
    let gpu = crate::io::dvm_display::gpu_atlas_info_snapshot()?;
    if width != gpu.width
        || height != gpu.height
        || pixel_format != display.pixel_format
        || binding_slot >= gpu.slot_count
    {
        return None;
    }
    let mut surface = DisplaySurfaceHandle::new_gpu_atlas(
        width,
        height,
        pixel_format,
        display.generation,
        binding_slot,
    )?;
    if surface.stride_bytes() != gpu.stride_bytes {
        return None;
    }
    let (phys_start, kernel_mapping, mapping_len) =
        crate::io::dvm_display::gpu_atlas_slot_mapping(binding_slot)?;
    if mapping_len != surface.mapping_len() as usize
        || !surface.set_external_physical_mapping(phys_start, kernel_mapping as u64, mapping_len)
    {
        return None;
    }
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

#[cfg(test)]
mod tests {
    use super::MAX_DISPLAY_SURFACES_PER_PROCESS;

    #[test]
    fn display_surface_capacity_covers_primary_and_exact_gpu_slots() {
        assert_eq!(MAX_DISPLAY_SURFACES_PER_PROCESS, 4);
    }
}
// RING3-MIGRATION-REFERENCE END: devmgrd/uiserver-owned display ioctl substrate exception.
