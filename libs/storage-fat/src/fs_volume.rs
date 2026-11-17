use alloc::string::{String, ToString};
use alloc::vec::Vec;
use core::cmp::min;

use fatfs::{Date, DateTime, FileAttributes, Read, Seek, SeekFrom, Time, Write};
use storage_core::fs::{
    dos_attrs, CasePolicy, FsCapabilities, FsDirEntry, FsError, FsMetadata, FsNodeKind,
    FsOpenOptions, FsStatFs, FsTimestamp, FsVolume,
};
use storage_core::{BlockDevice, StorageError};

use crate::{
    check_dir_walk, find_entry_case_insensitive, normalize_fat_path_cow, split_parent_child,
    FatError, FatFs, FatVolume, InnerDir, FILE_READ_CHUNK_CAP,
};

const MSDOS_SUPER_MAGIC: u64 = 0x4d44;
const FAT_MAX_NAME_LEN: u32 = 255;
const ZERO_FILL_CHUNK: [u8; 4096] = [0; 4096];

pub(crate) fn map_fat_error(err: FatError) -> FsError {
    match err {
        fatfs::Error::Io(e) => FsError::Io(e),
        fatfs::Error::UnexpectedEof => FsError::Io(StorageError::UnexpectedEof),
        fatfs::Error::WriteZero => FsError::Io(StorageError::WriteZero),
        fatfs::Error::InvalidInput => FsError::InvalidInput,
        fatfs::Error::NotFound => FsError::NotFound,
        fatfs::Error::AlreadyExists => FsError::AlreadyExists,
        fatfs::Error::DirectoryIsNotEmpty => FsError::DirectoryNotEmpty,
        fatfs::Error::CorruptedFileSystem => FsError::Io(StorageError::DeviceFault),
        fatfs::Error::NotEnoughSpace => FsError::NoSpace,
        fatfs::Error::InvalidFileNameLength => FsError::NameTooLong,
        fatfs::Error::UnsupportedFileNameCharacter => FsError::InvalidInput,
        _ => FsError::InvalidInput,
    }
}

fn node_id_for_canonical(canonical: &str) -> u64 {
    if canonical.is_empty() {
        return 1;
    }
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for &byte in canonical.as_bytes() {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x0100_0000_01b3);
    }
    if hash <= 1 { 2 } else { hash }
}

fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = y - if m <= 2 { 1 } else { 0 };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = (y - era * 400) as u64;
    let m = m as u64;
    let d = d as u64;
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe as i64 - 719_468
}

fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (y + if m <= 2 { 1 } else { 0 }, m as u32, d as u32)
}

fn date_to_timestamp(date: Date) -> FsTimestamp {
    if !(1980..=2107).contains(&date.year)
        || !(1..=12).contains(&date.month)
        || !(1..=31).contains(&date.day)
    {
        return FsTimestamp::default();
    }
    let days = days_from_civil(i64::from(date.year), u32::from(date.month), u32::from(date.day));
    FsTimestamp {
        secs: days * 86_400,
        nanos: 0,
    }
}

fn datetime_to_timestamp(dt: DateTime) -> FsTimestamp {
    let base = date_to_timestamp(dt.date);
    if base.secs == 0 && dt.date.year < 1980 {
        return FsTimestamp::default();
    }
    let hour = i64::from(dt.time.hour.min(23));
    let min = i64::from(dt.time.min.min(59));
    let sec = i64::from(dt.time.sec.min(59));
    let millis = u32::from(dt.time.millis.min(999));
    FsTimestamp {
        secs: base.secs + hour * 3_600 + min * 60 + sec,
        nanos: millis * 1_000_000,
    }
}

fn timestamp_to_date(ts: FsTimestamp) -> Date {
    let days = ts.secs.div_euclid(86_400);
    let (y, m, d) = civil_from_days(days);
    let year = y.clamp(1980, 2107) as u16;
    Date::new(year, m as u16, d as u16)
}

fn timestamp_to_datetime(ts: FsTimestamp) -> DateTime {
    let days = ts.secs.div_euclid(86_400);
    let rem = ts.secs.rem_euclid(86_400) as u64;
    let (y, m, d) = civil_from_days(days);
    let year = y.clamp(1980, 2107) as u16;
    let hour = (rem / 3_600) as u16;
    let min = ((rem % 3_600) / 60) as u16;
    let sec = (rem % 60) as u16;
    let millis = (ts.nanos / 1_000_000).min(999) as u16;
    DateTime::new(
        Date::new(year, m as u16, d as u16),
        Time::new(hour, min, sec, millis),
    )
}

fn append_canonical(parent_canon: &str, name: &str) -> String {
    if parent_canon.is_empty() {
        name.to_string()
    } else {
        let mut out = String::with_capacity(parent_canon.len() + 1 + name.len());
        out.push_str(parent_canon);
        out.push('/');
        out.push_str(name);
        out
    }
}

fn root_metadata(read_only: bool) -> FsMetadata {
    FsMetadata {
        node_id: 1,
        kind: FsNodeKind::Directory,
        len: 0,
        allocation_size: 0,
        nlink: 2,
        mode: if read_only { 0o040555 } else { 0o040755 },
        uid: 0,
        gid: 0,
        dos_attrs: dos_attrs::DIRECTORY,
        atime: FsTimestamp::default(),
        mtime: FsTimestamp::default(),
        ctime: FsTimestamp::default(),
        crtime: FsTimestamp::default(),
    }
}

fn entry_to_metadata<D: BlockDevice>(
    entry: &fatfs::DirEntry<'_, crate::FatDisk<D>, fatfs::DefaultTimeProvider, fatfs::LossyOemCpConverter>,
    canonical: &str,
    cluster_size: u64,
    read_only_volume: bool,
) -> FsMetadata {
    let is_dir = entry.is_dir();
    let len = if is_dir { 0 } else { entry.len() };
    let allocation_size = if len == 0 || cluster_size == 0 {
        0
    } else {
        len.div_ceil(cluster_size) * cluster_size
    };
    let attrs = entry.attributes();
    let dos = u32::from(attrs.bits());
    let ro = read_only_volume || attrs.contains(FileAttributes::READ_ONLY);
    let mode = if is_dir {
        if ro { 0o040555 } else { 0o040755 }
    } else if ro {
        0o100444
    } else {
        0o100644
    };
    let mtime = datetime_to_timestamp(entry.modified());
    let crtime = datetime_to_timestamp(entry.created());
    let atime = date_to_timestamp(entry.accessed());
    FsMetadata {
        node_id: node_id_for_canonical(canonical),
        kind: if is_dir {
            FsNodeKind::Directory
        } else {
            FsNodeKind::File
        },
        len,
        allocation_size,
        nlink: if is_dir { 2 } else { 1 },
        mode,
        uid: 0,
        gid: 0,
        dos_attrs: dos,
        atime,
        mtime,
        ctime: mtime,
        crtime,
    }
}

impl<D: BlockDevice> FatVolume<D> {
    fn open_dir_checked<'a>(
        &self,
        fs: &'a FatFs<D>,
        normalized_dir: &str,
    ) -> Result<(InnerDir<'a, D>, String), FsError> {
        let mut dir = fs.root_dir();
        let mut canonical = String::new();
        let limit = self.max_dir_entries();
        for component in normalized_dir.split('/').filter(|c| !c.is_empty()) {
            let entry =
                find_entry_case_insensitive(&dir, component, limit).map_err(map_fat_error)?;
            if !entry.is_dir() {
                return Err(FsError::NotADirectory);
            }
            if !canonical.is_empty() {
                canonical.push('/');
            }
            canonical.push_str(&entry.file_name());
            dir = entry.to_dir();
        }
        Ok((dir, canonical))
    }

    fn zero_fill(
        file: &mut fatfs::File<'_, crate::FatDisk<D>, fatfs::DefaultTimeProvider, fatfs::LossyOemCpConverter>,
        mut remaining: u64,
    ) -> Result<(), FsError> {
        while remaining > 0 {
            let chunk = min(remaining, ZERO_FILL_CHUNK.len() as u64) as usize;
            let written = file.write(&ZERO_FILL_CHUNK[..chunk]).map_err(map_fat_error)?;
            if written == 0 {
                return Err(FsError::Io(StorageError::WriteZero));
            }
            remaining -= written as u64;
        }
        Ok(())
    }
}

impl<D: BlockDevice + Send + 'static> FsVolume for FatVolume<D> {
    fn capabilities(&self) -> FsCapabilities {
        FsCapabilities {
            read_only: self.read_only,
            case_policy: CasePolicy::AsciiInsensitive,
            supports_symlinks: false,
            supports_hardlinks: false,
            supports_unlink_while_open: false,
            supports_sparse: false,
            supports_nt_attrs: true,
            supports_streams: false,
            supports_share_modes: false,
            max_name_len: FAT_MAX_NAME_LEN,
        }
    }

    fn is_immutable(&self) -> bool {
        self.read_only
    }

    fn canonical_key(&self, path: &str) -> Result<String, FsError> {
        self.canonical_path(path).map_err(map_fat_error)
    }

    fn metadata(&self, path: &str) -> Result<FsMetadata, FsError> {
        let normalized = normalize_fat_path_cow(path);
        if normalized.is_empty() {
            return Ok(root_metadata(self.read_only));
        }
        let (parent, child) = split_parent_child(normalized.as_ref()).ok_or(FsError::InvalidInput)?;
        let fs = self.lock_fs();
        let cluster_size = u64::from(fs.cluster_size());
        let (dir, parent_canon) = self.open_dir_checked(&fs, parent)?;
        let entry = find_entry_case_insensitive(&dir, child, self.max_dir_entries())
            .map_err(map_fat_error)?;
        let canon = append_canonical(&parent_canon, &entry.file_name());
        Ok(entry_to_metadata(&entry, &canon, cluster_size, self.read_only))
    }

    fn read_dir(&self, path: &str) -> Result<Vec<FsDirEntry>, FsError> {
        let normalized = normalize_fat_path_cow(path);
        let fs = self.lock_fs();
        let (dir, parent_canon) = self.open_dir_checked(&fs, normalized.as_ref())?;
        let limit = self.max_dir_entries();
        let mut entries = Vec::new();
        for (seen, entry_result) in dir.iter().enumerate() {
            check_dir_walk(seen, limit).map_err(map_fat_error)?;
            let entry = entry_result.map_err(map_fat_error)?;
            let name = entry.file_name();
            if name == "." || name == ".." {
                continue;
            }
            entries
                .try_reserve(1)
                .map_err(|_| FsError::NoSpace)?;
            let canon = append_canonical(&parent_canon, &name);
            let is_dir = entry.is_dir();
            entries.push(FsDirEntry {
                name,
                node_id: node_id_for_canonical(&canon),
                kind: if is_dir {
                    FsNodeKind::Directory
                } else {
                    FsNodeKind::File
                },
                len: if is_dir { 0 } else { entry.len() },
                dos_attrs: u32::from(entry.attributes().bits()),
            });
        }
        Ok(entries)
    }

    fn statfs(&self) -> Result<FsStatFs, FsError> {
        let fs = self.lock_fs();
        let stats = fs.stats().map_err(map_fat_error)?;
        let block_size = u64::from(stats.cluster_size());
        let total_blocks = u64::from(stats.total_clusters());
        let free_blocks = u64::from(stats.free_clusters());
        Ok(FsStatFs {
            fs_type_magic: MSDOS_SUPER_MAGIC,
            block_size,
            total_blocks,
            free_blocks,
            avail_blocks: free_blocks,
            total_inodes: 0,
            free_inodes: 0,
            max_name_len: FAT_MAX_NAME_LEN,
            fs_name: "vfat",
            case_sensitive: false,
            case_preserved: true,
        })
    }

    fn open_prepare(&self, path: &str, opts: FsOpenOptions) -> Result<FsMetadata, FsError> {
        let mutating = opts.create || opts.create_new || opts.truncate || opts.write || opts.append;
        if self.read_only && mutating {
            return Err(FsError::ReadOnly);
        }
        let normalized = normalize_fat_path_cow(path);
        if normalized.is_empty() {
            if opts.create_new {
                return Err(FsError::AlreadyExists);
            }
            if opts.write || opts.truncate || opts.append {
                return Err(FsError::IsADirectory);
            }
            return Ok(root_metadata(self.read_only));
        }
        let (parent, child) = split_parent_child(normalized.as_ref()).ok_or(FsError::InvalidInput)?;
        let fs = self.lock_fs();
        let cluster_size = u64::from(fs.cluster_size());
        let (dir, parent_canon) = self.open_dir_checked(&fs, parent)?;
        let limit = self.max_dir_entries();
        match find_entry_case_insensitive(&dir, child, limit) {
            Ok(entry) => {
                if opts.create_new {
                    return Err(FsError::AlreadyExists);
                }
                if entry.is_dir() {
                    if opts.write || opts.truncate || opts.append {
                        return Err(FsError::IsADirectory);
                    }
                    let canon = append_canonical(&parent_canon, &entry.file_name());
                    return Ok(entry_to_metadata(&entry, &canon, cluster_size, self.read_only));
                }
                if (opts.write || opts.truncate || opts.append)
                    && entry.attributes().contains(FileAttributes::READ_ONLY)
                {
                    return Err(FsError::PermissionDenied);
                }
                let canon = append_canonical(&parent_canon, &entry.file_name());
                let mut meta = entry_to_metadata(&entry, &canon, cluster_size, self.read_only);
                if opts.truncate {
                    self.bump_version()?;
                    let mut file = entry.to_file();
                    file.truncate().map_err(map_fat_error)?;
                    file.flush().map_err(map_fat_error)?;
                    meta.len = 0;
                    meta.allocation_size = 0;
                }
                Ok(meta)
            }
            Err(fatfs::Error::NotFound) => {
                if !(opts.create || opts.create_new) {
                    return Err(FsError::NotFound);
                }
                self.bump_version()?;
                let mut file = dir.create_file(child).map_err(map_fat_error)?;
                file.flush().map_err(map_fat_error)?;
                drop(file);
                let entry = find_entry_case_insensitive(&dir, child, limit)
                    .map_err(map_fat_error)?;
                let canon = append_canonical(&parent_canon, &entry.file_name());
                Ok(entry_to_metadata(&entry, &canon, cluster_size, self.read_only))
            }
            Err(err) => Err(map_fat_error(err)),
        }
    }

    fn read_at(&self, path: &str, offset: u64, out: &mut [u8]) -> Result<usize, FsError> {
        let normalized = normalize_fat_path_cow(path);
        if normalized.is_empty() {
            return Err(FsError::IsADirectory);
        }
        let (parent, child) = split_parent_child(normalized.as_ref()).ok_or(FsError::InvalidInput)?;
        let fs = self.lock_fs();
        let (dir, _) = self.open_dir_checked(&fs, parent)?;
        let entry = find_entry_case_insensitive(&dir, child, self.max_dir_entries())
            .map_err(map_fat_error)?;
        if entry.is_dir() {
            return Err(FsError::IsADirectory);
        }
        let len = entry.len();
        if out.is_empty() || offset >= len {
            return Ok(0);
        }
        let mut file = entry.to_file();
        file.seek(SeekFrom::Start(offset)).map_err(map_fat_error)?;
        let mut done = 0usize;
        while done < out.len() {
            let chunk = (out.len() - done).min(FILE_READ_CHUNK_CAP);
            let n = file.read(&mut out[done..done + chunk]).map_err(map_fat_error)?;
            if n == 0 {
                break;
            }
            done += n;
        }
        Ok(done)
    }

    fn write_at(&self, path: &str, offset: u64, data: &[u8]) -> Result<(usize, u64), FsError> {
        if self.read_only {
            return Err(FsError::ReadOnly);
        }
        let normalized = normalize_fat_path_cow(path);
        if normalized.is_empty() {
            return Err(FsError::IsADirectory);
        }
        let end = offset
            .checked_add(data.len() as u64)
            .ok_or(FsError::TooBig)?;
        if end > u64::from(u32::MAX) {
            return Err(FsError::TooBig);
        }
        let (parent, child) = split_parent_child(normalized.as_ref()).ok_or(FsError::InvalidInput)?;
        let fs = self.lock_fs();
        let (dir, _) = self.open_dir_checked(&fs, parent)?;
        let entry = find_entry_case_insensitive(&dir, child, self.max_dir_entries())
            .map_err(map_fat_error)?;
        if entry.is_dir() {
            return Err(FsError::IsADirectory);
        }
        if entry.attributes().contains(FileAttributes::READ_ONLY) {
            return Err(FsError::PermissionDenied);
        }
        let current_len = entry.len();
        self.bump_version()?;
        let mut file = entry.to_file();
        if offset > current_len {
            file.seek(SeekFrom::Start(current_len)).map_err(map_fat_error)?;
            Self::zero_fill(&mut file, offset - current_len)?;
        } else {
            file.seek(SeekFrom::Start(offset)).map_err(map_fat_error)?;
        }
        let mut written = 0usize;
        while written < data.len() {
            let n = file.write(&data[written..]).map_err(map_fat_error)?;
            if n == 0 {
                return Err(FsError::Io(StorageError::WriteZero));
            }
            written += n;
        }
        file.flush().map_err(map_fat_error)?;
        let new_len = current_len.max(offset + written as u64);
        Ok((written, new_len))
    }

    fn truncate(&self, path: &str, new_len: u64) -> Result<u64, FsError> {
        if self.read_only {
            return Err(FsError::ReadOnly);
        }
        if new_len > u64::from(u32::MAX) {
            return Err(FsError::TooBig);
        }
        let normalized = normalize_fat_path_cow(path);
        if normalized.is_empty() {
            return Err(FsError::IsADirectory);
        }
        let (parent, child) = split_parent_child(normalized.as_ref()).ok_or(FsError::InvalidInput)?;
        let fs = self.lock_fs();
        let (dir, _) = self.open_dir_checked(&fs, parent)?;
        let entry = find_entry_case_insensitive(&dir, child, self.max_dir_entries())
            .map_err(map_fat_error)?;
        if entry.is_dir() {
            return Err(FsError::IsADirectory);
        }
        if entry.attributes().contains(FileAttributes::READ_ONLY) {
            return Err(FsError::PermissionDenied);
        }
        let current_len = entry.len();
        self.bump_version()?;
        let mut file = entry.to_file();
        if new_len < current_len {
            file.seek(SeekFrom::Start(new_len)).map_err(map_fat_error)?;
            file.truncate().map_err(map_fat_error)?;
        } else if new_len > current_len {
            file.seek(SeekFrom::Start(current_len)).map_err(map_fat_error)?;
            Self::zero_fill(&mut file, new_len - current_len)?;
        }
        file.flush().map_err(map_fat_error)?;
        Ok(new_len)
    }

    fn mkdir(&self, path: &str) -> Result<(), FsError> {
        if self.read_only {
            return Err(FsError::ReadOnly);
        }
        let normalized = normalize_fat_path_cow(path);
        if normalized.is_empty() {
            return Err(FsError::AlreadyExists);
        }
        let (parent, child) = split_parent_child(normalized.as_ref()).ok_or(FsError::InvalidInput)?;
        let fs = self.lock_fs();
        let (dir, _) = self.open_dir_checked(&fs, parent)?;
        match find_entry_case_insensitive(&dir, child, self.max_dir_entries()) {
            Ok(_) => return Err(FsError::AlreadyExists),
            Err(fatfs::Error::NotFound) => {}
            Err(err) => return Err(map_fat_error(err)),
        }
        self.bump_version()?;
        dir.create_dir(child).map_err(map_fat_error)?;
        Ok(())
    }

    fn unlink(&self, path: &str) -> Result<(), FsError> {
        if self.read_only {
            return Err(FsError::ReadOnly);
        }
        let normalized = normalize_fat_path_cow(path);
        if normalized.is_empty() {
            return Err(FsError::IsADirectory);
        }
        let (parent, child) = split_parent_child(normalized.as_ref()).ok_or(FsError::InvalidInput)?;
        let fs = self.lock_fs();
        let (dir, _) = self.open_dir_checked(&fs, parent)?;
        let entry = find_entry_case_insensitive(&dir, child, self.max_dir_entries())
            .map_err(map_fat_error)?;
        if entry.is_dir() {
            return Err(FsError::IsADirectory);
        }
        drop(entry);
        self.bump_version()?;
        dir.remove(child).map_err(map_fat_error)?;
        Ok(())
    }

    fn rmdir(&self, path: &str) -> Result<(), FsError> {
        if self.read_only {
            return Err(FsError::ReadOnly);
        }
        let normalized = normalize_fat_path_cow(path);
        if normalized.is_empty() {
            return Err(FsError::Busy);
        }
        let (parent, child) = split_parent_child(normalized.as_ref()).ok_or(FsError::InvalidInput)?;
        let fs = self.lock_fs();
        let (dir, _) = self.open_dir_checked(&fs, parent)?;
        let entry = find_entry_case_insensitive(&dir, child, self.max_dir_entries())
            .map_err(map_fat_error)?;
        if !entry.is_dir() {
            return Err(FsError::NotADirectory);
        }
        drop(entry);
        self.bump_version()?;
        dir.remove(child).map_err(map_fat_error)?;
        Ok(())
    }

    fn rename(
        &self,
        src: &str,
        dst: &str,
        replace_existing: bool,
        no_replace: bool,
    ) -> Result<(), FsError> {
        if self.read_only {
            return Err(FsError::ReadOnly);
        }
        let src_norm = normalize_fat_path_cow(src);
        let dst_norm = normalize_fat_path_cow(dst);
        if src_norm.is_empty() || dst_norm.is_empty() {
            return Err(FsError::InvalidInput);
        }
        let (src_parent, src_name) =
            split_parent_child(src_norm.as_ref()).ok_or(FsError::InvalidInput)?;
        let (dst_parent, dst_name) =
            split_parent_child(dst_norm.as_ref()).ok_or(FsError::InvalidInput)?;
        let fs = self.lock_fs();
        let limit = self.max_dir_entries();
        let (src_dir, src_parent_canon) = self.open_dir_checked(&fs, src_parent)?;
        let src_entry = find_entry_case_insensitive(&src_dir, src_name, limit)
            .map_err(map_fat_error)?;
        let src_is_dir = src_entry.is_dir();
        let src_canon = append_canonical(&src_parent_canon, &src_entry.file_name());
        drop(src_entry);

        let (dst_dir, dst_parent_canon) = self.open_dir_checked(&fs, dst_parent)?;
        match find_entry_case_insensitive(&dst_dir, dst_name, limit) {
            Ok(dst_entry) => {
                let dst_canon = append_canonical(&dst_parent_canon, &dst_entry.file_name());
                if dst_canon == src_canon {
                    if no_replace {
                        return Err(FsError::AlreadyExists);
                    }
                    return Ok(());
                }
                if no_replace || !replace_existing {
                    return Err(FsError::AlreadyExists);
                }
                let dst_is_dir = dst_entry.is_dir();
                drop(dst_entry);
                if dst_is_dir && !src_is_dir {
                    return Err(FsError::IsADirectory);
                }
                if !dst_is_dir && src_is_dir {
                    return Err(FsError::NotADirectory);
                }
                self.bump_version()?;
                dst_dir.remove(dst_name).map_err(map_fat_error)?;
            }
            Err(fatfs::Error::NotFound) => {
                self.bump_version()?;
            }
            Err(err) => return Err(map_fat_error(err)),
        }

        src_dir
            .rename(src_name, &dst_dir, dst_name)
            .map_err(map_fat_error)?;
        Ok(())
    }

    fn set_times(
        &self,
        path: &str,
        atime: Option<FsTimestamp>,
        mtime: Option<FsTimestamp>,
        crtime: Option<FsTimestamp>,
    ) -> Result<(), FsError> {
        if self.read_only {
            return Err(FsError::ReadOnly);
        }
        let normalized = normalize_fat_path_cow(path);
        if normalized.is_empty() {
            return Err(FsError::Unsupported);
        }
        let (parent, child) = split_parent_child(normalized.as_ref()).ok_or(FsError::InvalidInput)?;
        let fs = self.lock_fs();
        let (dir, _) = self.open_dir_checked(&fs, parent)?;
        let entry = find_entry_case_insensitive(&dir, child, self.max_dir_entries())
            .map_err(map_fat_error)?;
        if entry.is_dir() {
            return Err(FsError::Unsupported);
        }
        self.bump_version()?;
        let mut file = entry.to_file();
        if let Some(ts) = atime {
            file.set_accessed(timestamp_to_date(ts));
        }
        if let Some(ts) = mtime {
            file.set_modified(timestamp_to_datetime(ts));
        }
        if let Some(ts) = crtime {
            file.set_created(timestamp_to_datetime(ts));
        }
        file.flush().map_err(map_fat_error)?;
        Ok(())
    }

    fn set_dos_attrs(&self, _path: &str, _dos_attrs: u32) -> Result<(), FsError> {
        if self.read_only {
            return Err(FsError::ReadOnly);
        }
        Err(FsError::Unsupported)
    }

    fn flush(&self) -> Result<(), FsError> {
        let _fs = self.lock_fs();
        self.device.lock().device.flush().map_err(FsError::Io)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tests::format_disk;
    use alloc::sync::Arc;
    use storage_core::file_io::{RangeError, ReadAt};
    use storage_core::MemBlockDevice;

    fn assert_send_sync<T: Send + Sync>() {}

    #[test]
    fn fat_volume_implements_send_sync_fs_volume() {
        assert_send_sync::<FatVolume<MemBlockDevice>>();
    }

    #[test]
    fn open_prepare_create_excl_trunc_and_errors() {
        let vol = FatVolume::new(format_disk(512, 4096, 0x1111_2222)).unwrap();
        let ro_opts = FsOpenOptions::default();
        assert_eq!(vol.open_prepare("missing.txt", ro_opts), Err(FsError::NotFound));
        assert_eq!(
            vol.open_prepare(
                "no_such_dir/file.txt",
                FsOpenOptions {
                    create: true,
                    write: true,
                    ..Default::default()
                }
            ),
            Err(FsError::NotFound)
        );

        let create_opts = FsOpenOptions {
            create: true,
            write: true,
            ..Default::default()
        };
        let meta = vol.open_prepare("hello.txt", create_opts).unwrap();
        assert_eq!(meta.kind, FsNodeKind::File);
        assert_eq!(meta.len, 0);
        assert_ne!(meta.node_id, 0);
        assert_ne!(meta.node_id, 1);

        // O_EXCL fails when file exists
        assert_eq!(
            vol.open_prepare(
                "hello.txt",
                FsOpenOptions {
                    create_new: true,
                    write: true,
                    ..Default::default()
                }
            ),
            Err(FsError::AlreadyExists)
        );

        // Parent is a file -> NotADirectory
        assert_eq!(
            vol.open_prepare("hello.txt/child", create_opts),
            Err(FsError::NotADirectory)
        );

        let (written, new_len) = vol.write_at("hello.txt", 0, b"abcdef").unwrap();
        assert_eq!((written, new_len), (6, 6));

        // O_TRUNC resets file length to 0
        let trunc_meta = vol
            .open_prepare(
                "hello.txt",
                FsOpenOptions {
                    truncate: true,
                    write: true,
                    ..Default::default()
                },
            )
            .unwrap();
        assert_eq!(trunc_meta.len, 0);
        assert_eq!(vol.metadata("hello.txt").unwrap().len, 0);

        // Opening directory for write fails with IsADirectory
        vol.mkdir("subdir").unwrap();
        assert_eq!(
            vol.open_prepare(
                "subdir",
                FsOpenOptions {
                    write: true,
                    ..Default::default()
                }
            ),
            Err(FsError::IsADirectory)
        );
        assert_eq!(
            vol.open_prepare("subdir", FsOpenOptions::default()).unwrap().kind,
            FsNodeKind::Directory
        );
    }

    #[test]
    fn write_at_sparse_extend_and_truncate_shrink_extend() {
        let vol = FatVolume::new(format_disk(512, 8192, 0x3333_4444)).unwrap();
        let fs: &dyn FsVolume = &vol;
        fs.open_prepare(
            "sparse.bin",
            FsOpenOptions {
                create_new: true,
                write: true,
                ..Default::default()
            },
        )
        .unwrap();

        assert_eq!(fs.write_at("sparse.bin", 0, b"AB").unwrap(), (2, 2));
        // Sparse extend: gap [2..10] must be zero-filled
        assert_eq!(fs.write_at("sparse.bin", 10, b"CD").unwrap(), (2, 12));
        let meta = fs.metadata("sparse.bin").unwrap();
        assert_eq!(meta.len, 12);
        assert!(meta.allocation_size >= 12);

        let mut buf = [0xffu8; 16];
        let n = fs.read_at("sparse.bin", 0, &mut buf).unwrap();
        assert_eq!(n, 12);
        assert_eq!(&buf[..12], b"AB\0\0\0\0\0\0\0\0CD");

        // Truncate extend to 20 -> zero-filled [12..20]
        assert_eq!(fs.truncate("sparse.bin", 20).unwrap(), 20);
        assert_eq!(fs.metadata("sparse.bin").unwrap().len, 20);
        let mut ext = [0xffu8; 20];
        assert_eq!(fs.read_at("sparse.bin", 0, &mut ext).unwrap(), 20);
        assert_eq!(&ext[..12], b"AB\0\0\0\0\0\0\0\0CD");
        assert_eq!(&ext[12..20], &[0u8; 8]);

        // Truncate shrink to 1
        assert_eq!(fs.truncate("sparse.bin", 1).unwrap(), 1);
        assert_eq!(fs.metadata("sparse.bin").unwrap().len, 1);
        let mut short = [0u8; 4];
        assert_eq!(vol.read_at("sparse.bin", 0, &mut short).unwrap(), 1);
        assert_eq!(short[0], b'A');
    }

    #[test]
    fn mkdir_unlink_rmdir_and_rename_semantics() {
        let vol = FatVolume::new(format_disk(512, 4096, 0x5555_6666)).unwrap();
        let fs: &dyn FsVolume = &vol;
        fs.mkdir("dir").unwrap();
        assert_eq!(fs.mkdir("dir"), Err(FsError::AlreadyExists));
        fs.open_prepare(
            "dir/a.txt",
            FsOpenOptions {
                create: true,
                write: true,
                ..Default::default()
            },
        )
        .unwrap();
        fs.write_at("dir/a.txt", 0, b"one").unwrap();
        fs.open_prepare(
            "dir/b.txt",
            FsOpenOptions {
                create: true,
                write: true,
                ..Default::default()
            },
        )
        .unwrap();
        fs.write_at("dir/b.txt", 0, b"two").unwrap();

        // no_replace rejects existing dst
        assert_eq!(
            fs.rename("dir/a.txt", "dir/b.txt", true, true),
            Err(FsError::AlreadyExists)
        );
        // replace_existing = false rejects existing dst
        assert_eq!(
            fs.rename("dir/a.txt", "dir/b.txt", false, false),
            Err(FsError::AlreadyExists)
        );
        // replace_existing = true replaces dst
        fs.rename("dir/a.txt", "dir/b.txt", true, false).unwrap();
        assert_eq!(fs.metadata("dir/a.txt"), Err(FsError::NotFound));
        let mut out = [0u8; 8];
        assert_eq!(fs.read_at("dir/b.txt", 0, &mut out).unwrap(), 3);
        assert_eq!(&out[..3], b"one");

        // rmdir non-empty fails; unlink on dir fails; rmdir on file fails
        assert_eq!(fs.rmdir("dir"), Err(FsError::DirectoryNotEmpty));
        assert_eq!(fs.unlink("dir"), Err(FsError::IsADirectory));
        assert_eq!(fs.rmdir("dir/b.txt"), Err(FsError::NotADirectory));

        fs.unlink("dir/b.txt").unwrap();
        fs.rmdir("dir").unwrap();
        fs.flush().unwrap();
    }

    #[test]
    fn statfs_capabilities_and_node_id_consistency() {
        let vol = FatVolume::new(format_disk(512, 4096, 0x7777_8888)).unwrap();
        let caps = vol.capabilities();
        assert!(!caps.read_only);
        assert_eq!(caps.case_policy, CasePolicy::AsciiInsensitive);
        assert_eq!(caps.max_name_len, 255);
        assert!(!vol.is_immutable());

        let st = vol.statfs().unwrap();
        assert_eq!(st.fs_type_magic, 0x4d44);
        assert_eq!(st.fs_name, "vfat");
        assert!(!st.case_sensitive);
        assert!(st.case_preserved);
        assert_eq!(st.max_name_len, 255);
        assert!(st.block_size >= 512);
        assert!(st.total_blocks > 0);
        assert!(st.free_blocks <= st.total_blocks);
        assert_eq!(st.avail_blocks, st.free_blocks);

        vol.mkdir("SubDir").unwrap();
        vol.open_prepare(
            "SubDir/Long File Name.txt",
            FsOpenOptions {
                create: true,
                write: true,
                ..Default::default()
            },
        )
        .unwrap();
        let fs: &dyn FsVolume = &vol;
        let meta_lfn = fs.metadata("SubDir/Long File Name.txt").unwrap();
        let meta_alias = fs.metadata("subdir/LONGFI~1.TXT").unwrap();
        assert_eq!(meta_lfn.node_id, meta_alias.node_id);
        let entries = fs.read_dir("subdir").unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].node_id, meta_lfn.node_id);
        assert_eq!(entries[0].dos_attrs, meta_lfn.dos_attrs);
        assert_eq!(
            vol.canonical_key("subdir/long file name.txt").unwrap(),
            "SubDir/Long File Name.txt"
        );
    }

    #[test]
    fn read_only_mode_enforces_erofs_on_all_mutations() {
        let mut vol = FatVolume::new(format_disk(512, 4096, 0x9999_aaaa)).unwrap();
        vol.mkdir("d").unwrap();
        vol.open_prepare(
            "f.txt",
            FsOpenOptions {
                create: true,
                write: true,
                ..Default::default()
            },
        )
        .unwrap();
        vol.write_at("f.txt", 0, b"data").unwrap();

        vol.set_read_only(true);
        assert!(vol.is_read_only());
        let fs: &dyn FsVolume = &vol;
        assert!(fs.is_immutable());
        assert!(fs.capabilities().read_only);

        assert_eq!(
            fs.open_prepare(
                "new.txt",
                FsOpenOptions {
                    create: true,
                    ..Default::default()
                }
            ),
            Err(FsError::ReadOnly)
        );
        assert_eq!(
            fs.open_prepare(
                "f.txt",
                FsOpenOptions {
                    write: true,
                    ..Default::default()
                }
            ),
            Err(FsError::ReadOnly)
        );
        assert_eq!(
            fs.open_prepare(
                "f.txt",
                FsOpenOptions {
                    truncate: true,
                    ..Default::default()
                }
            ),
            Err(FsError::ReadOnly)
        );
        assert_eq!(fs.write_at("f.txt", 0, b"x"), Err(FsError::ReadOnly));
        assert_eq!(fs.truncate("f.txt", 0), Err(FsError::ReadOnly));
        assert_eq!(fs.mkdir("d2"), Err(FsError::ReadOnly));
        assert_eq!(fs.unlink("f.txt"), Err(FsError::ReadOnly));
        assert_eq!(fs.rmdir("d"), Err(FsError::ReadOnly));
        assert_eq!(fs.rename("f.txt", "g.txt", false, false), Err(FsError::ReadOnly));
        assert_eq!(
            fs.set_times("f.txt", None, Some(FsTimestamp::default()), None),
            Err(FsError::ReadOnly)
        );
        assert_eq!(fs.set_dos_attrs("f.txt", 0), Err(FsError::ReadOnly));

        // Read operations still work
        let mut buf = [0u8; 4];
        assert_eq!(fs.read_at("f.txt", 0, &mut buf).unwrap(), 4);
        assert_eq!(&buf, b"data");
        assert!(fs.metadata("f.txt").is_ok());
        assert!(fs.read_dir("").is_ok());
        assert!(fs.statfs().is_ok());
        assert!(fs.flush().is_ok());
    }

    #[test]
    fn fs_volume_mutations_revoke_detached_read_sources() {
        let vol = FatVolume::new(format_disk(512, 8192, 0xbbbb_cccc)).unwrap();
        vol.open_prepare(
            "a.txt",
            FsOpenOptions {
                create: true,
                write: true,
                ..Default::default()
            },
        )
        .unwrap();
        vol.write_at("a.txt", 0, b"hello").unwrap();

        let assert_revoked = |op: &dyn Fn(&FatVolume<MemBlockDevice>)| {
            let map = Arc::new(vol.plan_read("a.txt", 4096).unwrap());
            let mut source = vol.read_source(map).unwrap();
            let mut buf = [0u8; 5];
            assert_eq!(source.read_at(0, &mut buf).unwrap(), 5);
            op(&vol);
            assert_eq!(source.read_at(0, &mut buf), Err(RangeError::Stale));
        };

        assert_revoked(&|v| {
            v.write_at("a.txt", 0, b"world").unwrap();
        });
        assert_revoked(&|v| {
            v.truncate("a.txt", 10).unwrap();
        });
        assert_revoked(&|v| {
            v.mkdir("sub").unwrap();
        });
        assert_revoked(&|v| {
            v.open_prepare(
                "b.txt",
                FsOpenOptions {
                    create: true,
                    write: true,
                    ..Default::default()
                },
            )
            .unwrap();
        });
        assert_revoked(&|v| {
            FsVolume::rename(v, "b.txt", "c.txt", false, false).unwrap();
        });
        assert_revoked(&|v| {
            v.unlink("c.txt").unwrap();
        });
        assert_revoked(&|v| {
            v.rmdir("sub").unwrap();
        });
    }
}
