#![no_std]

pub const BOOT_INFO_MAGIC: u64 = 0x5255_5354_4F53_4749; // "RUSTOSGI"
pub const BOOT_INFO_VERSION: u32 = 7;
pub const BOOT_FILE_MANIFEST_TRUNCATED: u32 = 1 << 0;

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
pub struct BootFileEntry {
    pub path_ptr: u64,
    pub path_len: u32,
    pub _reserved0: u32,
    pub data_ptr: u64,
    pub data_len: u64,
}

#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct BootFileManifest {
    pub entries_ptr: u64,
    pub entry_count: u32,
    pub flags: u32,
    pub total_bytes: u64,
}

impl BootFileManifest {
    pub const fn empty() -> Self {
        Self {
            entries_ptr: 0,
            entry_count: 0,
            flags: 0,
            total_bytes: 0,
        }
    }
}

#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct BootInfo {
    pub magic: u64,
    pub version: u32,
    pub _reserved0: u32,
    pub rng_seed: [u8; 32],
    pub acpi_rsdp_addr: u64,
    pub framebuffer: FramebufferInfo,
    pub boot_files: BootFileManifest,
}
