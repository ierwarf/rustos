//! Owned FAT file-range admission using the pinned upstream extent iterator.
//!
//! The volume remains the sole filesystem owner. Maps retain no File<'volume>
//! borrow, pathname, second FatVolume, or raw-device capability. Bind a map back
//! to its originating volume for each bounded read.
//!
//! Validity is per file (see `map_registry`): a map depends on its data
//! extents, the FAT entries of its cluster chain (every FAT copy) and its
//! directory entry. A write overlapping any of those revokes the map before it
//! can modify media; a write elsewhere on the volume does not. (FAT12 packs
//! neighbouring 12-bit entries into shared bytes, so there a neighbour's chain
//! update conservatively revokes too; that costs a replan, never correctness.)
//! Mutable VFS
//! opens are not enabled by this read-only view; later backends can implement
//! ReadAt directly. Malformed chains are bounded by the admitted length and
//! reject overlapping cluster ranges, premature EOF, out-of-device extents and
//! metadata pressure.

use super::*;
use crate::map_registry::{normalize, Span};
use storage_core::file_io::{FileCapabilities, FileIdentity, FileRange, FileReadMap, RangeError, RangeKind, ReadAt};

pub struct FatReadView<'a, D: BlockDevice> {
    volume: &'a FatVolume<D>,
    map: &'a FileReadMap,
    scratch: &'a mut [u8],
}

/// Owns only a checked map and the serialized device, not the FAT namespace.
/// Immutable readers can therefore perform data I/O without its owner lock.
pub struct FatReadSource<D: BlockDevice> {
    device: Arc<DeviceCell<D>>,
    map: Arc<FileReadMap>,
    scratch: Vec<u8>,
}

fn map_is_current<D>(dev: &SharedDevice<D>, map: &FileReadMap) -> bool {
    let id = map.identity();
    id.volume == dev.identity && dev.maps.is_current(id.object, id.generation)
}

/// On-media layout needed to turn data extents into the FAT entries that link
/// them. Parsed from the boot sector and cross-checked against fatfs.
struct FatGeometry {
    fat_type: fatfs::FatType,
    cluster_bytes: u64,
    data_start: u64,
    fat_starts: Vec<u64>,
    fat_bytes: u64,
}

impl FatGeometry {
    /// Device byte span of the FAT entries for clusters `first..=last`.
    fn entry_span(&self, fat_start: u64, first: u64, last: u64) -> Option<Span> {
        let (start, end) = match self.fat_type {
            fatfs::FatType::Fat12 => (first.checked_mul(3)? / 2, last.checked_mul(3)? / 2 + 2),
            fatfs::FatType::Fat16 => (first.checked_mul(2)?, last.checked_mul(2)? + 2),
            fatfs::FatType::Fat32 => (first.checked_mul(4)?, last.checked_mul(4)? + 4),
        };
        if end > self.fat_bytes { return None; }
        Some((fat_start.checked_add(start)?, fat_start.checked_add(end)?))
    }

    /// Everything that determines the extents in `physical` (sorted, disjoint
    /// data spans): the data itself, the chain's FAT entries in every FAT copy,
    /// and the 32-byte directory entry at `entry_offset`.
    fn dependencies(&self, physical: &[Span], entry_offset: u64) -> Result<Vec<Span>, RangeError> {
        let mut deps = Vec::new();
        let count = physical.len() * (1 + self.fat_starts.len()) + 1;
        deps.try_reserve_exact(count).map_err(|_| RangeError::NoMemory)?;
        for &(start, end) in physical {
            deps.push((start, end));
            let rel = start.checked_sub(self.data_start).ok_or(RangeError::Invalid)?;
            if rel % self.cluster_bytes != 0 { return Err(RangeError::Invalid); }
            let first = rel / self.cluster_bytes + 2;
            let last = first + (end - start).div_ceil(self.cluster_bytes) - 1;
            for &fat_start in &self.fat_starts {
                deps.push(self.entry_span(fat_start, first, last).ok_or(RangeError::Invalid)?);
            }
        }
        deps.push((entry_offset, entry_offset.checked_add(32).ok_or(RangeError::Invalid)?));
        normalize(&mut deps);
        Ok(deps)
    }
}

fn range_error(error: FatError) -> RangeError {
    match error {
        fatfs::Error::Io(error) => RangeError::Io(error),
        _ => RangeError::Invalid,
    }
}

impl<D: BlockDevice> FatVolume<D> {
    fn fat_geometry(&self, fs: &FatFs<D>) -> Result<FatGeometry, RangeError> {
        let mut block = Vec::new();
        {
            let mut dev = self.device.lock();
            let size = dev.device.logical_block_size();
            block.try_reserve_exact(size).map_err(|_| RangeError::NoMemory)?;
            block.resize(size, 0);
            dev.device.read_blocks(0, &mut block).map_err(RangeError::Io)?;
        }
        let u16_at = |at: usize| u64::from(u16::from_le_bytes([block[at], block[at + 1]]));
        let u32_at = |at: usize| u64::from(u32::from_le_bytes([block[at], block[at + 1], block[at + 2], block[at + 3]]));
        let sector = u16_at(11);
        let per_cluster = u64::from(block[13]);
        let reserved = u16_at(14);
        let fats = u64::from(block[16]);
        let root_entries = u16_at(17);
        let fat_sectors = match u16_at(22) { 0 => u32_at(36), small => small };
        let cluster_bytes = sector.checked_mul(per_cluster).ok_or(RangeError::Invalid)?;
        if !(512..=4096).contains(&sector) || !sector.is_power_of_two() || per_cluster == 0
            || fats == 0 || fat_sectors == 0 || cluster_bytes != u64::from(fs.cluster_size())
        {
            return Err(RangeError::Invalid);
        }
        let root_sectors = (root_entries * 32).div_ceil(sector);
        let fat_bytes = fat_sectors.checked_mul(sector).ok_or(RangeError::Invalid)?;
        let mut fat_starts = Vec::new();
        fat_starts.try_reserve_exact(fats as usize).map_err(|_| RangeError::NoMemory)?;
        for fat in 0..fats {
            fat_starts.push((reserved + fat * fat_sectors) * sector);
        }
        let data_start = (reserved + fats * fat_sectors + root_sectors) * sector;
        Ok(FatGeometry { fat_type: fs.fat_type(), cluster_bytes, data_start, fat_starts, fat_bytes })
    }

    pub fn read_capabilities(&self) -> FileCapabilities {
        FileCapabilities { max_file_bytes: u32::MAX as u64, writable: false, sparse: false }
    }

    /// Walk the FAT chain exactly once. Adjacent cluster ranges coalesce before
    /// admission, so a contiguous large file needs one range rather than a map
    /// entry per cluster. Temporary overlap-validation memory is also budgeted.
    pub fn plan_read(&self, path: &str, metadata_budget: usize) -> Result<FileReadMap, RangeError> {
        let fs = self.lock_fs();
        // Maps read the device directly, so staged FAT-layer blocks must have
        // reached it before the map can be called current. The namespace lock
        // is held until registration, so no write can intervene afterwards.
        fs.flush_disk().map_err(range_error)?;
        let geometry = self.fat_geometry(&fs)?;
        let (mut file, len) =
            Self::open_file_with_len_in(&fs, self.max_dir_entries(), path).map_err(range_error)?;
        let entry_offset = file.0.directory_entry_offset().ok_or(RangeError::Invalid)?;
        let object = entry_offset.checked_add(1).ok_or(RangeError::Invalid)?;
        if len > self.device_bytes || len > self.read_capabilities().max_file_bytes {
            return Err(RangeError::Invalid);
        }
        let (identity, generation) = {
            let dev = self.device.lock();
            (dev.identity, dev.maps.expected_generation(object))
        };
        let per_range = core::mem::size_of::<FileRange>() + core::mem::size_of::<(u64, u64)>();
        let limit = metadata_budget / per_range;
        let mut map = FileReadMap::new(
            FileIdentity { volume: identity, object, generation },
            len, self.device_bytes, limit * core::mem::size_of::<FileRange>(),
        )?;
        let mut left = len;
        let mut extents = file.0.extents();
        while left != 0 {
            let extent = extents.next().ok_or(RangeError::Invalid)?.map_err(range_error)?;
            let size = u64::from(extent.size);
            if size == 0 || size > left { return Err(RangeError::Invalid); }
            map.push(size, RangeKind::Data { device_offset: extent.offset })?;
            left -= size;
        }
        // Do not follow any unneeded tail of an untrusted cyclic cluster chain.
        drop(extents);
        drop(file);
        let map = map.finish()?;
        let mut physical = Vec::new();
        physical.try_reserve_exact(map.range_count()).map_err(|_| RangeError::NoMemory)?;
        for range in map.ranges() {
            if let RangeKind::Data { device_offset } = range.kind {
                physical.push((device_offset, device_offset.checked_add(range.len).ok_or(RangeError::Invalid)?));
            }
        }
        physical.sort_unstable();
        if physical.windows(2).any(|pair| pair[0].1 > pair[1].0) {
            return Err(RangeError::Invalid);
        }
        let deps = geometry.dependencies(&physical, entry_offset)?;
        self.device.lock().maps.admit(object, generation, deps)?;
        Ok(map)
    }

    pub fn read_map_is_current(&self, map: &FileReadMap) -> bool {
        map_is_current(&self.device.lock(), map)
    }

    pub fn read_source(&self, map: Arc<FileReadMap>) -> Result<FatReadSource<D>, RangeError> {
        let block_bytes = {
            let dev = self.device.lock();
            if !map_is_current(&dev, &map) { return Err(RangeError::Stale); }
            dev.device.logical_block_size()
        };
        let mut scratch = Vec::new();
        scratch.try_reserve_exact(block_bytes).map_err(|_| RangeError::NoMemory)?;
        scratch.resize(block_bytes, 0);
        Ok(FatReadSource { device: self.device.clone(), map, scratch })
    }

    pub fn read_view<'a>(&'a self, map: &'a FileReadMap, scratch: &'a mut [u8]) -> Result<FatReadView<'a, D>, RangeError> {
        let dev = self.device.lock();
        if !map_is_current(&dev, map) { return Err(RangeError::Stale); }
        if scratch.len() < dev.device.logical_block_size() { return Err(RangeError::Budget); }
        Ok(FatReadView { volume: self, map, scratch })
    }
}

impl<D: BlockDevice> ReadAt for FatReadView<'_, D> {
    type Error = RangeError;
    fn identity(&self) -> FileIdentity { self.map.identity() }
    fn len(&self) -> u64 { self.map.len() }
    fn read_at(&mut self, offset: u64, output: &mut [u8]) -> Result<usize, RangeError> {
        let mut dev = self.volume.device.lock();
        if !map_is_current(&dev, &self.map) { return Err(RangeError::Stale); }
        self.map.read_at(&mut dev.device, offset, output, self.scratch)
    }
}

impl<D: BlockDevice> ReadAt for FatReadSource<D> {
    type Error = RangeError;
    fn identity(&self) -> FileIdentity { self.map.identity() }
    fn len(&self) -> u64 { self.map.len() }
    fn read_at(&mut self, offset: u64, output: &mut [u8]) -> Result<usize, RangeError> {
        let mut dev = self.device.lock();
        if !map_is_current(&dev, &self.map) { return Err(RangeError::Stale); }
        self.map.read_at(&mut dev.device, offset, output, &mut self.scratch)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tests::{format_disk, CountingBlockDevice};

    #[test]
    fn owned_map_reads_large_file_without_rewalking_fat_and_rejects_mutation() {
        let disk = format_disk(512, 32768, 41);
        let mut volume = FatVolume::new(CountingBlockDevice { inner: disk, reads: 0 }).unwrap();
        let bytes = alloc::vec![0x43; 2 * 1024 * 1024 + 73];
        {
            let mut file = volume.create_file("large.bin").unwrap();
            let mut done = 0;
            while done < bytes.len() { done += file.write(&bytes[done..]).unwrap(); }
            file.flush().unwrap();
        }
        let map = volume.plan_read("large.bin", 128 * 1024).unwrap();
        assert_eq!(map.len(), bytes.len() as u64);
        assert_eq!(map.range_count(), 1);
        let before = volume.device.lock().device.reads;
        let mut scratch = [0; 512];
        let mut output = alloc::vec![0; 64 * 1024];
        let mut reader = volume.read_view(&map, &mut scratch).unwrap();
        let mut done = 0;
        while done < bytes.len() {
            let n = reader.read_at(done as u64, &mut output).unwrap();
            assert_eq!(&output[..n], &bytes[done..done + n]);
            done += n;
        }
        let reads = volume.device.lock().device.reads - before;
        assert!(reads <= 35, "range reads must not walk FAT: {reads}");
        drop(reader);
        {
            let mut file = volume.open_file("large.bin").unwrap();
            file.write(b"changed").unwrap();
            file.flush().unwrap();
        }
        assert!(matches!(volume.read_view(&map, &mut scratch), Err(RangeError::Stale)));
    }

    #[test]
    fn detached_source_survives_namespace_drop_and_observes_write_revocation() {
        let mut volume = FatVolume::new(format_disk(512, 32768, 15)).unwrap();
        { let mut file = volume.create_file("a").unwrap(); file.write(b"abcd").unwrap(); file.flush().unwrap(); }
        let map = Arc::new(volume.plan_read("a", 4096).unwrap());
        let mut source = volume.read_source(map.clone()).unwrap();
        let mut bytes = [0; 4];
        assert_eq!(source.read_at(0, &mut bytes).unwrap(), 4);
        assert_eq!(&bytes, b"abcd");
        { let mut file = volume.open_file("a").unwrap(); file.write(b"xy").unwrap(); file.flush().unwrap(); }
        assert!(matches!(source.read_at(0, &mut bytes), Err(RangeError::Stale)));
        let map = Arc::new(volume.plan_read("a", 4096).unwrap());
        let mut source = volume.read_source(map).unwrap();
        // Namespace drop may flush volume status and revoke the epoch. The
        // independent device remains alive, never a borrowed/dangling FAT file.
        drop(volume);
        let current = map_is_current(&source.device.lock(), &source.map);
        if current {
            assert_eq!(source.read_at(0, &mut bytes), Ok(4));
            assert_eq!(&bytes, b"xycd");
        } else {
            assert_eq!(source.read_at(0, &mut bytes), Err(RangeError::Stale));
        }
    }

    #[test]
    fn file_identity_is_stable_across_plans_and_case_aliases_until_this_file_changes() {
        let mut volume = FatVolume::new(format_disk(512, 32768, 51)).unwrap();
        { let mut file = volume.create_file("a.bin").unwrap(); file.write(b"a").unwrap(); file.flush().unwrap(); }
        let a = volume.plan_read("a.bin", 4096).unwrap().identity();
        assert_eq!(a, volume.plan_read("A.BIN", 4096).unwrap().identity());
        { let mut file = volume.create_file("b.bin").unwrap(); file.write(b"b").unwrap(); file.flush().unwrap(); }
        let same_a = volume.plan_read("a.bin", 4096).unwrap().identity();
        let b = volume.plan_read("b.bin", 4096).unwrap().identity();
        assert_eq!(a, same_a, "an unrelated create must not change a's identity");
        assert_eq!(a.volume, b.volume);
        assert_ne!(a.object, b.object);
        assert_ne!(a.generation, b.generation);
        { let mut file = volume.open_file("a.bin").unwrap(); file.write(b"z").unwrap(); file.flush().unwrap(); }
        let new_a = volume.plan_read("a.bin", 4096).unwrap().identity();
        assert_eq!(a.object, new_a.object);
        assert_ne!(a.generation, new_a.generation);
    }

    fn fill(volume: &mut FatVolume<storage_core::MemBlockDevice>, name: &str, byte: u8, len: usize) {
        let mut file = volume.create_file(name).unwrap();
        let bytes = alloc::vec![byte; len];
        let mut done = 0;
        while done < bytes.len() { done += file.write(&bytes[done..]).unwrap(); }
        file.flush().unwrap();
    }

    /// Plans `target`, runs `mutate`, and reports whether the map stayed valid.
    fn survives(mutate: impl FnOnce(&mut FatVolume<storage_core::MemBlockDevice>)) -> bool {
        let mut volume = FatVolume::new(format_disk(512, 32768, 77)).unwrap();
        let cluster = volume.lock_fs().cluster_size() as usize;
        fill(&mut volume, "target.bin", 0x11, cluster * 3 + 5);
        fill(&mut volume, "other.bin", 0x22, cluster * 2);
        let map = volume.plan_read("target.bin", 4096).unwrap();
        assert!(volume.read_map_is_current(&map));
        mutate(&mut volume);
        let current = volume.read_map_is_current(&map);
        let mut scratch = [0; 512];
        assert_eq!(volume.read_view(&map, &mut scratch).is_ok(), current);
        current
    }

    #[test]
    fn unrelated_writes_keep_the_map_but_every_change_to_the_file_revokes_it() {
        let cluster = {
            let volume = FatVolume::new(format_disk(512, 32768, 77)).unwrap();
            let size = volume.lock_fs().cluster_size() as usize;
            size
        };
        // Unrelated: create/append/overwrite/truncate/delete other files and dirs.
        assert!(survives(|v| fill(v, "new.bin", 0x33, cluster * 4)));
        assert!(survives(|v| {
            let mut other = v.open_file("other.bin").unwrap();
            other.seek(SeekFrom::End(0)).unwrap();
            other.write(&alloc::vec![9; cluster]).unwrap();
            other.flush().unwrap();
        }));
        assert!(survives(|v| {
            let mut other = v.open_file("other.bin").unwrap();
            other.write(b"overwrite").unwrap();
            other.flush().unwrap();
        }));
        assert!(survives(|v| {
            let mut other = v.open_file("other.bin").unwrap();
            other.truncate().unwrap();
            other.flush().unwrap();
        }));
        // This file: append (size + chain), in-place overwrite (data only),
        // truncate (size + chain).
        assert!(!survives(|v| {
            let mut file = v.open_file("target.bin").unwrap();
            file.seek(SeekFrom::End(0)).unwrap();
            file.write(b"tail").unwrap();
            file.flush().unwrap();
        }));
        assert!(!survives(|v| {
            let mut file = v.open_file("target.bin").unwrap();
            file.seek(SeekFrom::Start(cluster as u64 + 7)).unwrap();
            file.write(b"x").unwrap();
            file.flush().unwrap();
        }));
        assert!(!survives(|v| {
            let mut file = v.open_file("target.bin").unwrap();
            file.seek(SeekFrom::Start(cluster as u64)).unwrap();
            file.truncate().unwrap();
            file.flush().unwrap();
        }));
    }

    #[test]
    fn namespace_changes_to_the_file_revoke_the_map() {
        use storage_core::{FsOpenOptions, FsVolume};
        let create = FsOpenOptions { create: true, write: true, ..Default::default() };
        assert!(!survives(|v| { FsVolume::rename(v, "target.bin", "moved.bin", false, false).unwrap(); }));
        assert!(!survives(|v| { v.unlink("target.bin").unwrap(); }));
        // Delete and recreate under the same name: the directory slot and the
        // first cluster are typically reused, yet the old map must not survive.
        assert!(!survives(|v| {
            v.unlink("target.bin").unwrap();
            v.open_prepare("target.bin", create).unwrap();
            v.write_at("target.bin", 0, b"fresh").unwrap();
        }));
        // Replacing the file by renaming another file over it.
        assert!(!survives(|v| { FsVolume::rename(v, "other.bin", "target.bin", true, false).unwrap(); }));
    }

    #[test]
    fn freed_clusters_reallocated_to_another_file_revoke_the_map() {
        let mut volume = FatVolume::new(format_disk(512, 32768, 78)).unwrap();
        let cluster = volume.lock_fs().cluster_size() as usize;
        fill(&mut volume, "target.bin", 0x11, cluster * 2);
        let map = volume.plan_read("target.bin", 4096).unwrap();
        // Truncating through the volume interface changes only the FAT chain
        // and the size, the data clusters stay on the media until reused.
        storage_core::FsVolume::truncate(&volume, "target.bin", 0).unwrap();
        assert!(!volume.read_map_is_current(&map));
        let mut scratch = [0; 512];
        assert!(matches!(volume.read_view(&map, &mut scratch), Err(RangeError::Stale)));
    }

    #[test]
    fn fragmented_files_keep_range_reads_linear_and_map_memory_bounded() {
        let mut volume = FatVolume::new(format_disk(512, 32768, 23)).unwrap();
        let cluster = volume.lock_fs().cluster_size() as usize;
        volume.create_file("a").unwrap();
        volume.create_file("b").unwrap();
        let data = alloc::vec![0x7b; cluster];
        for _ in 0..48 {
            let mut a = volume.open_file("a").unwrap();
            a.seek(SeekFrom::End(0)).unwrap();
            let mut done = 0;
            while done < data.len() { done += a.write(&data[done..]).unwrap(); }
            a.flush().unwrap();
            drop(a);
            let mut b = volume.open_file("b").unwrap();
            b.seek(SeekFrom::End(0)).unwrap();
            let mut done = 0;
            while done < data.len() { done += b.write(&data[done..]).unwrap(); }
            b.flush().unwrap();
        }
        assert!(matches!(volume.plan_read("a", 48), Err(RangeError::Budget)));
        let map = volume.plan_read("a", 8192).unwrap();
        assert!(map.range_count() > 1);
        assert!(map.metadata_bytes() <= 8192);
        let mut output = alloc::vec![0; cluster + 31];
        let mut scratch = [0; 512];
        let mut reader = volume.read_view(&map, &mut scratch).unwrap();
        for offset in [0, 13, cluster as u64 - 1, 17 * cluster as u64, map.len() - 1] {
            let n = reader.read_at(offset, &mut output).unwrap();
            assert_eq!(n, (map.len() - offset).min(output.len() as u64) as usize);
            assert!(output[..n].iter().all(|byte| *byte == 0x7b));
        }
    }

    fn damaged_image(reserved_cluster: bool) -> storage_core::MemBlockDevice {
        let mut volume = FatVolume::new(format_disk(512, 32768, 31)).unwrap();
        let cluster_bytes = volume.lock_fs().cluster_size() as usize;
        let mut file = volume.create_file("bad.bin").unwrap();
        let bytes = alloc::vec![0x18; cluster_bytes * 2];
        let mut done = 0;
        while done < bytes.len() { done += file.write(&bytes[done..]).unwrap(); }
        file.flush().unwrap(); drop(file);
        let map = volume.plan_read("bad.bin", 4096).unwrap();
        let first = match map.ranges()[0].kind { RangeKind::Data { device_offset } => device_offset, _ => unreachable!() };
        let device = volume.device.clone();
        volume.unmount().unwrap();
        let mut device = Arc::try_unwrap(device).ok().unwrap().into_inner().device;
        let mut boot = [0; 512];
        device.read_blocks(0, &mut boot).unwrap();
        let reserved = u16::from_le_bytes([boot[14], boot[15]]) as u64;
        let fats = boot[16] as u64;
        let sectors_per_fat = u16::from_le_bytes([boot[22], boot[23]]) as u64;
        assert!(sectors_per_fat != 0, "fixture must be FAT16");
        let root_entries = u16::from_le_bytes([boot[17], boot[18]]) as u64;
        let root_lba = reserved + fats * sectors_per_fat;
        let mut block = [0; 512];
        if reserved_cluster {
            device.read_blocks(root_lba, &mut block).unwrap();
            let entry = block.chunks_exact_mut(32).find(|entry| &entry[..11] == b"BAD     BIN").unwrap();
            entry[26..28].copy_from_slice(&1u16.to_le_bytes());
            device.write_blocks(root_lba, &block).unwrap();
        } else {
            let data_lba = root_lba + (root_entries * 32).div_ceil(512);
            let cluster = (first - data_lba * 512) / cluster_bytes as u64 + 2;
            for fat in 0..fats {
                let lba = reserved + fat * sectors_per_fat + cluster * 2 / 512;
                device.read_blocks(lba, &mut block).unwrap();
                let offset = (cluster * 2 % 512) as usize;
                block[offset..offset + 2].copy_from_slice(&(cluster as u16).to_le_bytes());
                device.write_blocks(lba, &block).unwrap();
            }
        }
        device
    }

    #[test]
    fn cyclic_and_reserved_cluster_images_fail_without_hanging_or_panicking() {
        for reserved_cluster in [false, true] {
            let volume = FatVolume::new(damaged_image(reserved_cluster)).unwrap();
            assert!(matches!(volume.plan_read("bad.bin", 4096), Err(RangeError::Invalid)));
        }
    }

    #[test]
    fn maps_are_bound_to_volume_and_metadata_admission() {
        let mut a = FatVolume::new(format_disk(512, 16384, 1)).unwrap();
        let b = FatVolume::new(format_disk(512, 16384, 2)).unwrap();
        a.create_file("a").unwrap().write(b"content").unwrap();
        let map = a.plan_read("a", 4096).unwrap();
        let mut scratch = [0; 512];
        assert!(matches!(b.read_view(&map, &mut scratch), Err(RangeError::Stale)));
        assert!(matches!(a.plan_read("a", 0), Err(RangeError::Budget)));
    }
}
