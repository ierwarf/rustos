use core::mem::size_of;

use rustos_user_abi::syscall::{
    VfsIpcRequest, VfsIpcResponse, LINUX_STATX_SIZE, LINUX_STAT_SIZE,
};

use vfsd::linux_types::{LinuxStat, LinuxStatx, LinuxSyscallOffloadResponse};
use super::{DirEntry, Metadata, RemoteKind};
use super::{
    AT_FDCWD_U32, AT_FDCWD_U64, BOOT_DIRECTORY_MODE_BITS, BOOT_FILE_MODE_BITS, DEFAULT_BLOCK_SIZE,
    DEVICE_FILE_MODE_BITS, DT_CHR, DT_DIR, DT_LNK, DT_REG, DT_SOCK, EINVAL, S_IFLNK, S_IFSOCK,
};
use storage_fat::FatError;
pub(super) use vfsd::path::{normalize_absolute_path, path_inode};
use vfsd::storage_io_error_to_linux_errno;

const S_IFIFO: u32 = 0o010000;
const DT_FIFO: u8 = 1;

pub(super) fn is_at_fdcwd(dirfd: u64) -> bool {
    dirfd == AT_FDCWD_U64 || dirfd == AT_FDCWD_U32
}

pub(super) fn map_fat_error(err: FatError) -> i32 {
    match err {
        fatfs::Error::NotFound => super::ENOENT,
        fatfs::Error::InvalidInput => EINVAL,
        fatfs::Error::Io(error) => storage_io_error_to_linux_errno(error),
        _ => super::EIO,
    }
}

pub(super) fn handle_kind_u16(kind: RemoteKind) -> u16 {
    use rustos_user_abi::syscall::{
        VFS_IPC_HANDLE_KIND_DEVICE, VFS_IPC_HANDLE_KIND_DIR, VFS_IPC_HANDLE_KIND_FILE,
    };
    match kind {
        RemoteKind::File => VFS_IPC_HANDLE_KIND_FILE,
        RemoteKind::Directory => VFS_IPC_HANDLE_KIND_DIR,
        RemoteKind::Device => VFS_IPC_HANDLE_KIND_DEVICE,
        // Never a handle kind; unreachable for a published description.
        RemoteKind::Symlink | RemoteKind::Socket | RemoteKind::Fifo => VFS_IPC_HANDLE_KIND_FILE,
    }
}

fn timespec(t: storage_core::fs::FsTimestamp) -> vfsd::linux_types::LinuxTimespec {
    vfsd::linux_types::LinuxTimespec {
        tv_sec: t.secs,
        tv_nsec: i64::from(t.nanos),
    }
}

fn statx_timestamp(t: storage_core::fs::FsTimestamp) -> vfsd::linux_types::LinuxStatxTimestamp {
    vfsd::linux_types::LinuxStatxTimestamp {
        tv_sec: t.secs,
        tv_nsec: t.nanos,
        __reserved: 0,
    }
}

/// Full `st_mode` (type bits + permissions) of a node.
fn node_mode(metadata: &Metadata) -> u32 {
    match metadata.fs {
        Some(fs) => {
            let kind = match metadata.kind {
                RemoteKind::Directory => super::S_IFDIR,
                RemoteKind::Symlink => S_IFLNK,
                RemoteKind::Socket => S_IFSOCK,
                RemoteKind::Fifo => S_IFIFO,
                RemoteKind::File | RemoteKind::Device => super::S_IFREG,
            };
            kind | (fs.mode & 0o7777)
        }
        None => mode_bits(metadata.kind),
    }
}

pub(super) fn linux_makedev(major: u32, minor: u32) -> u64 {
    let major = u64::from(major);
    let minor = u64::from(minor);
    ((major & 0x0000_0fff) << 8)
        | ((major & 0xffff_f000) << 32)
        | (minor & 0x0000_00ff)
        | ((minor & 0xffff_ff00) << 12)
}

pub(super) fn device_rdev_for_path(path: &str) -> Option<(u32, u32)> {
    match path {
        "/dev/dri/card0" => Some((226, 0)),
        "/dev/null" => Some((1, 3)),
        "/dev/zero" => Some((1, 5)),
        "/dev/full" => Some((1, 7)),
        "/dev/random" => Some((1, 8)),
        "/dev/urandom" => Some((1, 9)),
        _ => None,
    }
}

pub(super) fn build_linux_stat(metadata: Metadata) -> [u8; LINUX_STAT_SIZE] {
    let mut stat = LinuxStat {
        st_dev: metadata.dev,
        st_ino: metadata.inode.max(1),
        st_nlink: if metadata.kind == RemoteKind::Directory {
            2
        } else {
            1
        },
        st_mode: node_mode(&metadata),
        st_rdev: metadata
            .rdev
            .map_or(0, |(major, minor)| linux_makedev(major, minor)),
        st_size: metadata.len.min(i64::MAX as u64) as i64,
        st_blksize: DEFAULT_BLOCK_SIZE as i64,
        st_blocks: metadata.len.div_ceil(512).min(i64::MAX as u64) as i64,
        ..LinuxStat::default()
    };
    if let Some(fs) = metadata.fs {
        stat.st_nlink = u64::from(fs.nlink);
        stat.st_uid = fs.uid;
        stat.st_gid = fs.gid;
        stat.st_blocks = (fs.allocation_size / 512).min(i64::MAX as u64) as i64;
        stat.st_atim = timespec(fs.atime);
        stat.st_mtim = timespec(fs.mtime);
        stat.st_ctim = timespec(fs.ctime);
    }
    let mut bytes = [0_u8; LINUX_STAT_SIZE];
    bytes.copy_from_slice(as_bytes(&stat));
    bytes
}

pub(super) fn build_linux_statx(metadata: Metadata) -> [u8; LINUX_STATX_SIZE] {
    let mut statx = LinuxStatx {
        stx_mask: 0x7ff,
        stx_blksize: DEFAULT_BLOCK_SIZE as u32,
        stx_nlink: if metadata.kind == RemoteKind::Directory {
            2
        } else {
            1
        },
        stx_mode: node_mode(&metadata) as u16,
        stx_ino: metadata.inode.max(1),
        stx_size: metadata.len,
        stx_blocks: metadata.len.div_ceil(512),
        stx_dev_minor: metadata.dev as u32,
        stx_rdev_major: metadata.rdev.map_or(0, |(major, _)| major),
        stx_rdev_minor: metadata.rdev.map_or(0, |(_, minor)| minor),
        stx_mnt_id: metadata.dev,
        ..LinuxStatx::default()
    };
    if let Some(fs) = metadata.fs {
        statx.stx_nlink = fs.nlink;
        statx.stx_uid = fs.uid;
        statx.stx_gid = fs.gid;
        statx.stx_blocks = fs.allocation_size / 512;
        statx.stx_atime = statx_timestamp(fs.atime);
        statx.stx_mtime = statx_timestamp(fs.mtime);
        statx.stx_ctime = statx_timestamp(fs.ctime);
        statx.stx_btime = statx_timestamp(fs.crtime);
    }
    let mut bytes = [0_u8; LINUX_STATX_SIZE];
    bytes.copy_from_slice(as_bytes(&statx));
    bytes
}

pub(super) fn mode_bits(kind: RemoteKind) -> u32 {
    match kind {
        RemoteKind::File => BOOT_FILE_MODE_BITS,
        RemoteKind::Directory => BOOT_DIRECTORY_MODE_BITS,
        RemoteKind::Device => DEVICE_FILE_MODE_BITS,
        RemoteKind::Symlink => S_IFLNK | 0o777,
        RemoteKind::Socket => S_IFSOCK | 0o755,
        RemoteKind::Fifo => S_IFIFO | 0o644,
    }
}

pub(super) fn write_payload_bytes(response: &mut LinuxSyscallOffloadResponse, bytes: &[u8]) {
    let len = bytes.len().min(response.payload.len());
    response.payload[..len].copy_from_slice(&bytes[..len]);
    response.payload_len = len as u32;
}

pub(super) fn write_vfs_payload_bytes(response: &mut VfsIpcResponse, bytes: &[u8]) {
    let len = bytes.len().min(response.payload.len());
    response.payload[..len].copy_from_slice(&bytes[..len]);
    response.payload_len = len as u32;
    response.value = len as u64;
}

pub(super) fn encode_dirent_into(
    entry: &DirEntry,
    next_offset: u64,
    dest: &mut [u8],
) -> Option<usize> {
    let base_len = 19 + entry.name.len() + 1;
    let record_len = (base_len + 7) & !7;
    if record_len > dest.len() {
        return None;
    }
    let out = &mut dest[..record_len];
    out[..8].copy_from_slice(&path_inode(entry.name.as_bytes()).to_le_bytes());
    out[8..16].copy_from_slice(&(next_offset as i64).to_le_bytes());
    out[16..18].copy_from_slice(&(record_len as u16).to_le_bytes());
    out[18] = match entry.kind {
        RemoteKind::File => DT_REG,
        RemoteKind::Directory => DT_DIR,
        RemoteKind::Device => DT_CHR,
        RemoteKind::Symlink => DT_LNK,
        RemoteKind::Socket => DT_SOCK,
        RemoteKind::Fifo => DT_FIFO,
    };
    out[19..19 + entry.name.len()].copy_from_slice(entry.name.as_bytes());
    out[19 + entry.name.len()..record_len].fill(0);
    Some(record_len)
}

pub(super) fn linux_request_path(
    request: &vfsd::linux_types::LinuxSyscallOffloadRequest,
) -> Option<&str> {
    let len = request.path_len as usize;
    if len > request.path.len() {
        return None;
    }
    core::str::from_utf8(&request.path[..len]).ok()
}

pub(super) fn vfs_request_path(request: &VfsIpcRequest) -> Option<&str> {
    let len = request.path_len as usize;
    if len > request.path.len() {
        return None;
    }
    core::str::from_utf8(&request.path[..len]).ok()
}

pub(super) fn as_bytes<T>(value: &T) -> &[u8] {
    unsafe { core::slice::from_raw_parts((value as *const T).cast::<u8>(), size_of::<T>()) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn device_rdev_and_stat_encoding_match_linux() {
        assert_eq!(device_rdev_for_path("/dev/dri/card0"), Some((226, 0)));
        assert_eq!(device_rdev_for_path("/dev/null"), Some((1, 3)));
        assert_eq!(device_rdev_for_path("/dev/zero"), Some((1, 5)));
        assert_eq!(device_rdev_for_path("/dev/full"), Some((1, 7)));
        assert_eq!(device_rdev_for_path("/dev/random"), Some((1, 8)));
        assert_eq!(device_rdev_for_path("/dev/urandom"), Some((1, 9)));
        assert_eq!(device_rdev_for_path("/dev/unknown"), None);

        let mut meta = Metadata::plain(RemoteKind::Device, 0, 42);
        meta.rdev = device_rdev_for_path("/dev/dri/card0");

        let stat_bytes = build_linux_stat(meta);
        let stat: LinuxStat =
            unsafe { core::ptr::read_unaligned(stat_bytes.as_ptr().cast::<LinuxStat>()) };
        assert_eq!(stat.st_rdev, linux_makedev(226, 0));
        assert_eq!(stat.st_rdev, 0xe200);

        let statx_bytes = build_linux_statx(meta);
        let statx: LinuxStatx =
            unsafe { core::ptr::read_unaligned(statx_bytes.as_ptr().cast::<LinuxStatx>()) };
        assert_eq!((statx.stx_rdev_major, statx.stx_rdev_minor), (226, 0));
    }
}
