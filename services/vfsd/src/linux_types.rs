use rustos_user_abi::syscall::{
    VfsIpcRequest, SYSCALL_OFFLOAD_ABI_VERSION, SYSCALL_OFFLOAD_OP_LINUX_STATX,
    SYSCALL_OFFLOAD_PATH_CAPACITY, SYSCALL_OFFLOAD_PAYLOAD_CAPACITY, VFS_IPC_ABI_VERSION,
    VFS_IPC_PATH_CAPACITY, VFS_IPC_REQUEST_PAYLOAD_CAPACITY,
};

use super::EINVAL;

#[repr(C)]
#[derive(Clone, Copy, Default)]
pub(crate) struct LinuxTimespec {
    pub(crate) tv_sec: i64,
    pub(crate) tv_nsec: i64,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
pub(crate) struct LinuxStat {
    pub(crate) st_dev: u64,
    pub(crate) st_ino: u64,
    pub(crate) st_nlink: u64,
    pub(crate) st_mode: u32,
    pub(crate) st_uid: u32,
    pub(crate) st_gid: u32,
    pub(crate) __pad0: u32,
    pub(crate) st_rdev: u64,
    pub(crate) st_size: i64,
    pub(crate) st_blksize: i64,
    pub(crate) st_blocks: i64,
    pub(crate) st_atim: LinuxTimespec,
    pub(crate) st_mtim: LinuxTimespec,
    pub(crate) st_ctim: LinuxTimespec,
    pub(crate) __glibc_reserved: [i64; 3],
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
pub(crate) struct LinuxStatxTimestamp {
    pub(crate) tv_sec: i64,
    pub(crate) tv_nsec: u32,
    pub(crate) __reserved: i32,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
pub(crate) struct LinuxStatx {
    pub(crate) stx_mask: u32,
    pub(crate) stx_blksize: u32,
    pub(crate) stx_attributes: u64,
    pub(crate) stx_nlink: u32,
    pub(crate) stx_uid: u32,
    pub(crate) stx_gid: u32,
    pub(crate) stx_mode: u16,
    pub(crate) __spare0: [u16; 1],
    pub(crate) stx_ino: u64,
    pub(crate) stx_size: u64,
    pub(crate) stx_blocks: u64,
    pub(crate) stx_attributes_mask: u64,
    pub(crate) stx_atime: LinuxStatxTimestamp,
    pub(crate) stx_btime: LinuxStatxTimestamp,
    pub(crate) stx_ctime: LinuxStatxTimestamp,
    pub(crate) stx_mtime: LinuxStatxTimestamp,
    pub(crate) stx_rdev_major: u32,
    pub(crate) stx_rdev_minor: u32,
    pub(crate) stx_dev_major: u32,
    pub(crate) stx_dev_minor: u32,
    pub(crate) stx_mnt_id: u64,
    pub(crate) stx_dio_mem_align: u32,
    pub(crate) stx_dio_offset_align: u32,
    pub(crate) __spare3: [u64; 12],
}

#[repr(C)]
#[derive(Clone, Copy)]
pub(crate) struct LinuxSyscallOffloadRequest {
    pub(crate) version: u16,
    pub(crate) op: u16,
    pub(crate) reserved0: u32,
    pub(crate) pid: u64,
    pub(crate) tid: u64,
    pub(crate) parent_pid: u64,
    pub(crate) session_handle: u64,
    pub(crate) uid: u32,
    pub(crate) gid: u32,
    pub(crate) euid: u32,
    pub(crate) egid: u32,
    pub(crate) dirfd: u64,
    pub(crate) flags: u64,
    pub(crate) arg0: u64,
    pub(crate) arg1: u64,
    pub(crate) arg2: u64,
    pub(crate) arg3: u64,
    pub(crate) mask: u32,
    pub(crate) path_len: u32,
    pub(crate) path: [u8; SYSCALL_OFFLOAD_PATH_CAPACITY],
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
pub(crate) struct LinuxSyscallOffloadResponse {
    pub(crate) version: u16,
    pub(crate) op: u16,
    pub(crate) status: i32,
    pub(crate) payload_len: u32,
    pub(crate) reserved0: u32,
    pub(crate) payload: [u8; SYSCALL_OFFLOAD_PAYLOAD_CAPACITY],
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

pub(crate) fn validate_linux_request(request: &LinuxSyscallOffloadRequest) -> Result<(), i32> {
    if request.version != SYSCALL_OFFLOAD_ABI_VERSION
        || request.reserved0 != 0
        || request.path_len as usize > SYSCALL_OFFLOAD_PATH_CAPACITY
    {
        return Err(EINVAL);
    }
    Ok(())
}

pub(crate) fn validate_vfs_request(request: &VfsIpcRequest) -> Result<(), i32> {
    if request.version != VFS_IPC_ABI_VERSION
        || (request.operation_hi == 0 && request.operation_lo == 0)
        || request.path_len as usize > VFS_IPC_PATH_CAPACITY
        || request.payload_len as usize > VFS_IPC_REQUEST_PAYLOAD_CAPACITY
    {
        return Err(EINVAL);
    }
    Ok(())
}
