use boot_protocol::FramebufferInfo;
use spin::Mutex;
use x86_64::instructions::interrupts;

use super::GuiDisplayInfo;
use super::framebuffer::{Framebuffer, FramebufferRect, build_framebuffer};
use crate::memory::paging::{self, ProcessAddressSpace};

const HUGE_2MIB: u64 = 2 * 1024 * 1024;
const ENABLE_FRAMEBUFFER_WRITE_COMBINE: bool = false;

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

impl DisplayBackend {
    const fn empty() -> Self {
        Self {
            instance: BackendInstance::Unavailable,
            generation: 0,
        }
    }

    fn install_framebuffer(&mut self, info: FramebufferInfo) {
        if ENABLE_FRAMEBUFFER_WRITE_COMBINE {
            mark_framebuffer_write_combine(info);
        }
        self.generation = next_display_generation(self.generation);
        self.instance = BackendInstance::Framebuffer(FramebufferDisplayBackend {
            framebuffer: build_framebuffer(info),
        });
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

pub(crate) fn install_driver_framebuffer(info: FramebufferInfo) {
    interrupts::without_interrupts(|| {
        DISPLAY_BACKEND.lock().install_framebuffer(info);
    });
}

pub(crate) fn try_with_framebuffer<R>(f: impl FnOnce(&mut Framebuffer) -> R) -> Option<R> {
    interrupts::without_interrupts(|| {
        let mut backend = DISPLAY_BACKEND.try_lock()?;
        backend.with_framebuffer(f)
    })
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
        if drawn {
            return Ok(framebuffer.present_scene());
        }
        Ok(false)
    }) else {
        return Ok(false);
    };

    result
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
            return Ok(framebuffer.present_scene());
        }
        Ok(false)
    }) else {
        return Ok(false);
    };

    result
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
