#![no_std]

use core::mem::{align_of, size_of};

pub const BOOT_INFO_MAGIC: u64 = 0x5255_5354_4F53_4749; // "RUSTOSGI"
pub const BOOT_INFO_VERSION: u32 = 18;
pub const MAX_BOOT_MEMORY_REGIONS: u32 = 4096;
pub const MAX_BOOT_FRAMEBUFFER_WIDTH: u32 = 7680;
pub const MAX_BOOT_FRAMEBUFFER_HEIGHT: u32 = 4320;
pub const MAX_EARLY_SYSTEM_IMAGE_BYTES: u64 = 256 * 1024 * 1024;
pub const EARLY_SYSTEM_MAGIC: [u8; 8] = *b"RSEARLY2";
pub const EARLY_SYSTEM_VERSION: u32 = 2;
pub const EARLY_SYSTEM_HEADER_BYTES: usize = 96;
pub const EARLY_SYSTEM_ENTRY_BYTES: usize = 160;
pub const EARLY_SYSTEM_MAX_ENTRIES: u32 = 64;
pub const EARLY_SYSTEM_MAX_PATH_BYTES: usize = 96;
pub const EARLY_SYSTEM_PAYLOAD_ALIGNMENT: u64 = 4096;

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

        if !(self.addr as usize).is_multiple_of(bytes_per_pixel) {
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
        if !(self.entries_ptr as usize).is_multiple_of(align_of::<BootMemoryRegion>()) {
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
pub struct EarlySystemImage {
    pub ptr: u64,
    pub len: u64,
}

impl EarlySystemImage {
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
        if self.ptr == 0
            || self.len < EARLY_SYSTEM_HEADER_BYTES as u64
            || self.len > MAX_EARLY_SYSTEM_IMAGE_BYTES
        {
            return Err(BootInfoValidationError::InvalidEarlySystemImage);
        }
        self.ptr
            .checked_add(self.len)
            .ok_or(BootInfoValidationError::InvalidEarlySystemImage)?;
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct EarlySystemHeader {
    pub entry_count: u32,
    pub payload_offset: u64,
    pub total_bytes: u64,
    /// Ed25519 verifying key for L0-authenticated storage transport epochs.
    ///
    /// The complete early-system image is admitted by the signed GRUB path,
    /// so this key is immutable ring0 bootstrap authority. The corresponding
    /// signing key remains on L0 and is never exposed to a storage DVM.
    pub storage_epoch_verifying_key: [u8; 32],
}

impl EarlySystemHeader {
    pub fn new(
        entry_count: u32,
        payload_offset: u64,
        total_bytes: u64,
        storage_epoch_verifying_key: [u8; 32],
    ) -> Option<Self> {
        let header = Self {
            entry_count,
            payload_offset,
            total_bytes,
            storage_epoch_verifying_key,
        };
        header.is_valid().then_some(header)
    }

    pub fn is_valid(self) -> bool {
        let table_bytes = u64::from(self.entry_count).checked_mul(EARLY_SYSTEM_ENTRY_BYTES as u64);
        self.entry_count != 0
            && self.entry_count <= EARLY_SYSTEM_MAX_ENTRIES
            && table_bytes
                .and_then(|bytes| (EARLY_SYSTEM_HEADER_BYTES as u64).checked_add(bytes))
                .is_some_and(|table_end| table_end <= self.payload_offset)
            && self
                .payload_offset
                .is_multiple_of(EARLY_SYSTEM_PAYLOAD_ALIGNMENT)
            && self.payload_offset < self.total_bytes
            && self.total_bytes <= MAX_EARLY_SYSTEM_IMAGE_BYTES
            && self
                .storage_epoch_verifying_key
                .iter()
                .any(|byte| *byte != 0)
    }

    pub fn encode(self) -> Option<[u8; EARLY_SYSTEM_HEADER_BYTES]> {
        if !self.is_valid() {
            return None;
        }
        let mut bytes = [0_u8; EARLY_SYSTEM_HEADER_BYTES];
        bytes[0..8].copy_from_slice(&EARLY_SYSTEM_MAGIC);
        bytes[8..12].copy_from_slice(&EARLY_SYSTEM_VERSION.to_le_bytes());
        bytes[12..16].copy_from_slice(&(EARLY_SYSTEM_HEADER_BYTES as u32).to_le_bytes());
        bytes[16..20].copy_from_slice(&self.entry_count.to_le_bytes());
        bytes[20..24].copy_from_slice(&(EARLY_SYSTEM_ENTRY_BYTES as u32).to_le_bytes());
        bytes[24..32].copy_from_slice(&(EARLY_SYSTEM_HEADER_BYTES as u64).to_le_bytes());
        bytes[32..40].copy_from_slice(&self.payload_offset.to_le_bytes());
        bytes[40..48].copy_from_slice(&self.total_bytes.to_le_bytes());
        bytes[48..80].copy_from_slice(&self.storage_epoch_verifying_key);
        Some(bytes)
    }

    pub fn decode(bytes: &[u8]) -> Option<Self> {
        if bytes.len() < EARLY_SYSTEM_HEADER_BYTES
            || bytes[0..8] != EARLY_SYSTEM_MAGIC
            || read_u32(bytes, 8)? != EARLY_SYSTEM_VERSION
            || read_u32(bytes, 12)? != EARLY_SYSTEM_HEADER_BYTES as u32
            || read_u32(bytes, 20)? != EARLY_SYSTEM_ENTRY_BYTES as u32
            || read_u64(bytes, 24)? != EARLY_SYSTEM_HEADER_BYTES as u64
            || bytes[80..EARLY_SYSTEM_HEADER_BYTES]
                .iter()
                .any(|byte| *byte != 0)
        {
            return None;
        }
        let header = Self {
            entry_count: read_u32(bytes, 16)?,
            payload_offset: read_u64(bytes, 32)?,
            total_bytes: read_u64(bytes, 40)?,
            storage_epoch_verifying_key: bytes[48..80].try_into().ok()?,
        };
        header.is_valid().then_some(header)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct EarlySystemEntry {
    pub path: [u8; EARLY_SYSTEM_MAX_PATH_BYTES],
    pub path_len: u16,
    pub payload_offset: u64,
    pub payload_len: u64,
    pub sha256: [u8; 32],
}

impl EarlySystemEntry {
    pub fn new(
        path: &[u8],
        payload_offset: u64,
        payload_len: u64,
        sha256: [u8; 32],
    ) -> Option<Self> {
        if path.len() > EARLY_SYSTEM_MAX_PATH_BYTES || !valid_early_system_path(path) {
            return None;
        }
        let mut path_bytes = [0_u8; EARLY_SYSTEM_MAX_PATH_BYTES];
        path_bytes[..path.len()].copy_from_slice(path);
        let entry = Self {
            path: path_bytes,
            path_len: u16::try_from(path.len()).ok()?,
            payload_offset,
            payload_len,
            sha256,
        };
        (entry.payload_len != 0).then_some(entry)
    }

    pub fn path_bytes(&self) -> Option<&[u8]> {
        let len = usize::from(self.path_len);
        let path = self.path.get(..len)?;
        if self.path.get(len..)?.iter().any(|byte| *byte != 0) || !valid_early_system_path(path) {
            return None;
        }
        Some(path)
    }

    pub fn is_valid_for(self, header: EarlySystemHeader) -> bool {
        self.path_bytes().is_some()
            && self.payload_len != 0
            && self.payload_offset >= header.payload_offset
            && self
                .payload_offset
                .checked_add(self.payload_len)
                .is_some_and(|end| end <= header.total_bytes)
    }

    pub fn encode(self, header: EarlySystemHeader) -> Option<[u8; EARLY_SYSTEM_ENTRY_BYTES]> {
        if !self.is_valid_for(header) {
            return None;
        }
        let mut bytes = [0_u8; EARLY_SYSTEM_ENTRY_BYTES];
        bytes[0..2].copy_from_slice(&self.path_len.to_le_bytes());
        bytes[8..16].copy_from_slice(&self.payload_offset.to_le_bytes());
        bytes[16..24].copy_from_slice(&self.payload_len.to_le_bytes());
        bytes[24..56].copy_from_slice(&self.sha256);
        bytes[56..152].copy_from_slice(&self.path);
        Some(bytes)
    }

    pub fn decode(bytes: &[u8], header: EarlySystemHeader) -> Option<Self> {
        if bytes.len() != EARLY_SYSTEM_ENTRY_BYTES
            || bytes[2..8].iter().any(|byte| *byte != 0)
            || bytes[152..160].iter().any(|byte| *byte != 0)
        {
            return None;
        }
        let mut path = [0_u8; EARLY_SYSTEM_MAX_PATH_BYTES];
        path.copy_from_slice(&bytes[56..152]);
        let mut sha256 = [0_u8; 32];
        sha256.copy_from_slice(&bytes[24..56]);
        let entry = Self {
            path,
            path_len: read_u16(bytes, 0)?,
            payload_offset: read_u64(bytes, 8)?,
            payload_len: read_u64(bytes, 16)?,
            sha256,
        };
        entry.is_valid_for(header).then_some(entry)
    }
}

pub fn valid_early_system_path(path: &[u8]) -> bool {
    if path.is_empty() || path.len() > EARLY_SYSTEM_MAX_PATH_BYTES || path[0] == b'/' {
        return false;
    }
    let mut segment_start = 0usize;
    for (index, byte) in path.iter().copied().enumerate() {
        if !(byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-' | b'/')) {
            return false;
        }
        if byte == b'/' {
            if invalid_path_segment(&path[segment_start..index]) {
                return false;
            }
            segment_start = index + 1;
        }
    }
    !invalid_path_segment(&path[segment_start..])
}

fn invalid_path_segment(segment: &[u8]) -> bool {
    segment.is_empty() || segment == b"." || segment == b".."
}

fn read_u16(bytes: &[u8], offset: usize) -> Option<u16> {
    let chunk = bytes.get(offset..offset.checked_add(2)?)?;
    Some(u16::from_le_bytes(chunk.try_into().ok()?))
}

fn read_u32(bytes: &[u8], offset: usize) -> Option<u32> {
    let chunk = bytes.get(offset..offset.checked_add(4)?)?;
    Some(u32::from_le_bytes(chunk.try_into().ok()?))
}

fn read_u64(bytes: &[u8], offset: usize) -> Option<u64> {
    let chunk = bytes.get(offset..offset.checked_add(8)?)?;
    Some(u64::from_le_bytes(chunk.try_into().ok()?))
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
    /// Retired physical boot-extent pointer/length. Must remain zero so an
    /// older loader cannot silently reactivate ring0 disk reads.
    pub _reserved_storage_bootstrap: [u64; 2],
    pub early_system_image: EarlySystemImage,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BootInfoValidationError {
    NullPointer,
    MagicMismatch,
    VersionMismatch,
    InvalidRngSeed,
    InvalidFramebuffer,
    InvalidKernelImage,
    InvalidMemoryMap,
    InvalidBootVolume,
    InvalidReservedStorageBootstrap,
    InvalidEarlySystemImage,
}

impl BootInfoValidationError {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::NullPointer => "boot info pointer is null",
            Self::MagicMismatch => "boot info magic mismatch",
            Self::VersionMismatch => "boot info version mismatch",
            Self::InvalidRngSeed => "boot info random seed is unavailable",
            Self::InvalidFramebuffer => "boot info framebuffer is invalid",
            Self::InvalidKernelImage => "boot info kernel image is invalid",
            Self::InvalidMemoryMap => "boot info memory map is invalid",
            Self::InvalidBootVolume => "boot info boot volume identity is invalid",
            Self::InvalidReservedStorageBootstrap => {
                "boot info retired storage bootstrap fields are nonzero"
            }
            Self::InvalidEarlySystemImage => "boot info early system image is invalid",
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
        if !rng_seed_usable(self.rng_seed) {
            return Err(BootInfoValidationError::InvalidRngSeed);
        }

        self.framebuffer.validate()?;
        self.boot_volume.validate()?;
        if self._reserved_storage_bootstrap != [0; 2] {
            return Err(BootInfoValidationError::InvalidReservedStorageBootstrap);
        }
        self.early_system_image.validate()?;

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

    /// # Safety
    ///
    /// `ptr` must reference readable memory containing a complete, aligned
    /// `BootInfo`. This function validates the encoded fields, but cannot make
    /// an unmapped or concurrently modified pointer safe to dereference.
    pub unsafe fn from_ptr<'a>(ptr: *const Self) -> Result<&'a Self, BootInfoValidationError> {
        if ptr.is_null() {
            return Err(BootInfoValidationError::NullPointer);
        }
        if !(ptr as usize).is_multiple_of(align_of::<Self>()) {
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
            _reserved_storage_bootstrap: [0; 2],
            early_system_image: EarlySystemImage::empty(),
        }
    }

    #[test]
    fn validates_final_boot_info() {
        assert!(valid_boot_info().validate().is_ok());
    }

    #[test]
    fn rejects_an_all_zero_rng_seed() {
        let mut info = valid_boot_info();
        info.rng_seed = [0; 32];
        assert_eq!(
            info.validate_staged(),
            Err(BootInfoValidationError::InvalidRngSeed)
        );
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
    fn rejects_retired_storage_bootstrap_fields() {
        let mut info = valid_boot_info();
        info._reserved_storage_bootstrap = [1, 4];
        assert_eq!(
            info.validate(),
            Err(BootInfoValidationError::InvalidReservedStorageBootstrap)
        );
    }

    #[test]
    fn early_system_records_are_fixed_bounded_and_canonical() {
        let header = EarlySystemHeader::new(1, 4096, 8192, [0x5a; 32]).expect("header");
        assert_eq!(
            EarlySystemHeader::decode(&header.encode().expect("encode")),
            Some(header)
        );
        let entry = EarlySystemEntry::new(b"services/rootd/rootd.elf", 4096, 32, [0x5a; 32])
            .expect("entry");
        assert_eq!(
            EarlySystemEntry::decode(&entry.encode(header).expect("encode"), header),
            Some(entry)
        );
        assert!(!valid_early_system_path(b"services/../rootd.elf"));
        assert!(!valid_early_system_path(b"/services/rootd.elf"));
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
