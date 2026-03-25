#![no_std]

extern crate alloc;

use alloc::borrow::Cow;
use alloc::string::String;
#[cfg(test)]
use alloc::vec;
use alloc::vec::Vec;
use core::cmp::min;

use fatfs::{IoBase, IoError, Read, Seek, SeekFrom, Write};

mod ata_pio;
mod boot_manifest;
#[cfg_attr(not(any(target_os = "none", test)), allow(dead_code))]
mod partition;

pub use ata_pio::{AtaDrive, AtaPioDevice};
#[cfg(test)]
use boot_manifest::fat_paths_match;
pub use boot_manifest::{BootFileBytes, boot_framebuffer_info, init_boot_info};
use boot_manifest::{CachedBootFile, CachedBootVolume};
use partition::LbaSliceDevice;
#[cfg(target_os = "none")]
use partition::detect_fat_volume_slice;
#[cfg(test)]
use partition::is_probable_fat_boot_sector;

pub const FAT_SECTOR_SIZE: usize = 512;

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

#[cfg(target_os = "none")]
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

#[cfg(not(target_os = "none"))]
fn open_ata_boot_volume() -> core::result::Result<BootVolumeFs, fatfs::Error<DiskIoError>> {
    Err(fatfs::Error::Io(DiskIoError::NotPresent))
}

enum BootVolumeFileInner<'a> {
    Cached(CachedBootFile),
    Fat(AtaBootVolumeFile<'a>),
}

pub struct BootVolumeFile<'a>(BootVolumeFileInner<'a>);

impl IoBase for BootVolumeFile<'_> {
    type Error = fatfs::Error<DiskIoError>;
}

impl Read for BootVolumeFile<'_> {
    fn read(&mut self, buf: &mut [u8]) -> core::result::Result<usize, fatfs::Error<DiskIoError>> {
        match &mut self.0 {
            BootVolumeFileInner::Cached(file) => {
                if buf.is_empty() || file.pos >= file.data.len() {
                    return Ok(0);
                }

                let read = min(buf.len(), file.data.len() - file.pos);
                buf[..read].copy_from_slice(&file.data[file.pos..file.pos + read]);
                file.pos += read;
                Ok(read)
            }
            BootVolumeFileInner::Fat(file) => file.read(buf),
        }
    }
}

impl Seek for BootVolumeFile<'_> {
    fn seek(&mut self, pos: SeekFrom) -> core::result::Result<u64, fatfs::Error<DiskIoError>> {
        match &mut self.0 {
            BootVolumeFileInner::Cached(file) => {
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
            BootVolumeFileInner::Fat(file) => file.seek(pos),
        }
    }
}

pub struct BootVolume {
    cached: Option<CachedBootVolume>,
    fs: Option<BootVolumeFs>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BootVolumeNodeKind {
    File,
    Directory,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct BootVolumeMetadata {
    pub kind: BootVolumeNodeKind,
    pub len: u64,
}

impl BootVolume {
    pub fn open() -> core::result::Result<Self, fatfs::Error<DiskIoError>> {
        let cached = CachedBootVolume::from_boot_info().filter(CachedBootVolume::is_manifest_valid);
        if cached.is_some() {
            return Ok(Self { cached, fs: None });
        }

        let fs = open_ata_boot_volume()?;
        Ok(Self {
            cached: None,
            fs: Some(fs),
        })
    }

    pub fn open_file(
        &self,
        path: &str,
    ) -> core::result::Result<BootVolumeFile<'_>, fatfs::Error<DiskIoError>> {
        let normalized_path = normalize_fat_path(path);
        if let Some(cached) = self.cached {
            return cached
                .open_file(normalized_path.as_str())
                .map(BootVolumeFileInner::Cached)
                .map(BootVolumeFile)
                .ok_or(fatfs::Error::NotFound);
        }

        let fs = self
            .fs
            .as_ref()
            .ok_or(fatfs::Error::Io(DiskIoError::NotPresent))?;
        let root = fs.root_dir();
        root.open_file(normalized_path.as_str())
            .map(BootVolumeFileInner::Fat)
            .map(BootVolumeFile)
    }

    pub fn metadata(
        &self,
        path: &str,
    ) -> core::result::Result<BootVolumeMetadata, fatfs::Error<DiskIoError>> {
        let normalized_path = normalize_fat_path(path);
        if let Some(cached) = self.cached {
            return cached
                .metadata(normalized_path.as_str())
                .ok_or(fatfs::Error::NotFound);
        }

        let fs = self
            .fs
            .as_ref()
            .ok_or(fatfs::Error::Io(DiskIoError::NotPresent))?;
        let root = fs.root_dir();

        if normalized_path.is_empty() {
            return Ok(BootVolumeMetadata {
                kind: BootVolumeNodeKind::Directory,
                len: 0,
            });
        }

        if let Ok(mut file) = root.open_file(normalized_path.as_str()) {
            let len = file.seek(SeekFrom::End(0))?;
            return Ok(BootVolumeMetadata {
                kind: BootVolumeNodeKind::File,
                len,
            });
        }

        if root.open_dir(normalized_path.as_str()).is_ok() {
            return Ok(BootVolumeMetadata {
                kind: BootVolumeNodeKind::Directory,
                len: 0,
            });
        }

        Err(fatfs::Error::NotFound)
    }

    pub fn close(self) -> core::result::Result<(), fatfs::Error<DiskIoError>> {
        match self.fs {
            Some(fs) => fs.unmount(),
            None => Ok(()),
        }
    }
}

#[allow(dead_code)]
pub fn read_file_bytes(
    path: &str,
) -> core::result::Result<BootFileBytes, fatfs::Error<DiskIoError>> {
    let volume = BootVolume::open()?;
    let normalized_path = normalize_fat_path(path);

    let result = if let Some(cached) = volume.cached {
        cached
            .open_file(normalized_path.as_str())
            .map(|file| Ok(BootFileBytes::Borrowed(file.data)))
            .unwrap_or(Err(fatfs::Error::NotFound))
    } else {
        let mut file = volume.open_file(normalized_path.as_str())?;
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

        Ok(BootFileBytes::Owned(bytes))
    };

    match (result, volume.close()) {
        (Ok(value), Ok(())) => Ok(value),
        (Ok(_), Err(err)) => Err(err),
        (Err(err), _) => Err(err),
    }
}

#[allow(dead_code)]
pub fn read_file_to_vec(path: &str) -> core::result::Result<Vec<u8>, fatfs::Error<DiskIoError>> {
    read_file_bytes(path).map(Cow::into_owned)
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
