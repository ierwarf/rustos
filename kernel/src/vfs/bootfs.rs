use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;

use fatfs::{Read, Seek, SeekFrom};
use spin::Mutex;

use crate::storage::block;
use crate::storage::fat::{self, BootVolumeNodeKind};
use crate::user::handles::{
    FileHandleWriteError, VfsDirectoryEntry, VfsDirectoryHandle, VfsFileHandle, VfsFileObject,
};

use super::{
    FilesystemProvider, MountError, MountSource, VfsBackend, VfsContext, VfsError, VfsMetadata,
    VfsNodeKind, VfsOpenResult,
};

pub(crate) static FAT_PROVIDER: FatFilesystemProvider = FatFilesystemProvider;
// The boot volume currently sits on drivers that only guarantee small, block-sized
// transfer windows in early/no-opt boot. Keep lazy reads aligned with the stable
// 4 KiB chunk size used by the eager FAT helpers instead of issuing huge reads.
const FAT_READ_CHUNK_CAP: usize = 4 * 1024;
// Keep common ELF binaries and shared libraries resident once opened so
// file-backed mmap does not repeatedly reopen and seek the FAT volume.
const FAT_WHOLE_FILE_CACHE_LIMIT: usize = 8 * 1024 * 1024;

pub(crate) struct FatFilesystemProvider;

pub(crate) struct FatFsBackend {
    handle: block::BlockDeviceHandle,
    _readonly: bool,
}

#[derive(Debug)]
enum FatFileSource {
    Registry(block::BlockDeviceHandle),
}

#[derive(Debug)]
struct ReadOnlyFatFile {
    path: String,
    fat_path: String,
    len: usize,
    source: FatFileSource,
    cached_bytes: Mutex<Option<Vec<u8>>>,
}

impl FilesystemProvider for FatFilesystemProvider {
    fn name(&self) -> &'static str {
        "fat"
    }

    fn mount(
        &self,
        source: MountSource,
        flags: u64,
        options: Option<&str>,
    ) -> Result<Arc<dyn VfsBackend>, MountError> {
        if options.is_some_and(|value| !value.trim().is_empty()) {
            return Err(MountError::InvalidArgument);
        }

        let handle = match source {
            MountSource::BlockDevice(handle) => handle,
            MountSource::None => return Err(MountError::InvalidSource),
        };
        let readonly = flags & crate::user::linux::MS_RDONLY != 0 || block::is_readonly(handle);
        Ok(Arc::new(FatFsBackend {
            handle,
            _readonly: readonly,
        }))
    }
}

impl VfsBackend for FatFsBackend {
    fn open(
        &self,
        absolute_path: &str,
        relative_path: &str,
        flags: u64,
        _mode: u64,
        context: &mut VfsContext<'_>,
    ) -> Result<VfsOpenResult, VfsError> {
        super::validate_read_only_open_flags(flags)?;

        let normalized = normalize_fat_path(relative_path)?;
        let metadata = self.metadata(absolute_path, relative_path, context)?;
        match metadata.kind {
            VfsNodeKind::File => {
                if flags & crate::user::linux::O_DIRECTORY != 0 {
                    return Err(VfsError::NotDirectory);
                }
                let len = usize::try_from(metadata.len).map_err(|_| VfsError::Unsupported)?;
                Ok(VfsOpenResult::File(VfsFileHandle::new(Arc::new(
                    ReadOnlyFatFile {
                        path: String::from(absolute_path),
                        fat_path: normalized,
                        len,
                        source: FatFileSource::Registry(self.handle),
                        cached_bytes: Mutex::new(None),
                    },
                ))))
            }
            VfsNodeKind::Directory => Ok(VfsOpenResult::Directory(VfsDirectoryHandle::new(
                String::from(absolute_path),
                self.read_dir(absolute_path, relative_path, context)?,
            ))),
            VfsNodeKind::Device => Err(VfsError::Unsupported),
        }
    }

    fn metadata(
        &self,
        absolute_path: &str,
        relative_path: &str,
        _context: &mut VfsContext<'_>,
    ) -> Result<VfsMetadata, VfsError> {
        let normalized = normalize_fat_path(relative_path)?;
        self.query_metadata(absolute_path, normalized.as_str())
    }

    fn check_access(
        &self,
        _absolute_path: &str,
        relative_path: &str,
        mode: u64,
        _context: &mut VfsContext<'_>,
    ) -> Result<(), VfsError> {
        super::ensure_read_access_only(mode)?;

        let normalized = normalize_fat_path(relative_path)?;
        let _ = self.query_metadata(relative_path, normalized.as_str())?;
        Ok(())
    }

    fn readlink(
        &self,
        _absolute_path: &str,
        _relative_path: &str,
        _context: &mut VfsContext<'_>,
    ) -> Result<String, VfsError> {
        Err(VfsError::NotFound)
    }

    fn read_dir(
        &self,
        absolute_path: &str,
        relative_path: &str,
        _context: &mut VfsContext<'_>,
    ) -> Result<Vec<VfsDirectoryEntry>, VfsError> {
        let normalized = normalize_fat_path(relative_path)?;
        let mut entries = self
            .read_dir_entries(normalized.as_str())?
            .into_iter()
            .map(|entry| {
                let child_path = if absolute_path == "/" {
                    alloc::format!("/{}", entry.name)
                } else {
                    alloc::format!("{absolute_path}/{}", entry.name)
                };
                super::directory_entry(
                    child_path.as_str(),
                    match entry.kind {
                        BootVolumeNodeKind::File => VfsNodeKind::File,
                        BootVolumeNodeKind::Directory => VfsNodeKind::Directory,
                    },
                )
            })
            .collect::<Vec<_>>();
        super::append_mount_entries(&mut entries, absolute_path);
        Ok(entries)
    }
}

impl FatFsBackend {
    fn query_metadata(
        &self,
        absolute_path: &str,
        normalized_path: &str,
    ) -> Result<VfsMetadata, VfsError> {
        let metadata = open_registry_metadata(self.handle, normalized_path)?;
        let metadata = super::default_metadata(
            absolute_path,
            match metadata.kind {
                BootVolumeNodeKind::File => VfsNodeKind::File,
                BootVolumeNodeKind::Directory => VfsNodeKind::Directory,
            },
            metadata.len,
        );
        Ok(metadata)
    }

    fn read_dir_entries(
        &self,
        normalized_path: &str,
    ) -> Result<Vec<fat::BootVolumeDirEntry>, VfsError> {
        open_registry_dir(self.handle, normalized_path)
    }
}

impl VfsFileObject for ReadOnlyFatFile {
    fn path(&self) -> &str {
        self.path.as_str()
    }

    fn len(&self) -> usize {
        self.len
    }

    fn read_at(&self, offset: usize, dest: &mut [u8]) -> usize {
        if dest.is_empty() || offset >= self.len {
            return 0;
        }

        if let Some(bytes) = self.cached_bytes.lock().as_ref() {
            let read_len = dest.len().min(bytes.len().saturating_sub(offset));
            dest[..read_len].copy_from_slice(&bytes[offset..offset + read_len]);
            return read_len;
        }

        if self.len <= FAT_WHOLE_FILE_CACHE_LIMIT {
            let loaded = match self.source {
                FatFileSource::Registry(handle) => {
                    read_registry_file_to_vec(handle, self.fat_path.as_str())
                }
            };
            match loaded {
                Ok(bytes) => {
                    let read_len = dest.len().min(bytes.len().saturating_sub(offset));
                    dest[..read_len].copy_from_slice(&bytes[offset..offset + read_len]);
                    *self.cached_bytes.lock() = Some(bytes);
                    return read_len;
                }
                Err(err) => {
                    crate::debug::println!(
                        "bootfs: cached file read failed, falling back to ranged reads path={} offset={} len={} err={:?}",
                        self.path,
                        offset,
                        dest.len().min(self.len - offset),
                        err
                    );
                }
            }
        }

        let read_len = dest.len().min(self.len - offset);
        let dest = &mut dest[..read_len];
        let result = match self.source {
            FatFileSource::Registry(handle) => {
                read_registry_file_range(handle, self.fat_path.as_str(), offset, dest)
            }
        };

        match result {
            Ok(count) => count.min(read_len),
            Err(range_err) => {
                crate::debug::println!(
                    "bootfs: ranged read failed path={} offset={} len={} err={:?}",
                    self.path,
                    offset,
                    read_len,
                    range_err
                );
                0
            }
        }
    }

    fn write_at(&self, _offset: usize, _src: &[u8]) -> Result<usize, FileHandleWriteError> {
        Err(FileHandleWriteError::ReadOnly)
    }
}

fn normalize_fat_path(path: &str) -> Result<String, VfsError> {
    let normalized = super::normalize_kernel_path(path)?;
    Ok(String::from(normalized.trim_start_matches('/')))
}

fn open_registry_metadata(
    handle: block::BlockDeviceHandle,
    path: &str,
) -> Result<fat::BootVolumeMetadata, VfsError> {
    with_registry_volume(handle, |volume| {
        volume.metadata(path).map_err(map_fat_error)
    })
}

fn open_registry_dir(
    handle: block::BlockDeviceHandle,
    path: &str,
) -> Result<Vec<fat::BootVolumeDirEntry>, VfsError> {
    with_registry_volume(handle, |volume| {
        volume.read_dir(path).map_err(map_fat_error)
    })
}

fn open_registry_volume(
    handle: block::BlockDeviceHandle,
) -> Result<fat::MountedFatVolume<block::FatRegistryDevice>, VfsError> {
    fat::open_volume(block::FatRegistryDevice::new(handle)).map_err(map_fat_error)
}

fn with_registry_volume<T>(
    handle: block::BlockDeviceHandle,
    f: impl FnOnce(&fat::MountedFatVolume<block::FatRegistryDevice>) -> Result<T, VfsError>,
) -> Result<T, VfsError> {
    let volume = open_registry_volume(handle)?;
    // This backend is read-only. Calling fatfs::FileSystem::unmount() forces a flush path that
    // mutably borrows the filesystem disk object, which can panic on concurrent bootfs readers.
    // Dropping the temporary volume is sufficient here because there is no dirty state to commit.
    f(&volume)
}

fn read_registry_file_range(
    handle: block::BlockDeviceHandle,
    path: &str,
    offset: usize,
    dest: &mut [u8],
) -> Result<usize, VfsError> {
    with_registry_volume(handle, |volume| {
        read_range_from_registry_volume(volume, path, offset, dest)
    })
}

fn read_registry_file_to_vec(
    handle: block::BlockDeviceHandle,
    path: &str,
) -> Result<Vec<u8>, VfsError> {
    with_registry_volume(handle, |volume| {
        volume.read_file_to_vec(path).map_err(map_fat_error)
    })
}

fn read_registry_file_into(
    handle: block::BlockDeviceHandle,
    path: &str,
    dest: &mut [u8],
) -> Result<usize, VfsError> {
    with_registry_volume(handle, |volume| {
        volume.read_file_into(path, dest).map_err(map_fat_error)
    })
}

fn read_range_from_registry_volume(
    volume: &fat::MountedFatVolume<block::FatRegistryDevice>,
    path: &str,
    offset: usize,
    dest: &mut [u8],
) -> Result<usize, VfsError> {
    let mut file = volume.open_file(path).map_err(map_fat_error)?;
    read_range_from_file(&mut file, offset, dest, false).map_err(map_fat_error)
}

fn read_range_from_file<R>(
    file: &mut R,
    offset: usize,
    dest: &mut [u8],
    trace: bool,
) -> Result<usize, fatfs::Error<fat::DiskIoError>>
where
    R: Read<Error = fatfs::Error<fat::DiskIoError>> + Seek<Error = fatfs::Error<fat::DiskIoError>>,
{
    if trace {
        crate::debug::println!("bootfs: ranged read seek begin offset={}", offset);
    }
    file.seek(SeekFrom::Start(offset as u64))?;
    if trace {
        crate::debug::println!("bootfs: ranged read seek done offset={}", offset);
    }
    let mut done = 0usize;
    while done < dest.len() {
        let chunk_len = (dest.len() - done).min(FAT_READ_CHUNK_CAP);
        if trace && done == 0 {
            crate::debug::println!(
                "bootfs: ranged read file.read begin offset={} chunk_len={}",
                offset,
                chunk_len
            );
        }
        let count = file.read(&mut dest[done..done + chunk_len])?;
        if trace && done == 0 {
            crate::debug::println!(
                "bootfs: ranged read file.read done offset={} count={}",
                offset,
                count
            );
        }
        if count == 0 {
            break;
        }
        done += count;
    }
    Ok(done)
}

fn map_fat_error(err: fatfs::Error<fat::DiskIoError>) -> VfsError {
    match err {
        fatfs::Error::NotFound => VfsError::NotFound,
        fatfs::Error::InvalidInput => VfsError::InvalidArgument,
        fatfs::Error::Io(fat::DiskIoError::InvalidInput) => VfsError::InvalidArgument,
        _ => VfsError::Unsupported,
    }
}

#[cfg(test)]
mod tests {
    use super::normalize_fat_path;

    #[test]
    fn fat_paths_are_normalized_relative_to_mount_root() {
        assert_eq!(
            normalize_fat_path("/system/bin/test").unwrap(),
            "system/bin/test"
        );
        assert_eq!(
            normalize_fat_path("/lib64//./x86_64-linux-gnu/../ld-linux-x86-64.so.2").unwrap(),
            "lib64/ld-linux-x86-64.so.2"
        );
        assert_eq!(normalize_fat_path("/").unwrap(), "");
        assert!(normalize_fat_path("/lib64/\0ld-linux-x86-64.so.2").is_err());
    }
}
