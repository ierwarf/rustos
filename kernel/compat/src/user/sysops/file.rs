use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec;
use alloc::vec::Vec;
use core::cmp::min;
use core::convert::TryFrom;

use x86_64::VirtAddr;

use crate::memory::paging;
use crate::multitask;
use crate::user::handles::{
    FileHandleSeekError, FileHandleSeekWhence, FileHandleWriteError, KernelHandle, VfsFileHandle,
};
use crate::user::linux as linux_abi;
use crate::user::memfd::{MemfdError, MemfdHandle};
use crate::vfs;

const FILE_IO_CHUNK_LEN: usize = 16 * 1024;
const MAX_OPEN_PATH_LEN: usize = 256;

enum CurrentFileHandle {
    Vfs(VfsFileHandle),
    Memfd(MemfdHandle),
}

impl CurrentFileHandle {
    fn read_into(&mut self, dest: &mut [u8]) -> usize {
        match self {
            Self::Vfs(file) => file.read_into(dest),
            Self::Memfd(file) => file.read_into(dest),
        }
    }

    fn read_at(&self, offset: usize, dest: &mut [u8]) -> usize {
        match self {
            Self::Vfs(file) => file.read_at(offset, dest),
            Self::Memfd(file) => file.read_at(offset, dest),
        }
    }

    fn write_from(&mut self, src: &[u8]) -> Result<usize, FileHandleWriteError> {
        match self {
            Self::Vfs(file) => file.write_from(src),
            Self::Memfd(file) => file.write_from(src).map_err(map_memfd_write_error),
        }
    }

    fn seek(
        &mut self,
        offset: i64,
        whence: FileHandleSeekWhence,
    ) -> Result<u64, FileHandleSeekError> {
        match self {
            Self::Vfs(file) => file.seek(offset, whence),
            Self::Memfd(file) => file.seek(offset, whence),
        }
    }
}

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

#[cfg_attr(not(test), allow(dead_code))]
pub(crate) fn read_path_for_current_process_to_vec(
    absolute_path: &str,
) -> Result<Vec<u8>, FileSysopError> {
    Ok(read_path_for_current_process_bytes(absolute_path)?
        .as_ref()
        .to_vec())
}

pub(crate) fn read_path_for_current_process_bytes(
    absolute_path: &str,
) -> Result<Arc<[u8]>, FileSysopError> {
    let handle = take_opened_handle_for_current_process(absolute_path)?;
    let (file_len, shared_bytes, mut read_at): (
        usize,
        Option<Arc<[u8]>>,
        alloc::boxed::Box<dyn FnMut(usize, &mut [u8]) -> usize>,
    ) = match handle {
        KernelHandle::VfsFile(file) => {
            let len = file.len();
            let shared = file.shared_bytes();
            (
                len,
                shared,
                alloc::boxed::Box::new(move |offset, dest| file.read_at(offset, dest)),
            )
        }
        KernelHandle::Memfd(file) => {
            let len = file.len();
            (
                len,
                None,
                alloc::boxed::Box::new(move |offset, dest| file.read_at(offset, dest)),
            )
        }
        _ => return Err(FileSysopError::PermissionDenied),
    };

    if let Some(bytes) = shared_bytes {
        return Ok(bytes);
    }

    let mut bytes = vec![0_u8; file_len];
    let mut copied = 0usize;
    while copied < bytes.len() {
        let read = read_at(copied, &mut bytes[copied..]);
        if read == 0 {
            bytes.truncate(copied);
            break;
        }
        copied += read;
    }

    Ok(Arc::<[u8]>::from(bytes.into_boxed_slice()))
}

pub(crate) fn open_path_for_current_process_file(
    absolute_path: &str,
) -> Result<VfsFileHandle, FileSysopError> {
    match take_opened_handle_for_current_process(absolute_path)? {
        KernelHandle::VfsFile(file) => Ok(file),
        _ => Err(FileSysopError::PermissionDenied),
    }
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

    let mut handle = match clone_current_process_file_handle(fd)? {
        Some(handle) => handle,
        None => return Ok(None),
    };
    multitask::with_current_mm(|address_space| {
        address_space.validate_user_write_buffer(VirtAddr::new(user_ptr), user_len)
    })
    .ok_or(FileSysopError::Unsupported)?
    .map_err(FileSysopError::AddressSpace)?;

    let mut copied = 0usize;
    let mut chunk = [0_u8; FILE_IO_CHUNK_LEN];
    while copied < user_len {
        let chunk_len = min(user_len - copied, chunk.len());
        let read = handle.read_into(&mut chunk[..chunk_len]);
        if read == 0 {
            break;
        }

        let chunk_ptr = user_ptr
            .checked_add(copied as u64)
            .ok_or(FileSysopError::InvalidArgument)?;
        multitask::with_current_mm(|address_space| {
            address_space.copy_into_user(VirtAddr::new(chunk_ptr), &chunk[..read])
        })
        .ok_or(FileSysopError::Unsupported)?
        .map_err(FileSysopError::AddressSpace)?;
        copied += read;
    }

    Ok(Some(copied))
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

    let handle = match clone_current_process_file_handle(fd)? {
        Some(handle) => handle,
        None => return Ok(None),
    };
    multitask::with_current_mm(|address_space| {
        address_space.validate_user_write_buffer(VirtAddr::new(user_ptr), user_len)
    })
    .ok_or(FileSysopError::Unsupported)?
    .map_err(FileSysopError::AddressSpace)?;

    let mut copied = 0usize;
    let mut chunk = [0_u8; FILE_IO_CHUNK_LEN];
    while copied < user_len {
        let chunk_len = min(user_len - copied, chunk.len());
        let chunk_offset = offset
            .checked_add(copied)
            .ok_or(FileSysopError::InvalidArgument)?;
        let read = handle.read_at(chunk_offset, &mut chunk[..chunk_len]);
        if read == 0 {
            break;
        }

        let chunk_ptr = user_ptr
            .checked_add(copied as u64)
            .ok_or(FileSysopError::InvalidArgument)?;
        multitask::with_current_mm(|address_space| {
            address_space.copy_into_user(VirtAddr::new(chunk_ptr), &chunk[..read])
        })
        .ok_or(FileSysopError::Unsupported)?
        .map_err(FileSysopError::AddressSpace)?;
        copied += read;
    }

    Ok(Some(copied))
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

    let mut handle = match clone_current_process_file_handle(fd)? {
        Some(handle) => handle,
        None => return Ok(None),
    };
    multitask::with_current_mm(|address_space| {
        address_space.validate_user_read_buffer(VirtAddr::new(user_ptr), user_len)
    })
    .ok_or(FileSysopError::Unsupported)?
    .map_err(FileSysopError::AddressSpace)?;

    let mut copied = 0usize;
    let mut chunk = [0_u8; FILE_IO_CHUNK_LEN];
    while copied < user_len {
        let chunk_len = min(user_len - copied, chunk.len());
        let chunk_ptr = user_ptr
            .checked_add(copied as u64)
            .ok_or(FileSysopError::InvalidArgument)?;
        multitask::with_current_mm(|address_space| {
            address_space.copy_from_user(VirtAddr::new(chunk_ptr), &mut chunk[..chunk_len])
        })
        .ok_or(FileSysopError::Unsupported)?
        .map_err(FileSysopError::AddressSpace)?;

        match handle.write_from(&chunk[..chunk_len]) {
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
}

pub(crate) fn seek_current_process_file(
    fd: u64,
    offset: i64,
    whence: FileHandleSeekWhence,
) -> Result<Option<u64>, FileSysopError> {
    let mut handle = match clone_current_process_file_handle(fd)? {
        Some(handle) => handle,
        None => return Ok(None),
    };
    handle
        .seek(offset, whence)
        .map(Some)
        .map_err(map_seek_error)
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
    let retained =
        multitask::retain_current_user_process_state().ok_or(FileSysopError::Unsupported)?;
    let process_state = retained.process_state();
    if is_at_fdcwd(dirfd) {
        return Ok(String::from(process_state.cwd()));
    }

    let Some(handle) = process_state.handles().get(dirfd) else {
        return Err(FileSysopError::BadFileDescriptor);
    };

    match handle {
        KernelHandle::VfsDirectory(directory) => Ok(String::from(directory.path())),
        KernelHandle::VfsFile(_) => Err(FileSysopError::NotDirectory),
        KernelHandle::Memfd(_) => Err(FileSysopError::NotDirectory),
        _ => Err(FileSysopError::InvalidArgument),
    }
}

fn clone_current_process_file_handle(fd: u64) -> Result<Option<CurrentFileHandle>, FileSysopError> {
    let retained =
        multitask::retain_current_user_process_state().ok_or(FileSysopError::Unsupported)?;
    let process_state = retained.process_state();
    let Some(handle) = process_state.handles().get(fd) else {
        return Err(FileSysopError::BadFileDescriptor);
    };

    match handle {
        KernelHandle::VfsFile(file) => Ok(Some(CurrentFileHandle::Vfs(file.clone()))),
        KernelHandle::Memfd(file) => Ok(Some(CurrentFileHandle::Memfd(file.clone()))),
        _ => Ok(None),
    }
}

fn take_opened_handle_for_current_process(
    absolute_path: &str,
) -> Result<KernelHandle, FileSysopError> {
    let fd = open_path_for_current_process(absolute_path, linux_abi::O_RDONLY, 0)?;

    let Some(result) = multitask::with_current_user_process_state_mut(
        |_, _, process_state| -> Result<KernelHandle, FileSysopError> {
            let handle = process_state
                .handles()
                .get(fd)
                .cloned()
                .ok_or(FileSysopError::BadFileDescriptor)?;
            let _ = process_state.handles_mut().close(fd);
            Ok(handle)
        },
    ) else {
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

fn map_memfd_write_error(err: MemfdError) -> FileHandleWriteError {
    match err {
        MemfdError::PermissionDenied => FileHandleWriteError::ReadOnly,
        MemfdError::Busy | MemfdError::InvalidArgument | MemfdError::NoMemory => {
            FileHandleWriteError::Unsupported
        }
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
