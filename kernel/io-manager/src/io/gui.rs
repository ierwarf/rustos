mod backend;
mod framebuffer;

use core::sync::atomic::{AtomicU8, Ordering};

use boot_protocol::{BootInfo, FramebufferInfo};
use driver_abi::{
    DISPLAY_FRAMEBUFFER_FLAG_BOOT_FRAMEBUFFER, DISPLAY_FRAMEBUFFER_FLAG_PRIMARY_PROVIDER,
    DISPLAY_FRAMEBUFFER_KNOWN_FLAGS, DisplayFramebufferRegistration, DisplayPixelFormat,
};
use embedded_graphics::pixelcolor::Rgb888;

use crate::user::abi::device::{
    DISPLAY_INFO_FLAG_BOOT_FRAMEBUFFER, DISPLAY_INFO_FLAG_PRIMARY_PROVIDER,
};

static USERSPACE_DISPLAY_MODE: AtomicU8 = AtomicU8::new(DISPLAY_MODE_BOOT_CONSOLE);

const DISPLAY_MODE_BOOT_CONSOLE: u8 = 0;
const DISPLAY_MODE_USER_TRANSITION: u8 = 1;
const DISPLAY_MODE_USER_ACTIVE: u8 = 2;

#[derive(Clone, Copy, Debug, Default)]
pub struct GuiDisplayInfo {
    pub width: u32,
    pub height: u32,
    pub stride_bytes: u32,
    pub bytes_per_pixel: u32,
    pub flags: u32,
    pub generation: u64,
}

pub fn try_present_panic_blackout() -> bool {
    backend::try_with_framebuffer(|framebuffer| {
        framebuffer.fill(Rgb888::new(0, 0, 0));
        framebuffer.present_scene();
        true
    })
    .unwrap_or(false)
}

pub fn init(boot_info_ptr: *const BootInfo) {
    let boot_info = boot_info_from_ptr(boot_info_ptr);
    let _ = backend::install_boot_framebuffer(
        boot_info.framebuffer,
        DISPLAY_INFO_FLAG_BOOT_FRAMEBUFFER,
    );
}

pub unsafe extern "C" fn register_driver_framebuffer(
    framebuffer: *const DisplayFramebufferRegistration,
) -> i32 {
    if nucleus_core::util::fault_injection::should_fail("display.provider.register") {
        crate::debug::warn!(
            display,
            "fault injection: display.provider.register rejected framebuffer"
        );
        return -5;
    }
    let Some(framebuffer) = (unsafe { framebuffer.as_ref() }) else {
        return -22;
    };

    let pixel_format = match framebuffer.pixel_format {
        value if value == DisplayPixelFormat::Rgb as u32 => boot_protocol::BootPixelFormat::Rgb,
        value if value == DisplayPixelFormat::Bgr as u32 => boot_protocol::BootPixelFormat::Bgr,
        value if value == DisplayPixelFormat::Bitmask as u32 => {
            boot_protocol::BootPixelFormat::Bitmask
        }
        value if value == DisplayPixelFormat::Unknown as u32 => {
            boot_protocol::BootPixelFormat::Unknown
        }
        _ => return -22,
    };

    if framebuffer.reserved != [0; 2]
        || framebuffer.flags == 0
        || framebuffer.flags & !DISPLAY_FRAMEBUFFER_KNOWN_FLAGS != 0
    {
        return -22;
    }

    let driver_framebuffer = FramebufferInfo {
        addr: framebuffer.addr,
        size: framebuffer.size,
        back_buffer_addr: framebuffer.back_buffer_addr,
        back_buffer_size: framebuffer.back_buffer_size,
        width: framebuffer.width,
        height: framebuffer.height,
        stride: framebuffer.stride,
        pixel_format,
        bytes_per_pixel: framebuffer.bytes_per_pixel,
        _reserved: [0; 3],
    };
    if driver_framebuffer.validate().is_err() {
        return -22;
    }

    let display_flags = display_flags_from_driver_registration(framebuffer.flags);

    if !backend::install_driver_framebuffer(driver_framebuffer, display_flags) {
        return -22;
    }
    0
}

pub(crate) fn install_native_driver_framebuffer(framebuffer: FramebufferInfo) -> bool {
    if framebuffer.validate().is_err() {
        return false;
    }
    backend::install_driver_framebuffer(framebuffer, DISPLAY_INFO_FLAG_PRIMARY_PROVIDER)
}

pub(crate) fn install_boot_framebuffer_fallback(framebuffer: FramebufferInfo) -> bool {
    if framebuffer.validate().is_err() {
        return false;
    }
    let flags = DISPLAY_INFO_FLAG_BOOT_FRAMEBUFFER | DISPLAY_INFO_FLAG_PRIMARY_PROVIDER;
    backend::install_driver_framebuffer(framebuffer, flags)
}

pub fn display_info() -> Option<GuiDisplayInfo> {
    backend::display_info()
}

fn display_flags_from_driver_registration(registration_flags: u8) -> u32 {
    let mut flags = 0;
    if registration_flags & DISPLAY_FRAMEBUFFER_FLAG_BOOT_FRAMEBUFFER != 0 {
        flags |= DISPLAY_INFO_FLAG_BOOT_FRAMEBUFFER;
    }
    if registration_flags & DISPLAY_FRAMEBUFFER_FLAG_PRIMARY_PROVIDER != 0 {
        flags |= DISPLAY_INFO_FLAG_PRIMARY_PROVIDER;
    }
    flags
}

pub fn present_userspace_frame_from_kernel_bgra8888(
    src_ptr: *const u8,
    width: usize,
    height: usize,
    stride_bytes: usize,
) -> bool {
    let claimed_boot_console = begin_userspace_display_transition();
    let presented = backend::present_bgra8888_from_kernel(src_ptr, width, height, stride_bytes);
    if presented {
        finish_userspace_display_transition();
    } else if claimed_boot_console {
        USERSPACE_DISPLAY_MODE.store(DISPLAY_MODE_BOOT_CONSOLE, Ordering::Release);
    }
    presented
}

pub fn present_userspace_frame_rect_from_kernel_bgra8888(
    src_ptr: *const u8,
    width: usize,
    height: usize,
    stride_bytes: usize,
    x: usize,
    y: usize,
    rect_width: usize,
    rect_height: usize,
) -> bool {
    let claimed_boot_console = begin_userspace_display_transition();
    let presented = backend::present_bgra8888_rect_from_kernel(
        src_ptr,
        width,
        height,
        stride_bytes,
        self::framebuffer::FramebufferRect {
            x,
            y,
            width: rect_width,
            height: rect_height,
        },
    );
    if presented {
        finish_userspace_display_transition();
    } else if claimed_boot_console {
        USERSPACE_DISPLAY_MODE.store(DISPLAY_MODE_BOOT_CONSOLE, Ordering::Release);
    }
    presented
}

pub fn is_userspace_display_active() -> bool {
    USERSPACE_DISPLAY_MODE.load(Ordering::Acquire) != DISPLAY_MODE_BOOT_CONSOLE
}

fn boot_info_from_ptr(boot_info_ptr: *const BootInfo) -> &'static BootInfo {
    match unsafe { BootInfo::from_ptr(boot_info_ptr) } {
        Ok(boot_info) => boot_info,
        Err(error) => panic!("{}", error.as_str()),
    }
}

fn begin_userspace_display_transition() -> bool {
    USERSPACE_DISPLAY_MODE
        .compare_exchange(
            DISPLAY_MODE_BOOT_CONSOLE,
            DISPLAY_MODE_USER_TRANSITION,
            Ordering::AcqRel,
            Ordering::Acquire,
        )
        .is_ok()
}

fn finish_userspace_display_transition() {
    if USERSPACE_DISPLAY_MODE.swap(DISPLAY_MODE_USER_ACTIVE, Ordering::AcqRel)
        != DISPLAY_MODE_USER_ACTIVE
    {
        crate::debug::println!("userspace display active");
    }
}
