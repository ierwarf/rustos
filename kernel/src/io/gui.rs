mod backend;
mod framebuffer;
mod terminal;

use core::sync::atomic::{AtomicU16, AtomicU8, Ordering};

use boot_protocol::{BOOT_INFO_MAGIC, BOOT_INFO_VERSION, BootInfo};
use spin::Mutex;

use self::framebuffer::{Framebuffer, FramebufferRect};
use self::terminal::{TerminalRenderer, TerminalState};
use crate::session::ConsoleSessionId;

const CURSOR_BLINK_TOGGLE_TICKS: u16 = 512;

static EMERGENCY_CONSOLE: Mutex<EmergencyConsoleUi> = Mutex::new(EmergencyConsoleUi::new());
static CURSOR_BLINK_TICKS: AtomicU16 = AtomicU16::new(0);
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

struct EmergencyConsoleUi {
    terminal: TerminalState,
    renderer: TerminalRenderer,
}

impl EmergencyConsoleUi {
    const fn new() -> Self {
        Self {
            terminal: TerminalState::new(),
            renderer: TerminalRenderer::new(),
        }
    }

    fn ensure_layout(&mut self, framebuffer: &Framebuffer) {
        let bounds = FramebufferRect {
            x: 0,
            y: 0,
            width: framebuffer.width(),
            height: framebuffer.height(),
        };
        self.terminal.ensure_layout(bounds);
        let _ = self.terminal.set_focused(true);
    }

    fn init(&mut self, framebuffer: &mut Framebuffer) {
        self.ensure_layout(framebuffer);
        self.terminal.render(framebuffer, &self.renderer);
    }

    fn write(&mut self, framebuffer: &mut Framebuffer, bytes: &[u8]) {
        self.ensure_layout(framebuffer);
        self.terminal.write_bytes(bytes);
        self.terminal.render(framebuffer, &self.renderer);
    }

    fn toggle_cursor(&mut self, framebuffer: &mut Framebuffer) -> bool {
        self.ensure_layout(framebuffer);
        if !self.terminal.toggle_cursor() {
            return false;
        }
        self.terminal.render(framebuffer, &self.renderer);
        true
    }
}

pub fn init_console() {
    reset_cursor_blink();
    let _ = backend::with_framebuffer(|framebuffer| {
        let mut console = EMERGENCY_CONSOLE.lock();
        console.init(framebuffer);
        framebuffer.present_scene();
    });
}

pub fn write_console_session(session: ConsoleSessionId, bytes: &[u8]) {
    let _ = session;
    if bytes.is_empty() || userspace_display_active() {
        return;
    }

    reset_cursor_blink();
    let _ = backend::with_framebuffer(|framebuffer| {
        let mut console = EMERGENCY_CONSOLE.lock();
        console.write(framebuffer, bytes);
        framebuffer.present_scene();
    });
}

pub fn try_write_console(bytes: &[u8]) -> bool {
    if bytes.is_empty() || userspace_display_active() {
        return true;
    }

    reset_cursor_blink();
    backend::try_with_framebuffer(|framebuffer| {
        let Some(mut console) = EMERGENCY_CONSOLE.try_lock() else {
            return false;
        };
        console.write(framebuffer, bytes);
        framebuffer.present_scene();
        true
    })
    .unwrap_or(false)
}

pub fn tick_console_cursor() {
    if userspace_display_active() {
        return;
    }

    let ticks = CURSOR_BLINK_TICKS.fetch_add(1, Ordering::Relaxed) + 1;
    if ticks < CURSOR_BLINK_TOGGLE_TICKS {
        return;
    }
    CURSOR_BLINK_TICKS.store(0, Ordering::Relaxed);

    let _ = backend::with_framebuffer(|framebuffer| {
        let mut console = EMERGENCY_CONSOLE.lock();
        let _ = console.toggle_cursor(framebuffer);
        framebuffer.present_scene();
    });
}

pub fn init(boot_info_ptr: *const BootInfo) {
    let boot_info = boot_info_from_ptr(boot_info_ptr);
    backend::init_gop(boot_info.framebuffer);
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

fn reset_cursor_blink() {
    CURSOR_BLINK_TICKS.store(0, Ordering::Relaxed);
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
