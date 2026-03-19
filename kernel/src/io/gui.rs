mod backend;
mod framebuffer;

use core::sync::atomic::{AtomicU8, Ordering};

use boot_protocol::{BootInfo, BOOT_INFO_MAGIC, BOOT_INFO_VERSION};
use driver_abi::{DisplayFramebufferRegistration, DisplayPixelFormat};
use embedded_graphics::pixelcolor::Rgb888;

use self::framebuffer::FramebufferRect;
use crate::session::ConsoleSessionId;

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
}

pub fn init_console() {
    // Boot and fatal output are routed to the debug log instead of a framebuffer terminal.
}

pub fn write_console_session(session: ConsoleSessionId, bytes: &[u8]) {
    let _ = session;
    let _ = bytes;
}

pub fn try_write_console(bytes: &[u8]) -> bool {
    let _ = bytes;
    true
}

pub fn try_present_panic_blackout() -> bool {
    backend::try_with_framebuffer(|framebuffer| {
        framebuffer.fill(Rgb888::new(0, 0, 0));
        framebuffer.present_scene();
        true
    })
    .unwrap_or(false)
}

pub fn tick_console_cursor() {}

pub fn init(boot_info_ptr: *const BootInfo) {
    let boot_info = boot_info_from_ptr(boot_info_ptr);
    backend::init_gop(boot_info.framebuffer);
}

pub(crate) unsafe extern "C" fn register_driver_framebuffer(
    framebuffer: *const DisplayFramebufferRegistration,
) -> i32 {
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

    if framebuffer.addr == 0
        || framebuffer.size == 0
        || framebuffer.width == 0
        || framebuffer.height == 0
        || framebuffer.stride == 0
        || !(3..=4).contains(&framebuffer.bytes_per_pixel)
    {
        return -22;
    }

    backend::install_driver_framebuffer(boot_protocol::FramebufferInfo {
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
    });
    0
}

pub fn display_info() -> Option<GuiDisplayInfo> {
    backend::display_info()
}

pub fn present_userspace_frame_bgra8888(
    width: usize,
    height: usize,
    stride_bytes: usize,
    bytes: &[u8],
) -> bool {
    let claimed_boot_console = begin_userspace_display_transition();
    let presented = backend::present_bgra8888(width, height, stride_bytes, bytes);
    if presented {
        finish_userspace_display_transition();
    } else if claimed_boot_console {
        USERSPACE_DISPLAY_MODE.store(DISPLAY_MODE_BOOT_CONSOLE, Ordering::Release);
    }
    presented
}

pub fn present_userspace_frame_from_user_bgra8888(
    address_space: &crate::paging::ProcessAddressSpace,
    user_ptr: u64,
    width: usize,
    height: usize,
    stride_bytes: usize,
) -> Result<bool, crate::paging::AddressSpaceError> {
    let claimed_boot_console = begin_userspace_display_transition();
    let presented =
        backend::present_bgra8888_from_user(address_space, user_ptr, width, height, stride_bytes)?;
    if presented {
        finish_userspace_display_transition();
    } else if claimed_boot_console {
        USERSPACE_DISPLAY_MODE.store(DISPLAY_MODE_BOOT_CONSOLE, Ordering::Release);
    }
    Ok(presented)
}

pub fn present_userspace_frame_rect_from_user_bgra8888(
    address_space: &crate::paging::ProcessAddressSpace,
    user_ptr: u64,
    width: usize,
    height: usize,
    stride_bytes: usize,
    x: usize,
    y: usize,
    rect_width: usize,
    rect_height: usize,
) -> Result<bool, crate::paging::AddressSpaceError> {
    let claimed_boot_console = begin_userspace_display_transition();
    let presented = backend::present_bgra8888_rect_from_user(
        address_space,
        user_ptr,
        width,
        height,
        stride_bytes,
        FramebufferRect {
            x,
            y,
            width: rect_width,
            height: rect_height,
        },
    )?;
    if presented {
        finish_userspace_display_transition();
    } else if claimed_boot_console {
        USERSPACE_DISPLAY_MODE.store(DISPLAY_MODE_BOOT_CONSOLE, Ordering::Release);
    }
    Ok(presented)
}

fn userspace_display_active() -> bool {
    USERSPACE_DISPLAY_MODE.load(Ordering::Acquire) != DISPLAY_MODE_BOOT_CONSOLE
}

pub fn is_userspace_display_active() -> bool {
    userspace_display_active()
}

fn boot_info_from_ptr(boot_info_ptr: *const BootInfo) -> &'static BootInfo {
    if boot_info_ptr.is_null() {
        panic!("boot info pointer is null");
    }

    let boot_info = unsafe { &*boot_info_ptr };
    if boot_info.magic != BOOT_INFO_MAGIC {
        panic!("boot info magic mismatch");
    }
    if boot_info.version != BOOT_INFO_VERSION {
        panic!("boot info version mismatch");
    }

    boot_info
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
