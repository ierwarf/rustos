mod framebuffer;
mod terminal;

use core::sync::atomic::{AtomicBool, AtomicU16, Ordering};

use boot_protocol::{BOOT_INFO_MAGIC, BOOT_INFO_VERSION, BootInfo, FramebufferInfo};
use spin::Mutex;
use x86_64::instructions::interrupts;

use self::framebuffer::{Framebuffer, FramebufferRect, build_framebuffer};
use self::terminal::{TerminalRenderer, TerminalState};
use crate::paging;
use crate::session::ConsoleSessionId;

const HUGE_2MIB: u64 = 2 * 1024 * 1024;
const CURSOR_BLINK_TOGGLE_TICKS: u16 = 512;

pub static GOP_SCREEN: Mutex<Framebuffer> = Mutex::new(Framebuffer::empty());
static EMERGENCY_CONSOLE: Mutex<EmergencyConsoleUi> = Mutex::new(EmergencyConsoleUi::new());
static CURSOR_BLINK_TICKS: AtomicU16 = AtomicU16::new(0);
static USERSPACE_DISPLAY_ACTIVE: AtomicBool = AtomicBool::new(false);

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
    interrupts::without_interrupts(|| {
        let mut framebuffer = GOP_SCREEN.lock();
        let mut console = EMERGENCY_CONSOLE.lock();
        console.init(&mut framebuffer);
        framebuffer.present_scene();
    });
}

pub fn write_console_session(session: ConsoleSessionId, bytes: &[u8]) {
    let _ = session;
    if bytes.is_empty() || userspace_display_active() {
        return;
    }

    reset_cursor_blink();
    interrupts::without_interrupts(|| {
        let mut framebuffer = GOP_SCREEN.lock();
        let mut console = EMERGENCY_CONSOLE.lock();
        console.write(&mut framebuffer, bytes);
        framebuffer.present_scene();
    });
}

pub fn try_write_console(bytes: &[u8]) -> bool {
    if bytes.is_empty() || userspace_display_active() {
        return true;
    }

    reset_cursor_blink();
    interrupts::without_interrupts(|| {
        let Some(mut framebuffer) = GOP_SCREEN.try_lock() else {
            return false;
        };
        let Some(mut console) = EMERGENCY_CONSOLE.try_lock() else {
            return false;
        };
        console.write(&mut framebuffer, bytes);
        framebuffer.present_scene();
        true
    })
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

    interrupts::without_interrupts(|| {
        let mut framebuffer = GOP_SCREEN.lock();
        let mut console = EMERGENCY_CONSOLE.lock();
        let _ = console.toggle_cursor(&mut framebuffer);
        framebuffer.present_scene();
    });
}

pub fn show_mouse_cursor() -> bool {
    false
}

pub fn move_mouse_cursor_relative(dx: i16, dy: i16) -> bool {
    let _ = (dx, dy);
    false
}

pub fn set_mouse_left_button(pressed: bool) -> bool {
    let _ = pressed;
    false
}

pub fn init(boot_info_ptr: *const BootInfo) {
    let boot_info = boot_info_from_ptr(boot_info_ptr);
    let framebuffer = build_framebuffer(boot_info.framebuffer);
    mark_framebuffer_write_combine(boot_info.framebuffer);
    *GOP_SCREEN.lock() = framebuffer;
}

pub fn display_info() -> Option<GuiDisplayInfo> {
    interrupts::without_interrupts(|| {
        let framebuffer = GOP_SCREEN.lock();
        if framebuffer.width() == 0 || framebuffer.height() == 0 {
            return None;
        }

        Some(GuiDisplayInfo {
            width: framebuffer.width() as u32,
            height: framebuffer.height() as u32,
            stride_bytes: framebuffer.stride_bytes() as u32,
            bytes_per_pixel: framebuffer.bytes_per_pixel() as u32,
        })
    })
}

pub fn present_userspace_frame_bgra8888(
    width: usize,
    height: usize,
    stride_bytes: usize,
    bytes: &[u8],
) -> bool {
    interrupts::without_interrupts(|| {
        let mut framebuffer = GOP_SCREEN.lock();
        if !framebuffer.draw_bgra8888_frame(width, height, stride_bytes, bytes) {
            return false;
        }
        framebuffer.present_scene();
        USERSPACE_DISPLAY_ACTIVE.store(true, Ordering::Release);
        true
    })
}

fn userspace_display_active() -> bool {
    USERSPACE_DISPLAY_ACTIVE.load(Ordering::Acquire)
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

fn reset_cursor_blink() {
    CURSOR_BLINK_TICKS.store(0, Ordering::Relaxed);
}
