use core::ptr;
use spin::Mutex;

pub const BOOT_INFO_MAGIC: u64 = 0x5255_5354_4F53_4749; // "RUSTOSGI"
pub const BOOT_INFO_VERSION: u32 = 1;

#[repr(u32)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BootPixelFormat {
    Rgb = 0,
    Bgr = 1,
    Bitmask = 2,
    Unknown = 0xff,
}

#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct FramebufferInfo {
    pub addr: u64,
    pub size: u64,
    pub back_buffer_addr: u64,
    pub back_buffer_size: u64,
    pub width: u32,
    pub height: u32,
    pub stride: u32,
    pub pixel_format: BootPixelFormat,
    pub bytes_per_pixel: u8,
    pub _reserved: [u8; 3],
}

#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct BootInfo {
    pub magic: u64,
    pub version: u32,
    pub _reserved0: u32,
    pub framebuffer: FramebufferInfo,
}

struct FramebufferState {
    front_base: *mut u8,
    size: usize,
    width: usize,
    height: usize,
    stride: usize,
    format: BootPixelFormat,
    bpp: usize,
    back_base: *mut u8,
    back_size: usize,
    use_double_buffer: bool,
}

unsafe impl Send for FramebufferState {}

static FRAMEBUFFER: Mutex<Option<FramebufferState>> = Mutex::new(None);

pub fn init(boot_info_ptr: *const BootInfo) -> Result<(), &'static str> {
    if boot_info_ptr.is_null() {
        return Err("boot info pointer is null");
    }

    let boot_info = unsafe { &*boot_info_ptr };
    if boot_info.magic != BOOT_INFO_MAGIC {
        return Err("boot info magic mismatch");
    }
    if boot_info.version != BOOT_INFO_VERSION {
        return Err("boot info version mismatch");
    }

    let fb = boot_info.framebuffer;
    if fb.addr == 0 || fb.size == 0 {
        return Err("framebuffer info is empty");
    }
    if fb.width == 0 || fb.height == 0 || fb.stride == 0 {
        return Err("framebuffer dimensions are invalid");
    }
    if fb.bytes_per_pixel < 3 {
        return Err("unsupported bytes_per_pixel (<3)");
    }

    let mut framebuffer = FRAMEBUFFER.lock();
    *framebuffer = Some(FramebufferState {
        front_base: fb.addr as *mut u8,
        size: fb.size as usize,
        width: fb.width as usize,
        height: fb.height as usize,
        stride: fb.stride as usize,
        format: fb.pixel_format,
        bpp: fb.bytes_per_pixel as usize,
        back_base: fb.back_buffer_addr as *mut u8,
        back_size: fb.back_buffer_size as usize,
        use_double_buffer: false,
    });

    Ok(())
}

pub fn enable_double_buffer() -> Result<(), &'static str> {
    let mut guard = FRAMEBUFFER.lock();
    let Some(fb) = guard.as_mut() else {
        return Err("framebuffer not initialized");
    };

    if fb.back_base.is_null() || fb.back_size < fb.size {
        return Err("back buffer not available");
    }

    fb.use_double_buffer = true;
    Ok(())
}

pub fn is_double_buffer_enabled() -> bool {
    FRAMEBUFFER
        .lock()
        .as_ref()
        .is_some_and(|fb| fb.use_double_buffer)
}

#[allow(dead_code)]
pub fn present() {
    let mut guard = FRAMEBUFFER.lock();
    let Some(fb) = guard.as_mut() else {
        return;
    };
    present_locked(fb);
}

pub fn render_boot_screen() {
    let mut guard = FRAMEBUFFER.lock();
    let Some(fb) = guard.as_mut() else {
        return;
    };

    clear_locked(fb, (0x0f, 0x14, 0x1f));

    let header_h = (fb.height / 9).max(32);
    fill_rect_locked(fb, 0, 0, fb.width, header_h, (0x1f, 0x3b, 0x54));

    let panel_w = (fb.width / 2).max(220);
    let panel_h = (fb.height / 3).max(140);
    let panel_x = (fb.width.saturating_sub(panel_w)) / 2;
    let panel_y = (fb.height.saturating_sub(panel_h)) / 2;
    fill_rect_locked(fb, panel_x, panel_y, panel_w, panel_h, (0x2e, 0x6e, 0x97));

    let bar_h = (panel_h / 7).max(8);
    fill_rect_locked(
        fb,
        panel_x + 10,
        panel_y + 10,
        panel_w.saturating_sub(20),
        bar_h,
        (0x9b, 0xca, 0xe4),
    );

    let cards_y = panel_y + panel_h.saturating_sub(40);
    for i in 0..3 {
        let card_w = (panel_w / 4).max(24);
        let gap = (panel_w / 12).max(8);
        let card_x = panel_x + gap + i * (card_w + gap);
        fill_rect_locked(fb, card_x, cards_y, card_w, 24, (0x4a, 0x93, 0xbe));
    }

    let white = (0xf2, 0xf7, 0xfc);
    let accent = (0xd9, 0xf0, 0xff);

    draw_text_locked(fb, panel_x + 16, panel_y + 20, "RUST OS", white);
    draw_text_locked(fb, panel_x + 16, panel_y + 36, "KERNEL GUI READY", accent);

    let db_text = if fb.use_double_buffer {
        "DOUBLE BUFFER ON"
    } else {
        "DOUBLE BUFFER OFF"
    };
    draw_text_locked(fb, panel_x + 16, panel_y + 52, db_text, white);

    present_locked(fb);
}

fn present_locked(fb: &mut FramebufferState) {
    if fb.use_double_buffer {
        unsafe {
            ptr::copy_nonoverlapping(fb.back_base, fb.front_base, fb.size);
        }
    }
}

fn clear_locked(fb: &mut FramebufferState, color: (u8, u8, u8)) {
    fill_rect_locked(fb, 0, 0, fb.width, fb.height, color);
}

fn fill_rect_locked(
    fb: &mut FramebufferState,
    x: usize,
    y: usize,
    w: usize,
    h: usize,
    color: (u8, u8, u8),
) {
    if w == 0 || h == 0 {
        return;
    }

    let max_x = (x + w).min(fb.width);
    let max_y = (y + h).min(fb.height);

    for py in y..max_y {
        for px in x..max_x {
            write_pixel_locked(fb, px, py, color);
        }
    }
}

fn draw_text_locked(
    fb: &mut FramebufferState,
    x: usize,
    y: usize,
    text: &str,
    color: (u8, u8, u8),
) {
    let mut cursor_x = x;
    let mut cursor_y = y;

    for ch in text.chars() {
        if ch == '\n' {
            cursor_x = x;
            cursor_y = cursor_y.saturating_add(font::HEIGHT + 2);
            continue;
        }

        draw_char_locked(fb, cursor_x, cursor_y, ch, color);
        cursor_x = cursor_x.saturating_add(font::WIDTH + 1);
    }
}

fn draw_char_locked(fb: &mut FramebufferState, x: usize, y: usize, ch: char, color: (u8, u8, u8)) {
    let glyph = font::glyph(ch);

    for (row, bits) in glyph.iter().enumerate() {
        for col in 0..font::WIDTH {
            if bits & (1 << (7 - col)) != 0 {
                write_pixel_locked(fb, x + col, y + row, color);
            }
        }
    }
}

fn write_pixel_locked(fb: &mut FramebufferState, x: usize, y: usize, color: (u8, u8, u8)) {
    if x >= fb.width || y >= fb.height {
        return;
    }

    let idx = (y * fb.stride + x) * fb.bpp;
    if idx + (fb.bpp - 1) >= fb.size {
        return;
    }

    let dst_base = if fb.use_double_buffer {
        fb.back_base
    } else {
        fb.front_base
    };

    let (r, g, b) = color;
    let bytes = match fb.format {
        BootPixelFormat::Rgb => [r, g, b, 0],
        _ => [b, g, r, 0],
    };

    unsafe {
        ptr::write_volatile(dst_base.add(idx), bytes[0]);
        ptr::write_volatile(dst_base.add(idx + 1), bytes[1]);
        ptr::write_volatile(dst_base.add(idx + 2), bytes[2]);
        if fb.bpp > 3 {
            ptr::write_volatile(dst_base.add(idx + 3), bytes[3]);
        }
    }
}

mod font {
    pub const WIDTH: usize = 8;
    pub const HEIGHT: usize = 8;

    pub fn glyph(ch: char) -> [u8; 8] {
        let c = if ch.is_ascii_lowercase() {
            ch.to_ascii_uppercase()
        } else {
            ch
        };

        match c {
            'A' => [0x18, 0x24, 0x42, 0x7e, 0x42, 0x42, 0x42, 0x00],
            'B' => [0x7c, 0x42, 0x42, 0x7c, 0x42, 0x42, 0x7c, 0x00],
            'C' => [0x3c, 0x42, 0x40, 0x40, 0x40, 0x42, 0x3c, 0x00],
            'D' => [0x78, 0x44, 0x42, 0x42, 0x42, 0x44, 0x78, 0x00],
            'E' => [0x7e, 0x40, 0x40, 0x7c, 0x40, 0x40, 0x7e, 0x00],
            'F' => [0x7e, 0x40, 0x40, 0x7c, 0x40, 0x40, 0x40, 0x00],
            'G' => [0x3c, 0x42, 0x40, 0x4e, 0x42, 0x42, 0x3c, 0x00],
            'H' => [0x42, 0x42, 0x42, 0x7e, 0x42, 0x42, 0x42, 0x00],
            'I' => [0x3e, 0x08, 0x08, 0x08, 0x08, 0x08, 0x3e, 0x00],
            'J' => [0x1e, 0x04, 0x04, 0x04, 0x44, 0x44, 0x38, 0x00],
            'K' => [0x42, 0x44, 0x48, 0x70, 0x48, 0x44, 0x42, 0x00],
            'L' => [0x40, 0x40, 0x40, 0x40, 0x40, 0x40, 0x7e, 0x00],
            'M' => [0x42, 0x66, 0x5a, 0x5a, 0x42, 0x42, 0x42, 0x00],
            'N' => [0x42, 0x62, 0x52, 0x4a, 0x46, 0x42, 0x42, 0x00],
            'O' => [0x3c, 0x42, 0x42, 0x42, 0x42, 0x42, 0x3c, 0x00],
            'P' => [0x7c, 0x42, 0x42, 0x7c, 0x40, 0x40, 0x40, 0x00],
            'Q' => [0x3c, 0x42, 0x42, 0x42, 0x4a, 0x44, 0x3a, 0x00],
            'R' => [0x7c, 0x42, 0x42, 0x7c, 0x48, 0x44, 0x42, 0x00],
            'S' => [0x3c, 0x42, 0x40, 0x3c, 0x02, 0x42, 0x3c, 0x00],
            'T' => [0x7f, 0x08, 0x08, 0x08, 0x08, 0x08, 0x08, 0x00],
            'U' => [0x42, 0x42, 0x42, 0x42, 0x42, 0x42, 0x3c, 0x00],
            'V' => [0x42, 0x42, 0x42, 0x42, 0x42, 0x24, 0x18, 0x00],
            'W' => [0x42, 0x42, 0x42, 0x5a, 0x5a, 0x66, 0x42, 0x00],
            'X' => [0x42, 0x42, 0x24, 0x18, 0x24, 0x42, 0x42, 0x00],
            'Y' => [0x42, 0x42, 0x24, 0x18, 0x08, 0x08, 0x08, 0x00],
            'Z' => [0x7e, 0x02, 0x04, 0x18, 0x20, 0x40, 0x7e, 0x00],
            '0' => [0x3c, 0x42, 0x46, 0x4a, 0x52, 0x62, 0x3c, 0x00],
            '1' => [0x08, 0x18, 0x28, 0x08, 0x08, 0x08, 0x3e, 0x00],
            '2' => [0x3c, 0x42, 0x02, 0x0c, 0x30, 0x40, 0x7e, 0x00],
            '3' => [0x3c, 0x42, 0x02, 0x1c, 0x02, 0x42, 0x3c, 0x00],
            '4' => [0x0c, 0x14, 0x24, 0x44, 0x7e, 0x04, 0x04, 0x00],
            '5' => [0x7e, 0x40, 0x7c, 0x02, 0x02, 0x42, 0x3c, 0x00],
            '6' => [0x1c, 0x20, 0x40, 0x7c, 0x42, 0x42, 0x3c, 0x00],
            '7' => [0x7e, 0x42, 0x04, 0x08, 0x10, 0x10, 0x10, 0x00],
            '8' => [0x3c, 0x42, 0x42, 0x3c, 0x42, 0x42, 0x3c, 0x00],
            '9' => [0x3c, 0x42, 0x42, 0x3e, 0x02, 0x04, 0x38, 0x00],
            ' ' => [0x00; 8],
            '-' => [0x00, 0x00, 0x00, 0x7e, 0x00, 0x00, 0x00, 0x00],
            ':' => [0x00, 0x18, 0x18, 0x00, 0x18, 0x18, 0x00, 0x00],
            '.' => [0x00, 0x00, 0x00, 0x00, 0x00, 0x18, 0x18, 0x00],
            '/' => [0x02, 0x04, 0x08, 0x10, 0x20, 0x40, 0x00, 0x00],
            '_' => [0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x7e],
            _ => [0x3c, 0x42, 0x02, 0x0c, 0x10, 0x00, 0x10, 0x00], // '?'
        }
    }
}
