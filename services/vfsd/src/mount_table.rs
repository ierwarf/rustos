//! Multi-mount table and filesystem-neutral volume helpers for `vfsd`.

use alloc::boxed::Box;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec;
use alloc::vec::Vec;
use spin::Mutex;

use storage_core::file_io::{FileReadMap, RangeError, ReadAt};
use storage_core::fs::{CasePolicy, FsVolume};
use storage_core::{detect_partitions, BlockDevice, BlockSlice, IoResult};
use storage_fat::{FatError, FatVolume};

use crate::{map_storage_error, EIO};

pub type CacheKey = (u64, String);

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct OpenNodeState {
    pub refs: u64,
    pub live_len: u64,
    pub pending_unlink: bool,
}

/// Shareable `BlockDevice` wrapper so GPT-partitioned disks can expose multiple
/// `BlockSlice`s over one underlying transport.
pub struct SharedBlockDevice<D: BlockDevice> {
    inner: Arc<Mutex<D>>,
    logical_block_size: usize,
    block_count: u64,
}

impl<D: BlockDevice> Clone for SharedBlockDevice<D> {
    fn clone(&self) -> Self {
        Self {
            inner: self.inner.clone(),
            logical_block_size: self.logical_block_size,
            block_count: self.block_count,
        }
    }
}

impl<D: BlockDevice> SharedBlockDevice<D> {
    pub fn new(device: D) -> Self {
        let logical_block_size = device.logical_block_size();
        let block_count = device.block_count();
        Self {
            inner: Arc::new(Mutex::new(device)),
            logical_block_size,
            block_count,
        }
    }
}

impl<D: BlockDevice> BlockDevice for SharedBlockDevice<D> {
    fn logical_block_size(&self) -> usize {
        self.logical_block_size
    }

    fn block_count(&self) -> u64 {
        self.block_count
    }

    fn read_blocks(&mut self, lba: u64, out: &mut [u8]) -> IoResult<()> {
        self.inner.lock().read_blocks(lba, out)
    }

    fn write_blocks(&mut self, lba: u64, input: &[u8]) -> IoResult<()> {
        self.inner.lock().write_blocks(lba, input)
    }

    fn flush(&mut self) -> IoResult<()> {
        self.inner.lock().flush()
    }
}

pub trait ReadMapSupport: Send + Sync {
    fn plan_read(&self, path: &str, max_map_bytes: usize) -> Result<FileReadMap, RangeError>;
    fn read_map_is_current(&self, map: &FileReadMap) -> bool;
    fn read_source(
        &self,
        map: Arc<FileReadMap>,
    ) -> Result<Box<dyn ReadAt<Error = RangeError> + Send>, RangeError>;
    fn read_mapped_at(
        &self,
        map: &FileReadMap,
        offset: u64,
        output: &mut [u8],
    ) -> Result<usize, RangeError>;
}

impl<D: BlockDevice + Send + 'static> ReadMapSupport for FatVolume<D> {
    fn plan_read(&self, path: &str, max_map_bytes: usize) -> Result<FileReadMap, RangeError> {
        FatVolume::plan_read(self, path, max_map_bytes)
    }

    fn read_map_is_current(&self, map: &FileReadMap) -> bool {
        FatVolume::read_map_is_current(self, map)
    }

    fn read_source(
        &self,
        map: Arc<FileReadMap>,
    ) -> Result<Box<dyn ReadAt<Error = RangeError> + Send>, RangeError> {
        Ok(Box::new(FatVolume::read_source(self, map)?))
    }

    fn read_mapped_at(
        &self,
        map: &FileReadMap,
        offset: u64,
        output: &mut [u8],
    ) -> Result<usize, RangeError> {
        let mut scratch = [0_u8; 4096];
        let mut view = FatVolume::read_view(self, map, &mut scratch)?;
        view.read_at(offset, output)
    }
}

pub struct MountEntry {
    pub id: u64,
    pub mount_point: String,
    pub volume: Arc<dyn FsVolume>,
    pub read_map_source: Option<Arc<dyn ReadMapSupport>>,
    pub storage_epoch: u64,
}

pub struct MountTable {
    entries: Vec<MountEntry>,
    next_id: u64,
}

impl MountTable {
    pub const fn new() -> Self {
        Self {
            entries: Vec::new(),
            next_id: 1,
        }
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn clear(&mut self) {
        self.entries.clear();
    }

    pub fn mount(
        &mut self,
        mount_point: &str,
        volume: Arc<dyn FsVolume>,
        read_map_source: Option<Arc<dyn ReadMapSupport>>,
        storage_epoch: u64,
    ) -> u64 {
        let id = self.next_id;
        self.next_id = self.next_id.saturating_add(1).max(1);
        self.entries.retain(|entry| entry.mount_point != mount_point);
        self.entries.push(MountEntry {
            id,
            mount_point: String::from(mount_point),
            volume,
            read_map_source,
            storage_epoch,
        });
        id
    }

    pub fn by_id(&self, id: u64) -> Option<&MountEntry> {
        self.entries.iter().find(|entry| entry.id == id)
    }

    /// Resolves an absolute normalized path to `(mount_entry, relative_path)`
    /// using longest mount-point prefix matching.
    pub fn resolve<'a>(&'a self, abs_path: &'a str) -> Option<(&'a MountEntry, &'a str)> {
        let mut best: Option<(&MountEntry, &str)> = None;
        for entry in &self.entries {
            let rel = if entry.mount_point == "/" {
                let Some(rel) = abs_path.strip_prefix('/') else {
                    continue;
                };
                rel
            } else if abs_path == entry.mount_point {
                ""
            } else if let Some(rest) = abs_path.strip_prefix(entry.mount_point.as_str()) {
                let Some(rel) = rest.strip_prefix('/') else {
                    continue;
                };
                rel
            } else {
                continue;
            };
            if best.is_none_or(|(prev, _)| entry.mount_point.len() > prev.mount_point.len()) {
                best = Some((entry, rel));
            }
        }
        best
    }

    /// Returns immediate child directory names of any submounts directly under `abs_dir`.
    pub fn submount_names_at(&self, abs_dir: &str) -> Vec<&str> {
        let mut names = Vec::new();
        for entry in &self.entries {
            if entry.mount_point == "/" {
                continue;
            }
            let child = if abs_dir == "/" {
                entry.mount_point.strip_prefix('/')
            } else {
                entry
                    .mount_point
                    .strip_prefix(abs_dir)
                    .and_then(|rest| rest.strip_prefix('/'))
            };
            if let Some(name) = child {
                if !name.is_empty() && !name.contains('/') {
                    names.push(name);
                }
            }
        }
        names
    }

    pub fn mount_fat_disk<D: BlockDevice + Send + 'static>(
        mut shared_dev: SharedBlockDevice<D>,
        device_read_only: bool,
        storage_epoch: u64,
        wait: fn(),
    ) -> Result<Self, i32> {
        let total_blocks = shared_dev.block_count();
        let partitions = detect_partitions(&mut shared_dev).unwrap_or_default();
        let mut table = Self::new();
        if partitions.is_empty() {
            let slice =
                BlockSlice::new(shared_dev, 0, total_blocks).map_err(map_storage_error)?;
            let vol = Arc::new(
                FatVolume::new_with_wait_and_mode(slice, wait, device_read_only)
                    .map_err(map_fat_err_to_errno)?,
            );
            table.mount("/", vol.clone(), Some(vol), storage_epoch);
        } else if partitions.len() == 1 {
            let p0 = partitions[0];
            let slice = BlockSlice::new(shared_dev, p0.start_lba, p0.block_count)
                .map_err(map_storage_error)?;
            let vol = Arc::new(
                FatVolume::new_with_wait_and_mode(slice, wait, device_read_only)
                    .map_err(map_fat_err_to_errno)?,
            );
            table.mount("/", vol.clone(), Some(vol), storage_epoch);
        } else {
            let p0 = partitions[0];
            let slice0 = BlockSlice::new(shared_dev.clone(), p0.start_lba, p0.block_count)
                .map_err(map_storage_error)?;
            // Force read-only on partition 0 when a second partition exists for /data.
            let vol0 = Arc::new(
                FatVolume::new_with_wait_and_mode(slice0, wait, true)
                    .map_err(map_fat_err_to_errno)?,
            );
            table.mount("/", vol0.clone(), Some(vol0), storage_epoch);

            let p1 = partitions[1];
            if let Ok(slice1) = BlockSlice::new(shared_dev, p1.start_lba, p1.block_count) {
                if let Ok(vol1) =
                    FatVolume::new_with_wait_and_mode(slice1, wait, device_read_only)
                {
                    let vol1 = Arc::new(vol1);
                    table.mount("/data", vol1.clone(), Some(vol1), storage_epoch);
                }
            }
        }
        Ok(table)
    }
}

pub fn map_fat_err_to_errno(err: FatError) -> i32 {
    match err {
        fatfs::Error::Io(e) => map_storage_error(e),
        fatfs::Error::NotFound => crate::ENOENT,
        fatfs::Error::AlreadyExists => 17,       // EEXIST
        fatfs::Error::DirectoryIsNotEmpty => 39, // ENOTEMPTY
        fatfs::Error::NotEnoughSpace => 28,      // ENOSPC
        fatfs::Error::InvalidFileNameLength => 36, // ENAMETOOLONG
        fatfs::Error::InvalidInput | fatfs::Error::UnsupportedFileNameCharacter => 22, // EINVAL
        fatfs::Error::UnexpectedEof | fatfs::Error::WriteZero | fatfs::Error::CorruptedFileSystem => EIO,
        _ => 22,
    }
}

pub fn cache_key_for_mount(mount_id: u64, case_policy: CasePolicy, rel_path: &str) -> CacheKey {
    let normalized = rel_path.trim_matches('/');
    let key_str = match case_policy {
        CasePolicy::Exact => String::from(normalized),
        CasePolicy::AsciiInsensitive | CasePolicy::UnicodePreservingInsensitive => {
            normalized.to_ascii_lowercase()
        }
    };
    (mount_id, key_str)
}

pub fn parent_rel_path(rel_path: &str) -> Option<&str> {
    let trimmed = rel_path.trim_matches('/');
    if trimmed.is_empty() {
        return None;
    }
    match trimmed.rfind('/') {
        Some(idx) => Some(&trimmed[..idx]),
        None => Some(""),
    }
}

pub fn read_volume_file_to_vec(
    volume: &dyn FsVolume,
    rel_path: &str,
    expected_len: usize,
) -> Result<Vec<u8>, i32> {
    let mut out = vec![0_u8; expected_len];
    let mut done = 0usize;
    while done < expected_len {
        let read = volume
            .read_at(rel_path, done as u64, &mut out[done..])
            .map_err(crate::map_fs_error)?;
        if read == 0 {
            return Err(EIO);
        }
        done += read;
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use rustos_user_abi::syscall::{
        DvmBlockInfoWire, BLOCK_BROKER_INFO_FLAG_READ_ONLY,
    };
    use storage_core::fs::{FsError, FsOpenOptions, FsTimestamp};
    use storage_core::MemBlockDevice;
    use storage_fat::FatDisk;

    fn format_fat_mem_disk(block_count: u64) -> MemBlockDevice {
        let dev = MemBlockDevice::new_zeroed(512, block_count);
        let mut disk = FatDisk::new(dev).expect("fat disk");
        fatfs::format_volume(&mut disk, fatfs::FormatVolumeOptions::new()).expect("format fat");
        disk.into_inner()
    }

    fn build_mbr_two_partition_disk(p0_blocks: u64, p1_blocks: u64) -> MemBlockDevice {
        let p0_start = 2048_u64;
        let p1_start = p0_start + p0_blocks;
        let total = p1_start + p1_blocks + 64;
        let mut disk = MemBlockDevice::new_zeroed(512, total);

        // Format partition 0 and partition 1 as FAT and copy them into the MBR disk.
        let p0_img = format_fat_mem_disk(p0_blocks);
        let p1_img = format_fat_mem_disk(p1_blocks);
        {
            let bytes = disk.as_bytes_mut();
            let p0_off = (p0_start as usize) * 512;
            bytes[p0_off..p0_off + p0_img.as_bytes().len()].copy_from_slice(p0_img.as_bytes());
            let p1_off = (p1_start as usize) * 512;
            bytes[p1_off..p1_off + p1_img.as_bytes().len()].copy_from_slice(p1_img.as_bytes());

            // Write MBR partition table entries (type 0x0c = FAT32 LBA)
            let write_mbr_entry = |buf: &mut [u8], idx: usize, start: u32, count: u32| {
                let off = 446 + idx * 16;
                buf[off + 4] = 0x0c;
                buf[off + 8..off + 12].copy_from_slice(&start.to_le_bytes());
                buf[off + 12..off + 16].copy_from_slice(&count.to_le_bytes());
            };
            write_mbr_entry(bytes, 0, p0_start as u32, p0_blocks as u32);
            write_mbr_entry(bytes, 1, p1_start as u32, p1_blocks as u32);
            bytes[510] = 0x55;
            bytes[511] = 0xaa;
        }
        disk
    }

    #[test]
    fn block_geometry_allows_rw_zero_or_ro_flag_and_rejects_unknown() {
        let info_rw = DvmBlockInfoWire {
            generation: 3,
            capacity_sectors: 4096,
            logical_block_size: 512,
            physical_block_size: 512,
            flags: 0,
            ..DvmBlockInfoWire::default()
        };
        assert_eq!(
            crate::admit_dvm_block_geometry_with_allowed_flags(
                info_rw,
                3,
                4096,
                64 * 1024,
                BLOCK_BROKER_INFO_FLAG_READ_ONLY
            ),
            Ok((512, 4096))
        );
        let info_ro = DvmBlockInfoWire {
            flags: BLOCK_BROKER_INFO_FLAG_READ_ONLY,
            ..info_rw
        };
        assert_eq!(
            crate::admit_dvm_block_geometry_with_allowed_flags(
                info_ro,
                3,
                4096,
                64 * 1024,
                BLOCK_BROKER_INFO_FLAG_READ_ONLY
            ),
            Ok((512, 4096))
        );
        let info_bad = DvmBlockInfoWire { flags: 3, ..info_rw };
        assert!(crate::admit_dvm_block_geometry_with_allowed_flags(
            info_bad,
            3,
            4096,
            64 * 1024,
            BLOCK_BROKER_INFO_FLAG_READ_ONLY
        )
        .is_err());
    }

    #[test]
    fn mount_table_longest_prefix_and_submount_listing() {
        let d1 = SharedBlockDevice::new(format_fat_mem_disk(4096));
        let d2 = SharedBlockDevice::new(format_fat_mem_disk(4096));
        let v1 = Arc::new(FatVolume::new_with_wait_and_mode(d1, || {}, true).unwrap());
        let v2 = Arc::new(FatVolume::new_with_wait_and_mode(d2, || {}, false).unwrap());

        let mut table = MountTable::new();
        let id1 = table.mount("/", v1.clone(), Some(v1), 1);
        let id2 = table.mount("/data", v2.clone(), Some(v2), 1);

        let (m, rel) = table.resolve("/bin/ls").unwrap();
        assert_eq!(m.id, id1);
        assert_eq!(rel, "bin/ls");

        let (m, rel) = table.resolve("/data").unwrap();
        assert_eq!(m.id, id2);
        assert_eq!(rel, "");

        let (m, rel) = table.resolve("/data/sub/file.txt").unwrap();
        assert_eq!(m.id, id2);
        assert_eq!(rel, "sub/file.txt");

        // `/database` must NOT match `/data`
        let (m, rel) = table.resolve("/database/x").unwrap();
        assert_eq!(m.id, id1);
        assert_eq!(rel, "database/x");

        assert_eq!(table.submount_names_at("/"), alloc::vec!["data"]);
        assert!(table.submount_names_at("/data").is_empty());
    }

    #[test]
    fn multi_partition_disk_mounts_root_ro_and_data_rw() {
        let disk = build_mbr_two_partition_disk(4096, 4096);
        let shared = SharedBlockDevice::new(disk);
        let table = MountTable::mount_fat_disk(shared, false, 9, || {}).unwrap();

        let (root_m, _) = table.resolve("/").unwrap();
        assert!(root_m.volume.is_immutable());
        assert!(root_m.volume.capabilities().read_only);

        let (data_m, _) = table.resolve("/data").unwrap();
        assert!(!data_m.volume.is_immutable());
        assert!(!data_m.volume.capabilities().read_only);

        // Mutations on `/` fail with ReadOnly; mutations on `/data` succeed.
        assert_eq!(
            root_m.volume.mkdir("nope"),
            Err(FsError::ReadOnly)
        );
        data_m.volume.mkdir("logs").unwrap();
        let meta = data_m
            .volume
            .open_prepare(
                "logs/app.txt",
                FsOpenOptions {
                    create: true,
                    create_new: true,
                    truncate: false,
                    write: true,
                    append: false,
                },
            )
            .unwrap();
        assert_eq!(meta.len, 0);
        let (written, new_len) = data_m.volume.write_at("logs/app.txt", 0, b"hello world").unwrap();
        assert_eq!(written, 11);
        assert_eq!(new_len, 11);
        let truncated = data_m.volume.truncate("logs/app.txt", 5).unwrap();
        assert_eq!(truncated, 5);
        let mut buf = [0_u8; 8];
        let n = data_m.volume.read_at("logs/app.txt", 0, &mut buf).unwrap();
        assert_eq!(&buf[..n], b"hello");

        // Rename + RENAME_NOREPLACE + unlink + rmdir + statfs + set_times + symlink
        data_m
            .volume
            .rename("logs/app.txt", "logs/renamed.txt", true, false)
            .unwrap();
        assert_eq!(
            data_m
                .volume
                .rename("logs/renamed.txt", "logs/renamed.txt", false, true),
            Err(FsError::AlreadyExists)
        );
        let st = data_m.volume.statfs().unwrap();
        assert!(st.total_blocks > 0);
        assert!(st.block_size >= 512);
        assert_eq!(
            data_m.volume.symlink("target", "logs/sym"),
            Err(FsError::Unsupported)
        );
        data_m
            .volume
            .set_times(
                "logs/renamed.txt",
                Some(FsTimestamp { secs: 1_700_000_000, nanos: 0 }),
                Some(FsTimestamp { secs: 1_700_000_000, nanos: 0 }),
                None,
            )
            .unwrap();
        data_m.volume.unlink("logs/renamed.txt").unwrap();
        data_m.volume.rmdir("logs").unwrap();
        data_m.volume.flush().unwrap();
    }

    #[test]
    fn cache_key_respects_case_policy_and_parent_rel_path() {
        assert_eq!(
            cache_key_for_mount(2, CasePolicy::AsciiInsensitive, "Logs/APP.txt"),
            (2, String::from("logs/app.txt"))
        );
        assert_eq!(
            cache_key_for_mount(3, CasePolicy::Exact, "Logs/APP.txt"),
            (3, String::from("Logs/APP.txt"))
        );
        assert_eq!(parent_rel_path("foo.txt"), Some(""));
        assert_eq!(parent_rel_path("a/b/c"), Some("a/b"));
        assert_eq!(parent_rel_path(""), None);
    }
}
