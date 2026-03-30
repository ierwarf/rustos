use core::cmp::min;

use crate::{BlockDevice, DiskIoError, FAT_SECTOR_SIZE, IoResult};

const FAT_VOLUME_SIG_OFFSET: usize = FAT_SECTOR_SIZE - 2;
const GPT_HEADER_LBA: u64 = 1;
const GPT_SIGNATURE: [u8; 8] = *b"EFI PART";
const MBR_PARTITION_TABLE_OFFSET: usize = 446;
const MBR_PARTITION_ENTRY_LEN: usize = 16;

/// Maps a whole device to a logical slice `[start_lba, start_lba + sectors)`.
pub(crate) struct LbaSliceDevice<D: BlockDevice> {
    inner: D,
    start_lba: u64,
    sectors: u64,
}

impl<D: BlockDevice> LbaSliceDevice<D> {
    pub(crate) fn new(inner: D, start_lba: u64, sectors: u64) -> IoResult<Self> {
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
pub(crate) struct FatVolumeMetadata {
    pub(crate) total_sectors: u32,
    pub(crate) volume_id: u32,
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

    let volume_id = match fat_type {
        FatType::Fat32 => {
            if root_entry_count != 0 || sectors_per_fat32 == 0 || le_u32(boot_sector, 44) < 2 {
                return None;
            }
            le_u32(boot_sector, 67)
        }
        FatType::Fat12 | FatType::Fat16 => {
            if root_entry_count == 0 || sectors_per_fat16 == 0 {
                return None;
            }
            le_u32(boot_sector, 39)
        }
    };

    Some(FatVolumeMetadata {
        total_sectors,
        volume_id,
    })
}

pub fn fat_volume_id_from_boot_sector(boot_sector: &[u8; FAT_SECTOR_SIZE]) -> Option<u32> {
    parse_fat_volume_metadata(boot_sector, u64::MAX).map(|metadata| metadata.volume_id)
}

#[cfg(test)]
pub(crate) fn is_probable_fat_boot_sector(
    sector0: &[u8; FAT_SECTOR_SIZE],
    available_sectors: u64,
) -> bool {
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

pub(crate) fn detect_fat_volume_slice<D: BlockDevice>(dev: &mut D) -> IoResult<(u64, u64)> {
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
