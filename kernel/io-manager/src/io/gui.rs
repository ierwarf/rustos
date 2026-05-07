mod backend;
mod framebuffer;

use core::str;
use core::sync::atomic::{AtomicU8, Ordering};

use boot_protocol::{BootInfo, FramebufferInfo};
use driver_abi::{DisplayFramebufferRegistration, DisplayPixelFormat};
use embedded_graphics::Drawable;
use embedded_graphics::geometry::Point;
use embedded_graphics::mono_font::{MonoTextStyle, ascii::FONT_9X18_BOLD};
use embedded_graphics::pixelcolor::Rgb888;
use embedded_graphics::prelude::RgbColor;
use embedded_graphics::text::{Baseline, Text};
use spin::Mutex;

use self::framebuffer::{Framebuffer, FramebufferRect};
use crate::io::session::ConsoleSessionHandle;

static USERSPACE_DISPLAY_MODE: AtomicU8 = AtomicU8::new(DISPLAY_MODE_BOOT_CONSOLE);
static EMERGENCY_CONSOLE: Mutex<EmergencyConsole> = Mutex::new(EmergencyConsole::new());

const DISPLAY_MODE_BOOT_CONSOLE: u8 = 0;
const DISPLAY_MODE_USER_TRANSITION: u8 = 1;
const DISPLAY_MODE_USER_ACTIVE: u8 = 2;
const EMERGENCY_PADDING_X: i32 = 12;
const EMERGENCY_PADDING_Y: i32 = 12;
const EMERGENCY_LINE_CAPACITY: usize = 240;
const EMERGENCY_HISTORY_CAPACITY: usize = 256;
const EMERGENCY_CONSOLE_ENABLED: bool = false;

#[derive(Clone, Copy, Debug, Default)]
pub struct GuiDisplayInfo {
    pub width: u32,
    pub height: u32,
    pub stride_bytes: u32,
    pub bytes_per_pixel: u32,
    pub generation: u64,
}

pub fn init_console() {
    // System console sessions are not mirrored into the early framebuffer console.
}

pub(crate) fn write_console_session(session: ConsoleSessionHandle, bytes: &[u8]) {
    let _ = session;
    let _ = bytes;
}

pub fn try_present_panic_blackout() -> bool {
    backend::try_with_framebuffer(|framebuffer| {
        framebuffer.fill(Rgb888::new(0, 0, 0));
        framebuffer.present_scene();
        true
    })
    .unwrap_or(false)
}

pub fn write_panic_console_line(bytes: &[u8]) -> bool {
    if !emergency_console_output_enabled() {
        return false;
    }
    let Some(mut console) = EMERGENCY_CONSOLE.try_lock() else {
        return false;
    };
    console.activate_panic_session();
    console.append_bytes(bytes);
    console.commit_current_line();
    console.render_pending = true;
    console.render()
}

pub fn tick_console_cursor() {
    flush_debug_console();
}

pub fn init(boot_info_ptr: *const BootInfo) {
    let boot_info = boot_info_from_ptr(boot_info_ptr);
    let mut console = EMERGENCY_CONSOLE.lock();
    if emergency_console_live_output_enabled() {
        console.start_live_session();
    } else {
        console.deactivate_live_session();
    }
    drop(console);

    if backend::install_boot_framebuffer(boot_info.framebuffer) {
        emergency_console_framebuffer_changed();
    }
}

pub unsafe extern "C" fn register_driver_framebuffer(
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

    let boot_framebuffer = FramebufferInfo {
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
    if boot_framebuffer.validate().is_err() {
        return -22;
    }

    if !backend::install_driver_framebuffer(boot_framebuffer) {
        return -22;
    }
    emergency_console_framebuffer_changed();
    0
}

pub(crate) fn install_native_driver_framebuffer(framebuffer: FramebufferInfo) -> bool {
    if framebuffer.validate().is_err() {
        return false;
    }
    if !backend::install_driver_framebuffer(framebuffer) {
        return false;
    }
    emergency_console_framebuffer_changed();
    true
}

pub fn display_info() -> Option<GuiDisplayInfo> {
    backend::display_info()
}

pub fn present_userspace_frame_from_user_bgra8888(
    address_space: &crate::memory::paging::ProcessAddressSpace,
    user_ptr: u64,
    width: usize,
    height: usize,
    stride_bytes: usize,
) -> Result<bool, crate::memory::paging::AddressSpaceError> {
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
    address_space: &crate::memory::paging::ProcessAddressSpace,
    user_ptr: u64,
    width: usize,
    height: usize,
    stride_bytes: usize,
    x: usize,
    y: usize,
    rect_width: usize,
    rect_height: usize,
) -> Result<bool, crate::memory::paging::AddressSpaceError> {
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
        FramebufferRect {
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

fn userspace_display_active() -> bool {
    USERSPACE_DISPLAY_MODE.load(Ordering::Acquire) != DISPLAY_MODE_BOOT_CONSOLE
}

pub fn is_userspace_display_active() -> bool {
    userspace_display_active()
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
        disable_emergency_console_live_output();
    }
}

#[allow(dead_code)]
pub(crate) fn write_debug_bytes(bytes: &[u8]) {
    if !emergency_console_output_enabled() {
        return;
    }
    let _ = write_emergency_console(bytes);
}

pub fn flush_debug_console() {
    if !emergency_console_output_enabled() {
        return;
    }
    let Some(mut console) = EMERGENCY_CONSOLE.try_lock() else {
        return;
    };
    if !console.live_enabled || !console.render_pending {
        return;
    }
    let _ = console.render();
}

#[allow(dead_code)]
fn write_emergency_console(bytes: &[u8]) -> bool {
    if !emergency_console_output_enabled() {
        return false;
    }
    if bytes.is_empty() {
        return true;
    }

    let Some(mut console) = EMERGENCY_CONSOLE.try_lock() else {
        return false;
    };
    if !console.live_enabled {
        return false;
    }

    let committed_before = console.history_count;
    console.append_bytes(bytes);
    if console.history_count == committed_before {
        return true;
    }
    console.render_pending = true;
    true
}

fn emergency_console_framebuffer_changed() {
    if !emergency_console_output_enabled() {
        return;
    }
    let Some(mut console) = EMERGENCY_CONSOLE.try_lock() else {
        return;
    };
    console.framebuffer_changed();
    console.render_pending = console.live_enabled && console.history_count != 0;
}

fn disable_emergency_console_live_output() {
    if let Some(mut console) = EMERGENCY_CONSOLE.try_lock() {
        console.deactivate_live_session();
    }
}

const fn emergency_console_live_output_enabled() -> bool {
    emergency_console_output_enabled() && cfg!(rustos_debug_print_enabled)
}

const fn emergency_console_output_enabled() -> bool {
    EMERGENCY_CONSOLE_ENABLED
}

#[allow(dead_code)]
struct EmergencyConsole {
    live_enabled: bool,
    initialized: bool,
    needs_full_redraw: bool,
    render_pending: bool,
    rendered_line_count: usize,
    history_head: usize,
    history_count: usize,
    history: [[u8; EMERGENCY_LINE_CAPACITY]; EMERGENCY_HISTORY_CAPACITY],
    history_lens: [u16; EMERGENCY_HISTORY_CAPACITY],
    current_line: [u8; EMERGENCY_LINE_CAPACITY],
    current_len: usize,
}

#[allow(dead_code)]
impl EmergencyConsole {
    const fn new() -> Self {
        Self {
            live_enabled: false,
            initialized: false,
            needs_full_redraw: false,
            render_pending: false,
            rendered_line_count: 0,
            history_head: 0,
            history_count: 0,
            history: [[0; EMERGENCY_LINE_CAPACITY]; EMERGENCY_HISTORY_CAPACITY],
            history_lens: [0; EMERGENCY_HISTORY_CAPACITY],
            current_line: [0; EMERGENCY_LINE_CAPACITY],
            current_len: 0,
        }
    }

    fn start_live_session(&mut self) {
        self.clear_history();
        self.live_enabled = true;
        self.initialized = false;
        self.needs_full_redraw = true;
        self.render_pending = false;
        self.rendered_line_count = 0;
    }

    fn activate_panic_session(&mut self) {
        if !self.live_enabled {
            self.clear_history();
        }
        self.live_enabled = true;
        self.initialized = false;
        self.needs_full_redraw = true;
        self.render_pending = true;
        self.rendered_line_count = 0;
    }

    fn deactivate_live_session(&mut self) {
        self.live_enabled = false;
        self.clear_history();
        self.initialized = false;
        self.needs_full_redraw = false;
        self.render_pending = false;
        self.rendered_line_count = 0;
    }

    fn clear_history(&mut self) {
        self.history_head = 0;
        self.history_count = 0;
        self.current_len = 0;
    }

    fn framebuffer_changed(&mut self) {
        self.initialized = false;
        self.needs_full_redraw = true;
        self.render_pending = self.history_count != 0;
        self.rendered_line_count = 0;
    }

    fn append_bytes(&mut self, bytes: &[u8]) {
        for &byte in bytes {
            match byte {
                b'\r' => {}
                b'\n' => self.commit_current_line(),
                b'\t' => self.push_byte(b' '),
                b' '..=b'~' => self.push_byte(byte),
                _ => self.push_byte(b'?'),
            }
        }
    }

    fn render(&mut self) -> bool {
        if !self.live_enabled {
            return false;
        }
        if self.history_count == 0 {
            self.render_pending = false;
            return true;
        }

        let Some(rendered) = backend::try_with_framebuffer(|framebuffer| {
            let visible_capacity = self.visible_line_capacity(framebuffer);
            let can_draw_incrementally = self.initialized
                && !self.needs_full_redraw
                && self.history_count == self.rendered_line_count + 1;

            if can_draw_incrementally {
                let last_index = self.history_count - 1;
                if self.history_count <= visible_capacity {
                    self.clear_line_at(framebuffer, last_index);
                    self.draw_line_at(framebuffer, last_index, self.line_from_oldest(last_index));
                } else {
                    self.scroll_visible_lines_up(framebuffer, visible_capacity);
                    self.draw_line_at(
                        framebuffer,
                        visible_capacity - 1,
                        self.line_from_oldest(last_index),
                    );
                }
            } else {
                framebuffer.fill(Rgb888::BLACK);
                let start = self.history_count.saturating_sub(visible_capacity);
                for row in start..self.history_count {
                    self.draw_line_at(framebuffer, row - start, self.line_from_oldest(row));
                }
                self.initialized = true;
            }

            self.needs_full_redraw = false;
            self.render_pending = false;
            self.rendered_line_count = self.history_count;
            framebuffer.present_scene()
        }) else {
            self.needs_full_redraw = true;
            self.render_pending = true;
            return false;
        };

        if !rendered {
            self.needs_full_redraw = true;
            self.render_pending = true;
        }
        rendered
    }

    fn push_byte(&mut self, byte: u8) {
        if self.current_len == self.current_line.len() {
            self.commit_current_line();
        }
        self.current_line[self.current_len] = byte;
        self.current_len += 1;
    }

    fn commit_current_line(&mut self) {
        let slot = if self.history_count == self.history.len() {
            let slot = self.history_head;
            self.history_head = (self.history_head + 1) % self.history.len();
            slot
        } else {
            let slot = (self.history_head + self.history_count) % self.history.len();
            self.history_count += 1;
            slot
        };

        self.history_lens[slot] = self.current_len as u16;
        if self.current_len != 0 {
            self.history[slot][..self.current_len]
                .copy_from_slice(&self.current_line[..self.current_len]);
        }
        self.current_len = 0;
    }

    fn line_from_oldest(&self, index: usize) -> &[u8] {
        let slot = (self.history_head + index) % self.history.len();
        &self.history[slot][..self.history_lens[slot] as usize]
    }

    fn visible_line_capacity(&self, framebuffer: &Framebuffer) -> usize {
        let line_height = FONT_9X18_BOLD.character_size.height as i32;
        let usable_height = (framebuffer.height() as i32 - EMERGENCY_PADDING_Y).max(line_height);
        let rows = (usable_height / line_height).max(1) as usize;
        rows.min(self.history.len())
    }

    fn scroll_visible_lines_up(&self, framebuffer: &mut Framebuffer, visible_capacity: usize) {
        let line_height = self.line_height();
        let console_top = EMERGENCY_PADDING_Y.max(0) as usize;
        let console_height = line_height * visible_capacity;
        framebuffer.scroll_rect_up(
            0,
            console_top,
            framebuffer.width(),
            console_height,
            line_height,
            Rgb888::BLACK,
        );
    }

    fn clear_line_at(&self, framebuffer: &mut Framebuffer, row: usize) {
        let y = self.line_y(row);
        framebuffer.fill_rect(
            0,
            y as i64,
            framebuffer.width() as u32,
            self.line_height() as u32,
            Rgb888::BLACK,
            255,
        );
    }

    fn draw_line_at(&self, framebuffer: &mut Framebuffer, row: usize, line: &[u8]) {
        let y = self.line_y(row);
        let style = MonoTextStyle::new(&FONT_9X18_BOLD, Rgb888::WHITE);
        let text = unsafe { str::from_utf8_unchecked(line) };
        let _ = Text::with_baseline(
            text,
            Point::new(EMERGENCY_PADDING_X, y),
            style,
            Baseline::Top,
        )
        .draw(framebuffer);
    }

    fn line_height(&self) -> usize {
        FONT_9X18_BOLD.character_size.height as usize
    }

    fn line_y(&self, row: usize) -> i32 {
        EMERGENCY_PADDING_Y + row as i32 * self.line_height() as i32
    }
}
