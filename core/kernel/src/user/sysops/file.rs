use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;
use core::cmp::min;
use core::convert::TryFrom;

use x86_64::VirtAddr;

use crate::memory::paging;
use crate::multitask;
use crate::user::handles::{
    FileHandleSeekError, FileHandleSeekWhence, FileHandleWriteError, KernelHandle,
};
use crate::user::linux as linux_abi;
use crate::vfs;

const FILE_IO_CHUNK_LEN: usize = 512;
const MAX_OPEN_PATH_LEN: usize = 256;

#[derive(Debug, Clone, Copy)]
pub(crate) enum FileSysopError {
    AddressSpace(paging::AddressSpaceError),
    BadFileDescriptor,
    InvalidArgument,
    NotFound,
    NotDirectory,
    PermissionDenied,
    ReadOnlyFilesystem,
    Unsupported,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum FileNodeKind {
    File,
    Directory,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct FileMetadata {
    pub len: u64,
    pub kind: FileNodeKind,
}

impl From<paging::AddressSpaceError> for FileSysopError {
    fn from(value: paging::AddressSpaceError) -> Self {
        Self::AddressSpace(value)
    }
}

impl From<vfs::VfsError> for FileSysopError {
    fn from(value: vfs::VfsError) -> Self {
        match value {
            vfs::VfsError::BadFileDescriptor => Self::BadFileDescriptor,
            vfs::VfsError::InvalidArgument => Self::InvalidArgument,
            vfs::VfsError::NotFound => Self::NotFound,
            vfs::VfsError::NotDirectory => Self::NotDirectory,
            vfs::VfsError::PermissionDenied => Self::PermissionDenied,
            vfs::VfsError::ReadOnlyFilesystem => Self::ReadOnlyFilesystem,
            vfs::VfsError::Unsupported => Self::Unsupported,
        }
    }
}

pub(crate) fn resolve_path_for_current_process(
    dirfd: u64,
    path: &str,
) -> Result<String, FileSysopError> {
    if path.is_empty() || path.len() > MAX_OPEN_PATH_LEN {
        return Err(FileSysopError::InvalidArgument);
    }

    let base_path = if path.starts_with('/') {
        String::from("/")
    } else {
        resolve_base_path_for_dirfd(dirfd)?
    };

    normalize_absolute_path(base_path.as_str(), path)
}

pub(crate) fn open_path_for_current_process(
    absolute_path: &str,
    flags: u64,
    mode: u64,
) -> Result<u64, FileSysopError> {
    let fd = vfs::open_path_for_current_process(absolute_path, flags, mode)
        .map_err(FileSysopError::from)?;
    Ok(fd)
}

pub(crate) fn read_path_for_current_process_to_vec(
    absolute_path: &str,
) -> Result<Vec<u8>, FileSysopError> {
    let fd = open_path_for_current_process(absolute_path, linux_abi::O_RDONLY, 0)?;

    let Some(result) = multitask::with_current_user_process_state_mut(|_, _, process_state| {
        let handle = process_state
            .handles()
            .get(fd)
            .cloned()
            .ok_or(FileSysopError::BadFileDescriptor);
        let _ = process_state.handles_mut().close(fd);

        let handle = handle?;
        let KernelHandle::VfsFile(file) = handle else {
            return Err(FileSysopError::PermissionDenied);
        };

        let file_len = file.len();
        let mut bytes = vec![0_u8; file_len];
        let mut copied = 0usize;
        while copied < bytes.len() {
            let read = file.read_at(copied, &mut bytes[copied..]);
            if read == 0 {
                bytes.truncate(copied);
                break;
            }
            copied += read;
        }

        Ok(bytes)
    }) else {
        return Err(FileSysopError::Unsupported);
    };

    result
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
            Some(KernelHandle::VfsFile(_))
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
                let Some(KernelHandle::VfsFile(file)) = process_state.handles_mut().get_mut(fd)
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
            Some(KernelHandle::VfsFile(_))
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
                let Some(KernelHandle::VfsFile(file)) = process_state.handles_mut().get_mut(fd)
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
            Some(KernelHandle::VfsFile(_))
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
                let Some(KernelHandle::VfsFile(file)) = process_state.handles_mut().get_mut(fd)
                else {
                    return Err(FileSysopError::BadFileDescriptor);
                };
                file.write_from(&chunk[..chunk_len])
            };

            match write_result {
                Ok(written) => copied += written,
                Err(FileHandleWriteError::ReadOnly) => {
                    return Err(FileSysopError::ReadOnlyFilesystem);
                }
                Err(FileHandleWriteError::Unsupported) => {
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
    whence: FileHandleSeekWhence,
) -> Result<Option<u64>, FileSysopError> {
    let Some(result) = multitask::with_current_user_process_state_mut(|_, _, process_state| {
        let Some(handle) = process_state.handles_mut().get_mut(fd) else {
            return Err(FileSysopError::BadFileDescriptor);
        };

        let KernelHandle::VfsFile(file) = handle else {
            return Ok(None);
        };

        file.seek(offset, whence).map(Some).map_err(map_seek_error)
    }) else {
        return Err(FileSysopError::Unsupported);
    };

    result
}

pub(crate) fn metadata_for_current_process_path(
    absolute_path: &str,
) -> Result<FileMetadata, FileSysopError> {
    let metadata = vfs::metadata_for_current_process_path(absolute_path)?;
    Ok(FileMetadata {
        len: metadata.len,
        kind: match metadata.kind {
            vfs::VfsNodeKind::File => FileNodeKind::File,
            vfs::VfsNodeKind::Directory => FileNodeKind::Directory,
            vfs::VfsNodeKind::Device => FileNodeKind::File,
        },
    })
}

pub(crate) fn check_access_for_current_process(
    absolute_path: &str,
    mode: u64,
) -> Result<(), FileSysopError> {
    vfs::check_access_for_current_process(absolute_path, mode).map_err(Into::into)
}

fn resolve_base_path_for_dirfd(dirfd: u64) -> Result<String, FileSysopError> {
    let Some(result) = multitask::with_current_user_process_state_mut(|_, _, process_state| {
        if is_at_fdcwd(dirfd) {
            return Ok(String::from(process_state.cwd()));
        }

        let Some(handle) = process_state.handles().get(dirfd) else {
            return Err(FileSysopError::BadFileDescriptor);
        };

        match handle {
            KernelHandle::VfsDirectory(directory) => Ok(String::from(directory.path())),
            KernelHandle::VfsFile(_) => Err(FileSysopError::NotDirectory),
            _ => Err(FileSysopError::InvalidArgument),
        }
    }) else {
        return Err(FileSysopError::Unsupported);
    };

    result
}

fn normalize_absolute_path(base_path: &str, path: &str) -> Result<String, FileSysopError> {
    let mut components: Vec<&str> = Vec::new();
    let path_is_absolute = path.starts_with('/');
    if !path_is_absolute {
        for component in base_path.split('/') {
            if component.is_empty() {
                continue;
            }
            components.push(component);
        }
    }

    for component in path.split('/') {
        if component.is_empty() || component == "." {
            continue;
        }
        if component == ".." {
            components.pop();
            continue;
        }
        components.push(component);
    }

    let mut normalized = String::from("/");
    for component in components {
        if normalized.len() > 1 {
            normalized.push('/');
        }
        normalized.push_str(component);
    }

    if normalized.len() > MAX_OPEN_PATH_LEN {
        return Err(FileSysopError::InvalidArgument);
    }

    Ok(normalized)
}

fn is_at_fdcwd(dirfd: u64) -> bool {
    dirfd == linux_at_fdcwd_sign_extended() || dirfd == linux_abi::AT_FDCWD as u32 as u64
}

fn linux_at_fdcwd_sign_extended() -> u64 {
    (linux_abi::AT_FDCWD as i64) as u64
}

fn map_seek_error(err: FileHandleSeekError) -> FileSysopError {
    match err {
        FileHandleSeekError::InvalidPosition => FileSysopError::InvalidArgument,
    }
}

#[cfg(test)]
mod tests {
    use super::normalize_absolute_path;

    #[test]
    fn absolute_paths_collapse_dot_segments() {
        assert_eq!(
            normalize_absolute_path("/", "/lib//x86_64-linux-gnu/./../ld-linux.so").unwrap(),
            "/lib/ld-linux.so"
        );
        assert_eq!(
            normalize_absolute_path("/usr/lib", "../libc.so.6").unwrap(),
            "/usr/libc.so.6"
        );
    }
}
