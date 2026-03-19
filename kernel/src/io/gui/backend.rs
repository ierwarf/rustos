use boot_protocol::FramebufferInfo;
use spin::Mutex;
use x86_64::instructions::interrupts;

use super::framebuffer::{build_framebuffer, Framebuffer, FramebufferRect};
use super::GuiDisplayInfo;
use crate::paging::{self, ProcessAddressSpace};

const HUGE_2MIB: u64 = 2 * 1024 * 1024;

enum BackendInstance {
    Unavailable,
    Framebuffer(FramebufferDisplayBackend),
}

struct FramebufferDisplayBackend {
    framebuffer: Framebuffer,
}

pub(crate) struct DisplayBackend {
    instance: BackendInstance,
}

static DISPLAY_BACKEND: Mutex<DisplayBackend> = Mutex::new(DisplayBackend::empty());

impl DisplayBackend {
    const fn empty() -> Self {
        Self {
            instance: BackendInstance::Unavailable,
        }
    }

    fn init_gop(&mut self, info: FramebufferInfo) {
        self.install_framebuffer(info);
    }

    fn install_framebuffer(&mut self, info: FramebufferInfo) {
        mark_framebuffer_write_combine(info);
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
            }),
        }
    }
}

pub(crate) fn init_gop(info: FramebufferInfo) {
    interrupts::without_interrupts(|| {
        DISPLAY_BACKEND.lock().init_gop(info);
    });
}

pub(crate) fn install_driver_framebuffer(info: FramebufferInfo) {
    interrupts::without_interrupts(|| {
        DISPLAY_BACKEND.lock().install_framebuffer(info);
    });
}

pub(crate) fn with_framebuffer<R>(f: impl FnOnce(&mut Framebuffer) -> R) -> Option<R> {
    interrupts::without_interrupts(|| DISPLAY_BACKEND.lock().with_framebuffer(f))
}

pub(crate) fn try_with_framebuffer<R>(f: impl FnOnce(&mut Framebuffer) -> R) -> Option<R> {
    interrupts::without_interrupts(|| {
        let mut backend = DISPLAY_BACKEND.try_lock()?;
        backend.with_framebuffer(f)
    })
}

pub(crate) fn display_info() -> Option<GuiDisplayInfo> {
    interrupts::without_interrupts(|| DISPLAY_BACKEND.lock().display_info())
}

pub(crate) fn present_bgra8888(
    width: usize,
    height: usize,
    stride_bytes: usize,
    bytes: &[u8],
) -> bool {
    let Some(presented) = DISPLAY_BACKEND.lock().with_framebuffer(|framebuffer| {
        if framebuffer.draw_bgra8888_frame(width, height, stride_bytes, bytes) {
            framebuffer.present_scene();
            return true;
        }
        false
    }) else {
        return false;
    };

    presented
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
            framebuffer.present_scene();
        }
        Ok(drawn)
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
            framebuffer.present_scene();
        }
        Ok(drawn)
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

    use crate::paging::KERNEL_PML4;

    interrupts::without_interrupts(|| {
        let mut pml4 = KERNEL_PML4.lock();
        for block_index in start_block..=end_block {
            pml4.add_flags(block_index, paging::WRITE_COMBINE_BIT);
        }
    });
}
