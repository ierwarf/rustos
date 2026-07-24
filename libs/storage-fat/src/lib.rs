#![no_std]

extern crate alloc;
#[cfg(test)]
extern crate std;

use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;
use core::cmp::min;
use core::convert::TryFrom;

use fatfs::{IoBase, Read, Seek, SeekFrom, Write};
use storage_core::{BlockDevice, BlockSlice, IoResult, StorageError};

pub type FatError = fatfs::Error<StorageError>;
const FILE_READ_CHUNK_CAP: usize = 256 * 1024;
const FAT_LOGICAL_BLOCK_SIZES: [usize; 4] = [512, 1024, 2048, 4096];

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FatNodeKind {
    File,
    Directory,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FatDirEntry {
    pub name: String,
    pub kind: FatNodeKind,
    pub len: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct FatMetadata {
    pub kind: FatNodeKind,
    pub len: u64,
}

pub struct FatDisk<D: BlockDevice> {
    dev: D,
    pos: u64,
    block_size: usize,
    bytes_len: u64,
    scratch: Vec<u8>,
}

impl<D: BlockDevice> FatDisk<D> {
    pub fn new(dev: D) -> IoResult<Self> {
        let block_size = dev.logical_block_size();
        let block_count = dev.block_count();
        if !FAT_LOGICAL_BLOCK_SIZES.contains(&block_size) || block_count == 0 {
            return Err(StorageError::InvalidInput);
        }
        let bytes_len = block_count
            .checked_mul(block_size as u64)
            .ok_or(StorageError::InvalidInput)?;
        let scratch = vec![0; block_size];
        Ok(Self {
            dev,
            pos: 0,
            block_size,
            bytes_len,
            scratch,
        })
    }

    pub fn into_inner(self) -> D {
        self.dev
    }

    fn bytes_len(&self) -> u64 {
        self.bytes_len
    }

    fn ensure_in_range(&self, pos: u64) -> IoResult<()> {
        if pos <= self.bytes_len() {
            Ok(())
        } else {
            Err(StorageError::InvalidInput)
        }
    }
}

impl<D: BlockDevice> IoBase for FatDisk<D> {
    type Error = StorageError;
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
            let lba = self.pos / self.block_size as u64;
            let off = (self.pos as usize) % self.block_size;
            if off == 0 {
                let direct_len = ((max_read - done) / self.block_size) * self.block_size;
                if direct_len != 0 {
                    self.dev
                        .read_blocks(lba, &mut buf[done..done + direct_len])?;
                    self.pos += direct_len as u64;
                    done += direct_len;
                    continue;
                }
            }
            self.dev.read_blocks(lba, &mut self.scratch)?;
            let n = min(self.block_size - off, max_read - done);
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
            let lba = self.pos / self.block_size as u64;
            let off = (self.pos as usize) % self.block_size;
            if off == 0 {
                let direct_len = ((max_write - done) / self.block_size) * self.block_size;
                if direct_len != 0 {
                    self.dev.write_blocks(lba, &buf[done..done + direct_len])?;
                    self.pos += direct_len as u64;
                    done += direct_len;
                    continue;
                }
            }
            let n = min(self.block_size - off, max_write - done);
            if off == 0 && n == self.block_size {
                self.dev.write_blocks(lba, &buf[done..done + n])?;
            } else {
                self.dev.read_blocks(lba, &mut self.scratch)?;
                self.scratch[off..off + n].copy_from_slice(&buf[done..done + n]);
                self.dev.write_blocks(lba, &self.scratch)?;
            }
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
                .ok_or(StorageError::InvalidInput)?,
            SeekFrom::Current(delta) => cur
                .checked_add(delta as i128)
                .ok_or(StorageError::InvalidInput)?,
        };
        if next < 0 {
            return Err(StorageError::InvalidInput);
        }
        let next_u64 = next as u64;
        self.ensure_in_range(next_u64)?;
        self.pos = next_u64;
        Ok(self.pos)
    }
}

type FatFs<D> = fatfs::FileSystem<FatDisk<D>>;
type InnerFile<'a, D> =
    fatfs::File<'a, FatDisk<D>, fatfs::DefaultTimeProvider, fatfs::LossyOemCpConverter>;
type InnerDir<'a, D> =
    fatfs::Dir<'a, FatDisk<D>, fatfs::DefaultTimeProvider, fatfs::LossyOemCpConverter>;

pub struct FatVolume<D: BlockDevice> {
    fs: FatFs<D>,
}

pub struct FatFile<'a, D: BlockDevice>(InnerFile<'a, D>);

impl<D: BlockDevice> FatVolume<D> {
    pub fn new(device: D) -> Result<Self, FatError> {
        let disk = FatDisk::new(device).map_err(fatfs::Error::Io)?;
        Self::from_disk(disk)
    }

    pub fn from_disk(disk: FatDisk<D>) -> Result<Self, FatError> {
        let fs = fatfs::FileSystem::new(disk, fatfs::FsOptions::new())?;
        Ok(Self { fs })
    }

    pub fn from_partition(
        device: D,
        start_lba: u64,
        block_count: u64,
    ) -> Result<FatVolume<BlockSlice<D>>, FatError> {
        let slice = BlockSlice::new(device, start_lba, block_count).map_err(fatfs::Error::Io)?;
        FatVolume::new(slice)
    }

    pub fn open_file(&self, path: &str) -> Result<FatFile<'_, D>, FatError> {
        let normalized = normalize_fat_path(path);
        self.fs
            .root_dir()
            .open_file(normalized.as_str())
            .map(FatFile)
    }

    pub fn open_file_with_len(&self, path: &str) -> Result<(FatFile<'_, D>, u64), FatError> {
        let normalized = normalize_fat_path(path);
        let mut components = normalized
            .split('/')
            .filter(|component| !component.is_empty());
        let Some(first) = components.next() else {
            return Err(fatfs::Error::InvalidInput);
        };

        let mut dir = self.fs.root_dir();
        let mut component = first;
        loop {
            let next = components.next();
            let want_dir = next.is_some();
            let entry = find_entry_case_insensitive(&dir, component)?;
            if want_dir {
                if !entry.is_dir() {
                    return Err(fatfs::Error::InvalidInput);
                }
                dir = entry.to_dir();
                component = next.unwrap();
                continue;
            }

            if entry.is_dir() {
                return Err(fatfs::Error::InvalidInput);
            }
            let len = entry.len();
            return Ok((FatFile(entry.to_file()), len));
        }
    }

    pub fn create_file(&self, path: &str) -> Result<FatFile<'_, D>, FatError> {
        let normalized = normalize_fat_path(path);
        self.fs
            .root_dir()
            .create_file(normalized.as_str())
            .map(FatFile)
    }

    pub fn metadata(&self, path: &str) -> Result<FatMetadata, FatError> {
        let normalized = normalize_fat_path(path);
        if normalized.is_empty() {
            return Ok(FatMetadata {
                kind: FatNodeKind::Directory,
                len: 0,
            });
        }
        let (parent, child) =
            split_parent_child(normalized.as_str()).ok_or(fatfs::Error::InvalidInput)?;
        let dir = self.open_dir_by_path(parent)?;
        for entry_result in dir.iter() {
            let entry = entry_result?;
            if entry.file_name() != child {
                continue;
            }
            return Ok(FatMetadata {
                kind: if entry.is_dir() {
                    FatNodeKind::Directory
                } else {
                    FatNodeKind::File
                },
                len: if entry.is_dir() { 0 } else { entry.len() },
            });
        }
        Err(fatfs::Error::NotFound)
    }

    pub fn read_dir(&self, path: &str) -> Result<Vec<FatDirEntry>, FatError> {
        let normalized = normalize_fat_path(path);
        let dir = self.open_dir_by_path(normalized.as_str())?;

        let mut entries = Vec::new();
        for entry_result in dir.iter() {
            let entry = entry_result?;
            let name = entry.file_name();
            if name == "." || name == ".." {
                continue;
            }
            entries.push(FatDirEntry {
                name,
                kind: if entry.is_dir() {
                    FatNodeKind::Directory
                } else {
                    FatNodeKind::File
                },
                len: if entry.is_dir() { 0 } else { entry.len() },
            });
        }
        Ok(entries)
    }

    pub fn create_dir(&self, path: &str) -> Result<(), FatError> {
        let normalized = normalize_fat_path(path);
        self.fs.root_dir().create_dir(normalized.as_str())?;
        Ok(())
    }

    pub fn remove_file(&self, path: &str) -> Result<(), FatError> {
        let normalized = normalize_fat_path(path);
        let root = self.fs.root_dir();
        let entry = root.open_file(normalized.as_str())?;
        drop(entry);
        root.remove(normalized.as_str())
    }

    pub fn remove_dir(&self, path: &str) -> Result<(), FatError> {
        let normalized = normalize_fat_path(path);
        let root = self.fs.root_dir();
        let dir = root.open_dir(normalized.as_str())?;
        drop(dir);
        root.remove(normalized.as_str())
    }

    pub fn rename(&self, src: &str, dst: &str) -> Result<(), FatError> {
        let src = normalize_fat_path(src);
        let dst = normalize_fat_path(dst);
        let (dst_parent, dst_name) =
            split_parent_child(dst.as_str()).ok_or(fatfs::Error::InvalidInput)?;
        let root = self.fs.root_dir();
        let dst_dir = if dst_parent.is_empty() {
            root.clone()
        } else {
            root.open_dir(dst_parent)?
        };
        root.rename(src.as_str(), &dst_dir, dst_name)
    }

    pub fn read_file_to_vec(&self, path: &str) -> Result<Vec<u8>, FatError> {
        let (mut file, len) = self.open_file_with_len(path)?;
        let expected_len = usize::try_from(len).map_err(|_| fatfs::Error::InvalidInput)?;
        let mut bytes = vec![0_u8; expected_len];
        let mut done = 0usize;
        while done < bytes.len() {
            let chunk_len = (bytes.len() - done).min(FILE_READ_CHUNK_CAP);
            let count = file.read(&mut bytes[done..done + chunk_len])?;
            if count == 0 {
                return Err(fatfs::Error::InvalidInput);
            }
            done += count;
        }
        Ok(bytes)
    }

    pub fn read_file_into(&self, path: &str, dest: &mut [u8]) -> Result<usize, FatError> {
        let mut file = self.open_file(path)?;
        let mut done = 0usize;
        while done < dest.len() {
            let chunk_len = (dest.len() - done).min(FILE_READ_CHUNK_CAP);
            let count = file.read(&mut dest[done..done + chunk_len])?;
            if count == 0 {
                break;
            }
            done += count;
        }
        Ok(done)
    }

    pub fn read_file_range_into(
        &self,
        path: &str,
        offset: u64,
        dest: &mut [u8],
    ) -> Result<usize, FatError> {
        let mut file = self.open_file(path)?;
        file.seek(SeekFrom::Start(offset))?;
        let mut done = 0usize;
        while done < dest.len() {
            let chunk_len = (dest.len() - done).min(FILE_READ_CHUNK_CAP);
            let count = file.read(&mut dest[done..done + chunk_len])?;
            if count == 0 {
                break;
            }
            done += count;
        }
        Ok(done)
    }

    pub fn unmount(self) -> Result<(), FatError> {
        self.fs.unmount()
    }

    fn open_dir_by_path(&self, normalized_path: &str) -> Result<InnerDir<'_, D>, FatError> {
        if normalized_path.is_empty() {
            Ok(self.fs.root_dir())
        } else {
            self.fs.root_dir().open_dir(normalized_path)
        }
    }
}

impl<D: BlockDevice> IoBase for FatFile<'_, D> {
    type Error = FatError;
}

impl<D: BlockDevice> Read for FatFile<'_, D> {
    fn read(&mut self, buf: &mut [u8]) -> Result<usize, FatError> {
        self.0.read(buf)
    }
}

impl<D: BlockDevice> Write for FatFile<'_, D> {
    fn write(&mut self, buf: &[u8]) -> Result<usize, FatError> {
        self.0.write(buf)
    }

    fn flush(&mut self) -> Result<(), FatError> {
        self.0.flush()
    }
}

impl<D: BlockDevice> Seek for FatFile<'_, D> {
    fn seek(&mut self, pos: SeekFrom) -> Result<u64, FatError> {
        self.0.seek(pos)
    }
}

impl<D: BlockDevice> FatFile<'_, D> {
    pub fn truncate(&mut self) -> Result<(), FatError> {
        self.0.truncate()
    }
}

pub fn normalize_fat_path(path: &str) -> String {
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

fn split_parent_child(path: &str) -> Option<(&str, &str)> {
    match path.rsplit_once('/') {
        Some((parent, child)) if !child.is_empty() => Some((parent, child)),
        None if !path.is_empty() => Some(("", path)),
        _ => None,
    }
}

fn find_entry_case_insensitive<'a, D: BlockDevice>(
    dir: &InnerDir<'a, D>,
    name: &str,
) -> Result<
    fatfs::DirEntry<'a, FatDisk<D>, fatfs::DefaultTimeProvider, fatfs::LossyOemCpConverter>,
    FatError,
> {
    for entry in dir.iter() {
        let entry = entry?;
        if entry.file_name().eq_ignore_ascii_case(name) {
            return Ok(entry);
        }
    }
    Err(fatfs::Error::NotFound)
}

#[cfg(test)]
mod tests {
    use super::*;
    use storage_core::MemBlockDevice;

    struct GeometryOnlyDevice {
        block_size: usize,
        block_count: u64,
    }

    impl BlockDevice for GeometryOnlyDevice {
        fn logical_block_size(&self) -> usize {
            self.block_size
        }

        fn block_count(&self) -> u64 {
            self.block_count
        }

        fn read_blocks(&mut self, _lba: u64, _out: &mut [u8]) -> IoResult<()> {
            Err(StorageError::NotPresent)
        }

        fn write_blocks(&mut self, _lba: u64, _input: &[u8]) -> IoResult<()> {
            Err(StorageError::NotPresent)
        }
    }

    fn format_disk(block_size: usize, block_count: u64, volume_id: u32) -> MemBlockDevice {
        let mut disk = MemBlockDevice::new_zeroed(block_size, block_count);
        let mut formatter = FatDisk::new(disk).expect("valid FAT disk geometry");
        fatfs::format_volume(
            &mut formatter,
            fatfs::FormatVolumeOptions::new()
                .bytes_per_sector(block_size as u16)
                .volume_id(volume_id),
        )
        .expect("format FAT volume");
        disk = formatter.into_inner();
        disk
    }

    #[test]
    fn fat_disk_rejects_untrusted_or_overflowing_geometry_before_allocation() {
        for (block_size, block_count) in [(0, 1), (511, 1), (8192, 1), (512, 0), (4096, u64::MAX)] {
            assert!(
                FatDisk::new(GeometryOnlyDevice {
                    block_size,
                    block_count,
                })
                .is_err()
            );
        }

        let disk = FatDisk::new(GeometryOnlyDevice {
            block_size: 4096,
            block_count: 1024,
        })
        .expect("valid bounded geometry");
        assert_eq!(disk.bytes_len(), 4 * 1024 * 1024);
    }

    #[test]
    fn malformed_fat_boot_sector_fails_without_mounting() {
        let disk = MemBlockDevice::new_zeroed(512, 4096);
        assert!(FatVolume::new(disk).is_err());
    }

    #[test]
    fn write_truncate_append_persists_after_reopen() {
        let disk = format_disk(512, 4096, 0x1234_5678);
        let volume = FatVolume::new(disk).expect("open FAT volume");
        volume.create_dir("logs").expect("create logs dir");
        {
            let mut file = volume
                .create_file("logs/test.txt")
                .expect("create log file");
            assert_eq!(file.write(b"old boot\n").expect("write log"), 9);
            file.flush().expect("flush log");
        }
        {
            let mut file = volume.create_file("logs/test.txt").expect("reopen log");
            file.truncate().expect("truncate log");
            assert_eq!(file.write(b"new boot\n").expect("rewrite log"), 9);
            file.flush().expect("flush log");
        }
        let bytes = volume
            .read_file_to_vec("logs/test.txt")
            .expect("read rewritten log");
        assert_eq!(bytes, b"new boot\n");
        volume.unmount().expect("unmount");
    }

    #[test]
    fn create_dir_rename_remove_round_trip() {
        let disk = format_disk(512, 4096, 0x0102_0304);
        let volume = FatVolume::new(disk).expect("open FAT volume");
        volume.create_dir("configs").expect("create configs dir");
        {
            let mut file = volume
                .create_file("configs/system.ini")
                .expect("create file");
            file.write(b"[boot]\n").expect("write file");
            file.flush().expect("flush file");
        }
        volume
            .rename("configs/system.ini", "configs/system.old")
            .expect("rename file");
        let entries = volume.read_dir("configs").expect("read configs dir");
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].name, "system.old");
        volume
            .remove_file("configs/system.old")
            .expect("remove file");
        volume.remove_dir("configs").expect("remove dir");
        volume.unmount().expect("unmount");
    }

    #[test]
    fn rename_between_directories_preserves_long_filenames() {
        let disk = format_disk(512, 4096, 0x0ace_feed);
        let volume = FatVolume::new(disk).expect("open FAT volume");
        volume
            .create_dir("/configs")
            .expect("create configs directory");
        volume
            .create_dir("/archive")
            .expect("create archive directory");
        {
            let mut file = volume
                .create_file("//configs/very-long-config-name.toml")
                .expect("create long filename");
            file.write(b"title = \"kernel\"\n").expect("write config");
            file.flush().expect("flush config");
        }

        volume
            .rename(
                "configs/very-long-config-name.toml",
                "archive/very-long-config-name.bak",
            )
            .expect("rename across directories");

        let entries = volume.read_dir("archive").expect("read archive");
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].name, "very-long-config-name.bak");
        assert_eq!(
            volume
                .read_file_to_vec("archive/very-long-config-name.bak")
                .expect("read renamed file"),
            b"title = \"kernel\"\n"
        );
        volume.unmount().expect("unmount");
    }

    #[test]
    fn remove_dir_rejects_non_empty_directory() {
        let disk = format_disk(512, 4096, 0x4444_5555);
        let volume = FatVolume::new(disk).expect("open FAT volume");
        volume.create_dir("logs").expect("create logs");
        {
            let mut file = volume.create_file("logs/boot.txt").expect("create file");
            file.write(b"boot\n").expect("write file");
            file.flush().expect("flush file");
        }

        assert!(volume.remove_dir("logs").is_err());
        volume
            .remove_file("logs/boot.txt")
            .expect("remove nested file");
        volume.remove_dir("logs").expect("remove empty dir");
        volume.unmount().expect("unmount");
    }

    #[test]
    fn supports_4k_block_devices() {
        let disk = format_disk(4096, 1024, 0x0bad_cafe);
        let volume = FatVolume::new(disk).expect("open FAT volume");
        {
            let mut file = volume
                .create_file("nucleus.elf")
                .expect("create nucleus file");
            file.write(b"ELF").expect("write nucleus");
            file.flush().expect("flush nucleus");
        }
        let bytes = volume
            .read_file_to_vec("nucleus.elf")
            .expect("read nucleus");
        assert_eq!(bytes, b"ELF");
        volume.unmount().expect("unmount");
    }

    #[test]
    fn read_file_to_vec_reads_entire_large_file() {
        let disk = format_disk(512, 16384, 0xfeed_beef);
        let volume = FatVolume::new(disk).expect("open FAT volume");
        let expected = vec![0x5a; 128 * 1024];
        {
            let mut file = volume.create_file("system.bin").expect("create file");
            let mut written = 0usize;
            while written < expected.len() {
                let count = file.write(&expected[written..]).expect("write large file");
                assert!(count != 0);
                written += count;
            }
            file.flush().expect("flush large file");
        }

        let actual = volume
            .read_file_to_vec("system.bin")
            .expect("read large file");
        assert_eq!(actual.len(), expected.len());
        assert_eq!(actual, expected);
        volume.unmount().expect("unmount");
    }

    #[test]
    fn read_file_into_reads_entire_large_file() {
        let disk = format_disk(512, 16384, 0xfeed_beee);
        let volume = FatVolume::new(disk).expect("open FAT volume");
        let expected = vec![0xa5; 128 * 1024 + 17];
        {
            let mut file = volume.create_file("kernel.bin").expect("create file");
            let mut written = 0usize;
            while written < expected.len() {
                let count = file.write(&expected[written..]).expect("write large file");
                assert!(count != 0);
                written += count;
            }
            file.flush().expect("flush large file");
        }

        let mut actual = vec![0_u8; expected.len() + 64];
        let read = volume
            .read_file_into("kernel.bin", &mut actual)
            .expect("read large file into");
        assert_eq!(read, expected.len());
        assert_eq!(&actual[..read], expected.as_slice());
        assert!(actual[read..].iter().all(|byte| *byte == 0));
        volume.unmount().expect("unmount");
    }

    #[test]
    fn read_file_range_into_reads_middle_window() {
        let disk = format_disk(512, 16384, 0xfeed_beee);
        let volume = FatVolume::new(disk).expect("open FAT volume");
        let expected = (0..(192 * 1024))
            .map(|i| (i % 251) as u8)
            .collect::<Vec<_>>();
        {
            let mut file = volume.create_file("range.bin").expect("create file");
            let mut written = 0usize;
            while written < expected.len() {
                let count = file.write(&expected[written..]).expect("write large file");
                assert!(count != 0);
                written += count;
            }
            file.flush().expect("flush large file");
        }

        let mut actual = vec![0_u8; 64 * 1024];
        let read = volume
            .read_file_range_into("range.bin", 73_211, &mut actual)
            .expect("read range into");
        assert_eq!(read, actual.len());
        assert_eq!(&actual[..], &expected[73_211..73_211 + actual.len()]);
        volume.unmount().expect("unmount");
    }

    #[test]
    fn metadata_reads_file_length_from_directory_entry() {
        let disk = format_disk(512, 4096, 0x1234_abcd);
        let volume = FatVolume::new(disk).expect("open FAT volume");
        volume.create_dir("services").expect("create services dir");
        {
            let mut file = volume
                .create_file("services/initd.elf")
                .expect("create initd image");
            file.write(b"0123456789abcdef").expect("write image");
            file.flush().expect("flush image");
        }

        let file_meta = volume
            .metadata("services/initd.elf")
            .expect("read file metadata");
        assert_eq!(
            file_meta,
            FatMetadata {
                kind: FatNodeKind::File,
                len: 16,
            }
        );

        let dir_meta = volume.metadata("services").expect("read dir metadata");
        assert_eq!(
            dir_meta,
            FatMetadata {
                kind: FatNodeKind::Directory,
                len: 0,
            }
        );

        let entries = volume.read_dir("services").expect("read services dir");
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].name, "initd.elf");
        assert_eq!(entries[0].kind, FatNodeKind::File);
        assert_eq!(entries[0].len, 16);
        volume.unmount().expect("unmount");
    }

    #[test]
    fn open_file_reads_nested_path_via_directory_entries() {
        let disk = format_disk(512, 4096, 0x1234_dcba);
        let volume = FatVolume::new(disk).expect("open FAT volume");
        volume.create_dir("services").expect("create services dir");
        {
            let mut file = volume
                .create_file("services/sessiond.elf")
                .expect("create sessiond image");
            file.write(b"ELF!").expect("write image");
            file.flush().expect("flush image");
        }

        let (mut file, len) = volume
            .open_file_with_len("services/sessiond.elf")
            .expect("open nested image with len");
        assert_eq!(len, 4);
        let mut buf = [0_u8; 4];
        let read = file.read(&mut buf).expect("read nested image");
        assert_eq!(read, 4);
        assert_eq!(&buf, b"ELF!");
        drop(file);
        volume.unmount().expect("unmount");
    }

    #[test]
    fn repeated_reopen_and_seek_on_nested_file_succeeds() {
        let disk = format_disk(512, 16384, 0x5566_7788);
        let volume = FatVolume::new(disk).expect("open FAT volume");
        volume.create_dir("services").expect("create services dir");
        volume
            .create_dir("services/initd")
            .expect("create initd dir");

        let mut expected = vec![0u8; 96 * 1024];
        for (index, byte) in expected.iter_mut().enumerate() {
            *byte = (index % 251) as u8;
        }
        {
            let mut file = volume
                .create_file("services/initd/initd.elf")
                .expect("create initd image");
            let mut written = 0usize;
            while written < expected.len() {
                let count = file.write(&expected[written..]).expect("write large image");
                assert_ne!(count, 0);
                written += count;
            }
            file.flush().expect("flush large image");
        }

        let mut scratch = [0u8; 4096];
        for offset in (0..expected.len()).step_by(scratch.len()) {
            let mut file = volume
                .open_file("services/initd/initd.elf")
                .expect("reopen initd image");
            assert_eq!(
                file.seek(SeekFrom::Start(offset as u64))
                    .expect("seek reopened image"),
                offset as u64
            );
            let mut done = 0usize;
            while done < scratch.len() {
                let count = file
                    .read(&mut scratch[done..])
                    .expect("read reopened image chunk");
                assert_ne!(count, 0, "offset={offset} done={done}");
                done += count;
            }
            assert_eq!(&scratch[..], &expected[offset..offset + scratch.len()]);
        }

        volume.unmount().expect("unmount");
    }
}
