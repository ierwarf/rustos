#![no_std]
#![no_main]

extern crate alloc;

use alloc::collections::BTreeMap;
use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec;
use alloc::vec::Vec;
use core::mem::size_of;
use core::str;

use rustos_svc_runtime::ipc;
use rustos_user_abi::syscall::{
    CommercialMaxCapabilityLeaseWire, CommercialMaxProtocolDescriptorWire,
    CommercialMaxProtocolRequest, CommercialMaxProtocolResponse, DevmgrdIpcRequest,
    DevmgrdIpcResponse, RustosBlockBrokerArgs, VfsIpcRequest, VfsIpcResponse,
    BLOCK_BROKER_ABI_VERSION, BLOCK_BROKER_MAX_IO_BYTES, BLOCK_BROKER_OP_BOOT_INFO,
    BLOCK_BROKER_OP_BOOT_READ, COMMERCIAL_MAX_PROTOCOL_ABI_VERSION,
    COMMERCIAL_MAX_PROTOCOL_MAX_DESCRIPTORS, COMMERCIAL_MAX_PROTOCOL_VFSD,
    COMMERCIAL_MAX_VFSD_OP_DIRECTORY_CURSOR, COMMERCIAL_MAX_VFSD_OP_FD_TABLE_PLAN,
    COMMERCIAL_MAX_VFSD_OP_FILE_CURSOR, COMMERCIAL_MAX_VFSD_OP_METADATA_POLICY,
    COMMERCIAL_MAX_VFSD_OP_MOUNT_GRAPH, COMMERCIAL_MAX_VFSD_OP_PATH_RESOLVE,
    DEVMGRD_IPC_ABI_VERSION, DEVMGRD_IPC_OP_LOOKUP, DEVMGRD_IPC_OP_READDIR,
    DEVMGRD_MAX_DIR_ENTRIES, DEVMGRD_NODE_KIND_DEVICE, DEVMGRD_NODE_KIND_DIR, IPC_MAX_INLINE_BYTES,
    IPC_SERVICE_DEVMGRD, IPC_SERVICE_VFSD, LINUX_STATX_SIZE, LINUX_STAT_SIZE,
    SYSCALL_OFFLOAD_ABI_VERSION, SYSCALL_OFFLOAD_OP_LINUX_ACCESS, SYSCALL_OFFLOAD_OP_LINUX_CHDIR,
    SYSCALL_OFFLOAD_OP_LINUX_CLOSE, SYSCALL_OFFLOAD_OP_LINUX_DUP, SYSCALL_OFFLOAD_OP_LINUX_FCNTL,
    SYSCALL_OFFLOAD_OP_LINUX_GETCWD, SYSCALL_OFFLOAD_OP_LINUX_GETDENTS64,
    SYSCALL_OFFLOAD_OP_LINUX_MKDIR, SYSCALL_OFFLOAD_OP_LINUX_MOUNT,
    SYSCALL_OFFLOAD_OP_LINUX_NEWFSTATAT, SYSCALL_OFFLOAD_OP_LINUX_OPENAT,
    SYSCALL_OFFLOAD_OP_LINUX_READLINKAT, SYSCALL_OFFLOAD_OP_LINUX_STATX,
    SYSCALL_OFFLOAD_OP_LINUX_UMOUNT2, SYSCALL_OFFLOAD_OP_LINUX_UNLINKAT,
    SYSCALL_OFFLOAD_PATH_CAPACITY, SYSCALL_OFFLOAD_PAYLOAD_CAPACITY, SYS_RUSTOS_BLOCK_BROKER,
    VFS_IPC_ABI_VERSION, VFS_IPC_HANDLE_KIND_DEVICE, VFS_IPC_HANDLE_KIND_DIR,
    VFS_IPC_HANDLE_KIND_FILE, VFS_IPC_OP_ACCESS, VFS_IPC_OP_CHDIR, VFS_IPC_OP_CLOSE,
    VFS_IPC_OP_DUP, VFS_IPC_OP_FCNTL, VFS_IPC_OP_FSTAT, VFS_IPC_OP_FTRUNCATE, VFS_IPC_OP_GETCWD,
    VFS_IPC_OP_GETDENTS64, VFS_IPC_OP_LSEEK, VFS_IPC_OP_MKDIR, VFS_IPC_OP_MOUNT,
    VFS_IPC_OP_NEWFSTATAT, VFS_IPC_OP_OPENAT, VFS_IPC_OP_PREAD64, VFS_IPC_OP_READ,
    VFS_IPC_OP_READLINKAT, VFS_IPC_OP_STATX, VFS_IPC_OP_UMOUNT2, VFS_IPC_OP_UNLINKAT,
    VFS_IPC_OP_WRITE, VFS_IPC_PATH_CAPACITY, VFS_IPC_PAYLOAD_CAPACITY,
    VFS_IPC_REQUEST_PAYLOAD_CAPACITY,
};
use storage_core::{BlockDevice, IoResult, StorageError};
use storage_fat::{FatDirEntry, FatNodeKind, FatVolume};

// Linux errno constants (x86_64)
const ENOENT: i32 = 2;
const EIO: i32 = 5;
const EAGAIN: i32 = 11;
const EBADF: i32 = 9;
const ENODEV: i32 = 19;
const ENOTDIR: i32 = 20;
const EISDIR: i32 = 21;
const EINVAL: i32 = 22;
const EROFS: i32 = 30;

// Linux lseek whence constants
const SEEK_SET: u64 = 0;
const SEEK_CUR: u64 = 1;
const SEEK_END: u64 = 2;

// Linux open flags (x86_64)
const O_CREAT: u64 = 0o100;
const O_TRUNC: u64 = 0o1000;
const O_DIRECTORY: u64 = 0o200000;

// AT_FDCWD as u64 (represents -100 in twos complement, or 0xFFFF_FFFF_FFFF_FF9C)
const AT_FDCWD_U64: u64 = (-100_i64) as u64;
// Also allow the truncated u32 form
const AT_FDCWD_U32: u64 = 0xFFFF_FF9C;

const DEFAULT_BLOCK_SIZE: u64 = 4096;
const DT_REG: u8 = 8;
const DT_DIR: u8 = 4;
const DT_CHR: u8 = 2;
const S_IFREG: u32 = 0o100000;
const S_IFDIR: u32 = 0o040000;
const S_IFCHR: u32 = 0o020000;
const BOOT_FILE_MODE_BITS: u32 = S_IFREG | 0o444;
const BOOT_DIRECTORY_MODE_BITS: u32 = S_IFDIR | 0o555;
const DEVICE_FILE_MODE_BITS: u32 = S_IFCHR | 0o600;

rustos_svc_runtime::entry!(service_main);

fn service_main() {
    let endpoint = ipc::endpoint_create();
    if endpoint < 0 {
        ipc::debug_line("vfsd: endpoint create failed");
        return;
    }

    let register = ipc::register_service_endpoint(IPC_SERVICE_VFSD, endpoint as u64);
    if register < 0 {
        ipc::debug_line("vfsd: endpoint register failed");
        return;
    }
    ipc::debug_line("vfsd: vfs policy endpoint registered");
    serve(endpoint as u64);
}

fn serve(endpoint: u64) {
    let mut state = VfsState::new();
    loop {
        let mut request = [0_u8; IPC_MAX_INLINE_BYTES];
        let mut reply_cap = 0_u64;
        let received = unsafe {
            ipc::recv(
                endpoint,
                request.as_mut_ptr(),
                request.len(),
                &mut reply_cap as *mut u64,
            )
        };
        if received < 0 {
            rustos_svc_runtime::syscall::sleep_millis(1);
            continue;
        }

        let reply = if received as usize == size_of::<VfsIpcRequest>() {
            let request = read_unaligned::<VfsIpcRequest>(&request);
            let response = state.handle_vfs_request(&request);
            unsafe {
                ipc::reply(
                    reply_cap,
                    (&response as *const VfsIpcResponse).cast::<u8>(),
                    size_of::<VfsIpcResponse>(),
                )
            }
        } else if received as usize == size_of::<CommercialMaxProtocolRequest>() {
            let request = read_unaligned::<CommercialMaxProtocolRequest>(&request);
            let response = state.handle_commercial_request(&request);
            unsafe {
                ipc::reply(
                    reply_cap,
                    (&response as *const CommercialMaxProtocolResponse).cast::<u8>(),
                    size_of::<CommercialMaxProtocolResponse>(),
                )
            }
        } else if received as usize == size_of::<LinuxSyscallOffloadRequest>() {
            let request = read_unaligned::<LinuxSyscallOffloadRequest>(&request);
            let response = state.handle_linux_request(&request);
            unsafe {
                ipc::reply(
                    reply_cap,
                    (&response as *const LinuxSyscallOffloadResponse).cast::<u8>(),
                    size_of::<LinuxSyscallOffloadResponse>(),
                )
            }
        } else {
            let response = LinuxSyscallOffloadResponse {
                status: EINVAL,
                ..LinuxSyscallOffloadResponse::default()
            };
            unsafe {
                ipc::reply(
                    reply_cap,
                    (&response as *const LinuxSyscallOffloadResponse).cast::<u8>(),
                    size_of::<LinuxSyscallOffloadResponse>(),
                )
            }
        };
        if reply < 0 {
            ipc::debug_line("vfsd: reply failed");
        }
    }
}

struct VfsState {
    volume: Option<FatVolume<BootBlockDevice>>,
    cwd: BTreeMap<u64, String>,
    handles: BTreeMap<u64, RemoteHandle>,
    file_cache: BTreeMap<String, Vec<u8>>,
    /// Positive + negative metadata cache. `Ok(_)` is a resolved entry;
    /// `Err(errno)` is a negative cache (e.g. ENOENT) so back-to-back stat()s
    /// of common missing libc paths return without touching FAT. The whole map
    /// is dropped whenever `mount_generation` changes.
    metadata_cache: BTreeMap<String, Result<Metadata, i32>>,
    /// Cached directory listings, keyed by absolute path. Linux startup
    /// re-reads `/`, `/dev`, library directories, etc.; FAT traversal per call
    /// is expensive enough to dominate boot time when libc walks PATH.
    dir_entries_cache: BTreeMap<String, Vec<DirEntry>>,
    next_handle: u64,
    mount_generation: u64,
    cache_generation: u64,
}

#[derive(Clone)]
struct RemoteHandle {
    kind: RemoteKind,
    path: String,
    cursor: u64,
    len: u64,
    refs: u64,
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum RemoteKind {
    File,
    Directory,
    Device,
}

#[derive(Clone, Copy)]
struct Metadata {
    kind: RemoteKind,
    len: u64,
    inode: u64,
}

impl VfsState {
    fn new() -> Self {
        Self {
            volume: None,
            cwd: BTreeMap::new(),
            handles: BTreeMap::new(),
            file_cache: BTreeMap::new(),
            metadata_cache: BTreeMap::new(),
            dir_entries_cache: BTreeMap::new(),
            next_handle: 1,
            mount_generation: 1,
            cache_generation: 1,
        }
    }

    fn invalidate_caches_if_remounted(&mut self) {
        if self.cache_generation != self.mount_generation {
            self.metadata_cache.clear();
            self.dir_entries_cache.clear();
            self.file_cache.clear();
            self.cache_generation = self.mount_generation;
        }
    }

    fn handle_linux_request(
        &mut self,
        request: &LinuxSyscallOffloadRequest,
    ) -> LinuxSyscallOffloadResponse {
        let mut response = LinuxSyscallOffloadResponse {
            op: request.op,
            ..LinuxSyscallOffloadResponse::default()
        };
        if let Err(errno) = validate_linux_request(request) {
            response.status = errno;
            return response;
        }
        match request.op {
            SYSCALL_OFFLOAD_OP_LINUX_STATX => self.linux_statx(request, &mut response),
            SYSCALL_OFFLOAD_OP_LINUX_NEWFSTATAT => self.linux_newfstatat(request, &mut response),
            SYSCALL_OFFLOAD_OP_LINUX_READLINKAT => self.linux_readlinkat(&mut response),
            SYSCALL_OFFLOAD_OP_LINUX_ACCESS => self.linux_access(request, &mut response),
            SYSCALL_OFFLOAD_OP_LINUX_GETCWD => self.linux_getcwd(request, &mut response),
            SYSCALL_OFFLOAD_OP_LINUX_CHDIR => self.linux_chdir(request, &mut response),
            SYSCALL_OFFLOAD_OP_LINUX_MKDIR => self.linux_mkdir(request, &mut response),
            SYSCALL_OFFLOAD_OP_LINUX_OPENAT => self.linux_openat(request, &mut response),
            SYSCALL_OFFLOAD_OP_LINUX_GETDENTS64 => self.linux_getdents64(request, &mut response),
            SYSCALL_OFFLOAD_OP_LINUX_CLOSE => self.linux_close(request, &mut response),
            SYSCALL_OFFLOAD_OP_LINUX_DUP => self.linux_dup(request, &mut response),
            SYSCALL_OFFLOAD_OP_LINUX_FCNTL => self.linux_fcntl(&mut response),
            SYSCALL_OFFLOAD_OP_LINUX_MOUNT => self.linux_mount(&mut response),
            SYSCALL_OFFLOAD_OP_LINUX_UMOUNT2 => self.linux_umount2(&mut response),
            SYSCALL_OFFLOAD_OP_LINUX_UNLINKAT => self.linux_unlinkat(request, &mut response),
            _ => response.status = EINVAL,
        }
        response
    }

    fn handle_commercial_request(
        &mut self,
        request: &CommercialMaxProtocolRequest,
    ) -> CommercialMaxProtocolResponse {
        let mut response = CommercialMaxProtocolResponse {
            header: request.header,
            ..CommercialMaxProtocolResponse::default()
        };
        response.header.version = COMMERCIAL_MAX_PROTOCOL_ABI_VERSION;
        if let Err(errno) = validate_commercial_request(request) {
            response.status = errno;
            return response;
        }
        match request.header.op {
            COMMERCIAL_MAX_VFSD_OP_MOUNT_GRAPH => {
                response.value0 = self.mount_generation;
                response.value1 = u64::from(self.volume.is_some());
                response.descriptor_count = 1;
                response.descriptors[0] =
                    vfs_descriptor("mount-graph", request.header.op, self.mount_generation, 0);
            }
            COMMERCIAL_MAX_VFSD_OP_PATH_RESOLVE => {
                let path = commercial_request_path(request);
                if let Some(path) = path {
                    match self.metadata(path) {
                        Ok(metadata) => {
                            response.value0 = metadata.inode;
                            response.value1 = metadata.len;
                            response.capability = vfs_capability("path-resolve", request.header.op);
                            response.descriptor_count = 1;
                            response.descriptors[0] = vfs_descriptor(
                                "path-resolve",
                                request.header.op,
                                metadata.inode,
                                metadata.len,
                            );
                        }
                        Err(errno) => response.status = errno,
                    }
                } else {
                    response.status = EINVAL;
                }
            }
            COMMERCIAL_MAX_VFSD_OP_FD_TABLE_PLAN => {
                response.value0 = self.handles.len() as u64;
                response.value1 = self.next_handle;
                response.descriptor_count = 1;
                response.descriptors[0] =
                    vfs_descriptor("fd-table", request.header.op, self.handles.len() as u64, 0);
            }
            COMMERCIAL_MAX_VFSD_OP_DIRECTORY_CURSOR => {
                fill_handle_descriptors(self, &mut response, RemoteKind::Directory);
            }
            COMMERCIAL_MAX_VFSD_OP_FILE_CURSOR => {
                fill_handle_descriptors(self, &mut response, RemoteKind::File);
            }
            COMMERCIAL_MAX_VFSD_OP_METADATA_POLICY => {
                response.value0 = self.metadata_cache.len() as u64;
                response.value1 = self.dir_entries_cache.len() as u64;
                response.capability = vfs_capability("metadata-policy", request.header.op);
                response.descriptor_count = 1;
                response.descriptors[0] = vfs_descriptor(
                    "metadata-policy",
                    request.header.op,
                    self.metadata_cache.len() as u64,
                    self.dir_entries_cache.len() as u64,
                );
            }
            _ => response.status = EINVAL,
        }
        response
    }

    fn handle_vfs_request(&mut self, request: &VfsIpcRequest) -> VfsIpcResponse {
        let mut response = VfsIpcResponse {
            op: request.op,
            ..VfsIpcResponse::default()
        };
        if let Err(errno) = validate_vfs_request(request) {
            response.status = errno;
            return response;
        }
        match request.op {
            VFS_IPC_OP_OPENAT => self.vfs_openat(request, &mut response),
            VFS_IPC_OP_CLOSE => self.vfs_close(request, &mut response),
            VFS_IPC_OP_DUP => self.vfs_dup(request, &mut response),
            VFS_IPC_OP_READ => self.vfs_read(request, &mut response, None),
            VFS_IPC_OP_PREAD64 => self.vfs_read(request, &mut response, Some(request.arg0)),
            VFS_IPC_OP_WRITE => response.status = EROFS,
            VFS_IPC_OP_LSEEK => self.vfs_lseek(request, &mut response),
            VFS_IPC_OP_FSTAT => self.vfs_fstat(request, &mut response),
            VFS_IPC_OP_FTRUNCATE => response.status = EROFS,
            VFS_IPC_OP_GETDENTS64 => self.vfs_getdents64(request, &mut response),
            VFS_IPC_OP_FCNTL => self.vfs_fcntl(&mut response),
            VFS_IPC_OP_STATX => self.vfs_path_statx(request, &mut response),
            VFS_IPC_OP_NEWFSTATAT => self.vfs_path_stat(request, &mut response),
            VFS_IPC_OP_READLINKAT => response.status = ENOENT,
            VFS_IPC_OP_ACCESS => self.vfs_access(request, &mut response),
            VFS_IPC_OP_GETCWD => self.vfs_getcwd(request, &mut response),
            VFS_IPC_OP_CHDIR => self.vfs_chdir(request, &mut response),
            VFS_IPC_OP_MKDIR => self.vfs_mkdir(request, &mut response),
            VFS_IPC_OP_MOUNT => self.linux_mount_vfs(&mut response),
            VFS_IPC_OP_UMOUNT2 => self.linux_umount_vfs(&mut response),
            VFS_IPC_OP_UNLINKAT => self.vfs_unlinkat(request, &mut response),
            _ => response.status = EINVAL,
        }
        response
    }

    fn linux_statx(
        &mut self,
        request: &LinuxSyscallOffloadRequest,
        response: &mut LinuxSyscallOffloadResponse,
    ) {
        let Some(path) = linux_request_path(request) else {
            response.status = EINVAL;
            return;
        };
        match self.metadata(path) {
            Ok(metadata) => {
                let statx = build_linux_statx(metadata);
                response.payload_len = LINUX_STATX_SIZE as u32;
                response.payload[..LINUX_STATX_SIZE].copy_from_slice(&statx);
            }
            Err(errno) => response.status = errno,
        }
    }

    fn linux_newfstatat(
        &mut self,
        request: &LinuxSyscallOffloadRequest,
        response: &mut LinuxSyscallOffloadResponse,
    ) {
        let Some(path) = linux_request_path(request) else {
            response.status = EINVAL;
            return;
        };
        match self.metadata(path) {
            Ok(metadata) => {
                let stat = build_linux_stat(metadata);
                response.payload_len = LINUX_STAT_SIZE as u32;
                response.payload[..LINUX_STAT_SIZE].copy_from_slice(&stat);
            }
            Err(errno) => response.status = errno,
        }
    }

    fn linux_readlinkat(&mut self, response: &mut LinuxSyscallOffloadResponse) {
        response.status = ENOENT;
    }

    fn linux_access(
        &mut self,
        request: &LinuxSyscallOffloadRequest,
        response: &mut LinuxSyscallOffloadResponse,
    ) {
        let Some(path) = linux_request_path(request) else {
            response.status = EINVAL;
            return;
        };
        response.status = match self.metadata(path) {
            Ok(_) => 0,
            Err(errno) => errno,
        };
    }

    fn linux_getcwd(
        &mut self,
        request: &LinuxSyscallOffloadRequest,
        response: &mut LinuxSyscallOffloadResponse,
    ) {
        let cwd = self.cwd_for_pid(request.pid);
        write_payload_bytes(response, cwd.as_bytes());
    }

    fn linux_chdir(
        &mut self,
        request: &LinuxSyscallOffloadRequest,
        response: &mut LinuxSyscallOffloadResponse,
    ) {
        let Some(path) = linux_request_path(request) else {
            response.status = EINVAL;
            return;
        };
        response.status = self.chdir(request.pid, path);
    }

    fn linux_mkdir(
        &mut self,
        request: &LinuxSyscallOffloadRequest,
        response: &mut LinuxSyscallOffloadResponse,
    ) {
        let Some(path) = linux_request_path(request) else {
            response.status = EINVAL;
            return;
        };
        response.status = mkdir_policy(path, request.euid);
    }

    fn linux_openat(
        &mut self,
        request: &LinuxSyscallOffloadRequest,
        response: &mut LinuxSyscallOffloadResponse,
    ) {
        let Some(path) = linux_request_path(request) else {
            response.status = EINVAL;
            return;
        };
        match self.open_remote(path, request.flags) {
            Ok((id, handle)) => {
                let mut payload = [0_u8; 32];
                payload[0..8].copy_from_slice(&id.to_le_bytes());
                payload[8..10].copy_from_slice(&handle_kind_u16(handle.kind).to_le_bytes());
                payload[16..24].copy_from_slice(&handle.len.to_le_bytes());
                response.payload_len = payload.len() as u32;
                response.payload[..payload.len()].copy_from_slice(&payload);
            }
            Err(errno) => response.status = errno,
        }
    }

    fn linux_getdents64(
        &mut self,
        request: &LinuxSyscallOffloadRequest,
        response: &mut LinuxSyscallOffloadResponse,
    ) {
        response.status = self.getdents_payload(
            request.dirfd,
            request.arg1 as usize,
            &mut response.payload,
            &mut response.payload_len,
        );
    }

    fn linux_close(
        &mut self,
        request: &LinuxSyscallOffloadRequest,
        response: &mut LinuxSyscallOffloadResponse,
    ) {
        response.status = self.close_remote(request.dirfd);
    }

    fn linux_dup(
        &mut self,
        request: &LinuxSyscallOffloadRequest,
        response: &mut LinuxSyscallOffloadResponse,
    ) {
        response.status = self.dup_remote(request.dirfd);
    }

    fn linux_fcntl(&mut self, response: &mut LinuxSyscallOffloadResponse) {
        response.payload_len = size_of::<u64>() as u32;
        response.payload[..size_of::<u64>()].copy_from_slice(&0_u64.to_le_bytes());
    }

    fn linux_mount(&mut self, response: &mut LinuxSyscallOffloadResponse) {
        self.mount_generation = self.mount_generation.saturating_add(1);
        response.status = 0;
    }

    fn linux_umount2(&mut self, response: &mut LinuxSyscallOffloadResponse) {
        self.mount_generation = self.mount_generation.saturating_add(1);
        response.status = 0;
    }

    fn linux_unlinkat(
        &mut self,
        request: &LinuxSyscallOffloadRequest,
        response: &mut LinuxSyscallOffloadResponse,
    ) {
        let Some(path) = linux_request_path(request) else {
            response.status = EINVAL;
            return;
        };
        response.status = unlink_policy(path);
    }

    fn vfs_openat(&mut self, request: &VfsIpcRequest, response: &mut VfsIpcResponse) {
        let Some(path) = vfs_request_path(request) else {
            response.status = EINVAL;
            return;
        };
        let path = match self.resolve_path(request.pid, request.dirfd, path) {
            Ok(path) => path,
            Err(errno) => {
                response.status = errno;
                return;
            }
        };
        match self.open_remote(path.as_str(), request.arg0) {
            Ok((id, handle)) => {
                response.remote_id = id;
                response.handle_kind = handle_kind_u16(handle.kind);
                response.value = handle.len;
            }
            Err(errno) => response.status = errno,
        }
    }

    fn vfs_close(&mut self, request: &VfsIpcRequest, response: &mut VfsIpcResponse) {
        response.status = self.close_remote(request.remote_id);
    }

    fn vfs_dup(&mut self, request: &VfsIpcRequest, response: &mut VfsIpcResponse) {
        response.status = self.dup_remote(request.remote_id);
        response.remote_id = request.remote_id;
    }

    fn vfs_read(
        &mut self,
        request: &VfsIpcRequest,
        response: &mut VfsIpcResponse,
        offset: Option<u64>,
    ) {
        let len = (request.arg1 as usize).min(VFS_IPC_PAYLOAD_CAPACITY);
        match self.read_remote_into(request.remote_id, offset, len, &mut response.payload) {
            Ok(read) => {
                response.payload_len = read as u32;
                response.value = read as u64;
            }
            Err(errno) => response.status = errno,
        }
    }

    fn vfs_lseek(&mut self, request: &VfsIpcRequest, response: &mut VfsIpcResponse) {
        let Some(handle) = self.handles.get_mut(&request.remote_id) else {
            response.status = EBADF;
            return;
        };
        let next = match request.arg1 {
            SEEK_SET => request.arg0 as i64,
            SEEK_CUR => handle.cursor as i64 + request.arg0 as i64,
            SEEK_END => handle.len as i64 + request.arg0 as i64,
            _ => {
                response.status = EINVAL;
                return;
            }
        };
        if next < 0 {
            response.status = EINVAL;
            return;
        }
        handle.cursor = next as u64;
        response.value = handle.cursor;
    }

    fn vfs_fstat(&mut self, request: &VfsIpcRequest, response: &mut VfsIpcResponse) {
        let Some(handle) = self.handles.get(&request.remote_id) else {
            response.status = EBADF;
            return;
        };
        let stat = build_linux_stat(Metadata {
            kind: handle.kind,
            len: handle.len,
            inode: path_inode(handle.path.as_bytes()),
        });
        response.payload_len = LINUX_STAT_SIZE as u32;
        response.payload[..LINUX_STAT_SIZE].copy_from_slice(&stat);
    }

    fn vfs_getdents64(&mut self, request: &VfsIpcRequest, response: &mut VfsIpcResponse) {
        response.status = self.getdents_payload(
            request.remote_id,
            request.arg1 as usize,
            &mut response.payload,
            &mut response.payload_len,
        );
        response.value = response.payload_len as u64;
    }

    fn vfs_fcntl(&mut self, response: &mut VfsIpcResponse) {
        response.value = 0;
    }

    fn vfs_path_statx(&mut self, request: &VfsIpcRequest, response: &mut VfsIpcResponse) {
        let Some(path) = vfs_request_path(request) else {
            response.status = EINVAL;
            return;
        };
        let path = match self.resolve_path(request.pid, request.dirfd, path) {
            Ok(path) => path,
            Err(errno) => {
                response.status = errno;
                return;
            }
        };
        match self.metadata(path.as_str()) {
            Ok(metadata) => {
                let statx = build_linux_statx(metadata);
                response.payload_len = LINUX_STATX_SIZE as u32;
                response.payload[..LINUX_STATX_SIZE].copy_from_slice(&statx);
            }
            Err(errno) => response.status = errno,
        }
    }

    fn vfs_path_stat(&mut self, request: &VfsIpcRequest, response: &mut VfsIpcResponse) {
        let Some(path) = vfs_request_path(request) else {
            response.status = EINVAL;
            return;
        };
        let path = match self.resolve_path(request.pid, request.dirfd, path) {
            Ok(path) => path,
            Err(errno) => {
                response.status = errno;
                return;
            }
        };
        match self.metadata(path.as_str()) {
            Ok(metadata) => {
                let stat = build_linux_stat(metadata);
                response.payload_len = LINUX_STAT_SIZE as u32;
                response.payload[..LINUX_STAT_SIZE].copy_from_slice(&stat);
            }
            Err(errno) => response.status = errno,
        }
    }

    fn vfs_access(&mut self, request: &VfsIpcRequest, response: &mut VfsIpcResponse) {
        let Some(path) = vfs_request_path(request) else {
            response.status = EINVAL;
            return;
        };
        response.status = match self.resolve_path(request.pid, request.dirfd, path) {
            Ok(path) => self.metadata(path.as_str()).err().unwrap_or(0),
            Err(errno) => errno,
        };
    }

    fn vfs_getcwd(&mut self, request: &VfsIpcRequest, response: &mut VfsIpcResponse) {
        let cwd = self.cwd_for_pid(request.pid);
        write_vfs_payload_bytes(response, cwd.as_bytes());
    }

    fn vfs_chdir(&mut self, request: &VfsIpcRequest, response: &mut VfsIpcResponse) {
        let Some(path) = vfs_request_path(request) else {
            response.status = EINVAL;
            return;
        };
        let path = match self.resolve_path(request.pid, request.dirfd, path) {
            Ok(path) => path,
            Err(errno) => {
                response.status = errno;
                return;
            }
        };
        response.status = self.chdir(request.pid, path.as_str());
    }

    fn vfs_mkdir(&mut self, request: &VfsIpcRequest, response: &mut VfsIpcResponse) {
        let Some(path) = vfs_request_path(request) else {
            response.status = EINVAL;
            return;
        };
        let path = match self.resolve_path(request.pid, request.dirfd, path) {
            Ok(path) => path,
            Err(errno) => {
                response.status = errno;
                return;
            }
        };
        response.status = mkdir_policy(path.as_str(), request.euid);
    }

    fn linux_mount_vfs(&mut self, response: &mut VfsIpcResponse) {
        self.mount_generation = self.mount_generation.saturating_add(1);
        response.status = 0;
    }

    fn linux_umount_vfs(&mut self, response: &mut VfsIpcResponse) {
        self.mount_generation = self.mount_generation.saturating_add(1);
        response.status = 0;
    }

    fn vfs_unlinkat(&mut self, request: &VfsIpcRequest, response: &mut VfsIpcResponse) {
        let Some(path) = vfs_request_path(request) else {
            response.status = EINVAL;
            return;
        };
        let path = match self.resolve_path(request.pid, request.dirfd, path) {
            Ok(path) => path,
            Err(errno) => {
                response.status = errno;
                return;
            }
        };
        response.status = unlink_policy(path.as_str());
    }

    fn cwd_for_pid(&mut self, pid: u64) -> String {
        self.cwd
            .entry(pid)
            .or_insert_with(|| String::from("/"))
            .clone()
    }

    fn chdir(&mut self, pid: u64, path: &str) -> i32 {
        match self.metadata(path) {
            Ok(metadata) if metadata.kind == RemoteKind::Directory => {
                self.cwd.insert(pid, path.to_string());
                0
            }
            Ok(_) => ENOTDIR,
            Err(errno) => errno,
        }
    }

    fn resolve_path(&mut self, pid: u64, dirfd: u64, path: &str) -> Result<String, i32> {
        if path.is_empty() || path.len() > VFS_IPC_PATH_CAPACITY {
            return Err(EINVAL);
        }
        let base = if path.starts_with('/') {
            "/".to_string()
        } else if is_at_fdcwd(dirfd) {
            self.cwd_for_pid(pid)
        } else {
            let handle = self.handles.get(&dirfd).ok_or(EBADF)?;
            if handle.kind != RemoteKind::Directory {
                return Err(ENOTDIR);
            }
            handle.path.clone()
        };
        normalize_absolute_path(base.as_str(), path)
    }

    fn open_remote(&mut self, path: &str, flags: u64) -> Result<(u64, RemoteHandle), i32> {
        let metadata = self.metadata(path)?;
        if flags & O_DIRECTORY != 0 && metadata.kind != RemoteKind::Directory {
            return Err(ENOTDIR);
        }
        if flags & (O_CREAT | O_TRUNC) != 0 {
            return Err(EROFS);
        }
        let id = self.next_handle;
        self.next_handle = self.next_handle.checked_add(1).unwrap_or(1);
        let handle = RemoteHandle {
            kind: metadata.kind,
            path: path.to_string(),
            cursor: 0,
            len: metadata.len,
            refs: 1,
        };
        self.handles.insert(id, handle.clone());
        Ok((id, handle))
    }

    fn close_remote(&mut self, id: u64) -> i32 {
        let Some(handle) = self.handles.get_mut(&id) else {
            return EBADF;
        };
        if handle.refs > 1 {
            handle.refs -= 1;
        } else {
            self.handles.remove(&id);
        }
        0
    }

    fn dup_remote(&mut self, id: u64) -> i32 {
        let Some(handle) = self.handles.get_mut(&id) else {
            return EBADF;
        };
        handle.refs = handle.refs.saturating_add(1);
        0
    }

    fn read_remote_into(
        &mut self,
        id: u64,
        offset: Option<u64>,
        len: usize,
        dest: &mut [u8],
    ) -> Result<usize, i32> {
        let (path, start, file_len) = {
            let handle = self.handles.get(&id).ok_or(EBADF)?;
            if handle.kind == RemoteKind::Device && is_input_device_node(handle.path.as_str()) {
                return Err(EAGAIN);
            }
            if handle.kind != RemoteKind::File {
                return Err(EISDIR);
            }
            (
                handle.path.clone(),
                offset.unwrap_or(handle.cursor),
                handle.len,
            )
        };
        let available = file_len.saturating_sub(start);
        let len = len.min(available as usize).min(dest.len());
        let read = if len == 0 {
            0
        } else {
            self.cached_file_slice_into(path.as_str(), start, &mut dest[..len])?
        };
        if offset.is_none() {
            if let Some(handle) = self.handles.get_mut(&id) {
                handle.cursor = handle.cursor.saturating_add(read as u64);
            }
        }
        Ok(read)
    }

    fn cached_file_slice_into(
        &mut self,
        path: &str,
        start: u64,
        dest: &mut [u8],
    ) -> Result<usize, i32> {
        if !self.file_cache.contains_key(path) {
            let bytes = self
                .volume()?
                .read_file_to_vec(path)
                .map_err(map_fat_error)?;
            self.file_cache.insert(path.to_string(), bytes);
        }
        let Some(bytes) = self.file_cache.get(path) else {
            return Err(ENOENT);
        };
        let start = usize::try_from(start).map_err(|_| EINVAL)?;
        if start >= bytes.len() {
            return Ok(0);
        }
        let end = start.saturating_add(dest.len()).min(bytes.len());
        let read = end - start;
        dest[..read].copy_from_slice(&bytes[start..end]);
        Ok(read)
    }

    fn getdents_payload(
        &mut self,
        id: u64,
        user_len: usize,
        payload: &mut [u8],
        payload_len: &mut u32,
    ) -> i32 {
        if user_len < 24 {
            return EINVAL;
        }
        let (path, cursor) = {
            let Some(handle) = self.handles.get(&id) else {
                return EBADF;
            };
            if handle.kind != RemoteKind::Directory {
                return ENOTDIR;
            }
            (handle.path.clone(), handle.cursor as usize)
        };
        let entries = match self.dir_entries(path.as_str()) {
            Ok(entries) => entries,
            Err(errno) => return errno,
        };
        let mut written = 0usize;
        let mut consumed = 0usize;
        for (index, entry) in entries.iter().enumerate().skip(cursor) {
            let record = encode_dirent(entry, index + 1);
            if written + record.len() > user_len.min(payload.len()) {
                if written == 0 {
                    return EINVAL;
                }
                break;
            }
            payload[written..written + record.len()].copy_from_slice(record.as_slice());
            written += record.len();
            consumed += 1;
        }
        if let Some(handle) = self.handles.get_mut(&id) {
            handle.cursor = handle.cursor.saturating_add(consumed as u64);
        }
        *payload_len = written as u32;
        0
    }

    fn metadata(&mut self, path: &str) -> Result<Metadata, i32> {
        if path == "/" || path == "/proc" || path == "/run" {
            return Ok(Metadata {
                kind: RemoteKind::Directory,
                len: 0,
                inode: path_inode(path.as_bytes()),
            });
        }
        if path == "/dev" || path.starts_with("/dev/") {
            match devmgrd_lookup(path) {
                Ok(kind) => {
                    return Ok(Metadata {
                        kind,
                        len: 0,
                        inode: path_inode(path.as_bytes()),
                    });
                }
                Err(errno) => return Err(errno),
            }
        }
        self.invalidate_caches_if_remounted();
        if let Some(entry) = self.metadata_cache.get(path) {
            return *entry;
        }
        let result = match self.volume()?.metadata(path) {
            Ok(meta) => Ok(Metadata {
                kind: match meta.kind {
                    FatNodeKind::File => RemoteKind::File,
                    FatNodeKind::Directory => RemoteKind::Directory,
                },
                len: meta.len,
                inode: path_inode(path.as_bytes()),
            }),
            Err(err) => Err(map_fat_error(err)),
        };
        self.metadata_cache.insert(path.to_string(), result);
        result
    }

    fn dir_entries(&mut self, path: &str) -> Result<Vec<DirEntry>, i32> {
        let mut entries = Vec::new();
        if path == "/" {
            entries.push(DirEntry::new("dev", RemoteKind::Directory));
            entries.push(DirEntry::new("proc", RemoteKind::Directory));
            entries.push(DirEntry::new("run", RemoteKind::Directory));
        }
        if path == "/dev" || path == "/dev/input" || path == "/dev/dri" {
            match devmgrd_dir_entries(path) {
                Ok(entries) => return Ok(entries),
                Err(errno) => return Err(errno),
            }
        }
        if path == "/proc" || path == "/run" {
            return Ok(entries);
        }
        self.invalidate_caches_if_remounted();
        if let Some(cached) = self.dir_entries_cache.get(path) {
            entries.extend_from_slice(cached);
            return Ok(entries);
        }
        let fat_entries = self.volume()?.read_dir(path).map_err(map_fat_error)?;
        let resolved: Vec<DirEntry> = fat_entries.into_iter().map(DirEntry::from_fat).collect();
        self.dir_entries_cache
            .insert(path.to_string(), resolved.clone());
        entries.extend(resolved);
        Ok(entries)
    }

    fn volume(&mut self) -> Result<&FatVolume<BootBlockDevice>, i32> {
        if self.volume.is_none() {
            self.volume = Some(FatVolume::new(BootBlockDevice::open()?).map_err(map_fat_error)?);
        }
        Ok(self.volume.as_ref().expect("volume initialized"))
    }
}

fn validate_commercial_request(request: &CommercialMaxProtocolRequest) -> Result<(), i32> {
    if request.header.version != COMMERCIAL_MAX_PROTOCOL_ABI_VERSION
        || request.header.protocol != COMMERCIAL_MAX_PROTOCOL_VFSD
        || request.path_len as usize > request.path.len()
        || request.payload_len as usize > request.payload.len()
    {
        return Err(EINVAL);
    }
    match request.header.op {
        COMMERCIAL_MAX_VFSD_OP_MOUNT_GRAPH
        | COMMERCIAL_MAX_VFSD_OP_PATH_RESOLVE
        | COMMERCIAL_MAX_VFSD_OP_FD_TABLE_PLAN
        | COMMERCIAL_MAX_VFSD_OP_DIRECTORY_CURSOR
        | COMMERCIAL_MAX_VFSD_OP_FILE_CURSOR
        | COMMERCIAL_MAX_VFSD_OP_METADATA_POLICY => Ok(()),
        _ => Err(EINVAL),
    }
}

fn commercial_request_path(request: &CommercialMaxProtocolRequest) -> Option<&str> {
    let len = request.path_len as usize;
    str::from_utf8(&request.path[..len]).ok()
}

fn fill_handle_descriptors(
    state: &VfsState,
    response: &mut CommercialMaxProtocolResponse,
    kind: RemoteKind,
) {
    let mut total = 0_u64;
    for (index, (id, handle)) in state
        .handles
        .iter()
        .filter(|(_, handle)| handle.kind == kind)
        .enumerate()
    {
        total += 1;
        if index < COMMERCIAL_MAX_PROTOCOL_MAX_DESCRIPTORS {
            response.descriptors[index] =
                vfs_descriptor(handle.path.as_str(), response.header.op, *id, handle.cursor);
            response.descriptor_count = (index + 1) as u16;
        }
    }
    response.value0 = total;
}

fn vfs_descriptor(
    label: &str,
    op: u16,
    value0: u64,
    value1: u64,
) -> CommercialMaxProtocolDescriptorWire {
    let mut descriptor = CommercialMaxProtocolDescriptorWire {
        protocol: COMMERCIAL_MAX_PROTOCOL_VFSD,
        op,
        flags: 0,
        service_id: IPC_SERVICE_VFSD,
        capability_mask: vfs_capability_mask(op),
        value0,
        value1,
        ..CommercialMaxProtocolDescriptorWire::default()
    };
    copy_label(label, &mut descriptor.name, &mut descriptor.name_len);
    descriptor
}

fn vfs_capability(label: &str, op: u16) -> CommercialMaxCapabilityLeaseWire {
    let mut capability = CommercialMaxCapabilityLeaseWire {
        lease_id: ((COMMERCIAL_MAX_PROTOCOL_VFSD as u64) << 32) | u64::from(op),
        service_id: IPC_SERVICE_VFSD,
        capability_mask: vfs_capability_mask(op),
        rights_mask: vfs_capability_mask(op),
        ..CommercialMaxCapabilityLeaseWire::default()
    };
    copy_label(label, &mut capability.label, &mut capability.label_len);
    capability
}

fn vfs_capability_mask(op: u16) -> u64 {
    match op {
        COMMERCIAL_MAX_VFSD_OP_MOUNT_GRAPH => 1 << 0,
        COMMERCIAL_MAX_VFSD_OP_PATH_RESOLVE => 1 << 1,
        COMMERCIAL_MAX_VFSD_OP_FD_TABLE_PLAN => 1 << 2,
        COMMERCIAL_MAX_VFSD_OP_DIRECTORY_CURSOR => 1 << 3,
        COMMERCIAL_MAX_VFSD_OP_FILE_CURSOR => 1 << 4,
        COMMERCIAL_MAX_VFSD_OP_METADATA_POLICY => 1 << 5,
        _ => 0,
    }
}

fn copy_label(label: &str, target: &mut [u8], len: &mut u16) {
    let bytes = label.as_bytes();
    let count = bytes.len().min(target.len());
    target[..count].copy_from_slice(&bytes[..count]);
    *len = count as u16;
}

#[derive(Clone)]
struct DirEntry {
    name: String,
    kind: RemoteKind,
}

impl DirEntry {
    fn new(name: &str, kind: RemoteKind) -> Self {
        Self {
            name: name.to_string(),
            kind,
        }
    }

    fn from_fat(entry: FatDirEntry) -> Self {
        Self {
            name: entry.name,
            kind: match entry.kind {
                FatNodeKind::File => RemoteKind::File,
                FatNodeKind::Directory => RemoteKind::Directory,
            },
        }
    }
}

fn devmgrd_lookup(path: &str) -> Result<RemoteKind, i32> {
    let mut request = devmgrd_request(DEVMGRD_IPC_OP_LOOKUP, path)?;
    let response = call_devmgrd(&mut request)?;
    devmgrd_kind_to_remote(response.kind)
}

fn devmgrd_dir_entries(path: &str) -> Result<Vec<DirEntry>, i32> {
    let mut request = devmgrd_request(DEVMGRD_IPC_OP_READDIR, path)?;
    let response = call_devmgrd(&mut request)?;
    if response.entry_count as usize > DEVMGRD_MAX_DIR_ENTRIES {
        return Err(EINVAL);
    }
    let mut entries = Vec::new();
    for entry in response.entries.iter().take(response.entry_count as usize) {
        let name_len = entry.name_len as usize;
        if name_len == 0 || name_len > entry.name.len() {
            return Err(EINVAL);
        }
        let name = str::from_utf8(&entry.name[..name_len]).map_err(|_| EINVAL)?;
        entries.push(DirEntry::new(name, devmgrd_kind_to_remote(entry.kind)?));
    }
    Ok(entries)
}

fn devmgrd_request(op: u16, path: &str) -> Result<DevmgrdIpcRequest, i32> {
    let bytes = path.as_bytes();
    if bytes.is_empty() || bytes.len() > VFS_IPC_PATH_CAPACITY {
        return Err(EINVAL);
    }
    let mut request = DevmgrdIpcRequest {
        op,
        path_len: bytes.len() as u32,
        ..DevmgrdIpcRequest::default()
    };
    request.path[..bytes.len()].copy_from_slice(bytes);
    Ok(request)
}

fn call_devmgrd(request: &mut DevmgrdIpcRequest) -> Result<DevmgrdIpcResponse, i32> {
    let endpoint = ipc::lookup_service_endpoint(IPC_SERVICE_DEVMGRD);
    if endpoint < 0 {
        return Err(ENODEV);
    }
    let mut response = DevmgrdIpcResponse::default();
    let received = unsafe {
        ipc::call(
            endpoint as u64,
            (request as *const DevmgrdIpcRequest).cast::<u8>(),
            size_of::<DevmgrdIpcRequest>(),
            (&mut response as *mut DevmgrdIpcResponse).cast::<u8>(),
            size_of::<DevmgrdIpcResponse>(),
        )
    };
    if received < 0 {
        return Err(ENODEV);
    }
    if received as usize != size_of::<DevmgrdIpcResponse>()
        || response.version != DEVMGRD_IPC_ABI_VERSION
        || response.op != request.op
        || response.reserved0 != 0
    {
        return Err(EINVAL);
    }
    if response.status != 0 {
        return Err(response.status.unsigned_abs() as i32);
    }
    Ok(response)
}

fn devmgrd_kind_to_remote(kind: u16) -> Result<RemoteKind, i32> {
    match kind {
        DEVMGRD_NODE_KIND_DIR => Ok(RemoteKind::Directory),
        DEVMGRD_NODE_KIND_DEVICE => Ok(RemoteKind::Device),
        _ => Err(EINVAL),
    }
}

fn is_input_device_node(path: &str) -> bool {
    matches!(path, "/dev/input0" | "/dev/input/event0")
}

struct BootBlockDevice {
    block_size: usize,
    block_count: u64,
}

impl BootBlockDevice {
    fn open() -> Result<Self, i32> {
        let mut block_size = 0_u64;
        let mut block_count = 0_u64;
        let args = RustosBlockBrokerArgs {
            abi_version: BLOCK_BROKER_ABI_VERSION,
            op: BLOCK_BROKER_OP_BOOT_INFO,
            out_logical_block_size_ptr: (&mut block_size as *mut u64) as u64,
            out_block_count_ptr: (&mut block_count as *mut u64) as u64,
            ..RustosBlockBrokerArgs::default()
        };
        let status = unsafe {
            rustos_svc_runtime::syscall::syscall1(
                SYS_RUSTOS_BLOCK_BROKER,
                (&args as *const RustosBlockBrokerArgs) as u64,
            )
        };
        if status < 0 {
            return Err((-status) as i32);
        }
        let block_size = usize::try_from(block_size).map_err(|_| EINVAL)?;
        if block_size < 512 || block_count == 0 {
            return Err(EINVAL);
        }
        Ok(Self {
            block_size,
            block_count,
        })
    }
}

impl BlockDevice for BootBlockDevice {
    fn logical_block_size(&self) -> usize {
        self.block_size
    }

    fn block_count(&self) -> u64 {
        self.block_count
    }

    fn read_blocks(&mut self, lba: u64, out: &mut [u8]) -> IoResult<()> {
        if self.block_size == 0
            || out.len() % self.block_size != 0
            || BLOCK_BROKER_MAX_IO_BYTES < self.block_size
        {
            return Err(StorageError::InvalidInput);
        }
        let max_blocks = (BLOCK_BROKER_MAX_IO_BYTES / self.block_size) as u64;
        let mut done = 0usize;
        while done < out.len() {
            let remaining_blocks = ((out.len() - done) / self.block_size) as u64;
            let block_count = remaining_blocks.min(max_blocks);
            let byte_len = block_count as usize * self.block_size;
            let args = RustosBlockBrokerArgs {
                abi_version: BLOCK_BROKER_ABI_VERSION,
                op: BLOCK_BROKER_OP_BOOT_READ,
                lba: lba + (done / self.block_size) as u64,
                block_count,
                buffer_ptr: out[done..done + byte_len].as_mut_ptr() as u64,
                buffer_len: byte_len as u64,
                ..RustosBlockBrokerArgs::default()
            };
            let status = unsafe {
                rustos_svc_runtime::syscall::syscall1(
                    SYS_RUSTOS_BLOCK_BROKER,
                    (&args as *const RustosBlockBrokerArgs) as u64,
                )
            };
            if status < 0 {
                return Err(StorageError::DeviceFault);
            }
            done += byte_len;
        }
        Ok(())
    }

    fn write_blocks(&mut self, _lba: u64, _input: &[u8]) -> IoResult<()> {
        Err(StorageError::Unsupported)
    }
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct LinuxTimespec {
    tv_sec: i64,
    tv_nsec: i64,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct LinuxStat {
    st_dev: u64,
    st_ino: u64,
    st_nlink: u64,
    st_mode: u32,
    st_uid: u32,
    st_gid: u32,
    __pad0: u32,
    st_rdev: u64,
    st_size: i64,
    st_blksize: i64,
    st_blocks: i64,
    st_atim: LinuxTimespec,
    st_mtim: LinuxTimespec,
    st_ctim: LinuxTimespec,
    __glibc_reserved: [i64; 3],
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct LinuxStatxTimestamp {
    tv_sec: i64,
    tv_nsec: u32,
    __reserved: i32,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct LinuxStatx {
    stx_mask: u32,
    stx_blksize: u32,
    stx_attributes: u64,
    stx_nlink: u32,
    stx_uid: u32,
    stx_gid: u32,
    stx_mode: u16,
    __spare0: [u16; 1],
    stx_ino: u64,
    stx_size: u64,
    stx_blocks: u64,
    stx_attributes_mask: u64,
    stx_atime: LinuxStatxTimestamp,
    stx_btime: LinuxStatxTimestamp,
    stx_ctime: LinuxStatxTimestamp,
    stx_mtime: LinuxStatxTimestamp,
    stx_rdev_major: u32,
    stx_rdev_minor: u32,
    stx_dev_major: u32,
    stx_dev_minor: u32,
    stx_mnt_id: u64,
    stx_dio_mem_align: u32,
    stx_dio_offset_align: u32,
    __spare3: [u64; 12],
}

#[repr(C)]
#[derive(Clone, Copy)]
struct LinuxSyscallOffloadRequest {
    version: u16,
    op: u16,
    reserved0: u32,
    pid: u64,
    tid: u64,
    parent_pid: u64,
    session_handle: u64,
    uid: u32,
    gid: u32,
    euid: u32,
    egid: u32,
    dirfd: u64,
    flags: u64,
    arg0: u64,
    arg1: u64,
    arg2: u64,
    arg3: u64,
    mask: u32,
    path_len: u32,
    path: [u8; SYSCALL_OFFLOAD_PATH_CAPACITY],
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
struct LinuxSyscallOffloadResponse {
    version: u16,
    op: u16,
    status: i32,
    payload_len: u32,
    reserved0: u32,
    payload: [u8; SYSCALL_OFFLOAD_PAYLOAD_CAPACITY],
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

fn validate_linux_request(request: &LinuxSyscallOffloadRequest) -> Result<(), i32> {
    if request.version != SYSCALL_OFFLOAD_ABI_VERSION
        || request.reserved0 != 0
        || request.path_len as usize > SYSCALL_OFFLOAD_PATH_CAPACITY
    {
        return Err(EINVAL);
    }
    Ok(())
}

fn validate_vfs_request(request: &VfsIpcRequest) -> Result<(), i32> {
    if request.version != VFS_IPC_ABI_VERSION
        || request.path_len as usize > VFS_IPC_PATH_CAPACITY
        || request.payload_len as usize > VFS_IPC_REQUEST_PAYLOAD_CAPACITY
    {
        return Err(EINVAL);
    }
    Ok(())
}

fn linux_request_path(request: &LinuxSyscallOffloadRequest) -> Option<&str> {
    let len = request.path_len as usize;
    if len > request.path.len() {
        return None;
    }
    core::str::from_utf8(&request.path[..len]).ok()
}

fn vfs_request_path(request: &VfsIpcRequest) -> Option<&str> {
    let len = request.path_len as usize;
    if len > request.path.len() {
        return None;
    }
    core::str::from_utf8(&request.path[..len]).ok()
}

fn mkdir_policy(path: &str, euid: u32) -> i32 {
    let run_user_path = format!("/run/user/{euid}");
    if path == "/run" || path == "/run/user" || path == run_user_path.as_str() {
        0
    } else {
        EROFS
    }
}

fn unlink_policy(path: &str) -> i32 {
    if path.starts_with("/run/") {
        ENOENT
    } else {
        EROFS
    }
}

fn normalize_absolute_path(base_path: &str, path: &str) -> Result<String, i32> {
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

fn is_at_fdcwd(dirfd: u64) -> bool {
    dirfd == AT_FDCWD_U64 || dirfd == AT_FDCWD_U32
}

fn map_fat_error(err: storage_fat::FatError) -> i32 {
    match err {
        fatfs::Error::NotFound => ENOENT,
        fatfs::Error::InvalidInput => EINVAL,
        fatfs::Error::Io(StorageError::NotPresent) => ENODEV,
        fatfs::Error::Io(StorageError::InvalidInput) => EINVAL,
        fatfs::Error::Io(_) => EIO,
        _ => EIO,
    }
}

fn handle_kind_u16(kind: RemoteKind) -> u16 {
    match kind {
        RemoteKind::File => VFS_IPC_HANDLE_KIND_FILE,
        RemoteKind::Directory => VFS_IPC_HANDLE_KIND_DIR,
        RemoteKind::Device => VFS_IPC_HANDLE_KIND_DEVICE,
    }
}

fn build_linux_stat(metadata: Metadata) -> [u8; LINUX_STAT_SIZE] {
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

fn build_linux_statx(metadata: Metadata) -> [u8; LINUX_STATX_SIZE] {
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

fn mode_bits(kind: RemoteKind) -> u32 {
    match kind {
        RemoteKind::File => BOOT_FILE_MODE_BITS,
        RemoteKind::Directory => BOOT_DIRECTORY_MODE_BITS,
        RemoteKind::Device => DEVICE_FILE_MODE_BITS,
    }
}

fn write_payload_bytes(response: &mut LinuxSyscallOffloadResponse, bytes: &[u8]) {
    let len = bytes.len().min(response.payload.len());
    response.payload[..len].copy_from_slice(&bytes[..len]);
    response.payload_len = len as u32;
}

fn write_vfs_payload_bytes(response: &mut VfsIpcResponse, bytes: &[u8]) {
    let len = bytes.len().min(response.payload.len());
    response.payload[..len].copy_from_slice(&bytes[..len]);
    response.payload_len = len as u32;
    response.value = len as u64;
}

fn encode_dirent(entry: &DirEntry, next_offset: usize) -> Vec<u8> {
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

fn path_inode(bytes: &[u8]) -> u64 {
    let mut hash = 0xcbf2_9ce4_8422_2325_u64;
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x1000_0000_01b3);
    }
    hash.max(1)
}

fn read_unaligned<T: Copy + Default>(bytes: &[u8]) -> T {
    let mut value = T::default();
    let dest = unsafe {
        core::slice::from_raw_parts_mut((&mut value as *mut T).cast::<u8>(), size_of::<T>())
    };
    dest.copy_from_slice(&bytes[..size_of::<T>()]);
    value
}

fn as_bytes<T>(value: &T) -> &[u8] {
    unsafe { core::slice::from_raw_parts((value as *const T).cast::<u8>(), size_of::<T>()) }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rustos_user_abi::syscall::IPC_MAX_INLINE_BYTES;

    #[test]
    fn vfs_requests_validate_inline_shape() {
        let request = VfsIpcRequest::default();
        assert_eq!(validate_vfs_request(&request), Ok(()));
        assert!(size_of::<VfsIpcRequest>() <= IPC_MAX_INLINE_BYTES);
        assert!(size_of::<VfsIpcResponse>() <= IPC_MAX_INLINE_BYTES);
    }

    #[test]
    fn normalizes_cwd_relative_paths() {
        assert_eq!(
            normalize_absolute_path("/usr/lib", "../bin/app").unwrap(),
            "/usr/bin/app"
        );
    }
}
