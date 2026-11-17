//! Filesystem-neutral VFS volume abstraction for block-backed filesystems.
//!
//! Designed to support FAT, ext4, and NTFS backends behind a uniform `&self`
//! trait interface (`FsVolume: Send + Sync`).

use alloc::string::String;
use alloc::vec::Vec;
use core::fmt;

use crate::StorageError;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CasePolicy {
    Exact,
    AsciiInsensitive,
    UnicodePreservingInsensitive,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct FsCapabilities {
    pub read_only: bool,
    pub case_policy: CasePolicy,
    pub supports_symlinks: bool,
    pub supports_hardlinks: bool,
    pub supports_unlink_while_open: bool,
    pub supports_sparse: bool,
    pub supports_nt_attrs: bool,
    pub supports_streams: bool,
    pub supports_share_modes: bool,
    pub max_name_len: u32,
}

pub mod dos_attrs {
    pub const READONLY: u32 = 0x1;
    pub const HIDDEN: u32 = 0x2;
    pub const SYSTEM: u32 = 0x4;
    pub const DIRECTORY: u32 = 0x10;
    pub const ARCHIVE: u32 = 0x20;
    pub const NORMAL: u32 = 0x80;
    pub const REPARSE_POINT: u32 = 0x400;
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FsNodeKind {
    File,
    Directory,
    Symlink,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct FsTimestamp {
    pub secs: i64,
    pub nanos: u32,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct FsMetadata {
    pub node_id: u64,
    pub kind: FsNodeKind,
    pub len: u64,
    pub allocation_size: u64,
    pub nlink: u32,
    pub mode: u32,
    pub uid: u32,
    pub gid: u32,
    pub dos_attrs: u32,
    pub atime: FsTimestamp,
    pub mtime: FsTimestamp,
    pub ctime: FsTimestamp,
    pub crtime: FsTimestamp,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FsDirEntry {
    pub name: String,
    pub node_id: u64,
    pub kind: FsNodeKind,
    pub len: u64,
    pub dos_attrs: u32,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct FsStatFs {
    pub fs_type_magic: u64,
    pub block_size: u64,
    pub total_blocks: u64,
    pub free_blocks: u64,
    pub avail_blocks: u64,
    pub total_inodes: u64,
    pub free_inodes: u64,
    pub max_name_len: u32,
    pub fs_name: &'static str,
    pub case_sensitive: bool,
    pub case_preserved: bool,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct FsOpenOptions {
    pub create: bool,
    pub create_new: bool,
    pub truncate: bool,
    pub write: bool,
    pub append: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FsError {
    NotFound,
    AlreadyExists,
    NotADirectory,
    IsADirectory,
    DirectoryNotEmpty,
    NoSpace,
    ReadOnly,
    InvalidInput,
    PermissionDenied,
    SharingViolation,
    Unsupported,
    Busy,
    TooBig,
    NameTooLong,
    Io(StorageError),
}

impl From<StorageError> for FsError {
    fn from(err: StorageError) -> Self {
        Self::Io(err)
    }
}

impl fmt::Display for FsError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotFound => f.write_str("not found"),
            Self::AlreadyExists => f.write_str("already exists"),
            Self::NotADirectory => f.write_str("not a directory"),
            Self::IsADirectory => f.write_str("is a directory"),
            Self::DirectoryNotEmpty => f.write_str("directory not empty"),
            Self::NoSpace => f.write_str("no space left"),
            Self::ReadOnly => f.write_str("read-only filesystem"),
            Self::InvalidInput => f.write_str("invalid input"),
            Self::PermissionDenied => f.write_str("permission denied"),
            Self::SharingViolation => f.write_str("sharing violation"),
            Self::Unsupported => f.write_str("unsupported operation"),
            Self::Busy => f.write_str("resource busy"),
            Self::TooBig => f.write_str("file too big"),
            Self::NameTooLong => f.write_str("file name too long"),
            Self::Io(err) => write!(f, "storage I/O error: {err:?}"),
        }
    }
}

pub trait FsVolume: Send + Sync {
    fn capabilities(&self) -> FsCapabilities;
    fn is_immutable(&self) -> bool;
    fn canonical_key(&self, path: &str) -> Result<String, FsError>;
    fn metadata(&self, path: &str) -> Result<FsMetadata, FsError>;
    fn read_dir(&self, path: &str) -> Result<Vec<FsDirEntry>, FsError>;
    fn statfs(&self) -> Result<FsStatFs, FsError>;
    fn open_prepare(&self, path: &str, opts: FsOpenOptions) -> Result<FsMetadata, FsError>;
    fn read_at(&self, path: &str, offset: u64, out: &mut [u8]) -> Result<usize, FsError>;
    fn write_at(&self, path: &str, offset: u64, data: &[u8]) -> Result<(usize, u64), FsError>;
    fn truncate(&self, path: &str, new_len: u64) -> Result<u64, FsError>;
    fn mkdir(&self, path: &str) -> Result<(), FsError>;
    fn unlink(&self, path: &str) -> Result<(), FsError>;
    fn rmdir(&self, path: &str) -> Result<(), FsError>;
    fn rename(
        &self,
        src: &str,
        dst: &str,
        replace_existing: bool,
        no_replace: bool,
    ) -> Result<(), FsError>;
    fn set_times(
        &self,
        _path: &str,
        _atime: Option<FsTimestamp>,
        _mtime: Option<FsTimestamp>,
        _crtime: Option<FsTimestamp>,
    ) -> Result<(), FsError> {
        Err(FsError::Unsupported)
    }
    fn set_dos_attrs(&self, _path: &str, _dos_attrs: u32) -> Result<(), FsError> {
        Err(FsError::Unsupported)
    }
    fn symlink(&self, _target: &str, _linkpath: &str) -> Result<(), FsError> {
        Err(FsError::Unsupported)
    }
    fn readlink(&self, _path: &str) -> Result<String, FsError> {
        Err(FsError::Unsupported)
    }
    fn flush(&self) -> Result<(), FsError>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fs_error_converts_from_storage_error() {
        let err: FsError = StorageError::DeviceFault.into();
        assert_eq!(err, FsError::Io(StorageError::DeviceFault));
        assert_eq!(
            FsError::from(StorageError::InvalidInput),
            FsError::Io(StorageError::InvalidInput)
        );
    }

    #[test]
    fn dos_attrs_constants_match_fat_and_nt_bits() {
        assert_eq!(dos_attrs::READONLY, 0x1);
        assert_eq!(dos_attrs::HIDDEN, 0x2);
        assert_eq!(dos_attrs::SYSTEM, 0x4);
        assert_eq!(dos_attrs::DIRECTORY, 0x10);
        assert_eq!(dos_attrs::ARCHIVE, 0x20);
        assert_eq!(dos_attrs::NORMAL, 0x80);
        assert_eq!(dos_attrs::REPARSE_POINT, 0x400);
    }

    #[test]
    fn fs_timestamp_and_open_options_default() {
        assert_eq!(FsTimestamp::default(), FsTimestamp { secs: 0, nanos: 0 });
        assert_eq!(
            FsOpenOptions::default(),
            FsOpenOptions {
                create: false,
                create_new: false,
                truncate: false,
                write: false,
                append: false,
            }
        );
    }
}
