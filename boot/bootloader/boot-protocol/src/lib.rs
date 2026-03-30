#![no_std]

pub const BOOT_INFO_MAGIC: u64 = 0x5255_5354_4F53_4749; // "RUSTOSGI"
pub const BOOT_INFO_VERSION: u32 = 9;
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

#[repr(u32)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BootMemoryKind {
    Usable = 0,
    Reserved = 1,
    AcpiReclaim = 2,
    AcpiNvs = 3,
    Mmio = 4,
    Framebuffer = 5,
}

#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct BootMemoryRegion {
    pub phys_start: u64,
    pub page_count: u64,
    pub kind: BootMemoryKind,
    pub _reserved0: u32,
}

#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct BootMemoryMap {
    pub entries_ptr: u64,
    pub entry_count: u32,
    pub _reserved0: u32,
}

impl BootMemoryMap {
    pub const fn empty() -> Self {
        Self {
            entries_ptr: 0,
            entry_count: 0,
            _reserved0: 0,
        }
    }
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct BootVolumeIdentity {
    pub fat_volume_id: u32,
    pub _reserved0: u32,
    pub volume_start_lba: u64,
    pub volume_sector_count: u64,
}

impl BootVolumeIdentity {
    pub const fn empty() -> Self {
        Self {
            fat_volume_id: 0,
            _reserved0: 0,
            volume_start_lba: 0,
            volume_sector_count: 0,
        }
    }

    pub const fn is_present(&self) -> bool {
        self.fat_volume_id != 0 || self.volume_start_lba != 0 || self.volume_sector_count != 0
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
    pub boot_volume: BootVolumeIdentity,
    pub framebuffer: FramebufferInfo,
    pub memory_map: BootMemoryMap,
    pub boot_files: BootFileManifest,
}
