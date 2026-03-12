use core::convert::Infallible;
use core::ptr;
use core::str;
use core::sync::atomic::{AtomicU16, Ordering};

use boot_protocol::{
    BOOT_INFO_MAGIC, BOOT_INFO_VERSION, BootInfo, BootPixelFormat, FramebufferInfo,
};
use embedded_graphics::draw_target::DrawTarget;
use embedded_graphics::geometry::{OriginDimensions, Point, Size};
use embedded_graphics::mono_font::{ascii::FONT_9X18_BOLD, MonoTextStyle};
use embedded_graphics::pixelcolor::Rgb888;
use embedded_graphics::prelude::{Pixel, RgbColor};
use embedded_graphics::text::{Baseline, Text};
use embedded_graphics::Drawable;
use spin::Mutex;
use x86_64::instructions::interrupts;

use crate::paging;

const HUGE_2MIB: u64 = 2 * 1024 * 1024;
const CONSOLE_PADDING_X: usize = 12;
const CONSOLE_PADDING_Y: usize = 12;
const CONSOLE_TAB_WIDTH: usize = 4;
const MAX_CONSOLE_COLS: usize = 240;
const MAX_CONSOLE_ROWS: usize = 128;
const MAX_CONSOLE_CELLS: usize = MAX_CONSOLE_COLS * MAX_CONSOLE_ROWS;
const CURSOR_BLINK_TOGGLE_TICKS: u16 = 512;
const CURSOR_UNDERLINE_HEIGHT: u32 = 3;

pub static GOP_SCREEN: Mutex<Framebuffer> = Mutex::new(Framebuffer {
    front_base: ptr::null_mut(),
    back_base: ptr::null_mut(),
    size: 0,
    width: 0,
    height: 0,
    stride_bytes: 0,
    bpp: 4,
    format: BootPixelFormat::Unknown,
    use_double_buffer: false,
});
static GUI_CONSOLE: Mutex<TextConsole> = Mutex::new(TextConsole::new());
static CURSOR_BLINK_TICKS: AtomicU16 = AtomicU16::new(0);

pub struct Framebuffer {
    front_base: *mut u8,
    back_base: *mut u8,
    size: usize,
    width: usize,
    height: usize,
    stride_bytes: usize,
    bpp: usize,
    format: BootPixelFormat,
    use_double_buffer: bool,
}

unsafe impl Send for Framebuffer {}

impl Framebuffer {
    fn color_bytes(&self, color: Rgb888) -> (u8, u8, u8) {
        match self.format {
            BootPixelFormat::Rgb => (color.r(), color.g(), color.b()),
            _ => (color.b(), color.g(), color.r()),
        }
    }

    fn clipped_rect(&self, x: i64, y: i64, w: u32, h: u32) -> Option<(usize, usize, usize, usize)> {
        if w == 0 || h == 0 {
            return None;
        }

        let x0 = x.max(0).min(self.width as i64) as usize;
        let y0 = y.max(0).min(self.height as i64) as usize;
        let x1 = x.saturating_add(w as i64).max(0).min(self.width as i64) as usize;
        let y1 = y.saturating_add(h as i64).max(0).min(self.height as i64) as usize;
        if x0 >= x1 || y0 >= y1 {
            return None;
        }

        Some((x0, y0, x1, y1))
    }

    pub fn fill_rect(&self, x: i64, y: i64, w: u32, h: u32, color: Rgb888, alpha: u8) {
        if alpha == 0 {
            return;
        }

        let Some((x0, y0, x1, y1)) = self.clipped_rect(x, y, w, h) else {
            return;
        };
        let cols = x1 - x0;
        let rows = y1 - y0;
        let Some(start) = y0.checked_mul(self.stride_bytes).and_then(|v| {
            x0.checked_mul(self.bpp)
                .and_then(|xoff| v.checked_add(xoff))
        }) else {
            return;
        };

        let base = self.active_buffer();
        let (c0, c1, c2) = self.color_bytes(color);

        unsafe {
            let mut row_ptr = base.add(start);
            if alpha == 255 {
                if self.bpp == 4 {
                    let px32 = u32::from_le_bytes([c0, c1, c2, 0]);
                    let aligned = (row_ptr as usize & 0x3) == 0;
                    for _ in 0..rows {
                        let mut p = row_ptr as *mut u32;
                        let mut n = cols;
                        while n >= 4 {
                            if aligned {
                                ptr::write(p, px32);
                                ptr::write(p.add(1), px32);
                                ptr::write(p.add(2), px32);
                                ptr::write(p.add(3), px32);
                            } else {
                                ptr::write_unaligned(p, px32);
                                ptr::write_unaligned(p.add(1), px32);
                                ptr::write_unaligned(p.add(2), px32);
                                ptr::write_unaligned(p.add(3), px32);
                            }
                            p = p.add(4);
                            n -= 4;
                        }
                        while n > 0 {
                            if aligned {
                                ptr::write(p, px32);
                            } else {
                                ptr::write_unaligned(p, px32);
                            }
                            p = p.add(1);
                            n -= 1;
                        }
                        row_ptr = row_ptr.add(self.stride_bytes);
                    }
                } else {
                    for _ in 0..rows {
                        let mut p = row_ptr;
                        for _ in 0..cols {
                            ptr::write(p, c0);
                            ptr::write(p.add(1), c1);
                            ptr::write(p.add(2), c2);
                            p = p.add(3);
                        }
                        row_ptr = row_ptr.add(self.stride_bytes);
                    }
                }
                return;
            }

            let a = alpha as u16;
            let inv = 256u16 - a;
            for _ in 0..rows {
                let mut p = row_ptr;
                for _ in 0..cols {
                    let d0 = ptr::read(p);
                    let d1 = ptr::read(p.add(1));
                    let d2 = ptr::read(p.add(2));
                    ptr::write(p, (((c0 as u16 * a) + (d0 as u16 * inv)) >> 8) as u8);
                    ptr::write(p.add(1), (((c1 as u16 * a) + (d1 as u16 * inv)) >> 8) as u8);
                    ptr::write(p.add(2), (((c2 as u16 * a) + (d2 as u16 * inv)) >> 8) as u8);
                    p = p.add(self.bpp);
                }
                row_ptr = row_ptr.add(self.stride_bytes);
            }
        }
    }

    pub fn fill(&self, color: Rgb888) {
        self.fill_rect(0, 0, self.width as u32, self.height as u32, color, 255);
    }

    pub fn draw_pixel(&self, x: usize, y: usize, color: Rgb888, alpha: u8) {
        if alpha == 0 || x >= self.width || y >= self.height {
            return;
        }

        let Some(idx) = y
            .checked_mul(self.stride_bytes)
            .and_then(|v| x.checked_mul(self.bpp).and_then(|xoff| v.checked_add(xoff)))
        else {
            return;
        };
        let Some(last) = idx.checked_add(self.bpp.saturating_sub(1)) else {
            return;
        };
        if last >= self.size {
            return;
        }

        let base = self.active_buffer();
        let (c0, c1, c2) = self.color_bytes(color);

        unsafe {
            if alpha == 255 {
                ptr::write(base.add(idx), c0);
                ptr::write(base.add(idx + 1), c1);
                ptr::write(base.add(idx + 2), c2);
                if self.bpp == 4 {
                    ptr::write(base.add(idx + 3), 0);
                }
                return;
            }

            let d0 = ptr::read(base.add(idx));
            let d1 = ptr::read(base.add(idx + 1));
            let d2 = ptr::read(base.add(idx + 2));
            let a = alpha as u16;
            let inv = 256u16 - a;
            ptr::write(
                base.add(idx),
                (((c0 as u16 * a) + (d0 as u16 * inv)) >> 8) as u8,
            );
            ptr::write(
                base.add(idx + 1),
                (((c1 as u16 * a) + (d1 as u16 * inv)) >> 8) as u8,
            );
            ptr::write(
                base.add(idx + 2),
                (((c2 as u16 * a) + (d2 as u16 * inv)) >> 8) as u8,
            );
            if self.bpp == 4 {
                ptr::write(base.add(idx + 3), 0);
            }
        }
    }

    fn active_buffer(&self) -> *mut u8 {
        if self.use_double_buffer {
            self.back_base
        } else {
            self.front_base
        }
    }

    pub fn refresh(&self) {
        if self.use_double_buffer {
            unsafe {
                crate::asmtools::copy_sse2(self.back_base, self.front_base, self.size);
            }
        }
    }
}

impl DrawTarget for Framebuffer {
    type Color = Rgb888;
    type Error = Infallible;

    fn draw_iter<I>(&mut self, pixels: I) -> Result<(), Self::Error>
    where
        I: IntoIterator<Item = Pixel<Self::Color>>,
    {
        for Pixel(point, color) in pixels {
            if point.x < 0 || point.y < 0 {
                continue;
            }
            self.draw_pixel(point.x as usize, point.y as usize, color, 255);
        }
        Ok(())
    }

    fn clear(&mut self, color: Self::Color) -> Result<(), Self::Error> {
        self.fill(color);
        Ok(())
    }
}

impl OriginDimensions for Framebuffer {
    fn size(&self) -> Size {
        Size::new(self.width as u32, self.height as u32)
    }
}

pub fn init_console() {
    reset_cursor_blink();
    interrupts::without_interrupts(|| {
        let mut framebuffer = GOP_SCREEN.lock();
        let mut console = GUI_CONSOLE.lock();
        console.reset(&mut framebuffer);
        framebuffer.refresh();
    });
}

pub fn write_console(bytes: &[u8]) {
    if bytes.is_empty() {
        return;
    }

    reset_cursor_blink();
    interrupts::without_interrupts(|| {
        let mut framebuffer = GOP_SCREEN.lock();
        let mut console = GUI_CONSOLE.lock();
        console.ensure_layout(&mut framebuffer);
        console.write_bytes(&mut framebuffer, bytes);
        framebuffer.refresh();
    });
}

pub fn tick_console_cursor() {
    let ticks = CURSOR_BLINK_TICKS.fetch_add(1, Ordering::Relaxed) + 1;
    if ticks < CURSOR_BLINK_TOGGLE_TICKS {
        return;
    }
    CURSOR_BLINK_TICKS.store(0, Ordering::Relaxed);

    interrupts::without_interrupts(|| {
        let mut framebuffer = GOP_SCREEN.lock();
        let mut console = GUI_CONSOLE.lock();
        console.ensure_layout(&mut framebuffer);
        if console.toggle_cursor(&mut framebuffer) {
            framebuffer.refresh();
        }
    });
}

pub fn init(boot_info_ptr: *const BootInfo) {
    let boot_info = boot_info_from_ptr(boot_info_ptr);
    let framebuffer = build_framebuffer(boot_info.framebuffer);
    mark_framebuffer_write_combine(boot_info.framebuffer);
    *GOP_SCREEN.lock() = framebuffer;
}

fn build_framebuffer(src: FramebufferInfo) -> Framebuffer {
    let width = src.width as usize;
    let height = src.height as usize;
    let stride = src.stride as usize;
    let bpp = src.bytes_per_pixel as usize;
    let size = src.size as usize;

    if src.addr == 0 || size == 0 {
        panic!("framebuffer info is empty");
    }
    if width == 0 || height == 0 || stride == 0 {
        panic!("framebuffer dimensions are invalid");
    }
    if stride < width {
        panic!("framebuffer stride is smaller than width");
    }
    if !(3..=4).contains(&bpp) {
        panic!("unsupported bytes_per_pixel");
    }

    let stride_bytes = stride
        .checked_mul(bpp)
        .expect("framebuffer geometry overflow");
    let min_size = stride_bytes
        .checked_mul(height)
        .expect("framebuffer geometry overflow");
    if min_size > size {
        panic!("framebuffer size is smaller than geometry");
    }

    let use_double_buffer = can_use_double_buffer(
        src.addr,
        src.back_buffer_addr,
        size,
        src.back_buffer_size as usize,
    );

    Framebuffer {
        front_base: src.addr as *mut u8,
        back_base: src.back_buffer_addr as *mut u8,
        size,
        width,
        height,
        stride_bytes,
        bpp,
        format: src.pixel_format,
        use_double_buffer,
    }
}

fn can_use_double_buffer(front_addr: u64, back_addr: u64, size: usize, back_size: usize) -> bool {
    if front_addr == 0 || back_addr == 0 || back_addr == front_addr || back_size < size {
        return false;
    }

    let front_start = front_addr as usize;
    let back_start = back_addr as usize;
    let Some(front_end) = front_start.checked_add(size) else {
        return false;
    };
    let Some(back_end) = back_start.checked_add(size) else {
        return false;
    };

    back_start >= front_end || front_start >= back_end
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

struct TextConsole {
    cols: usize,
    rows: usize,
    cursor_col: usize,
    cursor_row: usize,
    cells: [u8; MAX_CONSOLE_CELLS],
    cursor_visible: bool,
    initialized: bool,
}

impl TextConsole {
    const fn new() -> Self {
        Self {
            cols: 0,
            rows: 0,
            cursor_col: 0,
            cursor_row: 0,
            cells: [b' '; MAX_CONSOLE_CELLS],
            cursor_visible: true,
            initialized: false,
        }
    }

    fn ensure_layout(&mut self, framebuffer: &mut Framebuffer) {
        let cols = console_cols(framebuffer);
        let rows = console_rows(framebuffer);
        if !self.initialized || self.cols != cols || self.rows != rows {
            self.reset(framebuffer);
        }
    }

    fn reset(&mut self, framebuffer: &mut Framebuffer) {
        self.cols = console_cols(framebuffer);
        self.rows = console_rows(framebuffer);
        self.cursor_col = 0;
        self.cursor_row = 0;
        self.cursor_visible = true;
        self.initialized = true;
        self.clear_cells();
        self.redraw_full(framebuffer);
    }

    fn write_bytes(&mut self, framebuffer: &mut Framebuffer, bytes: &[u8]) {
        if self.cursor_visible && self.cols != 0 && self.rows != 0 {
            self.draw_cell(framebuffer, self.cursor_row, self.cursor_col);
        }
        self.cursor_visible = false;

        for &byte in bytes {
            self.write_byte(framebuffer, byte);
        }

        self.cursor_visible = true;
        self.draw_cursor(framebuffer);
    }

    fn write_byte(&mut self, framebuffer: &mut Framebuffer, byte: u8) {
        match byte {
            b'\r' => self.cursor_col = 0,
            b'\n' => self.new_line(framebuffer),
            0x08 => self.backspace(framebuffer),
            b'\t' => {
                let spaces = CONSOLE_TAB_WIDTH - (self.cursor_col % CONSOLE_TAB_WIDTH);
                for _ in 0..spaces {
                    self.put_char(framebuffer, b' ');
                }
            }
            0x20..=0x7e => self.put_char(framebuffer, byte),
            _ => {}
        }
    }

    fn put_char(&mut self, framebuffer: &mut Framebuffer, byte: u8) {
        if self.cols == 0 || self.rows == 0 {
            return;
        }
        if self.cursor_col >= self.cols {
            self.new_line(framebuffer);
        }

        self.set_cell(self.cursor_row, self.cursor_col, byte);
        self.draw_cell(framebuffer, self.cursor_row, self.cursor_col);
        self.cursor_col += 1;
        if self.cursor_col >= self.cols {
            self.new_line(framebuffer);
        }
    }

    fn backspace(&mut self, framebuffer: &mut Framebuffer) {
        if self.cursor_col == 0 {
            return;
        }

        self.cursor_col -= 1;
        self.set_cell(self.cursor_row, self.cursor_col, b' ');
        self.draw_cell(framebuffer, self.cursor_row, self.cursor_col);
    }

    fn new_line(&mut self, framebuffer: &mut Framebuffer) {
        self.cursor_col = 0;
        if self.rows == 0 {
            return;
        }
        if self.cursor_row + 1 < self.rows {
            self.cursor_row += 1;
            return;
        }

        self.scroll_up();
        self.redraw_full(framebuffer);
    }

    fn scroll_up(&mut self) {
        if self.rows <= 1 || self.cols == 0 {
            return;
        }

        let row_width = self.cols;
        for row in 1..self.rows {
            let src = row * row_width;
            let dst = (row - 1) * row_width;
            let end = src + row_width;
            self.cells.copy_within(src..end, dst);
        }

        let last_row_start = (self.rows - 1) * row_width;
        for cell in &mut self.cells[last_row_start..last_row_start + row_width] {
            *cell = b' ';
        }
    }

    fn redraw_full(&self, framebuffer: &mut Framebuffer) {
        framebuffer.fill(console_background());
        for row in 0..self.rows {
            for col in 0..self.cols {
                self.draw_cell(framebuffer, row, col);
            }
        }
        if self.cursor_visible {
            self.draw_cursor(framebuffer);
        }
    }

    fn draw_cell(&self, framebuffer: &mut Framebuffer, row: usize, col: usize) {
        let (x, y) = cell_origin(row, col);
        framebuffer.fill_rect(
            x as i64,
            y as i64,
            FONT_9X18_BOLD.character_size.width,
            FONT_9X18_BOLD.character_size.height,
            console_background(),
            255,
        );

        let byte = self.cell(row, col);
        if byte == b' ' {
            return;
        }

        let glyph = [byte];
        let style = MonoTextStyle::new(&FONT_9X18_BOLD, console_foreground());
        let text = unsafe { str::from_utf8_unchecked(&glyph) };
        let _ = Text::with_baseline(
            text,
            Point::new(x as i32, y as i32),
            style,
            Baseline::Top,
        )
        .draw(framebuffer);
    }

    fn toggle_cursor(&mut self, framebuffer: &mut Framebuffer) -> bool {
        if !self.initialized || self.cols == 0 || self.rows == 0 {
            return false;
        }

        if self.cursor_visible {
            self.cursor_visible = false;
            self.draw_cell(framebuffer, self.cursor_row, self.cursor_col);
        } else {
            self.cursor_visible = true;
            self.draw_cursor(framebuffer);
        }

        true
    }

    fn draw_cursor(&self, framebuffer: &mut Framebuffer) {
        if !self.cursor_visible || self.cols == 0 || self.rows == 0 {
            return;
        }

        let (x, y) = cell_origin(self.cursor_row, self.cursor_col);
        let cell_width = FONT_9X18_BOLD.character_size.width;
        let cell_height = FONT_9X18_BOLD.character_size.height;
        let underline_height = CURSOR_UNDERLINE_HEIGHT.min(cell_height);
        let underline_y = y + cell_height as usize - underline_height as usize;
        framebuffer.fill_rect(
            x as i64,
            underline_y as i64,
            cell_width,
            underline_height,
            console_cursor_color(),
            255,
        );
    }

    fn clear_cells(&mut self) {
        let visible = self.cols * self.rows;
        for cell in &mut self.cells[..visible] {
            *cell = b' ';
        }
    }

    fn cell(&self, row: usize, col: usize) -> u8 {
        self.cells[row * self.cols + col]
    }

    fn set_cell(&mut self, row: usize, col: usize, byte: u8) {
        self.cells[row * self.cols + col] = byte;
    }
}

fn console_cols(framebuffer: &Framebuffer) -> usize {
    let usable_width = framebuffer.width.saturating_sub(CONSOLE_PADDING_X * 2);
    let cell_width = FONT_9X18_BOLD.character_size.width as usize;
    usable_width
        .checked_div(cell_width)
        .unwrap_or(0)
        .clamp(1, MAX_CONSOLE_COLS)
}

fn console_rows(framebuffer: &Framebuffer) -> usize {
    let usable_height = framebuffer.height.saturating_sub(CONSOLE_PADDING_Y * 2);
    let cell_height = FONT_9X18_BOLD.character_size.height as usize;
    usable_height
        .checked_div(cell_height)
        .unwrap_or(0)
        .clamp(1, MAX_CONSOLE_ROWS)
}

fn cell_origin(row: usize, col: usize) -> (usize, usize) {
    (
        CONSOLE_PADDING_X + col * FONT_9X18_BOLD.character_size.width as usize,
        CONSOLE_PADDING_Y + row * FONT_9X18_BOLD.character_size.height as usize,
    )
}

fn reset_cursor_blink() {
    CURSOR_BLINK_TICKS.store(0, Ordering::Relaxed);
}

fn console_background() -> Rgb888 {
    Rgb888::new(0, 0, 0)
}

fn console_foreground() -> Rgb888 {
    Rgb888::new(232, 236, 239)
}

fn console_cursor_color() -> Rgb888 {
    Rgb888::new(255, 255, 255)
}
