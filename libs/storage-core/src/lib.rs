//! Shared block-range and partition-metadata admission.
//!
//! - **Owner:** storage services choose devices and mounts; this library validates bytes.
//! - **Boundary:** block geometry, FAT BPBs and GPT records are untrusted inputs.
//! - **Lifecycle:** validate complete ranges and checksums before exposing partitions.
//! - **Concurrency:** callers serialize device access; this library owns no global state.
//! - **Failure:** malformed, overflowing or overlapping metadata fails closed.
//! - **Evidence:** native parser tests and indexed bounded Kani block-range proofs;
//!   neither establishes physical-device durability or DVM lifetime correctness.
#![no_std]

extern crate alloc;
#[cfg(test)]
extern crate std;

pub mod file_io;

use alloc::boxed::Box;
use alloc::vec;
use alloc::vec::Vec;
use fatfs::IoError;

pub type IoResult<T> = core::result::Result<T, StorageError>;
const MAX_GPT_PARTITION_ENTRIES: usize = 4096;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum StorageError {
    Interrupted,
    WouldBlock,
    UnexpectedEof,
    WriteZero,
    InvalidInput,
    Timeout,
    DeviceFault,
    NotPresent,
    Unsupported,
}

impl IoError for StorageError {
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

pub trait BlockDevice: Send {
    fn logical_block_size(&self) -> usize;
    fn block_count(&self) -> u64;
    fn read_blocks(&mut self, lba: u64, out: &mut [u8]) -> IoResult<()>;
    fn write_blocks(&mut self, lba: u64, input: &[u8]) -> IoResult<()>;
    fn flush(&mut self) -> IoResult<()> {
        Ok(())
    }
}

impl<T: BlockDevice + ?Sized> BlockDevice for Box<T> {
    fn logical_block_size(&self) -> usize {
        (**self).logical_block_size()
    }

    fn block_count(&self) -> u64 {
        (**self).block_count()
    }

    fn read_blocks(&mut self, lba: u64, out: &mut [u8]) -> IoResult<()> {
        (**self).read_blocks(lba, out)
    }

    fn write_blocks(&mut self, lba: u64, input: &[u8]) -> IoResult<()> {
        (**self).write_blocks(lba, input)
    }

    fn flush(&mut self) -> IoResult<()> {
        (**self).flush()
    }
}

impl<T: BlockDevice + ?Sized> BlockDevice for &mut T {
    fn logical_block_size(&self) -> usize {
        (**self).logical_block_size()
    }

    fn block_count(&self) -> u64 {
        (**self).block_count()
    }

    fn read_blocks(&mut self, lba: u64, out: &mut [u8]) -> IoResult<()> {
        (**self).read_blocks(lba, out)
    }

    fn write_blocks(&mut self, lba: u64, input: &[u8]) -> IoResult<()> {
        (**self).write_blocks(lba, input)
    }

    fn flush(&mut self) -> IoResult<()> {
        (**self).flush()
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PartitionInfo {
    pub start_lba: u64,
    pub block_count: u64,
}

pub struct BlockSlice<D: BlockDevice> {
    inner: D,
    start_lba: u64,
    block_count: u64,
}

impl<D: BlockDevice> BlockSlice<D> {
    pub fn new(inner: D, start_lba: u64, block_count: u64) -> IoResult<Self> {
        if block_count == 0 {
            return Err(StorageError::InvalidInput);
        }
        let end = start_lba
            .checked_add(block_count)
            .ok_or(StorageError::InvalidInput)?;
        if end > inner.block_count() {
            return Err(StorageError::InvalidInput);
        }
        Ok(Self {
            inner,
            start_lba,
            block_count,
        })
    }

    pub fn into_inner(self) -> D {
        self.inner
    }
}

impl<D: BlockDevice> BlockDevice for BlockSlice<D> {
    fn logical_block_size(&self) -> usize {
        self.inner.logical_block_size()
    }

    fn block_count(&self) -> u64 {
        self.block_count
    }

    fn read_blocks(&mut self, lba: u64, out: &mut [u8]) -> IoResult<()> {
        validate_block_io(self.logical_block_size(), lba, self.block_count, out.len())?;
        self.inner.read_blocks(self.start_lba + lba, out)
    }

    fn write_blocks(&mut self, lba: u64, input: &[u8]) -> IoResult<()> {
        validate_block_io(
            self.logical_block_size(),
            lba,
            self.block_count,
            input.len(),
        )?;
        self.inner.write_blocks(self.start_lba + lba, input)
    }

    fn flush(&mut self) -> IoResult<()> {
        self.inner.flush()
    }
}

pub struct MemBlockDevice {
    block_size: usize,
    data: Vec<u8>,
}

impl MemBlockDevice {
    pub fn new_zeroed(block_size: usize, block_count: u64) -> Self {
        assert!(block_size >= 512);
        let len = block_size.saturating_mul(block_count as usize);
        Self {
            block_size,
            data: vec![0; len],
        }
    }

    pub fn from_bytes(block_size: usize, data: Vec<u8>) -> IoResult<Self> {
        if block_size < 512 || !data.len().is_multiple_of(block_size) {
            return Err(StorageError::InvalidInput);
        }
        Ok(Self { block_size, data })
    }

    pub fn as_bytes(&self) -> &[u8] {
        &self.data
    }

    pub fn as_bytes_mut(&mut self) -> &mut [u8] {
        &mut self.data
    }

    fn bounds(&self, lba: u64, len: usize) -> IoResult<(usize, usize)> {
        let start = (lba as usize)
            .checked_mul(self.block_size)
            .ok_or(StorageError::InvalidInput)?;
        let end = start.checked_add(len).ok_or(StorageError::InvalidInput)?;
        if end > self.data.len() {
            return Err(StorageError::InvalidInput);
        }
        Ok((start, end))
    }
}

impl BlockDevice for MemBlockDevice {
    fn logical_block_size(&self) -> usize {
        self.block_size
    }

    fn block_count(&self) -> u64 {
        (self.data.len() / self.block_size) as u64
    }

    fn read_blocks(&mut self, lba: u64, out: &mut [u8]) -> IoResult<()> {
        validate_block_io(self.block_size, lba, self.block_count(), out.len())?;
        let (start, end) = self.bounds(lba, out.len())?;
        out.copy_from_slice(&self.data[start..end]);
        Ok(())
    }

    fn write_blocks(&mut self, lba: u64, input: &[u8]) -> IoResult<()> {
        validate_block_io(self.block_size, lba, self.block_count(), input.len())?;
        let (start, end) = self.bounds(lba, input.len())?;
        self.data[start..end].copy_from_slice(input);
        Ok(())
    }
}

pub fn detect_partitions<D: BlockDevice>(dev: &mut D) -> IoResult<Vec<PartitionInfo>> {
    let block_size = dev.logical_block_size();
    if block_size < 512 {
        return Err(StorageError::InvalidInput);
    }
    let total_blocks = dev.block_count();
    if total_blocks == 0 {
        return Err(StorageError::NotPresent);
    }

    let mut block0 = vec![0_u8; block_size];
    dev.read_blocks(0, &mut block0)?;
    let mbr = &block0[..512];

    if let Some(partitions) = detect_gpt_partitions(dev, total_blocks, block_size, mbr)? {
        return Ok(partitions);
    }

    if mbr[510..512] != [0x55, 0xaa] {
        return Ok(Vec::new());
    }

    let mut partitions = Vec::new();
    for idx in 0..4 {
        let off = 446 + idx * 16;
        let start_lba = le_u32(mbr, off + 8) as u64;
        let block_count = le_u32(mbr, off + 12) as u64;
        if start_lba == 0 || block_count == 0 {
            continue;
        }
        let end = start_lba
            .checked_add(block_count)
            .ok_or(StorageError::InvalidInput)?;
        if end > total_blocks {
            continue;
        }
        partitions.push(PartitionInfo {
            start_lba,
            block_count,
        });
    }

    Ok(partitions)
}

pub fn detect_fat_boot_partition<D: BlockDevice>(dev: &mut D) -> IoResult<Option<PartitionInfo>> {
    let block_size = dev.logical_block_size();
    if block_size < 512 {
        return Err(StorageError::InvalidInput);
    }
    let total_blocks = dev.block_count();
    if total_blocks == 0 {
        return Err(StorageError::NotPresent);
    }

    let mut block0 = vec![0_u8; block_size];
    dev.read_blocks(0, &mut block0)?;
    if parse_fat_volume_metadata(&block0, u128::from(total_blocks) * block_size as u128).is_some() {
        return Ok(Some(PartitionInfo {
            start_lba: 0,
            block_count: total_blocks,
        }));
    }

    for partition in detect_partitions(dev)? {
        let mut slice = BlockSlice::new(&mut *dev, partition.start_lba, partition.block_count)?;
        let mut boot_sector = vec![0_u8; block_size];
        slice.read_blocks(0, &mut boot_sector)?;
        if parse_fat_volume_metadata(
            &boot_sector,
            u128::from(partition.block_count) * block_size as u128,
        ).is_some() {
            return Ok(Some(partition));
        }
    }

    Ok(None)
}

pub fn fat_volume_id_from_boot_sector(boot_sector: &[u8]) -> Option<u32> {
    parse_fat_volume_metadata(boot_sector, u128::MAX).map(|metadata| metadata.volume_id)
}

// UEFI GPT header and entry array, section 5:
// https://github.com/UEFI/UEFI-Specification-Release/blob/main/source/05_GUID_Partition_Table_Format.rst
const MAX_GPT_ARRAY_BYTES: usize = 16 * 1024 * 1024;

fn crc32_update(mut crc: u32, bytes: &[u8]) -> u32 {
    for &byte in bytes {
        crc ^= u32::from(byte);
        for _ in 0..8 {
            crc = (crc >> 1) ^ (0xedb8_8320 & (crc & 1).wrapping_neg());
        }
    }
    crc
}

fn detect_gpt_partitions<D: BlockDevice>(
    dev: &mut D, total_blocks: u64, block_size: usize, mbr: &[u8],
) -> IoResult<Option<Vec<PartitionInfo>>> {
    if total_blocks <= 1 { return Ok(None); }
    let protective = mbr[510..512] == [0x55, 0xaa]
        && (0..4).any(|i| mbr[446 + i * 16 + 4] == 0xee);
    let mut header = vec![0_u8; block_size];
    dev.read_blocks(1, &mut header)?;
    if !protective && &header[..8] != b"EFI PART" { return Ok(None); }

    match read_gpt_copy(dev, total_blocks, block_size, 1, header) {
        Ok(parts) => Ok(Some(parts)),
        Err(StorageError::InvalidInput) => {
            let mut backup = vec![0_u8; block_size];
            dev.read_blocks(total_blocks - 1, &mut backup)?;
            read_gpt_copy(dev, total_blocks, block_size, total_blocks - 1, backup).map(Some)
        }
        Err(error) => Err(error),
    }
}

fn read_gpt_copy<D: BlockDevice>(
    dev: &mut D, total_blocks: u64, block_size: usize, header_lba: u64,
    mut header: Vec<u8>,
) -> IoResult<Vec<PartitionInfo>> {
    let invalid = StorageError::InvalidInput;
    if &header[..8] != b"EFI PART" || le_u32(&header, 8) != 0x0001_0000
        || le_u32(&header, 20) != 0 || le_u64(&header, 24) != header_lba
        || le_u64(&header, 32) != if header_lba == 1 { total_blocks - 1 } else { 1 }
    { return Err(invalid); }
    let header_size = le_u32(&header, 12) as usize;
    if !(92..=block_size).contains(&header_size) { return Err(invalid); }
    let header_crc = le_u32(&header, 16);
    header[16..20].fill(0);
    if !crc32_update(!0, &header[..header_size]) != header_crc { return Err(invalid); }

    let first = le_u64(&header, 40);
    let last = le_u64(&header, 48);
    let entry_lba = le_u64(&header, 72);
    let count = le_u32(&header, 80) as usize;
    let entry_size = le_u32(&header, 84) as usize;
    if count == 0 || count > MAX_GPT_PARTITION_ENTRIES || entry_size < 128
        || !entry_size.is_multiple_of(8) || first < 2 || first > last
        || last >= total_blocks - 1
    { return Err(invalid); }
    // Older UEFI revisions permitted any multiple of eight; parse these as a
    // contiguous byte array too, including entries crossing block boundaries.
    let bytes = count.checked_mul(entry_size).ok_or(invalid)?;
    if bytes > MAX_GPT_ARRAY_BYTES { return Err(invalid); }
    let blocks = bytes.div_ceil(block_size);
    let end = entry_lba.checked_add(blocks as u64).ok_or(invalid)?;
    if (header_lba == 1 && (entry_lba < 2 || end > first))
        || (header_lba != 1 && (entry_lba <= last || end > header_lba))
    { return Err(invalid); }
    let mut data = vec![0; blocks.checked_mul(block_size).ok_or(invalid)?];
    for (i, block) in data.chunks_exact_mut(block_size).enumerate() {
        dev.read_blocks(entry_lba + i as u64, block)?;
    }
    if !crc32_update(!0, &data[..bytes]) != le_u32(&header, 88) { return Err(invalid); }
    let mut partitions = Vec::new();
    for entry in data[..bytes].chunks_exact(entry_size) {
        if entry[..16].iter().all(|&byte| byte == 0) { continue; }
        let start = le_u64(entry, 32);
        let end = le_u64(entry, 40);
        if start < first || end > last || start > end { return Err(invalid); }
        let part = PartitionInfo { start_lba: start, block_count: end - start + 1 };
        if partitions.iter().any(|existing: &PartitionInfo| {
            start < existing.start_lba + existing.block_count && existing.start_lba <= end
        }) { return Err(invalid); }
        partitions.push(part);
    }
    Ok(partitions)
}

#[derive(Clone, Copy)]
struct FatVolumeMetadata {
    volume_id: u32,
}

fn parse_fat_volume_metadata(
    boot_sector: &[u8],
    available_bytes: u128,
) -> Option<FatVolumeMetadata> {
    if boot_sector.len() < 512 {
        return None;
    }

    let has_jump = matches!(boot_sector[0], 0xEB | 0xE9);
    let sig_ok = boot_sector[510] == 0x55 && boot_sector[511] == 0xAA;
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
    let total_blocks = match (total_sectors16, total_sectors32) {
        (0, 0) => return None,
        (0, total) => total,
        (total, 0) => total as u32,
        (total16, total32) => (total16 as u32).min(total32),
    };
    if total_blocks == 0
        || u128::from(total_blocks) * u128::from(bytes_per_sector) > available_bytes
    {
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
    if first_data_sector >= total_blocks {
        return None;
    }

    let data_sectors = total_blocks - first_data_sector;
    let cluster_count = data_sectors / sectors_per_cluster as u32;
    if cluster_count == 0 {
        return None;
    }

    let fat_type = if cluster_count < 4_085 {
        12
    } else if cluster_count < 65_525 {
        16
    } else {
        32
    };

    let volume_id = if fat_type == 32 {
        if root_entry_count != 0 || sectors_per_fat32 == 0 || le_u32(boot_sector, 44) < 2 {
            return None;
        }
        le_u32(boot_sector, 67)
    } else {
        if root_entry_count == 0 || sectors_per_fat16 == 0 {
            return None;
        }
        le_u32(boot_sector, 39)
    };

    Some(FatVolumeMetadata { volume_id })
}

fn validate_block_io(block_size: usize, lba: u64, total_blocks: u64, len: usize) -> IoResult<()> {
    if block_size < 512 || len == 0 || !len.is_multiple_of(block_size) {
        return Err(StorageError::InvalidInput);
    }
    let blocks = (len / block_size) as u64;
    let end = lba.checked_add(blocks).ok_or(StorageError::InvalidInput)?;
    if end > total_blocks {
        return Err(StorageError::InvalidInput);
    }
    Ok(())
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

#[cfg(kani)]
mod proofs {
    use super::*;

    #[kani::proof]
    fn valid_block_io_never_exceeds_device() {
        let lba: u64 = kani::any();
        let total: u64 = kani::any();
        let len: usize = kani::any();
        // Both supported sector geometries, with symbolic full-width ranges.
        let block_size = if kani::any::<bool>() { 512 } else { 4096 };
        let result = validate_block_io(block_size, lba, total, len);
        if result.is_ok() {
            assert!(len > 0 && len % block_size == 0);
            assert!(lba <= total);
            assert!((len / block_size) as u64 <= total - lba);
        }
        if lba == 0 && total == 1 && len == block_size {
            assert!(result.is_ok());
        }
        kani::cover!(result.is_ok() && block_size == 512);
        kani::cover!(result.is_ok() && block_size == 4096);
        kani::cover!(result.is_err() && lba == u64::MAX && len == block_size);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn block_io_admission_rejects_wrap_misalignment_and_device_overrun() {
        for sector in [512, 4096] {
            assert_eq!(validate_block_io(sector, 0, 1, sector), Ok(()));
            assert_eq!(validate_block_io(sector, 1, 2, sector), Ok(()));
            for (lba, total, len) in [
                (0, 1, 0), (0, 1, sector - 1), (1, 1, sector),
                (u64::MAX, u64::MAX, sector),
            ] {
                assert_eq!(validate_block_io(sector, lba, total, len), Err(StorageError::InvalidInput));
            }
        }
    }

    fn fat_boot_sector(total_sectors: u16, bytes_per_sector: u16) -> [u8; 512] {
        let mut sector = [0_u8; 512];
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
    fn detects_superfloppy_volume() {
        let mut disk = MemBlockDevice::new_zeroed(512, 8);
        let sector = fat_boot_sector(8, 512);
        disk.write_blocks(0, &sector).expect("write sector 0");

        assert_eq!(
            detect_fat_boot_partition(&mut disk).expect("detect FAT partition"),
            Some(PartitionInfo {
                start_lba: 0,
                block_count: 8
            })
        );
    }

    #[test]
    fn detects_partitioned_volume_from_mbr() {
        let mut disk = MemBlockDevice::new_zeroed(512, 128);
        let mut mbr = [0_u8; 512];
        mbr[446 + 4] = 0x0C;
        mbr[446 + 8..446 + 12].copy_from_slice(&32_u32.to_le_bytes());
        mbr[446 + 12..446 + 16].copy_from_slice(&64_u32.to_le_bytes());
        mbr[510] = 0x55;
        mbr[511] = 0xAA;
        disk.write_blocks(0, &mbr).expect("write MBR");

        let boot_sector = fat_boot_sector(64, 512);
        disk.write_blocks(32, &boot_sector)
            .expect("write partition boot sector");

        assert_eq!(
            detect_fat_boot_partition(&mut disk).expect("detect FAT partition"),
            Some(PartitionInfo {
                start_lba: 32,
                block_count: 64
            })
        );
    }

    #[test]
    fn parses_4k_boot_sector_volume_id() {
        let mut sector = vec![0_u8; 4096];
        let boot = fat_boot_sector(128, 4096);
        sector[..512].copy_from_slice(&boot);
        sector[39..43].copy_from_slice(&0x1234_5678_u32.to_le_bytes());
        assert_eq!(fat_volume_id_from_boot_sector(&sector), Some(0x1234_5678));
    }

    #[test]
    fn fat_capacity_is_checked_in_bytes_not_device_blocks() {
        let mut small = MemBlockDevice::new_zeroed(512, 8);
        small.write_blocks(0, &fat_boot_sector(8, 4096)).unwrap();
        assert_eq!(detect_fat_boot_partition(&mut small), Ok(None));

        let mut large = MemBlockDevice::new_zeroed(4096, 8);
        let mut block = vec![0; 4096];
        block[..512].copy_from_slice(&fat_boot_sector(64, 512));
        large.write_blocks(0, &block).unwrap();
        assert_eq!(detect_fat_boot_partition(&mut large), Ok(Some(PartitionInfo {
            start_lba: 0,
            block_count: 8,
        })));
    }

    #[test]
    fn ignores_unsigned_mbr_entries() {
        let mut disk = MemBlockDevice::new_zeroed(512, 8);
        let mut block = [0; 512];
        block[454..458].copy_from_slice(&1_u32.to_le_bytes());
        block[458..462].copy_from_slice(&2_u32.to_le_bytes());
        disk.write_blocks(0, &block).unwrap();
        assert_eq!(detect_partitions(&mut disk), Ok(Vec::new()));
    }

    fn gpt_disk() -> MemBlockDevice {
        let mut disk = MemBlockDevice::new_zeroed(512, 100);
        let mut mbr = [0; 512];
        mbr[450] = 0xee;
        mbr[510..512].copy_from_slice(&[0x55, 0xaa]);
        disk.write_blocks(0, &mbr).unwrap();
        // Legacy-permitted 136-byte entries: the fourth crosses a block.
        let mut entries = vec![0; 1024];
        entries[3 * 136] = 1;
        entries[3 * 136 + 32..3 * 136 + 40].copy_from_slice(&10_u64.to_le_bytes());
        entries[3 * 136 + 40..3 * 136 + 48].copy_from_slice(&12_u64.to_le_bytes());
        disk.write_blocks(2, &entries).unwrap();
        disk.write_blocks(97, &entries).unwrap();
        for (at, alternate, array) in [(1_u64, 99_u64, 2_u64), (99, 1, 97)] {
            let mut header = [0; 512];
            header[..8].copy_from_slice(b"EFI PART");
            header[8..12].copy_from_slice(&0x0001_0000_u32.to_le_bytes());
            header[12..16].copy_from_slice(&92_u32.to_le_bytes());
            header[24..32].copy_from_slice(&at.to_le_bytes());
            header[32..40].copy_from_slice(&alternate.to_le_bytes());
            header[40..48].copy_from_slice(&4_u64.to_le_bytes());
            header[48..56].copy_from_slice(&96_u64.to_le_bytes());
            header[72..80].copy_from_slice(&array.to_le_bytes());
            header[80..84].copy_from_slice(&4_u32.to_le_bytes());
            header[84..88].copy_from_slice(&136_u32.to_le_bytes());
            header[88..92].copy_from_slice(&(!crc32_update(!0, &entries[..544])).to_le_bytes());
            let crc = !crc32_update(!0, &header[..92]);
            header[16..20].copy_from_slice(&crc.to_le_bytes());
            disk.write_blocks(at, &header).unwrap();
        }
        disk
    }

    #[test]
    fn gpt_cross_block_entries_and_backup_recovery() {
        let mut disk = gpt_disk();
        let expected = vec![PartitionInfo { start_lba: 10, block_count: 3 }];
        assert_eq!(detect_partitions(&mut disk), Ok(expected.clone()));
        disk.as_bytes_mut()[512 + 16] ^= 1;
        assert_eq!(detect_partitions(&mut disk), Ok(expected));
        disk.as_bytes_mut()[99 * 512 + 16] ^= 1;
        assert_eq!(detect_partitions(&mut disk), Err(StorageError::InvalidInput));
    }

    #[test]
    fn gpt_rejects_array_corruption_and_out_of_range_partitions() {
        let mut disk = gpt_disk();
        disk.as_bytes_mut()[2 * 512 + 1] ^= 1;
        disk.as_bytes_mut()[97 * 512 + 1] ^= 1;
        assert_eq!(detect_partitions(&mut disk), Err(StorageError::InvalidInput));
        let mut disk = gpt_disk();
        // Move the usable start beyond the partition, maintaining valid CRCs.
        for at in [1, 99] {
            let header = &mut disk.as_bytes_mut()[at * 512..(at + 1) * 512];
            header[40..48].copy_from_slice(&11_u64.to_le_bytes());
            header[16..20].fill(0);
            let crc = !crc32_update(!0, &header[..92]);
            header[16..20].copy_from_slice(&crc.to_le_bytes());
        }
        assert_eq!(detect_partitions(&mut disk), Err(StorageError::InvalidInput));
    }

    #[test]
    fn gpt_crc32_known_vector() {
        assert_eq!(!crc32_update(!0, b"123456789"), 0xcbf4_3926);
    }

    #[test]
    fn rejects_gpt_with_excessive_partition_count() {
        let mut disk = MemBlockDevice::new_zeroed(512, 4096);
        let mut mbr = [0_u8; 512];
        mbr[446 + 4] = 0xEE;
        mbr[510] = 0x55;
        mbr[511] = 0xAA;
        disk.write_blocks(0, &mbr).expect("write protective MBR");

        let mut gpt = [0_u8; 512];
        gpt[..8].copy_from_slice(b"EFI PART");
        gpt[80..84].copy_from_slice(&((MAX_GPT_PARTITION_ENTRIES as u32) + 1).to_le_bytes());
        gpt[84..88].copy_from_slice(&(128u32).to_le_bytes());
        disk.write_blocks(1, &gpt).expect("write GPT header");

        assert_eq!(
            detect_partitions(&mut disk),
            Err(StorageError::InvalidInput)
        );
    }
}
