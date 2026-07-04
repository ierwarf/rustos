#![no_std]

use core::mem::{align_of, size_of};

pub const BOOT_INFO_MAGIC: u64 = 0x5255_5354_4F53_4749; // "RUSTOSGI"
pub const BOOT_INFO_VERSION: u32 = 16;
pub const MAX_BOOT_MEMORY_REGIONS: u32 = 4096;
pub const MAX_BOOT_FRAMEBUFFER_WIDTH: u32 = 7680;
pub const MAX_BOOT_FRAMEBUFFER_HEIGHT: u32 = 4320;
pub const MAX_BOOT_EXTENT_MANIFEST_BYTES: u64 = 4 * 1024 * 1024;

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

impl FramebufferInfo {
    pub const fn empty() -> Self {
        Self {
            addr: 0,
            size: 0,
            back_buffer_addr: 0,
            back_buffer_size: 0,
            width: 0,
            height: 0,
            stride: 0,
            pixel_format: BootPixelFormat::Unknown,
            bytes_per_pixel: 0,
            _reserved: [0; 3],
        }
    }

    pub fn is_present(&self) -> bool {
        self.addr != 0
            || self.size != 0
            || self.back_buffer_addr != 0
            || self.back_buffer_size != 0
            || self.width != 0
            || self.height != 0
            || self.stride != 0
            || !matches!(self.pixel_format, BootPixelFormat::Unknown)
            || self.bytes_per_pixel != 0
    }

    pub fn validate(&self) -> Result<(), BootInfoValidationError> {
        if !self.is_present() {
            return Ok(());
        }

        if self.addr == 0 || self.size == 0 {
            return Err(BootInfoValidationError::InvalidFramebuffer);
        }
        self.addr
            .checked_add(self.size)
            .ok_or(BootInfoValidationError::InvalidFramebuffer)?;

        let width = self.width as usize;
        let height = self.height as usize;
        let stride = self.stride as usize;
        let bytes_per_pixel = self.bytes_per_pixel as usize;
        if width == 0 || height == 0 || stride < width || !(3..=4).contains(&bytes_per_pixel) {
            return Err(BootInfoValidationError::InvalidFramebuffer);
        }
        if self.width > MAX_BOOT_FRAMEBUFFER_WIDTH || self.height > MAX_BOOT_FRAMEBUFFER_HEIGHT {
            return Err(BootInfoValidationError::InvalidFramebuffer);
        }
        if !matches!(
            self.pixel_format,
            BootPixelFormat::Rgb | BootPixelFormat::Bgr
        ) {
            return Err(BootInfoValidationError::InvalidFramebuffer);
        }

        let stride_bytes = stride
            .checked_mul(bytes_per_pixel)
            .ok_or(BootInfoValidationError::InvalidFramebuffer)?;
        let min_size = stride_bytes
            .checked_mul(height)
            .ok_or(BootInfoValidationError::InvalidFramebuffer)?;
        if min_size > self.size as usize {
            return Err(BootInfoValidationError::InvalidFramebuffer);
        }

        if (self.addr as usize) % bytes_per_pixel != 0 {
            return Err(BootInfoValidationError::InvalidFramebuffer);
        }

        if self.back_buffer_addr == 0 && self.back_buffer_size != 0 {
            return Err(BootInfoValidationError::InvalidFramebuffer);
        }
        if self.back_buffer_addr != 0 && self.back_buffer_size < min_size as u64 {
            return Err(BootInfoValidationError::InvalidFramebuffer);
        }
        if self.back_buffer_addr != 0 {
            self.back_buffer_addr
                .checked_add(self.back_buffer_size)
                .ok_or(BootInfoValidationError::InvalidFramebuffer)?;
        }

        Ok(())
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

    pub fn validate(&self) -> Result<(), BootInfoValidationError> {
        if self.entry_count == 0 || self.entries_ptr == 0 {
            return Err(BootInfoValidationError::InvalidMemoryMap);
        }
        if self.entry_count > MAX_BOOT_MEMORY_REGIONS {
            return Err(BootInfoValidationError::InvalidMemoryMap);
        }

        let bytes = (self.entry_count as usize)
            .checked_mul(size_of::<BootMemoryRegion>())
            .ok_or(BootInfoValidationError::InvalidMemoryMap)?;
        if bytes == 0 {
            return Err(BootInfoValidationError::InvalidMemoryMap);
        }
        if (self.entries_ptr as usize) % align_of::<BootMemoryRegion>() != 0 {
            return Err(BootInfoValidationError::InvalidMemoryMap);
        }
        self.entries_ptr
            .checked_add(bytes as u64)
            .ok_or(BootInfoValidationError::InvalidMemoryMap)?;

        Ok(())
    }
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct NucleusImageInfo {
    pub phys_start: u64,
    pub size: u64,
    pub load_bias: u64,
    pub entry_point: u64,
}

impl NucleusImageInfo {
    pub const fn empty() -> Self {
        Self {
            phys_start: 0,
            size: 0,
            load_bias: 0,
            entry_point: 0,
        }
    }

    pub const fn is_present(&self) -> bool {
        self.size != 0 && self.load_bias != 0 && self.entry_point != 0
    }

    pub fn validate(&self) -> Result<(), BootInfoValidationError> {
        if !self.is_present() || self.phys_start == 0 {
            return Err(BootInfoValidationError::InvalidKernelImage);
        }

        self.phys_start
            .checked_add(self.size)
            .ok_or(BootInfoValidationError::InvalidKernelImage)?;
        self.load_bias
            .checked_add(self.size)
            .ok_or(BootInfoValidationError::InvalidKernelImage)?;
        Ok(())
    }
}

pub type KernelImageInfo = NucleusImageInfo;

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct BootVolumeIdentity {
    pub fat_volume_id: u32,
    pub _reserved0: u32,
    pub volume_start_lba: u64,
    pub volume_sector_count: u64,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct BootExtentManifest {
    pub ptr: u64,
    pub len: u64,
}

impl BootExtentManifest {
    pub const fn empty() -> Self {
        Self { ptr: 0, len: 0 }
    }

    pub const fn is_present(&self) -> bool {
        self.ptr != 0 || self.len != 0
    }

    pub fn validate(&self) -> Result<(), BootInfoValidationError> {
        if !self.is_present() {
            return Ok(());
        }
        if self.ptr == 0 || self.len == 0 || self.len > MAX_BOOT_EXTENT_MANIFEST_BYTES {
            return Err(BootInfoValidationError::InvalidBootExtentManifest);
        }
        self.ptr
            .checked_add(self.len)
            .ok_or(BootInfoValidationError::InvalidBootExtentManifest)?;
        Ok(())
    }
}

#[repr(u32)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum BootVolumeTransport {
    #[default]
    Unknown = 0,
    Ahci = 1,
    Nvme = 2,
    Usb = 3,
}

impl BootVolumeTransport {
    pub const fn from_raw(value: u32) -> Self {
        match value {
            1 => Self::Ahci,
            2 => Self::Nvme,
            3 => Self::Usb,
            _ => Self::Unknown,
        }
    }
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

    pub fn validate(&self) -> Result<(), BootInfoValidationError> {
        if !self.is_present() {
            return Ok(());
        }
        if self.fat_volume_id == 0 || self.volume_sector_count == 0 {
            return Err(BootInfoValidationError::InvalidBootVolume);
        }
        self.volume_start_lba
            .checked_add(self.volume_sector_count)
            .ok_or(BootInfoValidationError::InvalidBootVolume)?;
        Ok(())
    }

    pub const fn transport(self) -> BootVolumeTransport {
        BootVolumeTransport::from_raw(self._reserved0)
    }

    pub const fn with_transport(mut self, transport: BootVolumeTransport) -> Self {
        self._reserved0 = transport as u32;
        self
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
    pub nucleus_image: NucleusImageInfo,
    pub memory_map: BootMemoryMap,
    pub boot_extent_manifest: BootExtentManifest,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BootInfoValidationError {
    NullPointer,
    MagicMismatch,
    VersionMismatch,
    InvalidFramebuffer,
    InvalidKernelImage,
    InvalidMemoryMap,
    InvalidBootVolume,
    InvalidBootExtentManifest,
}

impl BootInfoValidationError {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::NullPointer => "boot info pointer is null",
            Self::MagicMismatch => "boot info magic mismatch",
            Self::VersionMismatch => "boot info version mismatch",
            Self::InvalidFramebuffer => "boot info framebuffer is invalid",
            Self::InvalidKernelImage => "boot info kernel image is invalid",
            Self::InvalidMemoryMap => "boot info memory map is invalid",
            Self::InvalidBootVolume => "boot info boot volume identity is invalid",
            Self::InvalidBootExtentManifest => "boot info boot extent manifest is invalid",
        }
    }
}

impl BootInfo {
    pub fn validate_staged(&self) -> Result<(), BootInfoValidationError> {
        if self.magic != BOOT_INFO_MAGIC {
            return Err(BootInfoValidationError::MagicMismatch);
        }
        if self.version != BOOT_INFO_VERSION {
            return Err(BootInfoValidationError::VersionMismatch);
        }

        self.framebuffer.validate()?;
        self.boot_volume.validate()?;
        self.boot_extent_manifest.validate()?;

        if self.nucleus_image.is_present() {
            self.nucleus_image.validate()?;
        } else if self.nucleus_image.phys_start != 0
            || self.nucleus_image.size != 0
            || self.nucleus_image.load_bias != 0
            || self.nucleus_image.entry_point != 0
        {
            return Err(BootInfoValidationError::InvalidKernelImage);
        }

        if self.memory_map.entry_count != 0 || self.memory_map.entries_ptr != 0 {
            self.memory_map.validate()?;
        }

        Ok(())
    }

    pub fn validate(&self) -> Result<(), BootInfoValidationError> {
        self.validate_staged()?;
        self.nucleus_image.validate()?;
        self.memory_map.validate()?;
        Ok(())
    }

    pub unsafe fn from_ptr<'a>(ptr: *const Self) -> Result<&'a Self, BootInfoValidationError> {
        if ptr.is_null() {
            return Err(BootInfoValidationError::NullPointer);
        }
        if (ptr as usize) % align_of::<Self>() != 0 {
            return Err(BootInfoValidationError::NullPointer);
        }

        let info = unsafe { &*ptr };
        info.validate()?;
        Ok(info)
    }
}

pub const fn rng_seed_usable(seed: [u8; 32]) -> bool {
    let mut index = 0usize;
    while index < seed.len() {
        if seed[index] != 0 {
            return true;
        }
        index += 1;
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    fn valid_boot_info() -> BootInfo {
        let memory_map = [BootMemoryRegion {
            phys_start: 0x1000,
            page_count: 16,
            kind: BootMemoryKind::Usable,
            _reserved0: 0,
        }];

        BootInfo {
            magic: BOOT_INFO_MAGIC,
            version: BOOT_INFO_VERSION,
            _reserved0: 0,
            rng_seed: [0x5a; 32],
            acpi_rsdp_addr: 0,
            boot_volume: BootVolumeIdentity {
                fat_volume_id: 0x1234_5678,
                _reserved0: BootVolumeTransport::Ahci as u32,
                volume_start_lba: 0,
                volume_sector_count: 128,
            },
            framebuffer: FramebufferInfo {
                addr: 0x2000,
                size: 4096,
                back_buffer_addr: 0x3000,
                back_buffer_size: 4096,
                width: 16,
                height: 16,
                stride: 16,
                pixel_format: BootPixelFormat::Rgb,
                bytes_per_pixel: 4,
                _reserved: [0; 3],
            },
            nucleus_image: NucleusImageInfo {
                phys_start: 0x20_0000,
                size: 0x4000,
                load_bias: 0x20_0000,
                entry_point: 0x20_1000,
            },
            memory_map: BootMemoryMap {
                entries_ptr: memory_map.as_ptr() as u64,
                entry_count: memory_map.len() as u32,
                _reserved0: 0,
            },
            boot_extent_manifest: BootExtentManifest::empty(),
        }
    }

    #[test]
    fn validates_final_boot_info() {
        assert!(valid_boot_info().validate().is_ok());
    }

    #[test]
    fn rejects_empty_memory_map_in_final_validation() {
        let mut info = valid_boot_info();
        info.memory_map = BootMemoryMap::empty();
        assert_eq!(
            info.validate(),
            Err(BootInfoValidationError::InvalidMemoryMap)
        );
        assert!(info.validate_staged().is_ok());
    }

    #[test]
    fn accepts_absent_optional_volume() {
        let mut info = valid_boot_info();
        info.boot_volume = BootVolumeIdentity::empty();
        assert!(info.validate_staged().is_ok());
    }

    #[test]
    fn accepts_absent_framebuffer() {
        let mut info = valid_boot_info();
        info.framebuffer = FramebufferInfo::empty();
        assert!(info.validate().is_ok());
    }

    #[test]
    fn validates_present_boot_extent_manifest() {
        let bytes = [1_u8, 2, 3, 4];
        let mut info = valid_boot_info();
        info.boot_extent_manifest = BootExtentManifest {
            ptr: bytes.as_ptr() as u64,
            len: bytes.len() as u64,
        };
        assert!(info.validate().is_ok());
    }

    #[test]
    fn rejects_partial_absent_framebuffer() {
        let mut info = valid_boot_info();
        info.framebuffer = FramebufferInfo {
            addr: 0x2000,
            ..FramebufferInfo::empty()
        };
        assert_eq!(
            info.validate(),
            Err(BootInfoValidationError::InvalidFramebuffer)
        );
    }

    #[test]
    fn rejects_oversized_boot_memory_map() {
        let mut info = valid_boot_info();
        info.memory_map.entry_count = MAX_BOOT_MEMORY_REGIONS + 1;
        assert_eq!(
            info.validate(),
            Err(BootInfoValidationError::InvalidMemoryMap)
        );
    }

    #[test]
    fn rejects_present_unknown_framebuffer_format() {
        let mut info = valid_boot_info();
        info.framebuffer.pixel_format = BootPixelFormat::Unknown;
        assert_eq!(
            info.validate(),
            Err(BootInfoValidationError::InvalidFramebuffer)
        );
    }
}
