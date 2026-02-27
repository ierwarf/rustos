use core::ptr;

use uefi::boot::{self, AllocateType, MemoryType};
use uefi::proto::console::gop::{FrameBuffer, GraphicsOutput, PixelFormat};

use crate::error::BootError;

pub const BOOT_INFO_MAGIC: u64 = 0x5255_5354_4F53_4749; // "RUSTOSGI"
pub const BOOT_INFO_VERSION: u32 = 1;
const PAGE_SIZE: usize = 4096;

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

pub fn prepare_boot_info() -> Result<BootInfo, BootError> {
    let handle = boot::get_handle_for_protocol::<GraphicsOutput>()
        .map_err(|err| BootError::Graphics(err.status()))?;
    let mut gop = boot::open_protocol_exclusive::<GraphicsOutput>(handle)
        .map_err(|err| BootError::Graphics(err.status()))?;

    let mode_info = gop.current_mode_info();
    if mode_info.pixel_format() == PixelFormat::BltOnly {
        return Err(BootError::GraphicsMode("BltOnly mode is not supported"));
    }

    let mut frame_buffer = gop.frame_buffer();
    let front_addr = frame_buffer.as_mut_ptr() as u64;
    let front_size = frame_buffer.size();

    let (back_addr, back_size) = allocate_back_buffer_and_seed(&mut frame_buffer, front_size);

    let fb_info = FramebufferInfo {
        addr: front_addr,
        size: front_size as u64,
        back_buffer_addr: back_addr,
        back_buffer_size: back_size,
        width: mode_info.resolution().0 as u32,
        height: mode_info.resolution().1 as u32,
        stride: mode_info.stride() as u32,
        pixel_format: map_pixel_format(mode_info.pixel_format()),
        bytes_per_pixel: 4,
        _reserved: [0; 3],
    };

    Ok(BootInfo {
        magic: BOOT_INFO_MAGIC,
        version: BOOT_INFO_VERSION,
        _reserved0: 0,
        framebuffer: fb_info,
    })
}

pub fn allocate_boot_info(boot_info: BootInfo) -> Result<*const BootInfo, BootError> {
    let ptr = boot::allocate_pages(AllocateType::AnyPages, MemoryType::LOADER_DATA, 1)
        .map_err(|err| BootError::BootInfoAlloc(err.status()))?;

    unsafe {
        ptr::write_bytes(ptr.as_ptr(), 0, PAGE_SIZE);
        let info_ptr = ptr.as_ptr().cast::<BootInfo>();
        ptr::write(info_ptr, boot_info);
        Ok(info_ptr.cast_const())
    }
}

fn allocate_back_buffer_and_seed(
    frame_buffer: &mut FrameBuffer<'_>,
    front_size: usize,
) -> (u64, u64) {
    if front_size == 0 {
        return (0, 0);
    }

    let page_count = front_size.div_ceil(PAGE_SIZE);
    let ptr =
        match boot::allocate_pages(AllocateType::AnyPages, MemoryType::LOADER_DATA, page_count) {
            Ok(ptr) => ptr,
            Err(_) => return (0, 0),
        };

    unsafe {
        ptr::copy_nonoverlapping(frame_buffer.as_mut_ptr(), ptr.as_ptr(), front_size);
    }

    (ptr.as_ptr() as u64, (page_count * PAGE_SIZE) as u64)
}

fn map_pixel_format(pixel_format: PixelFormat) -> BootPixelFormat {
    match pixel_format {
        PixelFormat::Rgb => BootPixelFormat::Rgb,
        PixelFormat::Bgr => BootPixelFormat::Bgr,
        PixelFormat::Bitmask => BootPixelFormat::Bitmask,
        PixelFormat::BltOnly => BootPixelFormat::Unknown,
    }
}
