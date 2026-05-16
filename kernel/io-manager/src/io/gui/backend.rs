use alloc::vec::Vec;
#[cfg(rustos_debug_print_enabled)]
use core::sync::atomic::AtomicUsize;
use core::sync::atomic::{AtomicU32, Ordering};

use boot_protocol::FramebufferInfo;
use x86_64::VirtAddr;
use x86_64::instructions::interrupts;

use super::GuiDisplayInfo;
use super::framebuffer::{Framebuffer, FramebufferRect, build_framebuffer};
use crate::memory::paging::{self, ProcessAddressSpace};
use crate::sync::KernelWaitLock;

const ENABLE_FRAMEBUFFER_WRITE_COMBINE: bool = false;
#[cfg(rustos_debug_print_enabled)]
const MAX_BACKEND_PRESENT_SAMPLE_LOGS: usize = 8;
const USER_PRESENT_STRIPE_BYTES: usize = 1024 * 1024;

enum BackendInstance {
    Unavailable,
    Framebuffer(FramebufferDisplayBackend),
}

struct FramebufferDisplayBackend {
    framebuffer: Framebuffer,
    flags: u32,
}

pub(crate) struct DisplayBackend {
    instance: BackendInstance,
    generation: u64,
}

static DISPLAY_BACKEND: KernelWaitLock<DisplayBackend> =
    KernelWaitLock::new(DisplayBackend::empty());
#[cfg(rustos_debug_print_enabled)]
static BACKEND_PRESENT_SAMPLE_LOG_COUNT: AtomicUsize = AtomicUsize::new(0);
static DISPLAY_WIDTH: AtomicU32 = AtomicU32::new(0);
static DISPLAY_HEIGHT: AtomicU32 = AtomicU32::new(0);

impl DisplayBackend {
    const fn empty() -> Self {
        Self {
            instance: BackendInstance::Unavailable,
            generation: 0,
        }
    }

    fn install_framebuffer(&mut self, info: FramebufferInfo, flags: u32) -> bool {
        if !framebuffer_info_is_valid(info) {
            return false;
        }
        if ENABLE_FRAMEBUFFER_WRITE_COMBINE {
            mark_framebuffer_write_combine(info);
        }
        self.generation = next_display_generation(self.generation);
        self.instance = BackendInstance::Framebuffer(FramebufferDisplayBackend {
            framebuffer: build_framebuffer(info),
            flags,
        });
        DISPLAY_WIDTH.store(info.width, Ordering::Release);
        DISPLAY_HEIGHT.store(info.height, Ordering::Release);
        true
    }

    fn with_framebuffer<R>(&mut self, f: impl FnOnce(&mut Framebuffer) -> R) -> Option<R> {
        match &mut self.instance {
            BackendInstance::Unavailable => None,
            BackendInstance::Framebuffer(backend) => Some(f(&mut backend.framebuffer)),
        }
    }

    fn display_info(&self) -> Option<GuiDisplayInfo> {
        match &self.instance {
            BackendInstance::Unavailable => None,
            BackendInstance::Framebuffer(backend) => Some(GuiDisplayInfo {
                width: backend.framebuffer.width() as u32,
                height: backend.framebuffer.height() as u32,
                stride_bytes: backend.framebuffer.stride_bytes() as u32,
                bytes_per_pixel: backend.framebuffer.bytes_per_pixel() as u32,
                flags: backend.flags,
                generation: self.generation,
            }),
        }
    }
}

fn next_display_generation(current: u64) -> u64 {
    let next = current.wrapping_add(1);
    if next == 0 { 1 } else { next }
}

pub(crate) fn install_boot_framebuffer(info: FramebufferInfo, flags: u32) -> bool {
    DISPLAY_BACKEND.lock().install_framebuffer(info, flags)
}

pub(crate) fn install_driver_framebuffer(info: FramebufferInfo, flags: u32) -> bool {
    DISPLAY_BACKEND.lock().install_framebuffer(info, flags)
}

pub(crate) fn try_with_framebuffer<R>(f: impl FnOnce(&mut Framebuffer) -> R) -> Option<R> {
    let mut backend = DISPLAY_BACKEND.try_lock()?;
    backend.with_framebuffer(f)
}

pub(crate) fn display_info() -> Option<GuiDisplayInfo> {
    DISPLAY_BACKEND.lock().display_info()
}

pub(crate) fn display_dimensions() -> Option<(u32, u32)> {
    let width = DISPLAY_WIDTH.load(Ordering::Acquire);
    let height = DISPLAY_HEIGHT.load(Ordering::Acquire);
    (width != 0 && height != 0).then_some((width, height))
}

pub(crate) fn present_bgra8888_from_user(
    address_space: &ProcessAddressSpace,
    user_ptr: u64,
    width: usize,
    height: usize,
    stride_bytes: usize,
) -> Result<bool, paging::AddressSpaceError> {
    if !present_context_allows_blocking() {
        return Ok(false);
    }
    if display_present_faulted() {
        return Ok(false);
    }
    let presented = copy_user_bgra8888_rect_in_stripes(
        address_space,
        user_ptr,
        width,
        height,
        stride_bytes,
        FramebufferRect {
            x: 0,
            y: 0,
            width,
            height,
        },
    )?;
    if presented {
        crate::driver::virtio_gpu::queue_primary_flush();
    }
    Ok(presented)
}

#[cfg(rustos_debug_print_enabled)]
fn log_backend_present_sample(framebuffer: &Framebuffer, drawn: bool) {
    let sample_index = BACKEND_PRESENT_SAMPLE_LOG_COUNT.fetch_add(1, Ordering::Relaxed);
    if sample_index >= MAX_BACKEND_PRESENT_SAMPLE_LOGS {
        return;
    }

    let (active, front) = framebuffer.debug_sample_buffers();
    crate::debug::println!(
        "display backend sample #{} drawn={} active={:02x}{:02x}{:02x}{:02x} front={:02x}{:02x}{:02x}{:02x} double_buffer={}",
        sample_index + 1,
        drawn,
        active[0],
        active[1],
        active[2],
        active[3],
        front[0],
        front[1],
        front[2],
        front[3],
        framebuffer.debug_uses_double_buffer(),
    );
}

#[cfg(not(rustos_debug_print_enabled))]
fn log_backend_present_sample(_framebuffer: &Framebuffer, _drawn: bool) {}

pub(crate) fn present_bgra8888_rect_from_user(
    address_space: &ProcessAddressSpace,
    user_ptr: u64,
    width: usize,
    height: usize,
    stride_bytes: usize,
    rect: FramebufferRect,
) -> Result<bool, paging::AddressSpaceError> {
    if !present_context_allows_blocking() {
        return Ok(false);
    }
    if display_present_faulted() {
        return Ok(false);
    }
    let presented = copy_user_bgra8888_rect_in_stripes(
        address_space,
        user_ptr,
        width,
        height,
        stride_bytes,
        rect,
    )?;
    if presented {
        crate::driver::virtio_gpu::queue_primary_flush_rect(
            rect.x as u32,
            rect.y as u32,
            rect.width as u32,
            rect.height as u32,
        );
    }
    Ok(presented)
}

fn copy_user_bgra8888_rect_in_stripes(
    address_space: &ProcessAddressSpace,
    user_ptr: u64,
    width: usize,
    height: usize,
    stride_bytes: usize,
    rect: FramebufferRect,
) -> Result<bool, paging::AddressSpaceError> {
    if rect.width == 0 || rect.height == 0 {
        return Ok(true);
    }
    let Some(info) = display_info() else {
        return Ok(false);
    };
    if width != info.width as usize || height != info.height as usize {
        return Ok(false);
    }
    let Some(min_stride) = width.checked_mul(4) else {
        return Ok(false);
    };
    if stride_bytes < min_stride {
        return Ok(false);
    }
    let Some(rect) = rect.intersection(FramebufferRect {
        x: 0,
        y: 0,
        width: info.width as usize,
        height: info.height as usize,
    }) else {
        return Ok(true);
    };

    let stripe_rows = user_present_stripe_rows(rect.width);
    let end_y = rect
        .y
        .checked_add(rect.height)
        .ok_or(paging::AddressSpaceError::AddressOverflow)?;
    let mut y = rect.y;
    let mut scratch = Vec::new();
    let mut copied = false;

    while y < end_y {
        let stripe_height = stripe_rows.min(end_y - y);
        let stripe = FramebufferRect {
            x: rect.x,
            y,
            width: rect.width,
            height: stripe_height,
        };
        let row_bytes = stripe
            .width
            .checked_mul(4)
            .ok_or(paging::AddressSpaceError::AddressOverflow)?;
        let scratch_len = copy_user_bgra8888_rect_to_scratch(
            address_space,
            user_ptr,
            stride_bytes,
            stripe,
            &mut scratch,
        )?;
        let Some(result) = DISPLAY_BACKEND.lock().with_framebuffer(|framebuffer| {
            let drawn = framebuffer.draw_bgra8888_rect_from_kernel(
                scratch[..scratch_len].as_ptr(),
                row_bytes,
                stripe,
            );
            log_backend_present_sample(framebuffer, drawn);
            if drawn {
                return Ok(framebuffer.present_scene());
            }
            Ok(drawn)
        }) else {
            return Ok(false);
        };

        if !result? {
            return Ok(false);
        }
        copied = true;
        y += stripe_height;
    }

    Ok(copied)
}

fn copy_user_bgra8888_rect_to_scratch(
    address_space: &ProcessAddressSpace,
    user_ptr: u64,
    stride_bytes: usize,
    rect: FramebufferRect,
    scratch: &mut Vec<u8>,
) -> Result<usize, paging::AddressSpaceError> {
    let row_bytes = rect
        .width
        .checked_mul(4)
        .ok_or(paging::AddressSpaceError::AddressOverflow)?;
    let total_bytes = row_bytes
        .checked_mul(rect.height)
        .ok_or(paging::AddressSpaceError::AddressOverflow)?;
    if scratch.capacity() < total_bytes {
        scratch
            .try_reserve_exact(total_bytes - scratch.capacity())
            .map_err(|_| paging::AddressSpaceError::OutOfFrames)?;
    }
    scratch.resize(total_bytes, 0);
    for row in 0..rect.height {
        let source_row = rect
            .y
            .checked_add(row)
            .ok_or(paging::AddressSpaceError::AddressOverflow)?;
        let row_offset = source_row
            .checked_mul(stride_bytes)
            .ok_or(paging::AddressSpaceError::AddressOverflow)?;
        let source_offset = row_offset
            .checked_add(
                rect.x
                    .checked_mul(4)
                    .ok_or(paging::AddressSpaceError::AddressOverflow)?,
            )
            .ok_or(paging::AddressSpaceError::AddressOverflow)?;
        let source = user_ptr
            .checked_add(source_offset as u64)
            .ok_or(paging::AddressSpaceError::AddressOverflow)?;
        let dest_offset = row
            .checked_mul(row_bytes)
            .ok_or(paging::AddressSpaceError::AddressOverflow)?;
        address_space.copy_from_user(
            VirtAddr::new(source),
            &mut scratch[dest_offset..dest_offset + row_bytes],
        )?;
    }
    Ok(total_bytes)
}

fn user_present_stripe_rows(width: usize) -> usize {
    let row_bytes = width.saturating_mul(4).max(1);
    (USER_PRESENT_STRIPE_BYTES / row_bytes).max(1)
}

pub(crate) fn present_bgra8888_from_kernel(
    src_ptr: *const u8,
    width: usize,
    height: usize,
    stride_bytes: usize,
) -> bool {
    if !present_context_allows_blocking() {
        return false;
    }
    if display_present_faulted() {
        return false;
    }
    let presented = DISPLAY_BACKEND
        .lock()
        .with_framebuffer(|framebuffer| {
            let drawn =
                framebuffer.draw_bgra8888_frame_from_kernel(src_ptr, width, height, stride_bytes);
            if drawn {
                return framebuffer.present_scene();
            }
            false
        })
        .unwrap_or(false);
    if presented {
        crate::driver::virtio_gpu::queue_primary_flush();
    }
    presented
}

pub(crate) fn present_bgra8888_rect_from_kernel(
    src_ptr: *const u8,
    width: usize,
    height: usize,
    stride_bytes: usize,
    rect: FramebufferRect,
) -> bool {
    if !present_context_allows_blocking() {
        return false;
    }
    if display_present_faulted() {
        return false;
    }
    let presented = DISPLAY_BACKEND
        .lock()
        .with_framebuffer(|framebuffer| {
            let drawn = framebuffer.draw_bgra8888_frame_rect_from_kernel(
                src_ptr,
                width,
                height,
                stride_bytes,
                rect,
            );
            if drawn {
                return framebuffer.present_scene();
            }
            false
        })
        .unwrap_or(false);
    if presented {
        crate::driver::virtio_gpu::queue_primary_flush_rect(
            rect.x as u32,
            rect.y as u32,
            rect.width as u32,
            rect.height as u32,
        );
    }
    presented
}

fn present_context_allows_blocking() -> bool {
    interrupts::are_enabled()
}

fn display_present_faulted() -> bool {
    if nucleus_core::util::fault_injection::should_fail("display.present") {
        crate::debug::warn!(display, "fault injection: display.present dropped present");
        return true;
    }
    false
}

fn mark_framebuffer_write_combine(info: FramebufferInfo) {
    let _ = crate::memory::paging::update_direct_map_range_flags(
        info.addr,
        info.size as usize,
        paging::WRITE_COMBINE_BIT,
        x86_64::structures::paging::PageTableFlags::empty(),
    );
}

fn framebuffer_info_is_valid(info: FramebufferInfo) -> bool {
    let width = info.width as usize;
    let height = info.height as usize;
    let stride = info.stride as usize;
    let bpp = info.bytes_per_pixel as usize;
    let size = info.size as usize;

    if info.addr == 0 || size == 0 {
        return false;
    }
    if info.addr.checked_add(info.size).is_none() {
        return false;
    }
    if width == 0 || height == 0 || stride == 0 {
        return false;
    }
    if width > super::framebuffer::MAX_FRAMEBUFFER_WIDTH
        || height > super::framebuffer::MAX_FRAMEBUFFER_HEIGHT
    {
        return false;
    }
    if stride < width || !(3..=4).contains(&bpp) {
        return false;
    }
    if !matches!(
        info.pixel_format,
        boot_protocol::BootPixelFormat::Rgb | boot_protocol::BootPixelFormat::Bgr
    ) {
        return false;
    }
    if info.back_buffer_addr == 0 && info.back_buffer_size != 0 {
        return false;
    }
    if info.back_buffer_addr != 0
        && (info.back_buffer_size < info.size
            || info
                .back_buffer_addr
                .checked_add(info.back_buffer_size)
                .is_none())
    {
        return false;
    }

    let Some(stride_bytes) = stride.checked_mul(bpp) else {
        return false;
    };
    let Some(min_size) = stride_bytes.checked_mul(height) else {
        return false;
    };

    min_size <= size
}
