mod desktop;
mod framebuffer;
mod terminal;
mod window_manager;

use core::sync::atomic::{AtomicU16, Ordering};

use boot_protocol::{BOOT_INFO_MAGIC, BOOT_INFO_VERSION, BootInfo, FramebufferInfo};
use spin::Mutex;
use x86_64::instructions::interrupts;

use self::desktop::GuiDesktop;
use self::framebuffer::{Framebuffer, build_framebuffer};
use crate::paging;
use crate::session::ConsoleSessionId;

const HUGE_2MIB: u64 = 2 * 1024 * 1024;
const CURSOR_BLINK_TOGGLE_TICKS: u16 = 512;

pub static GOP_SCREEN: Mutex<Framebuffer> = Mutex::new(Framebuffer::empty());
static GUI_DESKTOP: Mutex<GuiDesktop> = Mutex::new(GuiDesktop::new());
static CURSOR_BLINK_TICKS: AtomicU16 = AtomicU16::new(0);

pub fn init_console() {
    reset_cursor_blink();
    interrupts::without_interrupts(|| {
        let mut framebuffer = GOP_SCREEN.lock();
        let mut desktop = GUI_DESKTOP.lock();
        desktop.prepare_frame(&mut framebuffer);
        desktop.init_console(&mut framebuffer);
        desktop.present(&mut framebuffer);
    });
}

pub fn write_console(bytes: &[u8]) {
    if bytes.is_empty() {
        return;
    }

    reset_cursor_blink();
    interrupts::without_interrupts(|| {
        let mut framebuffer = GOP_SCREEN.lock();
        let mut desktop = GUI_DESKTOP.lock();
        desktop.prepare_frame(&mut framebuffer);
        desktop.write_console(&mut framebuffer, bytes);
        desktop.present(&mut framebuffer);
    });
}

pub fn write_console_session(session: ConsoleSessionId, bytes: &[u8]) {
    if bytes.is_empty() {
        return;
    }

    reset_cursor_blink();
    interrupts::without_interrupts(|| {
        let mut framebuffer = GOP_SCREEN.lock();
        let mut desktop = GUI_DESKTOP.lock();
        desktop.prepare_frame(&mut framebuffer);
        desktop.write_console_session(&mut framebuffer, session, bytes);
        desktop.present(&mut framebuffer);
    });
}

pub fn try_write_console(bytes: &[u8]) -> bool {
    if bytes.is_empty() {
        return true;
    }

    reset_cursor_blink();
    interrupts::without_interrupts(|| {
        let Some(mut framebuffer) = GOP_SCREEN.try_lock() else {
            return false;
        };
        let Some(mut desktop) = GUI_DESKTOP.try_lock() else {
            return false;
        };
        desktop.prepare_frame(&mut framebuffer);
        desktop.write_console(&mut framebuffer, bytes);
        desktop.present(&mut framebuffer);
        true
    })
}

pub fn tick_console_cursor() {
    let ticks = CURSOR_BLINK_TICKS.fetch_add(1, Ordering::Relaxed) + 1;
    if ticks < CURSOR_BLINK_TOGGLE_TICKS {
        return;
    }
    CURSOR_BLINK_TICKS.store(0, Ordering::Relaxed);

    interrupts::without_interrupts(|| {
        let mut framebuffer = GOP_SCREEN.lock();
        let mut desktop = GUI_DESKTOP.lock();
        desktop.prepare_frame(&mut framebuffer);
        let _ = desktop.toggle_console_cursor(&mut framebuffer);
        desktop.present(&mut framebuffer);
    });
}

pub fn show_mouse_cursor() -> bool {
    interrupts::without_interrupts(|| {
        let mut framebuffer = GOP_SCREEN.lock();
        let mut desktop = GUI_DESKTOP.lock();
        desktop.prepare_frame(&mut framebuffer);
        let changed = desktop.show_mouse_cursor(&mut framebuffer);
        desktop.present(&mut framebuffer);
        changed
    })
}

pub fn move_mouse_cursor_relative(dx: i16, dy: i16) -> bool {
    interrupts::without_interrupts(|| {
        let mut framebuffer = GOP_SCREEN.lock();
        let mut desktop = GUI_DESKTOP.lock();
        desktop.prepare_frame(&mut framebuffer);
        let changed = desktop.move_mouse_cursor_relative(&mut framebuffer, dx, dy);
        desktop.present(&mut framebuffer);
        changed
    })
}

pub fn set_mouse_left_button(pressed: bool) -> bool {
    interrupts::without_interrupts(|| {
        let mut framebuffer = GOP_SCREEN.lock();
        let mut desktop = GUI_DESKTOP.lock();
        desktop.prepare_frame(&mut framebuffer);
        let changed = desktop.set_mouse_left_button(&mut framebuffer, pressed);
        desktop.present(&mut framebuffer);
        changed
    })
}

pub fn init(boot_info_ptr: *const BootInfo) {
    let boot_info = boot_info_from_ptr(boot_info_ptr);
    let framebuffer = build_framebuffer(boot_info.framebuffer);
    mark_framebuffer_write_combine(boot_info.framebuffer);
    *GOP_SCREEN.lock() = framebuffer;
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
