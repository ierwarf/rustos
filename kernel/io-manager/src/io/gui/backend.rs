// RING3-MIGRATION-REFERENCE START: display-present-substrate exception:
// uiserver and the Linux DVM own display backend policy. Ring0 keeps framebuffer
// mapping, write-combine setup, and present copy substrate.
use boot_protocol::FramebufferInfo;

use super::framebuffer::{Framebuffer, FramebufferRect, build_framebuffer};
use super::{GuiDisplayInfo, GuiPresentOutcome};
use crate::memory::paging;
use crate::sync::KernelWaitLock;

const ENABLE_FRAMEBUFFER_WRITE_COMBINE: bool = true;

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

pub(crate) fn install_driver_framebuffer(info: FramebufferInfo, flags: u32) -> bool {
    // Serialize provider replacement with a GUI-DVM publish so the pool cannot
    // be detached while a slot is being copied and its fixed PRESENT record is
    // being committed.
    let mut backend = DISPLAY_BACKEND.lock();
    crate::io::dvm_display::on_framebuffer_installed(info.addr);
    backend.install_framebuffer(info, flags)
}

pub(crate) fn try_with_framebuffer<R>(f: impl FnOnce(&mut Framebuffer) -> R) -> Option<R> {
    let mut backend = DISPLAY_BACKEND.try_lock()?;
    backend.with_framebuffer(f)
}

/// `Err(())` means the backend lock is contended.  A syscall present must
/// preserve its damage and retry; it must not wait while the entry path may
/// have interrupts masked.
fn try_with_framebuffer_nonblocking<R>(
    f: impl FnOnce(&mut Framebuffer) -> R,
) -> Result<Option<R>, ()> {
    let Some(mut backend) = DISPLAY_BACKEND.try_lock() else {
        return Err(());
    };
    Ok(backend.with_framebuffer(f))
}

pub(crate) fn display_info() -> Option<GuiDisplayInfo> {
    DISPLAY_BACKEND.lock().display_info()
}

pub(crate) fn present_bgra8888_from_kernel(
    src_ptr: *const u8,
    width: usize,
    height: usize,
    stride_bytes: usize,
) -> GuiPresentOutcome {
    crate::io::dvm_display::ensure_installed_before_present();
    if display_present_faulted() {
        return GuiPresentOutcome::Unavailable;
    }
    // A display ioctl may arrive through an entry path with interrupts masked.
    // Presentation must never turn that into a spurious device removal or wait
    // on a contended backend lock.  A busy compositor has an explicit
    // non-blocking drop outcome; the next damage update will repaint it.
    let dvm_published = match try_with_framebuffer_nonblocking(|_| {
        crate::io::dvm_display::try_publish_full(src_ptr, width, height, stride_bytes)
    }) {
        Ok(value) => value,
        Err(()) => return GuiPresentOutcome::Backpressured,
    };
    match dvm_published {
        Some(crate::io::dvm_display::DvmPresentOutcome::Presented) => GuiPresentOutcome::Presented,
        Some(crate::io::dvm_display::DvmPresentOutcome::Backpressured) => {
            GuiPresentOutcome::Backpressured
        }
        Some(crate::io::dvm_display::DvmPresentOutcome::Unavailable) | None => {
            GuiPresentOutcome::Unavailable
        }
    }
}

pub(crate) fn present_bgra8888_rect_from_kernel(
    src_ptr: *const u8,
    width: usize,
    height: usize,
    stride_bytes: usize,
    rect: FramebufferRect,
) -> GuiPresentOutcome {
    crate::io::dvm_display::ensure_installed_before_present();
    if display_present_faulted() {
        return GuiPresentOutcome::Unavailable;
    }
    let dvm_published = match try_with_framebuffer_nonblocking(|_| {
        crate::io::dvm_display::try_publish_rect(
            src_ptr,
            width,
            height,
            stride_bytes,
            rect.x as u32,
            rect.y as u32,
            rect.width as u32,
            rect.height as u32,
        )
    }) {
        Ok(value) => value,
        Err(()) => return GuiPresentOutcome::Backpressured,
    };
    match dvm_published {
        Some(crate::io::dvm_display::DvmPresentOutcome::Presented) => GuiPresentOutcome::Presented,
        Some(crate::io::dvm_display::DvmPresentOutcome::Backpressured) => {
            GuiPresentOutcome::Backpressured
        }
        Some(crate::io::dvm_display::DvmPresentOutcome::Unavailable) | None => {
            GuiPresentOutcome::Unavailable
        }
    }
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
// RING3-MIGRATION-REFERENCE END: uiserver/DVM-owned display backend substrate exception.
