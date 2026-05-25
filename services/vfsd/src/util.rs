use alloc::format;
use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;
use core::mem::size_of;

use rustos_user_abi::syscall::{
    LINUX_STAT_SIZE, LINUX_STATX_SIZE, VFS_IPC_PATH_CAPACITY, VfsIpcRequest, VfsIpcResponse,
};

use super::{DirEntry, Metadata, RemoteKind};
use super::{
    AT_FDCWD_U32, AT_FDCWD_U64, BOOT_DIRECTORY_MODE_BITS, BOOT_FILE_MODE_BITS,
    DEFAULT_BLOCK_SIZE, DEVICE_FILE_MODE_BITS, DT_CHR, DT_DIR, DT_REG, EINVAL, EROFS,
};
use super::linux_types::{LinuxStat, LinuxStatx, LinuxSyscallOffloadResponse};
use storage_fat::FatError;
use storage_core::StorageError;

pub(super) fn normalize_absolute_path(base_path: &str, path: &str) -> Result<String, i32> {
    let mut components: Vec<&str> = Vec::new();
    if !path.starts_with('/') {
        for component in base_path.split('/') {
            if !component.is_empty() {
                components.push(component);
            }
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
    if normalized.len() > VFS_IPC_PATH_CAPACITY {
        return Err(EINVAL);
    }
    Ok(normalized)
}

pub(super) fn is_at_fdcwd(dirfd: u64) -> bool {
    dirfd == AT_FDCWD_U64 || dirfd == AT_FDCWD_U32
}

pub(super) fn map_fat_error(err: FatError) -> i32 {
    match err {
        fatfs::Error::NotFound => super::ENOENT,
        fatfs::Error::InvalidInput => EINVAL,
        fatfs::Error::Io(StorageError::NotPresent) => super::ENODEV,
        fatfs::Error::Io(StorageError::InvalidInput) => EINVAL,
        fatfs::Error::Io(_) => super::EIO,
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
    }
}

pub(super) fn build_linux_stat(metadata: Metadata) -> [u8; LINUX_STAT_SIZE] {
    let stat = LinuxStat {
        st_ino: metadata.inode.max(1),
        st_nlink: if metadata.kind == RemoteKind::Directory {
            2
        } else {
            1
        },
        st_mode: mode_bits(metadata.kind),
        st_size: metadata.len.min(i64::MAX as u64) as i64,
        st_blksize: DEFAULT_BLOCK_SIZE as i64,
        st_blocks: metadata.len.div_ceil(512).min(i64::MAX as u64) as i64,
        ..LinuxStat::default()
    };
    let mut bytes = [0_u8; LINUX_STAT_SIZE];
    bytes.copy_from_slice(as_bytes(&stat));
    bytes
}

pub(super) fn build_linux_statx(metadata: Metadata) -> [u8; LINUX_STATX_SIZE] {
    let statx = LinuxStatx {
        stx_mask: 0x7ff,
        stx_blksize: DEFAULT_BLOCK_SIZE as u32,
        stx_nlink: if metadata.kind == RemoteKind::Directory {
            2
        } else {
            1
        },
        stx_mode: mode_bits(metadata.kind) as u16,
        stx_ino: metadata.inode.max(1),
        stx_size: metadata.len,
        stx_blocks: metadata.len.div_ceil(512),
        ..LinuxStatx::default()
    };
    let mut bytes = [0_u8; LINUX_STATX_SIZE];
    bytes.copy_from_slice(as_bytes(&statx));
    bytes
}

pub(super) fn mode_bits(kind: RemoteKind) -> u32 {
    match kind {
        RemoteKind::File => BOOT_FILE_MODE_BITS,
        RemoteKind::Directory => BOOT_DIRECTORY_MODE_BITS,
        RemoteKind::Device => DEVICE_FILE_MODE_BITS,
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

pub(super) fn encode_dirent(entry: &DirEntry, next_offset: usize) -> Vec<u8> {
    let base_len = 19 + entry.name.len() + 1;
    let record_len = (base_len + 7) & !7;
    let mut bytes = vec![0_u8; record_len];
    bytes[..8].copy_from_slice(&path_inode(entry.name.as_bytes()).to_le_bytes());
    bytes[8..16].copy_from_slice(&(next_offset as i64).to_le_bytes());
    bytes[16..18].copy_from_slice(&(record_len as u16).to_le_bytes());
    bytes[18] = match entry.kind {
        RemoteKind::File => DT_REG,
        RemoteKind::Directory => DT_DIR,
        RemoteKind::Device => DT_CHR,
    };
    bytes[19..19 + entry.name.len()].copy_from_slice(entry.name.as_bytes());
    bytes[19 + entry.name.len()] = 0;
    bytes
}

pub(super) fn path_inode(bytes: &[u8]) -> u64 {
    let mut hash = 0xcbf2_9ce4_8422_2325_u64;
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x1000_0000_01b3);
    }
    hash.max(1)
}

pub(super) fn linux_request_path(
    request: &super::linux_types::LinuxSyscallOffloadRequest,
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

pub(super) fn mkdir_policy(path: &str, euid: u32) -> i32 {
    let run_user_path = format!("/run/user/{euid}");
    if path == "/run" || path == "/run/user" || path == run_user_path.as_str() {
        0
    } else {
        EROFS
    }
}

pub(super) fn unlink_policy(path: &str) -> i32 {
    if path.starts_with("/run/") {
        super::ENOENT
    } else {
        EROFS
    }
}

pub(super) fn read_unaligned<T: Copy + Default>(bytes: &[u8]) -> T {
    let mut value = T::default();
    let dest = unsafe {
        core::slice::from_raw_parts_mut((&mut value as *mut T).cast::<u8>(), size_of::<T>())
    };
    dest.copy_from_slice(&bytes[..size_of::<T>()]);
    value
}

pub(super) fn as_bytes<T>(value: &T) -> &[u8] {
    unsafe { core::slice::from_raw_parts((value as *const T).cast::<u8>(), size_of::<T>()) }
}
