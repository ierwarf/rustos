use boot_protocol::{BootInfo, BootPixelFormat, FramebufferInfo};
use core::fmt::{self, Write};
use core::ptr;
use core::str;
use core::sync::atomic::{AtomicPtr, Ordering};
use diag_abi::{DiagLevel, DiagProvider, DiagRecord, DiagSharedBufferHeader, DiagStage};

use embedded_graphics::Drawable;
use embedded_graphics::draw_target::DrawTarget;
use embedded_graphics::geometry::{OriginDimensions, Point, Size};
use embedded_graphics::mono_font::{MonoTextStyle, ascii::FONT_9X18_BOLD};
use embedded_graphics::pixelcolor::Rgb888;
use embedded_graphics::prelude::RgbColor;
use embedded_graphics::text::{Baseline, Text};
use spin::Mutex;

static BOOT_INFO_PTR: AtomicPtr<BootInfo> = AtomicPtr::new(ptr::null_mut());
static TRACE_CONSOLE: Mutex<TraceConsole> = Mutex::new(TraceConsole::new());

const PADDING_X: i32 = 12;
const PADDING_Y: i32 = 12;
const LINE_CAPACITY: usize = 240;

pub fn init(boot_info_ptr: *const BootInfo) {
    BOOT_INFO_PTR.store(boot_info_ptr.cast_mut(), Ordering::Release);
}

#[cfg(rustos_boot_trace_enabled)]
pub fn println_fmt(args: fmt::Arguments<'_>) {
    let mut line = LineBuffer::new();
    let _ = line.write_fmt(args);
    record_boot_line(line.as_str());
    super::write_bytes(line.as_str().as_bytes());
    super::println_newline();

    if let Some(mut framebuffer) = framebuffer_from_boot_info() {
        let mut console = TRACE_CONSOLE.lock();
        console.ensure_initialized(&mut framebuffer);
        console.write_line(&mut framebuffer, line.as_str());
    }
}

fn framebuffer_from_boot_info() -> Option<Framebuffer> {
    let boot_info_ptr = BOOT_INFO_PTR.load(Ordering::Acquire);
    let boot_info = unsafe { BootInfo::from_ptr(boot_info_ptr.cast_const()) }.ok()?;
    Framebuffer::from_info(boot_info.framebuffer)
}

struct Framebuffer {
    base: *mut u8,
    size: usize,
    width: usize,
    height: usize,
    stride_bytes: usize,
    bpp: usize,
    format: BootPixelFormat,
}

impl Framebuffer {
    fn from_info(info: FramebufferInfo) -> Option<Self> {
        info.validate().ok()?;
        let base = info.addr as *mut u8;
        let size = info.size as usize;
        let width = info.width as usize;
        let height = info.height as usize;
        let stride = info.stride as usize;
        let bpp = info.bytes_per_pixel as usize;

        if base.is_null() || size == 0 || width == 0 || height == 0 || stride < width {
            return None;
        }
        if !(3..=4).contains(&bpp) {
            return None;
        }

        let stride_bytes = stride.checked_mul(bpp)?;
        let min_size = stride_bytes.checked_mul(height)?;
        if min_size > size {
            return None;
        }

        Some(Self {
            base,
            size,
            width,
            height,
            stride_bytes,
            bpp,
            format: info.pixel_format,
        })
    }

    fn color_bytes(&self, color: Rgb888) -> (u8, u8, u8) {
        match self.format {
            BootPixelFormat::Rgb => (color.r(), color.g(), color.b()),
            _ => (color.b(), color.g(), color.r()),
        }
    }

    fn fill(&mut self, color: Rgb888) {
        let (c0, c1, c2) = self.color_bytes(color);
        for y in 0..self.height {
            let row = y * self.stride_bytes;
            for x in 0..self.width {
                let idx = row + x * self.bpp;
                unsafe {
                    ptr::write(self.base.add(idx), c0);
                    ptr::write(self.base.add(idx + 1), c1);
                    ptr::write(self.base.add(idx + 2), c2);
                    if self.bpp == 4 {
                        ptr::write(self.base.add(idx + 3), 0);
                    }
                }
            }
        }
    }

    fn draw_pixel_unchecked(&mut self, x: usize, y: usize, color: Rgb888) {
        let idx = y * self.stride_bytes + x * self.bpp;
        if idx + self.bpp > self.size {
            return;
        }

        let (c0, c1, c2) = self.color_bytes(color);
        unsafe {
            ptr::write(self.base.add(idx), c0);
            ptr::write(self.base.add(idx + 1), c1);
            ptr::write(self.base.add(idx + 2), c2);
            if self.bpp == 4 {
                ptr::write(self.base.add(idx + 3), 0);
            }
        }
    }
}

#[cfg(not(rustos_boot_trace_enabled))]
pub fn println_fmt(_args: fmt::Arguments<'_>) {}

#[cfg(rustos_boot_trace_enabled)]
fn record_boot_line(line: &str) {
    let boot_info_ptr = BOOT_INFO_PTR.load(Ordering::Acquire);
    let Ok(boot_info) = (unsafe { BootInfo::from_ptr(boot_info_ptr.cast_const()) }) else {
        return;
    };
    if boot_info.boot_diag.addr == 0 || boot_info.boot_diag.record_capacity == 0 {
        return;
    }

    unsafe {
        let header = &mut *(boot_info.boot_diag.addr as *mut DiagSharedBufferHeader);
        if header.magic != diag_abi::DIAG_BUFFER_MAGIC {
            *header = DiagSharedBufferHeader::empty(boot_info.boot_diag.record_capacity as u16);
        }
        let capacity = usize::from(header.record_capacity);
        if capacity == 0 {
            return;
        }
        let records_base = (boot_info.boot_diag.addr as usize
            + core::mem::size_of::<DiagSharedBufferHeader>())
            as *mut DiagRecord;
        let sequence = header.next_sequence;
        let slot = (sequence as usize) % capacity;
        let record = &mut *records_base.add(slot);
        *record = DiagRecord::empty();
        record.header.stage = DiagStage::KernelBoot as u8;
        record.header.level = DiagLevel::Info as u8;
        record.header.provider = DiagProvider::Boot as u16;
        record.header.sequence = sequence;
        record.set_payload_bytes(line.as_bytes());
        header.next_sequence = header.next_sequence.wrapping_add(1);
    }
}

#[cfg(not(rustos_boot_trace_enabled))]
fn record_boot_line(_line: &str) {}

impl DrawTarget for Framebuffer {
    type Color = Rgb888;
    type Error = core::convert::Infallible;

    fn draw_iter<I>(&mut self, pixels: I) -> Result<(), Self::Error>
    where
        I: IntoIterator<Item = embedded_graphics::Pixel<Self::Color>>,
    {
        for embedded_graphics::Pixel(point, color) in pixels {
            if point.x < 0 || point.y < 0 {
                continue;
            }
            let x = point.x as usize;
            let y = point.y as usize;
            if x >= self.width || y >= self.height {
                continue;
            }
            self.draw_pixel_unchecked(x, y, color);
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

struct TraceConsole {
    next_row: usize,
    initialized: bool,
}

impl TraceConsole {
    const fn new() -> Self {
        Self {
            next_row: 0,
            initialized: false,
        }
    }

    fn reset(&mut self, framebuffer: &mut Framebuffer) {
        framebuffer.fill(Rgb888::BLACK);
        self.next_row = 0;
        self.initialized = true;
    }

    fn ensure_initialized(&mut self, framebuffer: &mut Framebuffer) {
        if !self.initialized {
            self.reset(framebuffer);
        }
    }

    fn write_line(&mut self, framebuffer: &mut Framebuffer, line: &str) {
        let line_height = FONT_9X18_BOLD.character_size.height as i32;
        let y = PADDING_Y + self.next_row as i32 * line_height;
        if y + line_height > framebuffer.height as i32 {
            self.reset(framebuffer);
        }

        let y = PADDING_Y + self.next_row as i32 * line_height;
        let style = MonoTextStyle::new(&FONT_9X18_BOLD, Rgb888::WHITE);
        let _ = Text::with_baseline(line, Point::new(PADDING_X, y), style, Baseline::Top)
            .draw(framebuffer);
        self.next_row += 1;
    }
}

struct LineBuffer {
    bytes: [u8; LINE_CAPACITY],
    len: usize,
}

impl LineBuffer {
    const fn new() -> Self {
        Self {
            bytes: [0; LINE_CAPACITY],
            len: 0,
        }
    }

    fn push_byte(&mut self, byte: u8) {
        if self.len < self.bytes.len() {
            self.bytes[self.len] = byte;
            self.len += 1;
        }
    }

    fn as_str(&self) -> &str {
        unsafe { str::from_utf8_unchecked(&self.bytes[..self.len]) }
    }
}

impl Write for LineBuffer {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        for ch in s.chars() {
            match ch {
                '\n' | '\r' | '\t' => self.push_byte(b' '),
                ' '..='~' => self.push_byte(ch as u8),
                _ => self.push_byte(b'?'),
            }
        }
        Ok(())
    }
}
