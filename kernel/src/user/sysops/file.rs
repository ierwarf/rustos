use alloc::vec::Vec;
use core::cmp::min;
use core::convert::TryFrom;

use fatfs::{Seek, SeekFrom};
use x86_64::VirtAddr;

use crate::fat;
use crate::multitask;
use crate::paging;
use crate::user::handles::{
    BootFileHandle, BootFileSeekError, BootFileSeekWhence, BootFileWriteError, KernelHandle,
};

const FILE_IO_CHUNK_LEN: usize = 512;
const MAX_OPEN_PATH_LEN: usize = 256;
const READ_ONLY_OPEN_FLAGS: u64 = crate::user::linux::O_RDONLY | crate::user::linux::O_CLOEXEC;

#[derive(Debug, Clone, Copy)]
pub(crate) enum FileSysopError {
    AddressSpace(paging::AddressSpaceError),
    BadFileDescriptor,
    InvalidArgument,
    NotFound,
    PermissionDenied,
    ReadOnlyFilesystem,
    Unsupported,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct FileMetadata {
    pub len: u64,
}

impl From<paging::AddressSpaceError> for FileSysopError {
    fn from(value: paging::AddressSpaceError) -> Self {
        Self::AddressSpace(value)
    }
}

pub(crate) fn open_path_for_current_process(
    path: &str,
    flags: u64,
    _mode: u64,
) -> Result<u64, FileSysopError> {
    validate_open_flags(flags)?;
    let normalized = normalize_boot_volume_path(path)?;
    if !is_runtime_library_path(normalized) {
        ensure_current_process_file_access(normalized)?;
    }

    let bytes = fat::read_file_to_vec(normalized).map_err(map_fat_error)?;
    install_open_file(bytes)
}

pub(crate) fn read_current_process_file(
    fd: u64,
    user_ptr: u64,
    user_len: u64,
) -> Result<Option<usize>, FileSysopError> {
    let user_len = usize::try_from(user_len).map_err(|_| FileSysopError::InvalidArgument)?;
    if user_len == 0 {
        return Ok(Some(0));
    }

    let Some(result) = multitask::with_current_user_process_state_mut(|_, _, process_state| {
        if !matches!(
            process_state.handles().get(fd),
            Some(KernelHandle::BootFile(_))
        ) {
            return Ok(None);
        }
        process_state
            .address_space()
            .validate_user_write_buffer(VirtAddr::new(user_ptr), user_len)?;

        let mut copied = 0usize;
        let mut chunk = [0_u8; FILE_IO_CHUNK_LEN];
        while copied < user_len {
            let chunk_len = min(user_len - copied, chunk.len());
            let read = {
                let Some(KernelHandle::BootFile(file)) = process_state.handles_mut().get_mut(fd)
                else {
                    return Err(FileSysopError::BadFileDescriptor);
                };
                file.read_into(&mut chunk[..chunk_len])
            };
            if read == 0 {
                break;
            }

            let chunk_ptr = user_ptr
                .checked_add(copied as u64)
                .ok_or(FileSysopError::InvalidArgument)?;
            process_state
                .address_space()
                .copy_into_user(VirtAddr::new(chunk_ptr), &chunk[..read])?;
            copied += read;
        }

        Ok(Some(copied))
    }) else {
        return Err(FileSysopError::Unsupported);
    };

    result
}

pub(crate) fn pread_current_process_file(
    fd: u64,
    user_ptr: u64,
    user_len: u64,
    offset: u64,
) -> Result<Option<usize>, FileSysopError> {
    let user_len = usize::try_from(user_len).map_err(|_| FileSysopError::InvalidArgument)?;
    let offset = usize::try_from(offset).map_err(|_| FileSysopError::InvalidArgument)?;
    if user_len == 0 {
        return Ok(Some(0));
    }

    let Some(result) = multitask::with_current_user_process_state_mut(|_, _, process_state| {
        if !matches!(
            process_state.handles().get(fd),
            Some(KernelHandle::BootFile(_))
        ) {
            return Ok(None);
        }
        process_state
            .address_space()
            .validate_user_write_buffer(VirtAddr::new(user_ptr), user_len)?;

        let mut copied = 0usize;
        let mut chunk = [0_u8; FILE_IO_CHUNK_LEN];
        while copied < user_len {
            let chunk_len = min(user_len - copied, chunk.len());
            let read = {
                let Some(KernelHandle::BootFile(file)) = process_state.handles_mut().get_mut(fd)
                else {
                    return Err(FileSysopError::BadFileDescriptor);
                };
                let chunk_offset = offset
                    .checked_add(copied)
                    .ok_or(FileSysopError::InvalidArgument)?;
                file.read_at(chunk_offset, &mut chunk[..chunk_len])
            };
            if read == 0 {
                break;
            }

            let chunk_ptr = user_ptr
                .checked_add(copied as u64)
                .ok_or(FileSysopError::InvalidArgument)?;
            process_state
                .address_space()
                .copy_into_user(VirtAddr::new(chunk_ptr), &chunk[..read])?;
            copied += read;
        }

        Ok(Some(copied))
    }) else {
        return Err(FileSysopError::Unsupported);
    };

    result
}

pub(crate) fn write_current_process_file(
    fd: u64,
    user_ptr: u64,
    user_len: u64,
) -> Result<Option<usize>, FileSysopError> {
    let user_len = usize::try_from(user_len).map_err(|_| FileSysopError::InvalidArgument)?;
    if user_len == 0 {
        return Ok(Some(0));
    }

    let Some(result) = multitask::with_current_user_process_state_mut(|_, _, process_state| {
        if !matches!(
            process_state.handles().get(fd),
            Some(KernelHandle::BootFile(_))
        ) {
            return Ok(None);
        }
        process_state
            .address_space()
            .validate_user_read_buffer(VirtAddr::new(user_ptr), user_len)?;

        let mut copied = 0usize;
        let mut chunk = [0_u8; FILE_IO_CHUNK_LEN];
        while copied < user_len {
            let chunk_len = min(user_len - copied, chunk.len());
            let chunk_ptr = user_ptr
                .checked_add(copied as u64)
                .ok_or(FileSysopError::InvalidArgument)?;
            process_state
                .address_space()
                .copy_from_user(VirtAddr::new(chunk_ptr), &mut chunk[..chunk_len])?;

            let write_result = {
                let Some(KernelHandle::BootFile(file)) = process_state.handles_mut().get_mut(fd)
                else {
                    return Err(FileSysopError::BadFileDescriptor);
                };
                file.write_from(&chunk[..chunk_len])
            };

            match write_result {
                Ok(written) => copied += written,
                Err(BootFileWriteError::ReadOnly) => {
                    return Err(FileSysopError::ReadOnlyFilesystem);
                }
                Err(BootFileWriteError::Unsupported) => {
                    return Err(FileSysopError::Unsupported);
                }
            }
        }

        Ok(Some(copied))
    }) else {
        return Err(FileSysopError::Unsupported);
    };

    result
}

pub(crate) fn seek_current_process_file(
    fd: u64,
    offset: i64,
    whence: BootFileSeekWhence,
) -> Result<Option<u64>, FileSysopError> {
    let Some(result) = multitask::with_current_user_process_state_mut(|_, _, process_state| {
        let Some(handle) = process_state.handles_mut().get_mut(fd) else {
            return Err(FileSysopError::BadFileDescriptor);
        };

        let KernelHandle::BootFile(file) = handle else {
            return Ok(None);
        };

        file.seek(offset, whence).map(Some).map_err(map_seek_error)
    }) else {
        return Err(FileSysopError::Unsupported);
    };

    result
}

pub(crate) fn metadata_current_process_file(
    fd: u64,
) -> Result<Option<FileMetadata>, FileSysopError> {
    let Some(result) = multitask::with_current_user_process_state_mut(|_, _, process_state| {
        let Some(handle) = process_state.handles().get(fd) else {
            return Err(FileSysopError::BadFileDescriptor);
        };

        let KernelHandle::BootFile(file) = handle else {
            return Ok(None);
        };

        Ok(Some(FileMetadata {
            len: file.len() as u64,
        }))
    }) else {
        return Err(FileSysopError::Unsupported);
    };

    result
}

pub(crate) fn metadata_for_current_process_path(
    path: &str,
) -> Result<FileMetadata, FileSysopError> {
    let normalized = normalize_boot_volume_path(path)?;
    if !is_runtime_library_path(normalized) {
        ensure_current_process_file_access(normalized)?;
    }
    query_boot_volume_file_metadata(normalized)
}

pub(crate) fn check_access_for_current_process(
    path: &str,
    mode: u64,
) -> Result<(), FileSysopError> {
    let normalized = normalize_boot_volume_path(path)?;
    if mode
        & !(crate::user::linux::R_OK
            | crate::user::linux::W_OK
            | crate::user::linux::X_OK
            | crate::user::linux::F_OK)
        != 0
    {
        return Err(FileSysopError::InvalidArgument);
    }

    if !is_runtime_library_path(normalized) {
        ensure_current_process_file_access(normalized)?;
    }

    let _ = query_boot_volume_file_metadata(normalized)?;
    if mode & (crate::user::linux::W_OK | crate::user::linux::X_OK) != 0 {
        return Err(FileSysopError::PermissionDenied);
    }

    Ok(())
}

fn install_open_file(bytes: Vec<u8>) -> Result<u64, FileSysopError> {
    let Some(result) = multitask::with_current_user_process_state_mut(|_, _, process_state| {
        Ok(process_state
            .handles_mut()
            .install(KernelHandle::BootFile(BootFileHandle::read_only(bytes))))
    }) else {
        return Err(FileSysopError::Unsupported);
    };

    result
}

fn validate_open_flags(flags: u64) -> Result<(), FileSysopError> {
    if flags & !READ_ONLY_OPEN_FLAGS != 0 {
        return Err(FileSysopError::ReadOnlyFilesystem);
    }

    match flags & crate::user::linux::O_ACCMODE {
        crate::user::linux::O_RDONLY => Ok(()),
        crate::user::linux::O_WRONLY | crate::user::linux::O_RDWR => {
            Err(FileSysopError::ReadOnlyFilesystem)
        }
        _ => Err(FileSysopError::InvalidArgument),
    }
}

fn normalize_boot_volume_path(path: &str) -> Result<&str, FileSysopError> {
    if path.is_empty() || path.len() > MAX_OPEN_PATH_LEN {
        return Err(FileSysopError::InvalidArgument);
    }
    if path == "/" {
        return Err(FileSysopError::InvalidArgument);
    }
    Ok(path.trim_start_matches('/'))
}

fn ensure_current_process_file_access(path: &str) -> Result<(), FileSysopError> {
    let Some(permission_granted) =
        multitask::with_current_user_process_state_mut(|_, _, process_state| {
            process_state.require_logical_admin_for_file_access(path)
        })
    else {
        return Err(FileSysopError::Unsupported);
    };
    if !permission_granted {
        return Err(FileSysopError::PermissionDenied);
    }
    Ok(())
}

fn query_boot_volume_file_metadata(path: &str) -> Result<FileMetadata, FileSysopError> {
    let volume = fat::BootVolume::open().map_err(map_fat_error)?;
    let result = {
        let mut file = volume.open_file(path).map_err(map_fat_error)?;
        let len = file.seek(SeekFrom::End(0)).map_err(map_fat_error)?;
        Ok(FileMetadata { len })
    };

    match (result, volume.close()) {
        (Ok(value), Ok(())) => Ok(value),
        (Ok(_), Err(err)) => Err(map_fat_error(err)),
        (Err(err), _) => Err(err),
    }
}

fn is_runtime_library_path(path: &str) -> bool {
    path.starts_with("lib/")
        || path.starts_with("lib64/")
        || path.starts_with("usr/lib/")
        || path == "etc/ld.so.cache"
        || path == "etc/ld.so.preload"
        || path == "etc/ld.so.conf"
        || path.starts_with("etc/ld.so.conf.d/")
}

fn map_seek_error(err: BootFileSeekError) -> FileSysopError {
    match err {
        BootFileSeekError::InvalidPosition => FileSysopError::InvalidArgument,
    }
}

fn map_fat_error(err: fatfs::Error<fat::DiskIoError>) -> FileSysopError {
    match err {
        fatfs::Error::NotFound => FileSysopError::NotFound,
        fatfs::Error::InvalidInput => FileSysopError::InvalidArgument,
        fatfs::Error::Io(fat::DiskIoError::InvalidInput) => FileSysopError::InvalidArgument,
        _ => FileSysopError::Unsupported,
    }
}

#[cfg(test)]
mod tests {
    use super::{FileSysopError, normalize_boot_volume_path, validate_open_flags};

    #[test]
    fn read_only_flags_are_accepted() {
        assert_eq!(validate_open_flags(0).ok(), Some(()));
        assert_eq!(
            validate_open_flags(crate::user::linux::O_CLOEXEC).ok(),
            Some(())
        );
    }

    #[test]
    fn write_flags_are_rejected_on_read_only_boot_volume() {
        assert_eq!(
            validate_open_flags(crate::user::linux::O_WRONLY),
            Err(FileSysopError::ReadOnlyFilesystem)
        );
        assert_eq!(
            validate_open_flags(crate::user::linux::O_CREAT),
            Err(FileSysopError::ReadOnlyFilesystem)
        );
    }

    #[test]
    fn absolute_paths_are_normalized_for_boot_volume_lookup() {
        assert_eq!(
            normalize_boot_volume_path("/apps/init.elf").unwrap(),
            "apps/init.elf"
        );
        assert_eq!(
            normalize_boot_volume_path("apps/init.elf").unwrap(),
            "apps/init.elf"
        );
    }
}
