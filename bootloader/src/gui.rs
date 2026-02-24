use core::ptr;

use uefi::boot::{self, AllocateType, MemoryType};
use uefi::proto::console::gop::{FrameBuffer, GraphicsOutput, PixelFormat};

use crate::error::BootError;

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

impl BootInfo {
    fn new(framebuffer: FramebufferInfo) -> Self {
        Self {
            magic: BOOT_INFO_MAGIC,
            version: BOOT_INFO_VERSION,
            _reserved0: 0,
            framebuffer,
        }
    }
}

pub fn prepare_boot_info() -> Result<BootInfo, BootError> {
    let handle =
        boot::get_handle_for_protocol::<GraphicsOutput>().map_err(|err| BootError::Graphics(err.status()))?;
    let mut gop = boot::open_protocol_exclusive::<GraphicsOutput>(handle)
        .map_err(|err| BootError::Graphics(err.status()))?;

    let mode_info = gop.current_mode_info();
    if mode_info.pixel_format() == PixelFormat::BltOnly {
        return Err(BootError::GraphicsMode("BltOnly mode is not supported"));
    }

    let mut frame_buffer = gop.frame_buffer();
    let front_addr = frame_buffer.as_mut_ptr() as u64;
    let front_size_usize = frame_buffer.size();
    let front_size = front_size_usize as u64;

    let (back_addr, back_size) = allocate_back_buffer_and_seed(&mut frame_buffer, front_size_usize);

    let fb_info = FramebufferInfo {
        addr: front_addr,
        size: front_size,
        back_buffer_addr: back_addr,
        back_buffer_size: back_size,
        width: mode_info.resolution().0 as u32,
        height: mode_info.resolution().1 as u32,
        stride: mode_info.stride() as u32,
        pixel_format: map_pixel_format(mode_info.pixel_format()),
        bytes_per_pixel: 4,
        _reserved: [0; 3],
    };

    draw_boot_banner(&mut frame_buffer, &fb_info);

    Ok(BootInfo::new(fb_info))
}

pub fn allocate_boot_info(boot_info: BootInfo) -> Result<*const BootInfo, BootError> {
    let ptr = boot::allocate_pages(AllocateType::AnyPages, MemoryType::LOADER_DATA, 1)
        .map_err(|err| BootError::BootInfoAlloc(err.status()))?;

    unsafe {
        ptr::write_bytes(ptr.as_ptr(), 0, 4096);
        let info_ptr = ptr.as_ptr().cast::<BootInfo>();
        ptr::write(info_ptr, boot_info);
        Ok(info_ptr.cast_const())
    }
}

fn allocate_back_buffer_and_seed(frame_buffer: &mut FrameBuffer<'_>, front_size: usize) -> (u64, u64) {
    if front_size == 0 {
        return (0, 0);
    }

    let page_count = front_size.div_ceil(4096);
    let ptr = match boot::allocate_pages(AllocateType::AnyPages, MemoryType::LOADER_DATA, page_count) {
        Ok(ptr) => ptr,
        Err(_) => return (0, 0),
    };

    unsafe {
        ptr::copy_nonoverlapping(frame_buffer.as_mut_ptr(), ptr.as_ptr(), front_size);
    }

    (ptr.as_ptr() as u64, (page_count * 4096) as u64)
}

fn map_pixel_format(pixel_format: PixelFormat) -> BootPixelFormat {
    match pixel_format {
        PixelFormat::Rgb => BootPixelFormat::Rgb,
        PixelFormat::Bgr => BootPixelFormat::Bgr,
        PixelFormat::Bitmask => BootPixelFormat::Bitmask,
        PixelFormat::BltOnly => BootPixelFormat::Unknown,
    }
}

fn draw_boot_banner(frame_buffer: &mut FrameBuffer<'_>, fb: &FramebufferInfo) {
    let width = fb.width as usize;
    let height = fb.height as usize;

    fill_rect(frame_buffer, fb, 0, 0, width, height, (0x12, 0x1a, 0x26));

    let top_h = (height / 8).max(36);
    fill_rect(frame_buffer, fb, 0, 0, width, top_h, (0x26, 0x42, 0x58));

    let card_w = (width / 3).max(120);
    let card_h = (height / 4).max(80);
    let card_x = (width.saturating_sub(card_w)) / 2;
    let card_y = (height.saturating_sub(card_h)) / 2;
    fill_rect(frame_buffer, fb, card_x, card_y, card_w, card_h, (0x39, 0x74, 0x9a));

    let strip_h = (card_h / 6).max(6);
    fill_rect(
        frame_buffer,
        fb,
        card_x + 8,
        card_y + 8,
        card_w.saturating_sub(16),
        strip_h,
        (0xa7, 0xd1, 0xe9),
    );
}

fn fill_rect(
    frame_buffer: &mut FrameBuffer<'_>,
    fb: &FramebufferInfo,
    x: usize,
    y: usize,
    w: usize,
    h: usize,
    color: (u8, u8, u8),
) {
    if w == 0 || h == 0 {
        return;
    }

    let max_x = (x + w).min(fb.width as usize);
    let max_y = (y + h).min(fb.height as usize);

    for py in y..max_y {
        for px in x..max_x {
            write_pixel(frame_buffer, fb, px, py, color);
        }
    }
}

fn write_pixel(frame_buffer: &mut FrameBuffer<'_>, fb: &FramebufferInfo, x: usize, y: usize, color: (u8, u8, u8)) {
    let bpp = fb.bytes_per_pixel as usize;
    if bpp < 3 {
        return;
    }

    let idx = (y * fb.stride as usize + x) * bpp;
    if idx + (bpp - 1) >= fb.size as usize {
        return;
    }

    let (r, g, b) = color;
    let bytes = match fb.pixel_format {
        BootPixelFormat::Rgb => [r, g, b, 0],
        _ => [b, g, r, 0],
    };

    unsafe {
        frame_buffer.write_byte(idx, bytes[0]);
        frame_buffer.write_byte(idx + 1, bytes[1]);
        frame_buffer.write_byte(idx + 2, bytes[2]);
        if bpp > 3 {
            frame_buffer.write_byte(idx + 3, bytes[3]);
        }
    }
}
