use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec;
use alloc::vec::Vec;
use fatfs::{Read, Seek};

use crate::storage::block;
use crate::storage::fat::{self, BootVolumeNodeKind};
use crate::user::handles::{VfsDirectoryEntry, VfsDirectoryHandle, VfsFileHandle};

use super::{
    FilesystemProvider, MountError, MountSource, VfsBackend, VfsContext, VfsError, VfsMetadata,
    VfsNodeKind, VfsOpenResult,
};

pub(crate) static FAT_PROVIDER: FatFilesystemProvider = FatFilesystemProvider;

pub(crate) struct FatFilesystemProvider;

#[derive(Clone, Copy)]
enum FatMountSource {
    BootVolume,
    BlockDevice(block::BlockDeviceHandle),
}

pub(crate) struct FatFsBackend {
    source: FatMountSource,
    _readonly: bool,
}

impl FilesystemProvider for FatFilesystemProvider {
    fn name(&self) -> &'static str {
        "fat"
    }

    fn mount(
        &self,
        source: MountSource,
        flags: u64,
        _options: Option<&str>,
    ) -> Result<Arc<dyn VfsBackend>, MountError> {
        let source = match source {
            MountSource::BootVolume => FatMountSource::BootVolume,
            MountSource::BlockDevice(handle) => FatMountSource::BlockDevice(handle),
            MountSource::None => return Err(MountError::InvalidSource),
        };
        let readonly = match source {
            FatMountSource::BootVolume => true,
            FatMountSource::BlockDevice(handle) => {
                flags & crate::user::linux::MS_RDONLY != 0 || block::is_readonly(handle)
            }
        };
        Ok(Arc::new(FatFsBackend {
            source,
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
        ensure_boot_volume_access(context, normalized.as_str())?;

        let metadata = self.metadata(absolute_path, relative_path, context)?;
        match metadata.kind {
            VfsNodeKind::File => {
                if flags & crate::user::linux::O_DIRECTORY != 0 {
                    return Err(VfsError::NotDirectory);
                }
                let bytes = self.read_file_to_vec(normalized.as_str())?;
                Ok(VfsOpenResult::File(VfsFileHandle::read_only_memory(
                    String::from(absolute_path),
                    bytes,
                )))
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
        context: &mut VfsContext<'_>,
    ) -> Result<VfsMetadata, VfsError> {
        let normalized = normalize_fat_path(relative_path)?;
        ensure_boot_volume_access(context, normalized.as_str())?;
        self.query_metadata(absolute_path, normalized.as_str())
    }

    fn check_access(
        &self,
        _absolute_path: &str,
        relative_path: &str,
        mode: u64,
        context: &mut VfsContext<'_>,
    ) -> Result<(), VfsError> {
        super::ensure_read_access_only(mode)?;

        let normalized = normalize_fat_path(relative_path)?;
        ensure_boot_volume_access(context, normalized.as_str())?;
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
        context: &mut VfsContext<'_>,
    ) -> Result<Vec<VfsDirectoryEntry>, VfsError> {
        let normalized = normalize_fat_path(relative_path)?;
        ensure_boot_volume_access(context, normalized.as_str())?;
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
    fn read_file_to_vec(&self, path: &str) -> Result<Vec<u8>, VfsError> {
        crate::debug::println!(
            "bootfs read begin: source={} path={}",
            self.source_name(),
            path
        );
        match self.source {
            FatMountSource::BootVolume => {
                let bytes = fat::read_file_to_vec(path).map_err(map_fat_error)?;
                crate::debug::println!(
                    "bootfs read done: source=boot path={} bytes={}",
                    path,
                    bytes.len()
                );
                Ok(bytes)
            }
            FatMountSource::BlockDevice(handle) => {
                let descriptor = block::descriptor(handle);
                if let Some(descriptor) = descriptor.as_ref() {
                    crate::debug::println!(
                        "bootfs registry read: handle={} dev={} start_lba={} sectors={}",
                        handle.id(),
                        descriptor.path,
                        descriptor.start_lba,
                        descriptor.sector_count
                    );
                }
                let file_metadata = open_registry_metadata(handle, path)?;
                let fs = open_registry_filesystem(handle)?;
                let root = fs.root_dir();
                let mut file = root.open_file(path).map_err(map_fat_error)?;
                let file_len =
                    usize::try_from(file_metadata.len).map_err(|_| VfsError::InvalidArgument)?;
                let mut bytes = vec![0_u8; file_len];
                let read = file.read(&mut bytes).map_err(map_fat_error)?;
                bytes.truncate(read);
                crate::debug::println!(
                    "bootfs read done: source=block path={} bytes={}",
                    path,
                    bytes.len()
                );
                Ok(bytes)
            }
        }
    }

    fn query_metadata(
        &self,
        absolute_path: &str,
        normalized_path: &str,
    ) -> Result<VfsMetadata, VfsError> {
        crate::debug::println!(
            "bootfs metadata begin: source={} absolute={} path={}",
            self.source_name(),
            absolute_path,
            normalized_path
        );
        match self.source {
            FatMountSource::BootVolume => {
                let metadata = query_boot_volume_file_metadata(absolute_path, normalized_path)?;
                crate::debug::println!(
                    "bootfs metadata done: source=boot absolute={} kind={:?} len={}",
                    absolute_path,
                    metadata.kind,
                    metadata.len
                );
                Ok(metadata)
            }
            FatMountSource::BlockDevice(handle) => {
                let metadata = open_registry_metadata(handle, normalized_path)?;
                let metadata = super::default_metadata(
                    absolute_path,
                    match metadata.kind {
                        BootVolumeNodeKind::File => VfsNodeKind::File,
                        BootVolumeNodeKind::Directory => VfsNodeKind::Directory,
                    },
                    metadata.len,
                );
                crate::debug::println!(
                    "bootfs metadata done: source=block absolute={} kind={:?} len={}",
                    absolute_path,
                    metadata.kind,
                    metadata.len
                );
                Ok(metadata)
            }
        }
    }

    fn read_dir_entries(
        &self,
        normalized_path: &str,
    ) -> Result<Vec<fat::BootVolumeDirEntry>, VfsError> {
        match self.source {
            FatMountSource::BootVolume => {
                let volume = fat::BootVolume::open().map_err(map_fat_error)?;
                let result = volume.read_dir(normalized_path).map_err(map_fat_error);
                match (result, volume.close()) {
                    (Ok(value), Ok(())) => Ok(value),
                    (Ok(_), Err(err)) => Err(map_fat_error(err)),
                    (Err(err), _) => Err(err),
                }
            }
            FatMountSource::BlockDevice(handle) => open_registry_dir(handle, normalized_path),
        }
    }

    fn source_name(&self) -> &'static str {
        match self.source {
            FatMountSource::BootVolume => "boot",
            FatMountSource::BlockDevice(_) => "block",
        }
    }
}

fn normalize_fat_path(path: &str) -> Result<String, VfsError> {
    if !path.starts_with('/') {
        return Err(VfsError::InvalidArgument);
    }
    Ok(String::from(path.trim_start_matches('/')))
}

fn ensure_boot_volume_access(context: &mut VfsContext<'_>, path: &str) -> Result<(), VfsError> {
    if context.is_kernel() || is_runtime_library_path(path) {
        return Ok(());
    }

    if context
        .process_state_mut()
        .map(|state| state.require_logical_admin_for_file_access(path))
        .unwrap_or(false)
    {
        Ok(())
    } else {
        Err(VfsError::PermissionDenied)
    }
}

fn query_boot_volume_file_metadata(
    absolute_path: &str,
    normalized_path: &str,
) -> Result<VfsMetadata, VfsError> {
    let volume = fat::BootVolume::open().map_err(map_fat_error)?;
    let result = {
        let metadata = volume.metadata(normalized_path).map_err(map_fat_error)?;
        Ok(super::default_metadata(
            absolute_path,
            match metadata.kind {
                BootVolumeNodeKind::File => VfsNodeKind::File,
                BootVolumeNodeKind::Directory => VfsNodeKind::Directory,
            },
            metadata.len,
        ))
    };

    match (result, volume.close()) {
        (Ok(value), Ok(())) => Ok(value),
        (Ok(_), Err(err)) => Err(map_fat_error(err)),
        (Err(err), _) => Err(err),
    }
}

fn open_registry_metadata(
    handle: block::BlockDeviceHandle,
    path: &str,
) -> Result<fat::BootVolumeMetadata, VfsError> {
    let fs = open_registry_filesystem(handle)?;
    let root = fs.root_dir();
    if path.is_empty() {
        return Ok(fat::BootVolumeMetadata {
            kind: BootVolumeNodeKind::Directory,
            len: 0,
        });
    }

    if let Ok(dir) = root.open_dir(path) {
        let _ = dir;
        return Ok(fat::BootVolumeMetadata {
            kind: BootVolumeNodeKind::Directory,
            len: 0,
        });
    }

    let mut file = root.open_file(path).map_err(map_fat_error)?;
    let len = file.seek(fatfs::SeekFrom::End(0)).map_err(map_fat_error)?;
    Ok(fat::BootVolumeMetadata {
        kind: BootVolumeNodeKind::File,
        len,
    })
}

fn open_registry_dir(
    handle: block::BlockDeviceHandle,
    path: &str,
) -> Result<Vec<fat::BootVolumeDirEntry>, VfsError> {
    let fs = open_registry_filesystem(handle)?;
    let root = fs.root_dir();
    let dir = if path.is_empty() {
        root
    } else {
        root.open_dir(path).map_err(map_fat_error)?
    };

    let mut entries = Vec::new();
    for entry_result in dir.iter() {
        let entry = entry_result.map_err(map_fat_error)?;
        let name = entry.file_name();
        if name == "." || name == ".." {
            continue;
        }
        entries.push(fat::BootVolumeDirEntry {
            name,
            kind: if entry.is_dir() {
                BootVolumeNodeKind::Directory
            } else {
                BootVolumeNodeKind::File
            },
        });
    }
    Ok(entries)
}

fn open_registry_filesystem(
    handle: block::BlockDeviceHandle,
) -> Result<
    fatfs::FileSystem<
        fat::FatDisk<block::FatRegistryDevice>,
        fatfs::DefaultTimeProvider,
        fatfs::LossyOemCpConverter,
    >,
    VfsError,
> {
    fatfs::FileSystem::new(
        fat::FatDisk::new(block::FatRegistryDevice::new(handle)),
        fatfs::FsOptions::new(),
    )
    .map_err(map_fat_error)
}

fn is_runtime_library_path(path: &str) -> bool {
    path.is_empty()
        || path == "lib"
        || path.starts_with("lib/")
        || path == "lib64"
        || path.starts_with("lib64/")
        || path == "usr"
        || path == "usr/lib"
        || path.starts_with("usr/lib/")
        || path == "etc"
        || path == "etc/ld.so.cache"
        || path == "etc/ld.so.preload"
        || path == "etc/ld.so.conf"
        || path.starts_with("etc/ld.so.conf.d/")
}

fn map_fat_error(err: fatfs::Error<fat::DiskIoError>) -> VfsError {
    match err {
        fatfs::Error::NotFound => VfsError::NotFound,
        fatfs::Error::InvalidInput => VfsError::InvalidArgument,
        fatfs::Error::Io(fat::DiskIoError::InvalidInput) => VfsError::InvalidArgument,
        _ => VfsError::Unsupported,
    }
}
