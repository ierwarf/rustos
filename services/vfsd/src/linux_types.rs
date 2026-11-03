use rustos_user_abi::syscall::{
    VfsIpcRequest, SYSCALL_OFFLOAD_ABI_VERSION, SYSCALL_OFFLOAD_OP_LINUX_STATX,
    SYSCALL_OFFLOAD_PATH_CAPACITY, SYSCALL_OFFLOAD_PAYLOAD_CAPACITY, VFS_IPC_ABI_VERSION,
    VFS_IPC_PATH_CAPACITY, VFS_IPC_REQUEST_PAYLOAD_CAPACITY,
};

use crate::EINVAL;

#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct LinuxTimespec {
    pub tv_sec: i64,
    pub tv_nsec: i64,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct LinuxStat {
    pub st_dev: u64,
    pub st_ino: u64,
    pub st_nlink: u64,
    pub st_mode: u32,
    pub st_uid: u32,
    pub st_gid: u32,
    pub __pad0: u32,
    pub st_rdev: u64,
    pub st_size: i64,
    pub st_blksize: i64,
    pub st_blocks: i64,
    pub st_atim: LinuxTimespec,
    pub st_mtim: LinuxTimespec,
    pub st_ctim: LinuxTimespec,
    pub __glibc_reserved: [i64; 3],
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct LinuxStatxTimestamp {
    pub tv_sec: i64,
    pub tv_nsec: u32,
    pub __reserved: i32,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct LinuxStatx {
    pub stx_mask: u32,
    pub stx_blksize: u32,
    pub stx_attributes: u64,
    pub stx_nlink: u32,
    pub stx_uid: u32,
    pub stx_gid: u32,
    pub stx_mode: u16,
    pub __spare0: [u16; 1],
    pub stx_ino: u64,
    pub stx_size: u64,
    pub stx_blocks: u64,
    pub stx_attributes_mask: u64,
    pub stx_atime: LinuxStatxTimestamp,
    pub stx_btime: LinuxStatxTimestamp,
    pub stx_ctime: LinuxStatxTimestamp,
    pub stx_mtime: LinuxStatxTimestamp,
    pub stx_rdev_major: u32,
    pub stx_rdev_minor: u32,
    pub stx_dev_major: u32,
    pub stx_dev_minor: u32,
    pub stx_mnt_id: u64,
    pub stx_dio_mem_align: u32,
    pub stx_dio_offset_align: u32,
    pub __spare3: [u64; 12],
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct LinuxSyscallOffloadRequest {
    pub version: u16,
    pub op: u16,
    pub reserved0: u32,
    pub pid: u64,
    pub tid: u64,
    pub parent_pid: u64,
    pub session_handle: u64,
    pub uid: u32,
    pub gid: u32,
    pub euid: u32,
    pub egid: u32,
    pub dirfd: u64,
    pub flags: u64,
    pub arg0: u64,
    pub arg1: u64,
    pub arg2: u64,
    pub arg3: u64,
    pub mask: u32,
    pub path_len: u32,
    pub path: [u8; SYSCALL_OFFLOAD_PATH_CAPACITY],
}

impl Default for LinuxSyscallOffloadRequest {
    fn default() -> Self {
        Self {
            version: SYSCALL_OFFLOAD_ABI_VERSION,
            op: SYSCALL_OFFLOAD_OP_LINUX_STATX,
            reserved0: 0,
            pid: 0,
            tid: 0,
            parent_pid: 0,
            session_handle: 0,
            uid: 0,
            gid: 0,
            euid: 0,
            egid: 0,
            dirfd: 0,
            flags: 0,
            arg0: 0,
            arg1: 0,
            arg2: 0,
            arg3: 0,
            mask: 0,
            path_len: 0,
            path: [0; SYSCALL_OFFLOAD_PATH_CAPACITY],
        }
    }
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct LinuxSyscallOffloadResponse {
    pub version: u16,
    pub op: u16,
    pub status: i32,
    pub payload_len: u32,
    pub reserved0: u32,
    pub payload: [u8; SYSCALL_OFFLOAD_PAYLOAD_CAPACITY],
}

impl Default for LinuxSyscallOffloadResponse {
    fn default() -> Self {
        Self {
            version: SYSCALL_OFFLOAD_ABI_VERSION,
            op: SYSCALL_OFFLOAD_OP_LINUX_STATX,
            status: 0,
            payload_len: 0,
            reserved0: 0,
            payload: [0; SYSCALL_OFFLOAD_PAYLOAD_CAPACITY],
        }
    }
}

pub fn validate_linux_request(request: &LinuxSyscallOffloadRequest) -> Result<(), i32> {
    if request.version != SYSCALL_OFFLOAD_ABI_VERSION
        || request.reserved0 != 0
        || request.path_len as usize > SYSCALL_OFFLOAD_PATH_CAPACITY
    {
        return Err(EINVAL);
    }
    Ok(())
}

pub fn validate_vfs_request(request: &VfsIpcRequest) -> Result<(), i32> {
    if request.version != VFS_IPC_ABI_VERSION
        || (request.operation_hi == 0 && request.operation_lo == 0)
        || request.path_len as usize > VFS_IPC_PATH_CAPACITY
        || request.payload_len as usize > VFS_IPC_REQUEST_PAYLOAD_CAPACITY
    {
        return Err(EINVAL);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::mem::size_of;
    use rustos_user_abi::syscall::{VfsIpcResponse, IPC_MAX_INLINE_BYTES};

    #[test]
    fn vfs_requests_validate_inline_shape() {
        let request = VfsIpcRequest {
            operation_lo: 1,
            ..VfsIpcRequest::default()
        };
        assert_eq!(validate_vfs_request(&request), Ok(()));
        assert!(size_of::<VfsIpcRequest>() <= IPC_MAX_INLINE_BYTES);
        assert_eq!(size_of::<VfsIpcResponse>(), IPC_MAX_INLINE_BYTES);
        assert_eq!(validate_vfs_request(&VfsIpcRequest::default()), Err(EINVAL));
    }
}
