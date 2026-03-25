use alloc::string::String;

use crate::storage::fat::{self, BootVolumeNodeKind};
use crate::user::handles::{VfsDirectoryHandle, VfsFileHandle};

use super::{VfsBackend, VfsContext, VfsError, VfsMetadata, VfsNodeKind, VfsOpenResult};

pub(crate) static BOOTFS: BootFsBackend = BootFsBackend;

pub(crate) struct BootFsBackend;

impl VfsBackend for BootFsBackend {
    fn open(
        &self,
        absolute_path: &str,
        relative_path: &str,
        flags: u64,
        _mode: u64,
        context: &mut VfsContext<'_>,
    ) -> Result<VfsOpenResult, VfsError> {
        super::validate_read_only_open_flags(flags)?;

        let normalized = normalize_boot_volume_path(relative_path)?;
        ensure_boot_volume_access(context, normalized.as_str())?;

        let metadata = query_boot_volume_file_metadata(absolute_path, normalized.as_str())?;
        match metadata.kind {
            VfsNodeKind::File => {
                if flags & crate::user::linux::O_DIRECTORY != 0 {
                    return Err(VfsError::NotDirectory);
                }
                let bytes = fat::read_file_to_vec(normalized.as_str()).map_err(map_fat_error)?;
                Ok(VfsOpenResult::File(VfsFileHandle::read_only_memory(
                    String::from(absolute_path),
                    bytes,
                )))
            }
            VfsNodeKind::Directory => Ok(VfsOpenResult::Directory(VfsDirectoryHandle::new(
                String::from(absolute_path),
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
        let normalized = normalize_boot_volume_path(relative_path)?;
        ensure_boot_volume_access(context, normalized.as_str())?;
        query_boot_volume_file_metadata(absolute_path, normalized.as_str())
    }

    fn check_access(
        &self,
        _absolute_path: &str,
        relative_path: &str,
        mode: u64,
        context: &mut VfsContext<'_>,
    ) -> Result<(), VfsError> {
        super::ensure_read_access_only(mode)?;

        let normalized = normalize_boot_volume_path(relative_path)?;
        ensure_boot_volume_access(context, normalized.as_str())?;
        let _ = query_boot_volume_file_metadata(relative_path, normalized.as_str())?;
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
}

fn normalize_boot_volume_path(path: &str) -> Result<String, VfsError> {
    if !path.starts_with('/') {
        return Err(VfsError::InvalidArgument);
    }
    Ok(String::from(path.trim_start_matches('/')))
}

fn ensure_boot_volume_access(context: &mut VfsContext<'_>, path: &str) -> Result<(), VfsError> {
    if is_runtime_library_path(path) {
        return Ok(());
    }

    if context
        .process_state_mut()
        .require_logical_admin_for_file_access(path)
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
