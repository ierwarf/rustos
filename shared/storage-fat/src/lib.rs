#![no_std]

extern crate alloc;
#[cfg(test)]
extern crate std;

use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;
use core::cmp::min;

use fatfs::{IoBase, Read, Seek, SeekFrom, Write};
use storage_core::{BlockDevice, BlockSlice, IoResult, StorageError};

pub type FatError = fatfs::Error<StorageError>;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FatNodeKind {
    File,
    Directory,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FatDirEntry {
    pub name: String,
    pub kind: FatNodeKind,
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
    scratch: Vec<u8>,
}

impl<D: BlockDevice> FatDisk<D> {
    pub fn new(dev: D) -> Self {
        let block_size = dev.logical_block_size();
        Self {
            dev,
            pos: 0,
            block_size,
            scratch: vec![0; block_size],
        }
    }

    pub fn into_inner(self) -> D {
        self.dev
    }

    fn bytes_len(&self) -> u64 {
        self.dev
            .block_count()
            .saturating_mul(self.block_size as u64)
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

pub struct FatVolume<D: BlockDevice> {
    fs: FatFs<D>,
}

pub struct FatFile<'a, D: BlockDevice>(InnerFile<'a, D>);

impl<D: BlockDevice> FatVolume<D> {
    pub fn new(device: D) -> Result<Self, FatError> {
        let fs = fatfs::FileSystem::new(FatDisk::new(device), fatfs::FsOptions::new())?;
        Ok(Self { fs })
    }

    pub fn from_partition(
        device: D,
        start_lba: u64,
        block_count: u64,
    ) -> Result<FatVolume<BlockSlice<D>>, FatError> {
        let slice = BlockSlice::new(device, start_lba, block_count)
            .map_err(fatfs::Error::Io)?;
        FatVolume::new(slice)
    }

    pub fn open_file(&self, path: &str) -> Result<FatFile<'_, D>, FatError> {
        let normalized = normalize_fat_path(path);
        self.fs.root_dir().open_file(normalized.as_str()).map(FatFile)
    }

    pub fn create_file(&self, path: &str) -> Result<FatFile<'_, D>, FatError> {
        let normalized = normalize_fat_path(path);
        self.fs.root_dir().create_file(normalized.as_str()).map(FatFile)
    }

    pub fn metadata(&self, path: &str) -> Result<FatMetadata, FatError> {
        let normalized = normalize_fat_path(path);
        let root = self.fs.root_dir();
        if normalized.is_empty() {
            return Ok(FatMetadata {
                kind: FatNodeKind::Directory,
                len: 0,
            });
        }
        if let Ok(mut entry) = root.open_file(normalized.as_str()) {
            return Ok(FatMetadata {
                kind: FatNodeKind::File,
                len: entry.seek(SeekFrom::End(0))?,
            });
        }
        if root.open_dir(normalized.as_str()).is_ok() {
            return Ok(FatMetadata {
                kind: FatNodeKind::Directory,
                len: 0,
            });
        }
        Err(fatfs::Error::NotFound)
    }

    pub fn read_dir(&self, path: &str) -> Result<Vec<FatDirEntry>, FatError> {
        let normalized = normalize_fat_path(path);
        let root = self.fs.root_dir();
        let dir = if normalized.is_empty() {
            root
        } else {
            root.open_dir(normalized.as_str())?
        };

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
        let (dst_parent, dst_name) = split_parent_child(dst.as_str()).ok_or(fatfs::Error::InvalidInput)?;
        let root = self.fs.root_dir();
        let dst_dir = if dst_parent.is_empty() {
            root.clone()
        } else {
            root.open_dir(dst_parent)?
        };
        root.rename(src.as_str(), &dst_dir, dst_name)
    }

    pub fn read_file_to_vec(&self, path: &str) -> Result<Vec<u8>, FatError> {
        let mut file = self.open_file(path)?;
        let len = file.seek(SeekFrom::End(0))?;
        let capacity = usize::try_from(len).map_err(|_| fatfs::Error::Io(StorageError::InvalidInput))?;
        file.seek(SeekFrom::Start(0))?;
        let mut bytes = vec![0_u8; capacity];
        let mut read = 0usize;
        while read < bytes.len() {
            let count = file.read(&mut bytes[read..])?;
            if count == 0 {
                break;
            }
            read += count;
        }
        bytes.truncate(read);
        Ok(bytes)
    }

    pub fn unmount(self) -> Result<(), FatError> {
        self.fs.unmount()
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

#[cfg(test)]
mod tests {
    use super::*;
    use storage_core::MemBlockDevice;

    fn format_disk(block_size: usize, block_count: u64, volume_id: u32) -> MemBlockDevice {
        let mut disk = MemBlockDevice::new_zeroed(block_size, block_count);
        let mut formatter = FatDisk::new(disk);
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
    fn write_truncate_append_persists_after_reopen() {
        let disk = format_disk(512, 4096, 0x1234_5678);
        let volume = FatVolume::new(disk).expect("open FAT volume");
        volume.create_dir("logs").expect("create logs dir");
        {
            let mut file = volume.create_file("logs/test.txt").expect("create log file");
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
            let mut file = volume.create_file("configs/system.ini").expect("create file");
            file.write(b"[boot]\n").expect("write file");
            file.flush().expect("flush file");
        }
        volume
            .rename("configs/system.ini", "configs/system.old")
            .expect("rename file");
        let entries = volume.read_dir("configs").expect("read configs dir");
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].name, "system.old");
        volume.remove_file("configs/system.old").expect("remove file");
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
            file.write(b"title = \"kernel\"\n")
                .expect("write config");
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
        volume.remove_file("logs/boot.txt").expect("remove nested file");
        volume.remove_dir("logs").expect("remove empty dir");
        volume.unmount().expect("unmount");
    }

    #[test]
    fn supports_4k_block_devices() {
        let disk = format_disk(4096, 1024, 0x0bad_cafe);
        let volume = FatVolume::new(disk).expect("open FAT volume");
        {
            let mut file = volume.create_file("kernel.elf").expect("create kernel file");
            file.write(b"ELF").expect("write kernel");
            file.flush().expect("flush kernel");
        }
        let bytes = volume.read_file_to_vec("kernel.elf").expect("read kernel");
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
                let count = file
                    .write(&expected[written..])
                    .expect("write large file");
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
}
