use crate::vfs_core::path_inode;
use alloc::string::String;

use crate::multitask;
use crate::user::handles::KernelHandle;
use crate::vfs;

use super::file;

const DEFAULT_BLOCK_SIZE: u64 = 4096;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum KernelNodeKind {
    File,
    Directory,
    Device,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct KernelTimestamp {
    pub(crate) sec: i64,
    pub(crate) nsec: i64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct KernelStat {
    pub(crate) inode: u64,
    pub(crate) kind: KernelNodeKind,
    pub(crate) link_count: u64,
    pub(crate) size: u64,
    pub(crate) block_size: u64,
    pub(crate) blocks: u64,
    pub(crate) atime: KernelTimestamp,
    pub(crate) mtime: KernelTimestamp,
    pub(crate) ctime: KernelTimestamp,
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub(crate) enum StatLookupError {
    BadFileDescriptor,
    InvalidArgument,
    NotFound,
    NotDirectory,
    PermissionDenied,
    Unsupported,
}

impl From<file::FileSysopError> for StatLookupError {
    fn from(value: file::FileSysopError) -> Self {
        match value {
            file::FileSysopError::AddressSpace(_) => Self::InvalidArgument,
            file::FileSysopError::BadFileDescriptor => Self::BadFileDescriptor,
            file::FileSysopError::InvalidArgument => Self::InvalidArgument,
            file::FileSysopError::NotFound => Self::NotFound,
            file::FileSysopError::NotDirectory => Self::NotDirectory,
            file::FileSysopError::PermissionDenied => Self::PermissionDenied,
            file::FileSysopError::ReadOnlyFilesystem => Self::PermissionDenied,
            file::FileSysopError::Unsupported => Self::Unsupported,
        }
    }
}

pub(crate) fn for_fd(fd: u64) -> Result<KernelStat, StatLookupError> {
    if fd <= 2 {
        return Ok(build_device_stat(fd));
    }

    let retained =
        multitask::retain_current_user_process_state().ok_or(StatLookupError::Unsupported)?;
    let process_state = retained.process_state();
    let Some(handle) = process_state.handles().get(fd) else {
        return Err(StatLookupError::BadFileDescriptor);
    };

    match handle {
        KernelHandle::Console(_)
        | KernelHandle::Device(_)
        | KernelHandle::Socket(_)
        | KernelHandle::Epoll(_)
        | KernelHandle::SharedRegion(_) => Ok(build_device_stat(fd)),
        KernelHandle::Memfd(file) => {
            let path = file.path();
            Ok(build_regular_file_stat(
                path_inode(path.as_bytes()),
                file.len() as u64,
            ))
        }
        KernelHandle::VfsDirectory(directory) => {
            let path = String::from(directory.path());
            let metadata = file::metadata_for_current_process_path(path.as_str())?;
            Ok(build_path_stat(path.as_bytes(), metadata))
        }
        KernelHandle::DisplaySurface(surface) => {
            Ok(build_regular_file_stat(fd, surface.frame_len()))
        }
        KernelHandle::VfsFile(file) => {
            let path = file.path();
            Ok(build_regular_file_stat(
                path_inode(path.as_bytes()),
                file.len() as u64,
            ))
        }
    }
}

pub(crate) fn for_absolute_path(absolute_path: &str) -> Result<KernelStat, StatLookupError> {
    let metadata =
        vfs::metadata_for_current_process_path(absolute_path).map_err(map_vfs_error_to_stat)?;
    Ok(kernel_stat_from_vfs_metadata(metadata))
}

fn build_regular_file_stat(inode: u64, len: u64) -> KernelStat {
    KernelStat {
        inode: inode.max(1),
        kind: KernelNodeKind::File,
        link_count: 1,
        size: len,
        block_size: DEFAULT_BLOCK_SIZE,
        blocks: len.div_ceil(512),
        atime: KernelTimestamp::default(),
        mtime: KernelTimestamp::default(),
        ctime: KernelTimestamp::default(),
    }
}

fn build_directory_stat(inode: u64, len: u64) -> KernelStat {
    KernelStat {
        inode: inode.max(1),
        kind: KernelNodeKind::Directory,
        link_count: 2,
        size: len,
        block_size: DEFAULT_BLOCK_SIZE,
        blocks: len.div_ceil(512),
        atime: KernelTimestamp::default(),
        mtime: KernelTimestamp::default(),
        ctime: KernelTimestamp::default(),
    }
}

fn build_device_stat(inode: u64) -> KernelStat {
    KernelStat {
        inode: inode.max(1),
        kind: KernelNodeKind::Device,
        link_count: 1,
        size: 0,
        block_size: DEFAULT_BLOCK_SIZE,
        blocks: 0,
        atime: KernelTimestamp::default(),
        mtime: KernelTimestamp::default(),
        ctime: KernelTimestamp::default(),
    }
}

fn build_path_stat(path: &[u8], metadata: file::FileMetadata) -> KernelStat {
    match metadata.kind {
        file::FileNodeKind::File => build_regular_file_stat(path_inode(path), metadata.len),
        file::FileNodeKind::Directory => build_directory_stat(path_inode(path), metadata.len),
    }
}

fn kernel_stat_from_vfs_metadata(metadata: vfs::VfsMetadata) -> KernelStat {
    KernelStat {
        inode: metadata.inode.max(1),
        kind: match metadata.kind {
            vfs::VfsNodeKind::File => KernelNodeKind::File,
            vfs::VfsNodeKind::Directory => KernelNodeKind::Directory,
            vfs::VfsNodeKind::Device => KernelNodeKind::Device,
        },
        link_count: metadata.link_count,
        size: metadata.len,
        block_size: metadata.block_size,
        blocks: metadata.blocks,
        atime: KernelTimestamp {
            sec: metadata.atime.sec,
            nsec: metadata.atime.nsec,
        },
        mtime: KernelTimestamp {
            sec: metadata.mtime.sec,
            nsec: metadata.mtime.nsec,
        },
        ctime: KernelTimestamp {
            sec: metadata.ctime.sec,
            nsec: metadata.ctime.nsec,
        },
    }
}

fn map_vfs_error_to_stat(err: vfs::VfsError) -> StatLookupError {
    match err {
        vfs::VfsError::BadFileDescriptor => StatLookupError::BadFileDescriptor,
        vfs::VfsError::InvalidArgument => StatLookupError::InvalidArgument,
        vfs::VfsError::NotFound => StatLookupError::NotFound,
        vfs::VfsError::NotDirectory => StatLookupError::NotDirectory,
        vfs::VfsError::PermissionDenied => StatLookupError::PermissionDenied,
        vfs::VfsError::ReadOnlyFilesystem => StatLookupError::PermissionDenied,
        vfs::VfsError::Unsupported => StatLookupError::Unsupported,
    }
}
