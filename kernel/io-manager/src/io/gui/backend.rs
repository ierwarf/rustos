use core::sync::atomic::{AtomicUsize, Ordering};

use boot_protocol::FramebufferInfo;
use spin::Mutex;
use x86_64::instructions::interrupts;

use super::GuiDisplayInfo;
use super::framebuffer::{Framebuffer, FramebufferRect, build_framebuffer};
use crate::memory::paging::{self, ProcessAddressSpace};

const HUGE_2MIB: u64 = 2 * 1024 * 1024;
const ENABLE_FRAMEBUFFER_WRITE_COMBINE: bool = false;
const MAX_BACKEND_PRESENT_SAMPLE_LOGS: usize = 8;

enum BackendInstance {
    Unavailable,
    Framebuffer(FramebufferDisplayBackend),
}

struct FramebufferDisplayBackend {
    framebuffer: Framebuffer,
}

pub(crate) struct DisplayBackend {
    instance: BackendInstance,
    generation: u64,
}

static DISPLAY_BACKEND: Mutex<DisplayBackend> = Mutex::new(DisplayBackend::empty());
static BACKEND_PRESENT_SAMPLE_LOG_COUNT: AtomicUsize = AtomicUsize::new(0);

impl DisplayBackend {
    const fn empty() -> Self {
        Self {
            instance: BackendInstance::Unavailable,
            generation: 0,
        }
    }

    fn install_framebuffer(&mut self, info: FramebufferInfo) -> bool {
        if !framebuffer_info_is_valid(info) {
            return false;
        }
        if ENABLE_FRAMEBUFFER_WRITE_COMBINE {
            mark_framebuffer_write_combine(info);
        }
        self.generation = next_display_generation(self.generation);
        self.instance = BackendInstance::Framebuffer(FramebufferDisplayBackend {
            framebuffer: build_framebuffer(info),
        });
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
                generation: self.generation,
            }),
        }
    }
}

fn next_display_generation(current: u64) -> u64 {
    let next = current.wrapping_add(1);
    if next == 0 { 1 } else { next }
}

pub(crate) fn install_boot_framebuffer(info: FramebufferInfo) -> bool {
    DISPLAY_BACKEND.lock().install_framebuffer(info)
}

pub(crate) fn install_driver_framebuffer(info: FramebufferInfo) -> bool {
    DISPLAY_BACKEND.lock().install_framebuffer(info)
}

pub(crate) fn try_with_framebuffer<R>(f: impl FnOnce(&mut Framebuffer) -> R) -> Option<R> {
    let mut backend = DISPLAY_BACKEND.try_lock()?;
    backend.with_framebuffer(f)
}

pub(crate) fn display_info() -> Option<GuiDisplayInfo> {
    DISPLAY_BACKEND.lock().display_info()
}

pub(crate) fn present_bgra8888_from_user(
    address_space: &ProcessAddressSpace,
    user_ptr: u64,
    width: usize,
    height: usize,
    stride_bytes: usize,
) -> Result<bool, paging::AddressSpaceError> {
    let Some(result) = DISPLAY_BACKEND.lock().with_framebuffer(|framebuffer| {
        let drawn = framebuffer.draw_bgra8888_frame_from_user(
            address_space,
            user_ptr,
            width,
            height,
            stride_bytes,
        )?;
        log_backend_present_sample(framebuffer, drawn);
        if drawn {
            let presented = framebuffer.present_scene();
            if presented {
                crate::driver::virtio_gpu::flush_primary();
            }
            return Ok(presented);
        }
        Ok(false)
    }) else {
        return Ok(false);
    };

    result
}

#[cfg_attr(not(rustos_debug_print_enabled), allow(unused_variables))]
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

pub(crate) fn present_bgra8888_rect_from_user(
    address_space: &ProcessAddressSpace,
    user_ptr: u64,
    width: usize,
    height: usize,
    stride_bytes: usize,
    rect: FramebufferRect,
) -> Result<bool, paging::AddressSpaceError> {
    let Some(result) = DISPLAY_BACKEND.lock().with_framebuffer(|framebuffer| {
        let drawn = framebuffer.draw_bgra8888_frame_rect_from_user(
            address_space,
            user_ptr,
            width,
            height,
            stride_bytes,
            rect,
        )?;
        if drawn {
            let presented = framebuffer.present_scene();
            if presented {
                crate::driver::virtio_gpu::flush_primary();
            }
            return Ok(presented);
        }
        Ok(false)
    }) else {
        return Ok(false);
    };

    result
}

pub(crate) fn present_bgra8888_from_kernel(
    src_ptr: *const u8,
    width: usize,
    height: usize,
    stride_bytes: usize,
) -> bool {
    DISPLAY_BACKEND
        .lock()
        .with_framebuffer(|framebuffer| {
            let drawn =
                framebuffer.draw_bgra8888_frame_from_kernel(src_ptr, width, height, stride_bytes);
            if drawn {
                let presented = framebuffer.present_scene();
                if presented {
                    crate::driver::virtio_gpu::flush_primary();
                }
                return presented;
            }
            false
        })
        .unwrap_or(false)
}

pub(crate) fn present_bgra8888_rect_from_kernel(
    src_ptr: *const u8,
    width: usize,
    height: usize,
    stride_bytes: usize,
    rect: FramebufferRect,
) -> bool {
    DISPLAY_BACKEND
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
                let presented = framebuffer.present_scene();
                if presented {
                    crate::driver::virtio_gpu::flush_primary();
                }
                return presented;
            }
            false
        })
        .unwrap_or(false)
}

fn mark_framebuffer_write_combine(info: FramebufferInfo) {
    let end_addr = info
        .addr
        .checked_add(info.size.saturating_sub(1))
        .expect("framebuffer end address overflow");
    let start_block = info.addr / HUGE_2MIB;
    let end_block = end_addr / HUGE_2MIB;

    use crate::memory::paging::KERNEL_PML4;

    interrupts::without_interrupts(|| {
        let mut pml4 = KERNEL_PML4.lock();
        for block_index in start_block..=end_block {
            pml4.add_flags(block_index, paging::WRITE_COMBINE_BIT);
        }
    });
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
