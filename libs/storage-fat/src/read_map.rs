//! Owned FAT file-range admission using the pinned upstream extent iterator.
//!
//! The volume remains the sole filesystem owner. Maps retain no File<'volume>
//! borrow, pathname, second FatVolume, or raw-device capability. Bind a map back
//! to its originating volume/version for each bounded read. Every write attempt
//! revokes prior maps before it can modify media. Mutable VFS opens are not
//! enabled by this read-only view; later backends can implement ReadAt directly.
//! Malformed chains are bounded by the admitted length and reject overlapping
//! cluster ranges, premature EOF, out-of-device extents and metadata pressure.

use super::*;
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

fn range_error(error: FatError) -> RangeError {
    match error {
        fatfs::Error::Io(error) => RangeError::Io(error),
        _ => RangeError::Invalid,
    }
}

impl<D: BlockDevice> FatVolume<D> {
    pub fn read_capabilities(&self) -> FileCapabilities {
        FileCapabilities { max_file_bytes: u32::MAX as u64, writable: false, sparse: false }
    }

    /// Walk the FAT chain exactly once. Adjacent cluster ranges coalesce before
    /// admission, so a contiguous large file needs one range rather than a map
    /// entry per cluster. Temporary overlap-validation memory is also budgeted.
    pub fn plan_read(&self, path: &str, metadata_budget: usize) -> Result<FileReadMap, RangeError> {
        let (identity, version) = {
            let dev = self.device.lock();
            (dev.identity, dev.version)
        };
        let (mut file, len) = self.open_file_with_len(path).map_err(range_error)?;
        let object = file.0.directory_entry_offset().and_then(|offset| offset.checked_add(1))
            .ok_or(RangeError::Invalid)?;
        if len > self.device_bytes || len > self.read_capabilities().max_file_bytes {
            return Err(RangeError::Invalid);
        }
        let per_range = core::mem::size_of::<FileRange>() + core::mem::size_of::<(u64, u64)>();
        let limit = metadata_budget / per_range;
        let mut map = FileReadMap::new(
            FileIdentity { volume: identity, object, generation: version },
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
        if self.device.lock().version != version { return Err(RangeError::Stale); }
        Ok(map)
    }

    pub fn read_map_is_current(&self, map: &FileReadMap) -> bool {
        let dev = self.device.lock();
        let id = map.identity();
        id.volume == dev.identity && id.generation == dev.version
    }

    pub fn read_source(&self, map: Arc<FileReadMap>) -> Result<FatReadSource<D>, RangeError> {
        let block_bytes = {
            let dev = self.device.lock();
            let id = map.identity();
            if id.volume != dev.identity || id.generation != dev.version { return Err(RangeError::Stale); }
            dev.device.logical_block_size()
        };
        let mut scratch = Vec::new();
        scratch.try_reserve_exact(block_bytes).map_err(|_| RangeError::NoMemory)?;
        scratch.resize(block_bytes, 0);
        Ok(FatReadSource { device: self.device.clone(), map, scratch })
    }

    pub fn read_view<'a>(&'a self, map: &'a FileReadMap, scratch: &'a mut [u8]) -> Result<FatReadView<'a, D>, RangeError> {
        let dev = self.device.lock();
        let id = map.identity();
        if id.volume != dev.identity || id.generation != dev.version { return Err(RangeError::Stale); }
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
        let id = self.map.identity();
        if id.volume != dev.identity || id.generation != dev.version { return Err(RangeError::Stale); }
        self.map.read_at(&mut dev.device, offset, output, self.scratch)
    }
}

impl<D: BlockDevice> ReadAt for FatReadSource<D> {
    type Error = RangeError;
    fn identity(&self) -> FileIdentity { self.map.identity() }
    fn len(&self) -> u64 { self.map.len() }
    fn read_at(&mut self, offset: u64, output: &mut [u8]) -> Result<usize, RangeError> {
        let mut dev = self.device.lock();
        let id = self.map.identity();
        if id.volume != dev.identity || id.generation != dev.version { return Err(RangeError::Stale); }
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
        let volume = FatVolume::new(CountingBlockDevice { inner: disk, reads: 0 }).unwrap();
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
        let volume = FatVolume::new(format_disk(512, 32768, 15)).unwrap();
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
        let current = source.device.lock().version == source.identity().generation;
        if current {
            assert_eq!(source.read_at(0, &mut bytes), Ok(4));
            assert_eq!(&bytes, b"xycd");
        } else {
            assert_eq!(source.read_at(0, &mut bytes), Err(RangeError::Stale));
        }
    }

    #[test]
    fn file_identity_is_stable_across_plans_and_case_aliases_but_not_mutation() {
        let volume = FatVolume::new(format_disk(512, 32768, 51)).unwrap();
        { let mut file = volume.create_file("a.bin").unwrap(); file.write(b"a").unwrap(); file.flush().unwrap(); }
        let a = volume.plan_read("a.bin", 4096).unwrap().identity();
        assert_eq!(a, volume.plan_read("A.BIN", 4096).unwrap().identity());
        { let mut file = volume.create_file("b.bin").unwrap(); file.write(b"b").unwrap(); file.flush().unwrap(); }
        let new_a = volume.plan_read("a.bin", 4096).unwrap().identity();
        let b = volume.plan_read("b.bin", 4096).unwrap().identity();
        assert_eq!(a.object, new_a.object);
        assert_ne!(a.generation, new_a.generation);
        assert_ne!(new_a.object, b.object);
    }

    #[test]
    fn fragmented_files_keep_range_reads_linear_and_map_memory_bounded() {
        let volume = FatVolume::new(format_disk(512, 32768, 23)).unwrap();
        let cluster = volume.fs.cluster_size() as usize;
        let mut a = volume.create_file("a").unwrap();
        let mut b = volume.create_file("b").unwrap();
        let data = alloc::vec![0x7b; cluster];
        for _ in 0..48 {
            let mut done = 0;
            while done < data.len() { done += a.write(&data[done..]).unwrap(); }
            let mut done = 0;
            while done < data.len() { done += b.write(&data[done..]).unwrap(); }
        }
        a.flush().unwrap(); b.flush().unwrap(); drop(a); drop(b);
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
        let volume = FatVolume::new(format_disk(512, 32768, 31)).unwrap();
        let cluster_bytes = volume.fs.cluster_size() as usize;
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
        let a = FatVolume::new(format_disk(512, 16384, 1)).unwrap();
        let b = FatVolume::new(format_disk(512, 16384, 2)).unwrap();
        a.create_file("a").unwrap().write(b"content").unwrap();
        let map = a.plan_read("a", 4096).unwrap();
        let mut scratch = [0; 512];
        assert!(matches!(b.read_view(&map, &mut scratch), Err(RangeError::Stale)));
        assert!(matches!(a.plan_read("a", 0), Err(RangeError::Budget)));
    }
}
