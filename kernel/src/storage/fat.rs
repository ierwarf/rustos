use alloc::string::String;
#[cfg(test)]
use alloc::vec;
use alloc::vec::Vec;
use boot_protocol::{BootFileEntry, BootFileManifest, BootInfo};
use core::cmp::min;
use core::hint::spin_loop;
use core::ptr;
use core::slice;
use core::str;
use core::sync::atomic::{AtomicPtr, Ordering};

use fatfs::{IoBase, IoError, Read, Seek, SeekFrom, Write};
use x86_64::instructions::{interrupts, port::Port};

pub const FAT_SECTOR_SIZE: usize = 512;
const FAT_VOLUME_SIG_OFFSET: usize = FAT_SECTOR_SIZE - 2;
const GPT_HEADER_LBA: u64 = 1;
const GPT_SIGNATURE: [u8; 8] = *b"EFI PART";
const MBR_PARTITION_TABLE_OFFSET: usize = 446;
const MBR_PARTITION_ENTRY_LEN: usize = 16;

static BOOT_INFO_PTR: AtomicPtr<BootInfo> = AtomicPtr::new(ptr::null_mut());

pub type IoResult<T> = core::result::Result<T, DiskIoError>;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DiskIoError {
    // Reserved for block drivers that can surface retryable I/O aborts.
    #[allow(dead_code)]
    Interrupted,
    UnexpectedEof,
    WriteZero,
    InvalidInput,
    Timeout,
    DeviceFault,
    NotPresent,
}

impl IoError for DiskIoError {
    fn is_interrupted(&self) -> bool {
        matches!(self, Self::Interrupted)
    }

    fn new_unexpected_eof_error() -> Self {
        Self::UnexpectedEof
    }

    fn new_write_zero_error() -> Self {
        Self::WriteZero
    }
}

pub(crate) fn init_boot_info(boot_info_ptr: *const BootInfo) {
    BOOT_INFO_PTR.store(boot_info_ptr.cast_mut(), Ordering::Release);
}

fn boot_info() -> Option<&'static BootInfo> {
    let boot_info_ptr = BOOT_INFO_PTR.load(Ordering::Acquire);
    if boot_info_ptr.is_null() {
        None
    } else {
        Some(unsafe { &*boot_info_ptr.cast_const() })
    }
}

#[derive(Clone, Copy)]
struct CachedBootVolume {
    manifest: BootFileManifest,
}

impl CachedBootVolume {
    fn from_boot_info() -> Option<Self> {
        let boot_info = boot_info()?;
        if boot_info.boot_files.entry_count == 0 {
            return None;
        }
        Some(Self {
            manifest: boot_info.boot_files,
        })
    }

    fn open_file(&self, normalized_path: &str) -> Option<CachedBootFile> {
        let entries = boot_file_entries(&self.manifest)?;
        for entry in entries {
            let path = boot_file_path(entry)?;
            if fat_paths_match(normalized_path, path) {
                return Some(CachedBootFile {
                    data: boot_file_data(entry)?,
                    pos: 0,
                });
            }
        }
        None
    }
}

#[derive(Clone, Copy)]
pub(crate) struct CachedBootFile {
    data: &'static [u8],
    pos: usize,
}

fn boot_file_entries(manifest: &BootFileManifest) -> Option<&'static [BootFileEntry]> {
    if manifest.entry_count == 0 {
        return Some(&[]);
    }
    if manifest.entries_ptr == 0 {
        return None;
    }

    Some(unsafe {
        slice::from_raw_parts(
            manifest.entries_ptr as *const BootFileEntry,
            manifest.entry_count as usize,
        )
    })
}

fn boot_file_path(entry: &BootFileEntry) -> Option<&'static str> {
    if entry.path_len == 0 || entry.path_ptr == 0 {
        return None;
    }

    let bytes =
        unsafe { slice::from_raw_parts(entry.path_ptr as *const u8, entry.path_len as usize) };
    str::from_utf8(bytes).ok()
}

fn boot_file_data(entry: &BootFileEntry) -> Option<&'static [u8]> {
    if entry.data_len == 0 {
        return Some(&[]);
    }
    if entry.data_ptr == 0 {
        return None;
    }

    Some(unsafe { slice::from_raw_parts(entry.data_ptr as *const u8, entry.data_len as usize) })
}

fn fat_paths_match(lhs: &str, rhs: &str) -> bool {
    lhs.eq_ignore_ascii_case(rhs)
}

/// FAT adapter target: provide raw sector read/write for your storage backend.
pub trait BlockDevice {
    fn sector_count(&self) -> u64;
    fn read_sector(&mut self, lba: u64, out: &mut [u8; FAT_SECTOR_SIZE]) -> IoResult<()>;
    fn write_sector(&mut self, lba: u64, input: &[u8; FAT_SECTOR_SIZE]) -> IoResult<()>;
    fn flush(&mut self) -> IoResult<()> {
        Ok(())
    }
}

/// Simple in-memory block device for FAT testing/development.
#[cfg(test)]
pub struct MemBlockDevice {
    data: Vec<u8>,
}

#[cfg(test)]
impl MemBlockDevice {
    pub fn new_zeroed(sectors: u64) -> Self {
        let bytes = sectors.saturating_mul(FAT_SECTOR_SIZE as u64) as usize;
        Self {
            data: vec![0; bytes],
        }
    }

    pub fn from_bytes(data: Vec<u8>) -> IoResult<Self> {
        if data.len() % FAT_SECTOR_SIZE != 0 {
            return Err(DiskIoError::InvalidInput);
        }
        Ok(Self { data })
    }

    pub fn as_bytes(&self) -> &[u8] {
        &self.data
    }

    pub fn as_bytes_mut(&mut self) -> &mut [u8] {
        &mut self.data
    }

    fn sector_bounds(&self, lba: u64) -> IoResult<(usize, usize)> {
        let start = (lba as usize)
            .checked_mul(FAT_SECTOR_SIZE)
            .ok_or(DiskIoError::InvalidInput)?;
        let end = start
            .checked_add(FAT_SECTOR_SIZE)
            .ok_or(DiskIoError::InvalidInput)?;
        if end > self.data.len() {
            return Err(DiskIoError::InvalidInput);
        }
        Ok((start, end))
    }
}

#[cfg(test)]
impl BlockDevice for MemBlockDevice {
    fn sector_count(&self) -> u64 {
        (self.data.len() / FAT_SECTOR_SIZE) as u64
    }

    fn read_sector(&mut self, lba: u64, out: &mut [u8; FAT_SECTOR_SIZE]) -> IoResult<()> {
        let (start, end) = self.sector_bounds(lba)?;
        out.copy_from_slice(&self.data[start..end]);
        Ok(())
    }

    fn write_sector(&mut self, lba: u64, input: &[u8; FAT_SECTOR_SIZE]) -> IoResult<()> {
        let (start, end) = self.sector_bounds(lba)?;
        self.data[start..end].copy_from_slice(input);
        Ok(())
    }
}

/// Maps a whole device to a logical slice `[start_lba, start_lba + sectors)`.
pub(crate) struct LbaSliceDevice<D: BlockDevice> {
    inner: D,
    start_lba: u64,
    sectors: u64,
}

impl<D: BlockDevice> LbaSliceDevice<D> {
    fn new(inner: D, start_lba: u64, sectors: u64) -> IoResult<Self> {
        let total = inner.sector_count();
        if sectors == 0 {
            return Err(DiskIoError::InvalidInput);
        }
        let end = start_lba
            .checked_add(sectors)
            .ok_or(DiskIoError::InvalidInput)?;
        if end > total {
            return Err(DiskIoError::InvalidInput);
        }
        Ok(Self {
            inner,
            start_lba,
            sectors,
        })
    }
}

impl<D: BlockDevice> BlockDevice for LbaSliceDevice<D> {
    fn sector_count(&self) -> u64 {
        self.sectors
    }

    fn read_sector(&mut self, lba: u64, out: &mut [u8; FAT_SECTOR_SIZE]) -> IoResult<()> {
        if lba >= self.sectors {
            return Err(DiskIoError::InvalidInput);
        }
        self.inner.read_sector(self.start_lba + lba, out)
    }

    fn write_sector(&mut self, lba: u64, input: &[u8; FAT_SECTOR_SIZE]) -> IoResult<()> {
        if lba >= self.sectors {
            return Err(DiskIoError::InvalidInput);
        }
        self.inner.write_sector(self.start_lba + lba, input)
    }

    fn flush(&mut self) -> IoResult<()> {
        self.inner.flush()
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum FatType {
    Fat12,
    Fat16,
    Fat32,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct FatVolumeMetadata {
    bytes_per_sector: u16,
    sectors_per_cluster: u8,
    total_sectors: u32,
    cluster_count: u32,
    fat_type: FatType,
}

fn le_u16(bytes: &[u8], offset: usize) -> u16 {
    u16::from_le_bytes([bytes[offset], bytes[offset + 1]])
}

fn le_u32(bytes: &[u8], offset: usize) -> u32 {
    u32::from_le_bytes([
        bytes[offset],
        bytes[offset + 1],
        bytes[offset + 2],
        bytes[offset + 3],
    ])
}

fn le_u64(bytes: &[u8], offset: usize) -> u64 {
    u64::from_le_bytes([
        bytes[offset],
        bytes[offset + 1],
        bytes[offset + 2],
        bytes[offset + 3],
        bytes[offset + 4],
        bytes[offset + 5],
        bytes[offset + 6],
        bytes[offset + 7],
    ])
}

fn parse_fat_volume_metadata(
    boot_sector: &[u8; FAT_SECTOR_SIZE],
    available_sectors: u64,
) -> Option<FatVolumeMetadata> {
    let has_jump = matches!(boot_sector[0], 0xEB | 0xE9);
    let sig_ok = boot_sector[FAT_VOLUME_SIG_OFFSET] == 0x55
        && boot_sector[FAT_VOLUME_SIG_OFFSET + 1] == 0xAA;
    if !has_jump || !sig_ok {
        return None;
    }

    let bytes_per_sector = le_u16(boot_sector, 11);
    if !matches!(bytes_per_sector, 512 | 1024 | 2048 | 4096) {
        return None;
    }

    let sectors_per_cluster = boot_sector[13];
    if sectors_per_cluster == 0 || !sectors_per_cluster.is_power_of_two() {
        return None;
    }

    let reserved_sectors = le_u16(boot_sector, 14);
    let fat_count = boot_sector[16];
    let root_entry_count = le_u16(boot_sector, 17);
    if reserved_sectors == 0 || fat_count == 0 {
        return None;
    }

    let total_sectors16 = le_u16(boot_sector, 19);
    let total_sectors32 = le_u32(boot_sector, 32);
    let total_sectors = match (total_sectors16, total_sectors32) {
        (0, 0) => return None,
        (0, total) => total,
        (total, 0) => total as u32,
        (total16, total32) => (total16 as u32).min(total32),
    };
    if total_sectors == 0 || u64::from(total_sectors) > available_sectors {
        return None;
    }

    let sectors_per_fat16 = le_u16(boot_sector, 22) as u32;
    let sectors_per_fat32 = le_u32(boot_sector, 36);
    let sectors_per_fat = if sectors_per_fat16 != 0 {
        sectors_per_fat16
    } else {
        sectors_per_fat32
    };
    if sectors_per_fat == 0 {
        return None;
    }

    let root_dir_bytes = (root_entry_count as u32).checked_mul(32)?;
    let root_dir_sectors = root_dir_bytes.div_ceil(bytes_per_sector as u32);
    let fat_area_sectors = (fat_count as u32).checked_mul(sectors_per_fat)?;
    let first_data_sector = (reserved_sectors as u32)
        .checked_add(fat_area_sectors)?
        .checked_add(root_dir_sectors)?;
    if first_data_sector >= total_sectors {
        return None;
    }

    let data_sectors = total_sectors - first_data_sector;
    let cluster_count = data_sectors / sectors_per_cluster as u32;
    if cluster_count == 0 {
        return None;
    }

    let fat_type = if cluster_count < 4_085 {
        FatType::Fat12
    } else if cluster_count < 65_525 {
        FatType::Fat16
    } else {
        FatType::Fat32
    };

    match fat_type {
        FatType::Fat32 => {
            if root_entry_count != 0 || sectors_per_fat32 == 0 || le_u32(boot_sector, 44) < 2 {
                return None;
            }
        }
        FatType::Fat12 | FatType::Fat16 => {
            if root_entry_count == 0 || sectors_per_fat16 == 0 {
                return None;
            }
        }
    }

    Some(FatVolumeMetadata {
        bytes_per_sector,
        sectors_per_cluster,
        total_sectors,
        cluster_count,
        fat_type,
    })
}

#[cfg(test)]
fn is_probable_fat_boot_sector(sector0: &[u8; FAT_SECTOR_SIZE], available_sectors: u64) -> bool {
    parse_fat_volume_metadata(sector0, available_sectors).is_some()
}

fn probe_fat_volume_slice<D: BlockDevice>(
    dev: &mut D,
    start_lba: u64,
    available_sectors: u64,
) -> IoResult<Option<u64>> {
    if available_sectors == 0 {
        return Ok(None);
    }

    let mut boot_sector = [0_u8; FAT_SECTOR_SIZE];
    dev.read_sector(start_lba, &mut boot_sector)?;
    Ok(parse_fat_volume_metadata(&boot_sector, available_sectors)
        .map(|metadata| u64::from(metadata.total_sectors)))
}

fn read_device_bytes<D: BlockDevice>(dev: &mut D, offset: u64, out: &mut [u8]) -> IoResult<()> {
    if out.is_empty() {
        return Ok(());
    }

    let mut scratch = [0_u8; FAT_SECTOR_SIZE];
    let mut copied = 0usize;
    while copied < out.len() {
        let absolute = offset
            .checked_add(copied as u64)
            .ok_or(DiskIoError::InvalidInput)?;
        let lba = absolute / FAT_SECTOR_SIZE as u64;
        let sector_offset = (absolute as usize) % FAT_SECTOR_SIZE;
        dev.read_sector(lba, &mut scratch)?;

        let chunk = min(FAT_SECTOR_SIZE - sector_offset, out.len() - copied);
        out[copied..copied + chunk].copy_from_slice(&scratch[sector_offset..sector_offset + chunk]);
        copied += chunk;
    }

    Ok(())
}

fn detect_gpt_fat_volume_slice<D: BlockDevice>(
    dev: &mut D,
    total_sectors: u64,
) -> IoResult<Option<(u64, u64)>> {
    if total_sectors <= GPT_HEADER_LBA {
        return Ok(None);
    }

    let mut header = [0_u8; FAT_SECTOR_SIZE];
    dev.read_sector(GPT_HEADER_LBA, &mut header)?;
    if header[..GPT_SIGNATURE.len()] != GPT_SIGNATURE {
        return Ok(None);
    }

    let header_size = le_u32(&header, 12) as usize;
    if !(92..=FAT_SECTOR_SIZE).contains(&header_size) {
        return Ok(None);
    }

    let entries_lba = le_u64(&header, 72);
    let entry_count = le_u32(&header, 80) as u64;
    let entry_size = le_u32(&header, 84) as u64;
    if entry_count == 0 || entry_size < 128 || entry_size % 8 != 0 {
        return Ok(None);
    }

    let Some(table_offset) = entries_lba.checked_mul(FAT_SECTOR_SIZE as u64) else {
        return Ok(None);
    };
    let Some(table_bytes) = entry_count.checked_mul(entry_size) else {
        return Ok(None);
    };
    let Some(table_end) = table_offset.checked_add(table_bytes) else {
        return Ok(None);
    };
    let Some(disk_bytes) = total_sectors.checked_mul(FAT_SECTOR_SIZE as u64) else {
        return Ok(None);
    };
    if table_end > disk_bytes {
        return Ok(None);
    }

    let mut entry_prefix = [0_u8; 48];
    for index in 0..entry_count {
        let Some(entry_offset) = index
            .checked_mul(entry_size)
            .and_then(|offset| table_offset.checked_add(offset))
        else {
            continue;
        };
        read_device_bytes(dev, entry_offset, &mut entry_prefix)?;
        if entry_prefix[..16].iter().all(|byte| *byte == 0) {
            continue;
        }

        let first_lba = le_u64(&entry_prefix, 32);
        let last_lba = le_u64(&entry_prefix, 40);
        let Some(sectors) = last_lba
            .checked_sub(first_lba)
            .and_then(|delta| delta.checked_add(1))
        else {
            continue;
        };
        let Some(end_lba) = first_lba.checked_add(sectors) else {
            continue;
        };
        if first_lba == 0 || end_lba > total_sectors {
            continue;
        }

        if let Some(volume_sectors) = probe_fat_volume_slice(dev, first_lba, sectors)? {
            return Ok(Some((first_lba, volume_sectors)));
        }
    }

    Ok(None)
}

fn detect_fat_volume_slice<D: BlockDevice>(dev: &mut D) -> IoResult<(u64, u64)> {
    let total = dev.sector_count();
    if total == 0 {
        return Err(DiskIoError::NotPresent);
    }

    let mut sector0 = [0_u8; FAT_SECTOR_SIZE];
    dev.read_sector(0, &mut sector0)?;
    if let Some(metadata) = parse_fat_volume_metadata(&sector0, total) {
        return Ok((0, u64::from(metadata.total_sectors)));
    }

    let sig_ok =
        sector0[FAT_VOLUME_SIG_OFFSET] == 0x55 && sector0[FAT_VOLUME_SIG_OFFSET + 1] == 0xAA;
    if !sig_ok {
        return Err(DiskIoError::InvalidInput);
    }

    let mut gpt_checked = false;
    for idx in 0..4 {
        let off = MBR_PARTITION_TABLE_OFFSET + idx * MBR_PARTITION_ENTRY_LEN;
        let part_type = sector0[off + 4];
        if part_type == 0 {
            continue;
        }

        if part_type == 0xEE && !gpt_checked {
            gpt_checked = true;
            if let Some(volume) = detect_gpt_fat_volume_slice(dev, total)? {
                return Ok(volume);
            }
            continue;
        }

        let start_lba = le_u32(&sector0, off + 8) as u64;
        let sectors = le_u32(&sector0, off + 12) as u64;
        let Some(end_lba) = start_lba.checked_add(sectors) else {
            continue;
        };
        if start_lba == 0 || sectors == 0 || end_lba > total {
            continue;
        }

        if let Some(volume_sectors) = probe_fat_volume_slice(dev, start_lba, sectors)? {
            return Ok((start_lba, volume_sectors));
        }
    }

    if !gpt_checked {
        if let Some(volume) = detect_gpt_fat_volume_slice(dev, total)? {
            return Ok(volume);
        }
    }

    Err(DiskIoError::InvalidInput)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AtaDrive {
    Master,
    Slave,
}

impl AtaDrive {
    fn select_bits(self) -> u8 {
        match self {
            Self::Master => 0,
            Self::Slave => 1 << 4,
        }
    }
}

/// Legacy ATA PIO controller (IDE compatibility mode).
///
/// This works only when firmware/chipset exposes a legacy ATA channel
/// (for example some QEMU setups). Many modern laptops with NVMe-only
/// storage will not expose this path.
pub struct AtaPioDevice {
    io_base: u16,
    ctrl_base: u16,
    drive: AtaDrive,
    total_sectors: u64,
    lba48: bool,
}

impl AtaPioDevice {
    const REG_DATA: u16 = 0;
    const REG_SECTOR_COUNT: u16 = 2;
    const REG_LBA0: u16 = 3;
    const REG_LBA1: u16 = 4;
    const REG_LBA2: u16 = 5;
    const REG_DRIVE_HEAD: u16 = 6;
    const REG_STATUS_COMMAND: u16 = 7;

    const STATUS_ERR: u8 = 1 << 0;
    const STATUS_DRQ: u8 = 1 << 3;
    const STATUS_DF: u8 = 1 << 5;
    const STATUS_BSY: u8 = 1 << 7;

    const CMD_IDENTIFY: u8 = 0xEC;
    const CMD_READ_SECTORS: u8 = 0x20;
    const CMD_WRITE_SECTORS: u8 = 0x30;
    const CMD_READ_SECTORS_EXT: u8 = 0x24;
    const CMD_WRITE_SECTORS_EXT: u8 = 0x34;
    const CMD_FLUSH_CACHE: u8 = 0xE7;
    const CMD_FLUSH_CACHE_EXT: u8 = 0xEA;

    const WAIT_SPINS: usize = 2_000_000;

    pub fn primary_master() -> IoResult<Self> {
        Self::new(0x1F0, 0x3F6, AtaDrive::Master)
    }

    pub fn primary_slave() -> IoResult<Self> {
        Self::new(0x1F0, 0x3F6, AtaDrive::Slave)
    }

    pub fn secondary_master() -> IoResult<Self> {
        Self::new(0x170, 0x376, AtaDrive::Master)
    }

    pub fn secondary_slave() -> IoResult<Self> {
        Self::new(0x170, 0x376, AtaDrive::Slave)
    }

    pub fn new(io_base: u16, ctrl_base: u16, drive: AtaDrive) -> IoResult<Self> {
        let mut dev = Self {
            io_base,
            ctrl_base,
            drive,
            total_sectors: 0,
            lba48: false,
        };
        dev.identify()?;
        Ok(dev)
    }

    fn read_u8(&self, reg: u16) -> u8 {
        unsafe {
            let mut port: Port<u8> = Port::new(self.io_base + reg);
            port.read()
        }
    }

    fn write_u8(&self, reg: u16, value: u8) {
        unsafe {
            let mut port: Port<u8> = Port::new(self.io_base + reg);
            port.write(value);
        }
    }

    fn read_data_u16(&self) -> u16 {
        unsafe {
            let mut port: Port<u16> = Port::new(self.io_base + Self::REG_DATA);
            port.read()
        }
    }

    fn write_data_u16(&self, value: u16) {
        unsafe {
            let mut port: Port<u16> = Port::new(self.io_base + Self::REG_DATA);
            port.write(value);
        }
    }

    fn read_alt_status(&self) -> u8 {
        unsafe {
            let mut port: Port<u8> = Port::new(self.ctrl_base);
            port.read()
        }
    }

    fn status_400ns_delay(&self) {
        let _ = self.read_alt_status();
        let _ = self.read_alt_status();
        let _ = self.read_alt_status();
        let _ = self.read_alt_status();
    }

    fn wait_not_busy(&self) -> IoResult<u8> {
        for _ in 0..Self::WAIT_SPINS {
            let status = self.read_u8(Self::REG_STATUS_COMMAND);
            if status & Self::STATUS_BSY == 0 {
                if status & Self::STATUS_DF != 0 {
                    return Err(DiskIoError::DeviceFault);
                }
                if status & Self::STATUS_ERR != 0 {
                    return Err(DiskIoError::InvalidInput);
                }
                return Ok(status);
            }
            spin_loop();
        }
        Err(DiskIoError::Timeout)
    }

    fn wait_drq(&self) -> IoResult<()> {
        for _ in 0..Self::WAIT_SPINS {
            let status = self.read_u8(Self::REG_STATUS_COMMAND);
            if status & Self::STATUS_BSY != 0 {
                spin_loop();
                continue;
            }
            if status & Self::STATUS_DF != 0 {
                return Err(DiskIoError::DeviceFault);
            }
            if status & Self::STATUS_ERR != 0 {
                return Err(DiskIoError::InvalidInput);
            }
            if status & Self::STATUS_DRQ != 0 {
                return Ok(());
            }
            spin_loop();
        }
        Err(DiskIoError::Timeout)
    }

    fn select_drive_base(&self) {
        self.write_u8(Self::REG_DRIVE_HEAD, 0xE0 | self.drive.select_bits());
        self.status_400ns_delay();
    }

    fn select_drive_lba28(&self, lba: u64) -> IoResult<()> {
        if lba > 0x0FFF_FFFF {
            return Err(DiskIoError::InvalidInput);
        }
        self.write_u8(
            Self::REG_DRIVE_HEAD,
            0xE0 | self.drive.select_bits() | (((lba >> 24) as u8) & 0x0F),
        );
        self.status_400ns_delay();
        Ok(())
    }

    fn identify(&mut self) -> IoResult<()> {
        interrupts::without_interrupts(|| {
            self.select_drive_base();
            self.write_u8(Self::REG_SECTOR_COUNT, 0);
            self.write_u8(Self::REG_LBA0, 0);
            self.write_u8(Self::REG_LBA1, 0);
            self.write_u8(Self::REG_LBA2, 0);
            self.write_u8(Self::REG_STATUS_COMMAND, Self::CMD_IDENTIFY);

            if self.read_u8(Self::REG_STATUS_COMMAND) == 0 {
                return Err(DiskIoError::NotPresent);
            }

            self.wait_not_busy()?;
            // ATAPI/SATA packet devices can show non-zero here in IDENTIFY.
            if self.read_u8(Self::REG_LBA1) != 0 || self.read_u8(Self::REG_LBA2) != 0 {
                return Err(DiskIoError::InvalidInput);
            }
            self.wait_drq()?;

            let mut id = [0u16; 256];
            for word in &mut id {
                *word = self.read_data_u16();
            }

            let lba28 = ((id[61] as u32) << 16) | (id[60] as u32);
            let lba48_supported = (id[83] & (1 << 10)) != 0;
            let lba48 = ((id[103] as u64) << 48)
                | ((id[102] as u64) << 32)
                | ((id[101] as u64) << 16)
                | (id[100] as u64);

            self.lba48 = lba48_supported && lba48 > 0;
            self.total_sectors = if self.lba48 { lba48 } else { lba28 as u64 };
            if self.total_sectors == 0 {
                return Err(DiskIoError::NotPresent);
            }
            Ok(())
        })
    }

    fn read_sector_lba28(&mut self, lba: u64, out: &mut [u8; FAT_SECTOR_SIZE]) -> IoResult<()> {
        self.select_drive_lba28(lba)?;
        self.write_u8(Self::REG_SECTOR_COUNT, 1);
        self.write_u8(Self::REG_LBA0, (lba & 0xFF) as u8);
        self.write_u8(Self::REG_LBA1, ((lba >> 8) & 0xFF) as u8);
        self.write_u8(Self::REG_LBA2, ((lba >> 16) & 0xFF) as u8);
        self.write_u8(Self::REG_STATUS_COMMAND, Self::CMD_READ_SECTORS);
        self.wait_drq()?;

        for i in 0..(FAT_SECTOR_SIZE / 2) {
            let v = self.read_data_u16();
            out[i * 2] = (v & 0x00FF) as u8;
            out[i * 2 + 1] = (v >> 8) as u8;
        }
        Ok(())
    }

    fn write_sector_lba28(&mut self, lba: u64, input: &[u8; FAT_SECTOR_SIZE]) -> IoResult<()> {
        self.select_drive_lba28(lba)?;
        self.write_u8(Self::REG_SECTOR_COUNT, 1);
        self.write_u8(Self::REG_LBA0, (lba & 0xFF) as u8);
        self.write_u8(Self::REG_LBA1, ((lba >> 8) & 0xFF) as u8);
        self.write_u8(Self::REG_LBA2, ((lba >> 16) & 0xFF) as u8);
        self.write_u8(Self::REG_STATUS_COMMAND, Self::CMD_WRITE_SECTORS);
        self.wait_drq()?;

        for i in 0..(FAT_SECTOR_SIZE / 2) {
            let lo = input[i * 2] as u16;
            let hi = (input[i * 2 + 1] as u16) << 8;
            self.write_data_u16(lo | hi);
        }
        self.write_u8(Self::REG_STATUS_COMMAND, Self::CMD_FLUSH_CACHE);
        let _ = self.wait_not_busy()?;
        Ok(())
    }

    fn program_lba48_regs(&mut self, lba: u64) {
        self.write_u8(Self::REG_SECTOR_COUNT, 0);
        self.write_u8(Self::REG_LBA0, ((lba >> 24) & 0xFF) as u8);
        self.write_u8(Self::REG_LBA1, ((lba >> 32) & 0xFF) as u8);
        self.write_u8(Self::REG_LBA2, ((lba >> 40) & 0xFF) as u8);

        self.write_u8(Self::REG_SECTOR_COUNT, 1);
        self.write_u8(Self::REG_LBA0, (lba & 0xFF) as u8);
        self.write_u8(Self::REG_LBA1, ((lba >> 8) & 0xFF) as u8);
        self.write_u8(Self::REG_LBA2, ((lba >> 16) & 0xFF) as u8);
    }

    fn read_sector_lba48(&mut self, lba: u64, out: &mut [u8; FAT_SECTOR_SIZE]) -> IoResult<()> {
        self.select_drive_base();
        self.program_lba48_regs(lba);
        self.write_u8(Self::REG_STATUS_COMMAND, Self::CMD_READ_SECTORS_EXT);
        self.wait_drq()?;
        for i in 0..(FAT_SECTOR_SIZE / 2) {
            let v = self.read_data_u16();
            out[i * 2] = (v & 0x00FF) as u8;
            out[i * 2 + 1] = (v >> 8) as u8;
        }
        Ok(())
    }

    fn write_sector_lba48(&mut self, lba: u64, input: &[u8; FAT_SECTOR_SIZE]) -> IoResult<()> {
        self.select_drive_base();
        self.program_lba48_regs(lba);
        self.write_u8(Self::REG_STATUS_COMMAND, Self::CMD_WRITE_SECTORS_EXT);
        self.wait_drq()?;
        for i in 0..(FAT_SECTOR_SIZE / 2) {
            let lo = input[i * 2] as u16;
            let hi = (input[i * 2 + 1] as u16) << 8;
            self.write_data_u16(lo | hi);
        }
        self.write_u8(Self::REG_STATUS_COMMAND, Self::CMD_FLUSH_CACHE_EXT);
        let _ = self.wait_not_busy()?;
        Ok(())
    }
}

impl BlockDevice for AtaPioDevice {
    fn sector_count(&self) -> u64 {
        self.total_sectors
    }

    fn read_sector(&mut self, lba: u64, out: &mut [u8; FAT_SECTOR_SIZE]) -> IoResult<()> {
        if lba >= self.total_sectors {
            return Err(DiskIoError::InvalidInput);
        }

        interrupts::without_interrupts(|| {
            if self.lba48 {
                self.read_sector_lba48(lba, out)
            } else {
                self.read_sector_lba28(lba, out)
            }
        })
    }

    fn write_sector(&mut self, lba: u64, input: &[u8; FAT_SECTOR_SIZE]) -> IoResult<()> {
        if lba >= self.total_sectors {
            return Err(DiskIoError::InvalidInput);
        }

        interrupts::without_interrupts(|| {
            if self.lba48 {
                self.write_sector_lba48(lba, input)
            } else {
                self.write_sector_lba28(lba, input)
            }
        })
    }

    fn flush(&mut self) -> IoResult<()> {
        interrupts::without_interrupts(|| {
            self.write_u8(
                Self::REG_STATUS_COMMAND,
                if self.lba48 {
                    Self::CMD_FLUSH_CACHE_EXT
                } else {
                    Self::CMD_FLUSH_CACHE
                },
            );
            let _ = self.wait_not_busy()?;
            Ok(())
        })
    }
}

/// Wraps a sector device and exposes byte-stream IO required by `fatfs`.
pub struct FatDisk<D: BlockDevice> {
    dev: D,
    pos: u64,
    scratch: [u8; FAT_SECTOR_SIZE],
}

impl<D: BlockDevice> FatDisk<D> {
    pub fn new(dev: D) -> Self {
        Self {
            dev,
            pos: 0,
            scratch: [0; FAT_SECTOR_SIZE],
        }
    }

    fn bytes_len(&self) -> u64 {
        self.dev
            .sector_count()
            .saturating_mul(FAT_SECTOR_SIZE as u64)
    }

    fn ensure_in_range(&self, pos: u64) -> IoResult<()> {
        if pos <= self.bytes_len() {
            Ok(())
        } else {
            Err(DiskIoError::InvalidInput)
        }
    }
}

impl<D: BlockDevice> IoBase for FatDisk<D> {
    type Error = DiskIoError;
}

impl<D: BlockDevice> Read for FatDisk<D> {
    fn read(&mut self, buf: &mut [u8]) -> IoResult<usize> {
        if buf.is_empty() {
            return Ok(0);
        }

        let disk_len = self.bytes_len();
        if self.pos >= disk_len {
            return Ok(0);
        }

        let max_read = min(buf.len() as u64, disk_len - self.pos) as usize;
        let mut done = 0usize;

        while done < max_read {
            let lba = self.pos / FAT_SECTOR_SIZE as u64;
            let off = (self.pos as usize) % FAT_SECTOR_SIZE;

            self.dev.read_sector(lba, &mut self.scratch)?;

            let n = min(FAT_SECTOR_SIZE - off, max_read - done);
            buf[done..done + n].copy_from_slice(&self.scratch[off..off + n]);

            self.pos += n as u64;
            done += n;
        }

        Ok(done)
    }
}

impl<D: BlockDevice> Write for FatDisk<D> {
    fn write(&mut self, buf: &[u8]) -> IoResult<usize> {
        if buf.is_empty() {
            return Ok(0);
        }

        let disk_len = self.bytes_len();
        if self.pos >= disk_len {
            return Ok(0);
        }

        let max_write = min(buf.len() as u64, disk_len - self.pos) as usize;
        let mut done = 0usize;

        while done < max_write {
            let lba = self.pos / FAT_SECTOR_SIZE as u64;
            let off = (self.pos as usize) % FAT_SECTOR_SIZE;
            let remaining = max_write - done;
            let n = min(FAT_SECTOR_SIZE - off, remaining);

            if off == 0 && n == FAT_SECTOR_SIZE {
                self.scratch
                    .copy_from_slice(&buf[done..done + FAT_SECTOR_SIZE]);
            } else {
                self.dev.read_sector(lba, &mut self.scratch)?;
                self.scratch[off..off + n].copy_from_slice(&buf[done..done + n]);
            }

            self.dev.write_sector(lba, &self.scratch)?;
            self.pos += n as u64;
            done += n;
        }

        Ok(done)
    }

    fn flush(&mut self) -> IoResult<()> {
        self.dev.flush()
    }
}

impl<D: BlockDevice> Seek for FatDisk<D> {
    fn seek(&mut self, pos: SeekFrom) -> IoResult<u64> {
        let len = self.bytes_len() as i128;
        let cur = self.pos as i128;

        let next = match pos {
            SeekFrom::Start(p) => p as i128,
            SeekFrom::End(delta) => len
                .checked_add(delta as i128)
                .ok_or(DiskIoError::InvalidInput)?,
            SeekFrom::Current(delta) => cur
                .checked_add(delta as i128)
                .ok_or(DiskIoError::InvalidInput)?,
        };

        if next < 0 {
            return Err(DiskIoError::InvalidInput);
        }

        let next_u64 = next as u64;
        self.ensure_in_range(next_u64)?;
        self.pos = next_u64;
        Ok(self.pos)
    }
}

type BootVolumeDevice = LbaSliceDevice<AtaPioDevice>;
type BootVolumeDisk = FatDisk<BootVolumeDevice>;
type BootVolumeFs = fatfs::FileSystem<BootVolumeDisk>;
type AtaBootVolumeFile<'a> =
    fatfs::File<'a, BootVolumeDisk, fatfs::DefaultTimeProvider, fatfs::LossyOemCpConverter>;

fn open_ata_boot_volume() -> core::result::Result<BootVolumeFs, fatfs::Error<DiskIoError>> {
    let dev = AtaPioDevice::primary_master()
        .or_else(|_| AtaPioDevice::primary_slave())
        .or_else(|_| AtaPioDevice::secondary_master())
        .or_else(|_| AtaPioDevice::secondary_slave());
    let mut dev = match dev {
        Ok(dev) => dev,
        Err(_) => return Err(fatfs::Error::Io(DiskIoError::NotPresent)),
    };

    let (start_lba, sectors) = detect_fat_volume_slice(&mut dev)?;
    let disk_dev = LbaSliceDevice::new(dev, start_lba, sectors)?;
    let disk = FatDisk::new(disk_dev);
    fatfs::FileSystem::new(disk, fatfs::FsOptions::new())
}

pub(crate) enum BootVolumeFile<'a> {
    Cached(CachedBootFile),
    Fat(AtaBootVolumeFile<'a>),
}

impl IoBase for BootVolumeFile<'_> {
    type Error = fatfs::Error<DiskIoError>;
}

impl Read for BootVolumeFile<'_> {
    fn read(&mut self, buf: &mut [u8]) -> core::result::Result<usize, fatfs::Error<DiskIoError>> {
        match self {
            Self::Cached(file) => {
                if buf.is_empty() || file.pos >= file.data.len() {
                    return Ok(0);
                }

                let read = min(buf.len(), file.data.len() - file.pos);
                buf[..read].copy_from_slice(&file.data[file.pos..file.pos + read]);
                file.pos += read;
                Ok(read)
            }
            Self::Fat(file) => file.read(buf),
        }
    }
}

impl Seek for BootVolumeFile<'_> {
    fn seek(&mut self, pos: SeekFrom) -> core::result::Result<u64, fatfs::Error<DiskIoError>> {
        match self {
            Self::Cached(file) => {
                let len = file.data.len() as i128;
                let cur = file.pos as i128;
                let next = match pos {
                    SeekFrom::Start(offset) => offset as i128,
                    SeekFrom::End(delta) => len
                        .checked_add(delta as i128)
                        .ok_or(fatfs::Error::InvalidInput)?,
                    SeekFrom::Current(delta) => cur
                        .checked_add(delta as i128)
                        .ok_or(fatfs::Error::InvalidInput)?,
                };
                if next < 0 || next > len {
                    return Err(fatfs::Error::InvalidInput);
                }

                file.pos = next as usize;
                Ok(next as u64)
            }
            Self::Fat(file) => file.seek(pos),
        }
    }
}

pub(crate) struct BootVolume {
    cached: Option<CachedBootVolume>,
    fs: Option<BootVolumeFs>,
}

impl BootVolume {
    pub(crate) fn open() -> core::result::Result<Self, fatfs::Error<DiskIoError>> {
        let cached = CachedBootVolume::from_boot_info()
            .filter(|volume| boot_file_entries(&volume.manifest).is_some());
        if cached.is_some() {
            return Ok(Self { cached, fs: None });
        }

        let fs = open_ata_boot_volume()?;
        Ok(Self {
            cached: None,
            fs: Some(fs),
        })
    }

    pub(crate) fn open_file(
        &self,
        path: &str,
    ) -> core::result::Result<BootVolumeFile<'_>, fatfs::Error<DiskIoError>> {
        let normalized_path = normalize_fat_path(path);
        if let Some(cached) = self.cached {
            return cached
                .open_file(normalized_path.as_str())
                .map(BootVolumeFile::Cached)
                .ok_or(fatfs::Error::NotFound);
        }

        let fs = self
            .fs
            .as_ref()
            .ok_or(fatfs::Error::Io(DiskIoError::NotPresent))?;
        let root = fs.root_dir();
        root.open_file(normalized_path.as_str())
            .map(BootVolumeFile::Fat)
    }

    pub(crate) fn close(self) -> core::result::Result<(), fatfs::Error<DiskIoError>> {
        match self.fs {
            Some(fs) => fs.unmount(),
            None => Ok(()),
        }
    }
}

#[allow(dead_code)]
pub(crate) fn read_file_to_vec(
    path: &str,
) -> core::result::Result<Vec<u8>, fatfs::Error<DiskIoError>> {
    let volume = BootVolume::open()?;
    let result = {
        let mut file = volume.open_file(path)?;
        let file_len = file.seek(SeekFrom::End(0))?;
        let capacity =
            usize::try_from(file_len).map_err(|_| fatfs::Error::Io(DiskIoError::InvalidInput))?;
        file.seek(SeekFrom::Start(0))?;

        let mut bytes = Vec::with_capacity(capacity);
        let mut chunk = [0_u8; 4096];
        loop {
            let read = file.read(&mut chunk)?;
            if read == 0 {
                break;
            }
            bytes.extend_from_slice(&chunk[..read]);
        }

        Ok(bytes)
    };

    match (result, volume.close()) {
        (Ok(value), Ok(())) => Ok(value),
        (Ok(_), Err(err)) => Err(err),
        (Err(err), _) => Err(err),
    }
}

fn normalize_fat_path(path: &str) -> String {
    let mut normalized = String::with_capacity(path.len());
    for component in path.split(['/', '\\']) {
        if component.is_empty() || component == "." {
            continue;
        }
        if !normalized.is_empty() {
            normalized.push('/');
        }
        normalized.push_str(component);
    }
    normalized
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fat_boot_sector(total_sectors: u16, bytes_per_sector: u16) -> [u8; FAT_SECTOR_SIZE] {
        let mut sector = [0_u8; FAT_SECTOR_SIZE];
        sector[0] = 0xEB;
        sector[2] = 0x90;
        sector[11..13].copy_from_slice(&bytes_per_sector.to_le_bytes());
        sector[13] = 1;
        sector[14..16].copy_from_slice(&1_u16.to_le_bytes());
        sector[16] = 2;
        sector[17..19].copy_from_slice(&32_u16.to_le_bytes());
        sector[19..21].copy_from_slice(&total_sectors.to_le_bytes());
        sector[21] = 0xF8;
        sector[22..24].copy_from_slice(&1_u16.to_le_bytes());
        sector[510] = 0x55;
        sector[511] = 0xAA;
        sector
    }

    #[test]
    fn detects_superfloppy_volume_at_lba_zero() {
        let mut disk = MemBlockDevice::new_zeroed(8);
        let sector = fat_boot_sector(8, FAT_SECTOR_SIZE as u16);
        disk.write_sector(0, &sector).expect("write sector 0");

        assert_eq!(detect_fat_volume_slice(&mut disk), Ok((0, 8)));
    }

    #[test]
    fn detects_partitioned_volume_from_mbr_entry() {
        let mut disk = MemBlockDevice::new_zeroed(128);
        let mut mbr = [0_u8; FAT_SECTOR_SIZE];
        let partition_offset = 446;
        mbr[partition_offset + 4] = 0x0C;
        mbr[partition_offset + 8..partition_offset + 12].copy_from_slice(&32_u32.to_le_bytes());
        mbr[partition_offset + 12..partition_offset + 16].copy_from_slice(&64_u32.to_le_bytes());
        mbr[510] = 0x55;
        mbr[511] = 0xAA;
        disk.write_sector(0, &mbr).expect("write MBR");
        let boot_sector = fat_boot_sector(64, FAT_SECTOR_SIZE as u16);
        disk.write_sector(32, &boot_sector)
            .expect("write partition boot sector");

        assert_eq!(detect_fat_volume_slice(&mut disk), Ok((32, 64)));
    }

    #[test]
    fn rejects_empty_device() {
        let mut disk = MemBlockDevice::new_zeroed(0);
        assert_eq!(
            detect_fat_volume_slice(&mut disk),
            Err(DiskIoError::NotPresent)
        );
    }

    #[test]
    fn normalize_fat_path_unifies_separators() {
        assert_eq!(
            normalize_fat_path("//EFI\\\\BOOT/./BOOTX64.EFI"),
            "EFI/BOOT/BOOTX64.EFI"
        );
        assert_eq!(normalize_fat_path("/kernel.elf"), "kernel.elf");
    }

    #[test]
    fn detects_partitioned_volume_from_gpt_entry() {
        let mut disk = MemBlockDevice::new_zeroed(256);

        let mut protective_mbr = [0_u8; FAT_SECTOR_SIZE];
        protective_mbr[446 + 4] = 0xEE;
        protective_mbr[446 + 8..446 + 12].copy_from_slice(&1_u32.to_le_bytes());
        protective_mbr[446 + 12..446 + 16].copy_from_slice(&255_u32.to_le_bytes());
        protective_mbr[510] = 0x55;
        protective_mbr[511] = 0xAA;
        disk.write_sector(0, &protective_mbr)
            .expect("write protective MBR");

        let mut gpt_header = [0_u8; FAT_SECTOR_SIZE];
        gpt_header[..8].copy_from_slice(b"EFI PART");
        gpt_header[8..12].copy_from_slice(&0x0001_0000_u32.to_le_bytes());
        gpt_header[12..16].copy_from_slice(&92_u32.to_le_bytes());
        gpt_header[72..80].copy_from_slice(&2_u64.to_le_bytes());
        gpt_header[80..84].copy_from_slice(&1_u32.to_le_bytes());
        gpt_header[84..88].copy_from_slice(&128_u32.to_le_bytes());
        disk.write_sector(1, &gpt_header).expect("write GPT header");

        let mut entry_sector = [0_u8; FAT_SECTOR_SIZE];
        entry_sector[0] = 1;
        entry_sector[32..40].copy_from_slice(&40_u64.to_le_bytes());
        entry_sector[40..48].copy_from_slice(&103_u64.to_le_bytes());
        disk.write_sector(2, &entry_sector)
            .expect("write GPT entry sector");

        let boot_sector = fat_boot_sector(64, FAT_SECTOR_SIZE as u16);
        disk.write_sector(40, &boot_sector)
            .expect("write GPT partition boot sector");

        assert_eq!(detect_fat_volume_slice(&mut disk), Ok((40, 64)));
    }

    #[test]
    fn accepts_fat_boot_sector_with_4k_logical_sectors() {
        let sector = fat_boot_sector(128, 4096);
        assert!(is_probable_fat_boot_sector(&sector, 128));
    }

    #[test]
    fn cached_paths_match_case_insensitively() {
        assert!(fat_paths_match(
            "efi/boot/bootx64.efi",
            "EFI/BOOT/BOOTX64.EFI"
        ));
    }
}
