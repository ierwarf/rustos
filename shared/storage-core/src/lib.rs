#![no_std]

extern crate alloc;
#[cfg(test)]
extern crate std;

use alloc::boxed::Box;
use alloc::vec;
use alloc::vec::Vec;
use boot_protocol::BootVolumeIdentity;
use core::cmp::min;
use fatfs::IoError;

pub type IoResult<T> = core::result::Result<T, StorageError>;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum StorageError {
    Interrupted,
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
pub enum TransportKind {
    Ahci,
    Nvme,
    Usb,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PartitionInfo {
    pub start_lba: u64,
    pub block_count: u64,
}

pub struct BootVolumeLocator {
    identity: BootVolumeIdentity,
}

impl BootVolumeLocator {
    pub fn new(identity: BootVolumeIdentity) -> Option<Self> {
        identity.is_present().then_some(Self { identity })
    }

    pub fn identity(&self) -> BootVolumeIdentity {
        self.identity
    }

    pub fn matches_partition<D: BlockDevice>(
        &self,
        dev: &mut D,
        partition: PartitionInfo,
    ) -> IoResult<bool> {
        if partition.start_lba != self.identity.volume_start_lba
            || partition.block_count != self.identity.volume_sector_count
        {
            return Ok(false);
        }

        let mut slice = BlockSlice::new(&mut *dev, partition.start_lba, partition.block_count)?;
        let block_size = slice.logical_block_size();
        if block_size < 512 {
            return Err(StorageError::InvalidInput);
        }
        let mut block = vec![0_u8; block_size];
        slice.read_blocks(0, &mut block)?;
        Ok(fat_volume_id_from_boot_sector(&block) == Some(self.identity.fat_volume_id))
    }
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
        validate_block_io(self.logical_block_size(), lba, self.block_count, input.len())?;
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
        if block_size < 512 || data.len() % block_size != 0 {
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
        let end = start
            .checked_add(len)
            .ok_or(StorageError::InvalidInput)?;
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
    if parse_fat_volume_metadata(&block0, total_blocks).is_some() {
        return Ok(Some(PartitionInfo {
            start_lba: 0,
            block_count: total_blocks,
        }));
    }

    for partition in detect_partitions(dev)? {
        let mut slice = BlockSlice::new(&mut *dev, partition.start_lba, partition.block_count)?;
        let mut boot_sector = vec![0_u8; block_size];
        slice.read_blocks(0, &mut boot_sector)?;
        if parse_fat_volume_metadata(&boot_sector, partition.block_count).is_some() {
            return Ok(Some(partition));
        }
    }

    Ok(None)
}

pub fn fat_volume_id_from_boot_sector(boot_sector: &[u8]) -> Option<u32> {
    parse_fat_volume_metadata(boot_sector, u64::MAX).map(|metadata| metadata.volume_id)
}

fn detect_gpt_partitions<D: BlockDevice>(
    dev: &mut D,
    total_blocks: u64,
    block_size: usize,
    mbr: &[u8],
) -> IoResult<Option<Vec<PartitionInfo>>> {
    if total_blocks <= 1 {
        return Ok(None);
    }
    let protective = mbr[446 + 4] == 0xee;
    let mut header = vec![0_u8; block_size];
    dev.read_blocks(1, &mut header)?;
    if !protective && &header[..8] != b"EFI PART" {
        return Ok(None);
    }
    if &header[..8] != b"EFI PART" {
        return Ok(None);
    }

    let entry_lba = le_u64(&header, 72);
    let entry_count = le_u32(&header, 80) as usize;
    let entry_size = le_u32(&header, 84) as usize;
    if entry_count == 0 || entry_size < 56 || entry_size > block_size {
        return Err(StorageError::InvalidInput);
    }

    let entries_per_block = block_size / entry_size;
    if entries_per_block == 0 {
        return Err(StorageError::InvalidInput);
    }

    let entry_block_count = entry_count.div_ceil(entries_per_block);
    let mut partitions = Vec::new();
    let mut entry_block = vec![0_u8; block_size];
    for block_index in 0..entry_block_count {
        let lba = entry_lba
            .checked_add(block_index as u64)
            .ok_or(StorageError::InvalidInput)?;
        if lba >= total_blocks {
            return Err(StorageError::InvalidInput);
        }
        dev.read_blocks(lba, &mut entry_block)?;
        let entries_in_this_block = min(entries_per_block, entry_count - block_index * entries_per_block);
        for entry_index in 0..entries_in_this_block {
            let off = entry_index * entry_size;
            if entry_block[off..off + 16].iter().all(|byte| *byte == 0) {
                continue;
            }
            let start_lba = le_u64(&entry_block, off + 32);
            let end_lba = le_u64(&entry_block, off + 40);
            if start_lba == 0 || end_lba < start_lba {
                continue;
            }
            let block_count = end_lba
                .checked_sub(start_lba)
                .and_then(|count| count.checked_add(1))
                .ok_or(StorageError::InvalidInput)?;
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
    }

    Ok(Some(partitions))
}

#[derive(Clone, Copy)]
struct FatVolumeMetadata {
    volume_id: u32,
}

fn parse_fat_volume_metadata(boot_sector: &[u8], available_blocks: u64) -> Option<FatVolumeMetadata> {
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
    if total_blocks == 0 || u64::from(total_blocks) > available_blocks {
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

    Some(FatVolumeMetadata {
        volume_id,
    })
}

fn validate_block_io(
    block_size: usize,
    lba: u64,
    total_blocks: u64,
    len: usize,
) -> IoResult<()> {
    if block_size < 512 || len == 0 || len % block_size != 0 {
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

#[cfg(test)]
mod tests {
    use super::*;

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
}
