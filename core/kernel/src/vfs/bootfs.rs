use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;

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
enum FatAccessPolicy {
    Standard,
    Boot,
}

pub(crate) struct FatFsBackend {
    handle: block::BlockDeviceHandle,
    _readonly: bool,
    access_policy: FatAccessPolicy,
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
        let handle = match source {
            MountSource::BlockDevice(handle) => handle,
            MountSource::None => return Err(MountError::InvalidSource),
        };
        let readonly = flags & crate::user::linux::MS_RDONLY != 0 || block::is_readonly(handle);
        let access_policy = parse_mount_access_policy(options)?;
        Ok(Arc::new(FatFsBackend {
            handle,
            _readonly: readonly,
            access_policy,
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
        ensure_mount_access(context, normalized.as_str(), self.access_policy)?;

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
        ensure_mount_access(context, normalized.as_str(), self.access_policy)?;
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
        ensure_mount_access(context, normalized.as_str(), self.access_policy)?;
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
        ensure_mount_access(context, normalized.as_str(), self.access_policy)?;
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
        let descriptor = block::descriptor(self.handle);
        if let Some(_descriptor) = descriptor.as_ref() {
            crate::debug::println!(
                "bootfs read: handle={} dev={} path={} block_size={} start_block={} blocks={}",
                self.handle.id(),
                _descriptor.path,
                path,
                _descriptor.logical_block_size,
                _descriptor.start_block,
                _descriptor.block_count
            );
        }
        with_registry_volume(self.handle, |volume| {
            volume.read_file_to_vec(path).map_err(map_fat_error)
        })
    }

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
        crate::debug::println!(
            "bootfs metadata: handle={} absolute={} kind={:?} len={}",
            self.handle.id(),
            absolute_path,
            metadata.kind,
            metadata.len
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

fn parse_mount_access_policy(options: Option<&str>) -> Result<FatAccessPolicy, MountError> {
    let mut access_policy = FatAccessPolicy::Standard;
    let Some(options) = options else {
        return Ok(access_policy);
    };

    for option in options.split(',') {
        let option = option.trim();
        if option.is_empty() {
            continue;
        }
        match option {
            "access=standard" => access_policy = FatAccessPolicy::Standard,
            "access=boot" => access_policy = FatAccessPolicy::Boot,
            _ => return Err(MountError::InvalidArgument),
        }
    }

    Ok(access_policy)
}

fn normalize_fat_path(path: &str) -> Result<String, VfsError> {
    if !path.starts_with('/') {
        return Err(VfsError::InvalidArgument);
    }
    Ok(String::from(path.trim_start_matches('/')))
}

fn ensure_mount_access(
    context: &mut VfsContext<'_>,
    path: &str,
    access_policy: FatAccessPolicy,
) -> Result<(), VfsError> {
    if !matches!(access_policy, FatAccessPolicy::Boot) {
        return Ok(());
    }

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

fn open_registry_metadata(
    handle: block::BlockDeviceHandle,
    path: &str,
) -> Result<fat::BootVolumeMetadata, VfsError> {
    with_registry_volume(handle, |volume| volume.metadata(path).map_err(map_fat_error))
}

fn open_registry_dir(
    handle: block::BlockDeviceHandle,
    path: &str,
) -> Result<Vec<fat::BootVolumeDirEntry>, VfsError> {
    with_registry_volume(handle, |volume| volume.read_dir(path).map_err(map_fat_error))
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
    let result = f(&volume);
    match (result, volume.unmount().map_err(map_fat_error)) {
        (Ok(value), Ok(())) => Ok(value),
        (Ok(_), Err(err)) => Err(err),
        (Err(err), _) => Err(err),
    }
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

#[cfg(test)]
mod tests {
    use super::{parse_mount_access_policy, FatAccessPolicy};
    use crate::vfs::MountError;

    #[test]
    fn parse_mount_access_policy_defaults_to_standard() {
        assert!(matches!(
            parse_mount_access_policy(None).unwrap(),
            FatAccessPolicy::Standard
        ));
        assert!(matches!(
            parse_mount_access_policy(Some("")).unwrap(),
            FatAccessPolicy::Standard
        ));
    }

    #[test]
    fn parse_mount_access_policy_accepts_boot_and_standard() {
        assert!(matches!(
            parse_mount_access_policy(Some("access=boot")).unwrap(),
            FatAccessPolicy::Boot
        ));
        assert!(matches!(
            parse_mount_access_policy(Some("access=boot,access=standard")).unwrap(),
            FatAccessPolicy::Standard
        ));
    }

    #[test]
    fn parse_mount_access_policy_rejects_unknown_options() {
        assert!(matches!(
            parse_mount_access_policy(Some("rw")),
            Err(MountError::InvalidArgument)
        ));
    }
}
