#![no_std]
#![no_main]

extern crate alloc;

use alloc::collections::BTreeMap;
use alloc::string::{String, ToString};
use alloc::vec::Vec;
use core::mem::size_of;
use core::str;

use rustos_svc_runtime::ipc;
use rustos_user_abi::linux as linux_abi;
use rustos_user_abi::syscall::{
    CommercialMaxCapabilityLeaseWire, CommercialMaxProtocolDescriptorWire,
    CommercialMaxProtocolRequest, CommercialMaxProtocolResponse, VfsIpcRequest, VfsIpcResponse,
    COMMERCIAL_MAX_PROTOCOL_ABI_VERSION, COMMERCIAL_MAX_PROTOCOL_MAX_DESCRIPTORS,
    COMMERCIAL_MAX_PROTOCOL_VFSD, COMMERCIAL_MAX_VFSD_OP_DIRECTORY_CURSOR,
    COMMERCIAL_MAX_VFSD_OP_FD_TABLE_PLAN, COMMERCIAL_MAX_VFSD_OP_FILE_CURSOR,
    COMMERCIAL_MAX_VFSD_OP_METADATA_POLICY, COMMERCIAL_MAX_VFSD_OP_MOUNT_GRAPH,
    COMMERCIAL_MAX_VFSD_OP_PATH_RESOLVE, IPC_MAX_INLINE_BYTES, IPC_SERVICE_VFSD, LINUX_STATX_SIZE,
    LINUX_STAT_SIZE, SYSCALL_OFFLOAD_OP_LINUX_ACCESS, SYSCALL_OFFLOAD_OP_LINUX_CHDIR,
    SYSCALL_OFFLOAD_OP_LINUX_CLOSE, SYSCALL_OFFLOAD_OP_LINUX_DUP, SYSCALL_OFFLOAD_OP_LINUX_FCNTL,
    SYSCALL_OFFLOAD_OP_LINUX_GETCWD, SYSCALL_OFFLOAD_OP_LINUX_GETDENTS64,
    SYSCALL_OFFLOAD_OP_LINUX_MKDIR, SYSCALL_OFFLOAD_OP_LINUX_MOUNT,
    SYSCALL_OFFLOAD_OP_LINUX_NEWFSTATAT, SYSCALL_OFFLOAD_OP_LINUX_OPENAT,
    SYSCALL_OFFLOAD_OP_LINUX_READLINKAT, SYSCALL_OFFLOAD_OP_LINUX_STATX,
    SYSCALL_OFFLOAD_OP_LINUX_UMOUNT2, SYSCALL_OFFLOAD_OP_LINUX_UNLINKAT, VFS_IPC_OP_ACCESS,
    VFS_IPC_OP_CHDIR, VFS_IPC_OP_CLOSE, VFS_IPC_OP_DUP, VFS_IPC_OP_FCNTL, VFS_IPC_OP_FSTAT,
    VFS_IPC_OP_FTRUNCATE, VFS_IPC_OP_GETCWD, VFS_IPC_OP_GETDENTS64, VFS_IPC_OP_LSEEK,
    VFS_IPC_OP_MKDIR, VFS_IPC_OP_MOUNT, VFS_IPC_OP_NEWFSTATAT, VFS_IPC_OP_OPENAT,
    VFS_IPC_OP_POLL_QUERY, VFS_IPC_OP_PREAD64, VFS_IPC_OP_READ, VFS_IPC_OP_READLINKAT,
    VFS_IPC_OP_STATX, VFS_IPC_OP_UMOUNT2, VFS_IPC_OP_UNLINKAT, VFS_IPC_OP_WRITE,
    VFS_IPC_PATH_CAPACITY, VFS_IPC_PAYLOAD_CAPACITY, VFS_POLL_QUERY_EPOLL_CREATE,
    VFS_POLL_QUERY_EPOLL_CTL, VFS_POLL_QUERY_EPOLL_WAIT, VFS_POLL_QUERY_POLL,
};
use storage_fat::{FatDirEntry, FatNodeKind, FatVolume};

mod block;
mod devmgrd;
mod linux_types;
mod util;

use block::BootBlockDevice;
use devmgrd::{devmgrd_dir_entries, devmgrd_lookup};
use linux_types::{
    validate_linux_request, validate_vfs_request, LinuxSyscallOffloadRequest,
    LinuxSyscallOffloadResponse,
};
use util::{
    build_linux_stat, build_linux_statx, encode_dirent, handle_kind_u16, is_at_fdcwd,
    linux_request_path, map_fat_error, mkdir_policy, normalize_absolute_path, path_inode,
    read_unaligned, unlink_policy, vfs_request_path, write_payload_bytes, write_vfs_payload_bytes,
};

// Linux errno constants (x86_64)
pub(crate) const ENOENT: i32 = 2;
pub(crate) const EIO: i32 = 5;
pub(crate) const EAGAIN: i32 = 11;
pub(crate) const EBADF: i32 = 9;
pub(crate) const EEXIST: i32 = 17;
pub(crate) const ENODEV: i32 = 19;
pub(crate) const ENOTDIR: i32 = 20;
pub(crate) const EISDIR: i32 = 21;
pub(crate) const EINVAL: i32 = 22;
pub(crate) const EROFS: i32 = 30;

// Linux lseek whence constants
const SEEK_SET: u64 = 0;
const SEEK_CUR: u64 = 1;
const SEEK_END: u64 = 2;

// Linux open flags (x86_64)
const O_CREAT: u64 = 0o100;
const O_TRUNC: u64 = 0o1000;
const O_DIRECTORY: u64 = 0o200000;

// AT_FDCWD as u64 (represents -100 in twos complement, or 0xFFFF_FFFF_FFFF_FF9C)
pub(crate) const AT_FDCWD_U64: u64 = (-100_i64) as u64;
// Also allow the truncated u32 form
pub(crate) const AT_FDCWD_U32: u64 = 0xFFFF_FF9C;

pub(crate) const DEFAULT_BLOCK_SIZE: u64 = 4096;
pub(crate) const DT_REG: u8 = 8;
pub(crate) const DT_DIR: u8 = 4;
pub(crate) const DT_CHR: u8 = 2;
const S_IFREG: u32 = 0o100000;
const S_IFDIR: u32 = 0o040000;
const S_IFCHR: u32 = 0o020000;
pub(crate) const BOOT_FILE_MODE_BITS: u32 = S_IFREG | 0o444;
pub(crate) const BOOT_DIRECTORY_MODE_BITS: u32 = S_IFDIR | 0o555;
pub(crate) const DEVICE_FILE_MODE_BITS: u32 = S_IFCHR | 0o600;

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
    epolls: BTreeMap<u64, EpollState>,
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

#[derive(Clone)]
struct EpollInterest {
    events: u32,
    data: u64,
}

#[derive(Clone)]
struct EpollState {
    interests: BTreeMap<u64, EpollInterest>,
}

#[derive(Clone, Copy, Eq, PartialEq)]
pub(crate) enum RemoteKind {
    File,
    Directory,
    Device,
}

#[derive(Clone, Copy)]
pub(crate) struct Metadata {
    pub(crate) kind: RemoteKind,
    pub(crate) len: u64,
    pub(crate) inode: u64,
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
            epolls: BTreeMap::new(),
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
            VFS_IPC_OP_POLL_QUERY => self.vfs_poll_query(request, &mut response),
            _ => response.status = EINVAL,
        }
        response
    }

    fn vfs_poll_query(&mut self, request: &VfsIpcRequest, response: &mut VfsIpcResponse) {
        match request.arg0 {
            VFS_POLL_QUERY_POLL => self.vfs_poll_once(request, response),
            VFS_POLL_QUERY_EPOLL_CREATE => {
                self.epolls.entry(request.remote_id).or_insert(EpollState {
                    interests: BTreeMap::new(),
                });
            }
            VFS_POLL_QUERY_EPOLL_CTL => self.vfs_epoll_ctl(request, response),
            VFS_POLL_QUERY_EPOLL_WAIT => self.vfs_epoll_wait(request, response),
            _ => response.status = EINVAL,
        }
    }

    fn vfs_poll_once(&mut self, request: &VfsIpcRequest, response: &mut VfsIpcResponse) {
        const POLLFD_SIZE: usize = size_of::<linux_abi::LinuxPollFd>();
        let len = request.payload_len as usize;
        if len % POLLFD_SIZE != 0 || len > response.payload.len() {
            response.status = EINVAL;
            return;
        }
        let mut ready = 0_u64;
        for offset in (0..len).step_by(POLLFD_SIZE) {
            let fd = i32::from_le_bytes(request.payload[offset..offset + 4].try_into().unwrap());
            let events =
                i16::from_le_bytes(request.payload[offset + 4..offset + 6].try_into().unwrap());
            let revents = if fd < 0 {
                0
            } else {
                let ready_bits = poll_ready_bits(events as u32) as i16;
                if ready_bits != 0 {
                    ready += 1;
                }
                ready_bits
            };
            response.payload[offset..offset + 6]
                .copy_from_slice(&request.payload[offset..offset + 6]);
            response.payload[offset + 6..offset + 8].copy_from_slice(&revents.to_le_bytes());
        }
        response.value = ready;
        response.payload_len = len as u32;
    }

    fn vfs_epoll_ctl(&mut self, request: &VfsIpcRequest, response: &mut VfsIpcResponse) {
        let Some(epoll) = self.epolls.get_mut(&request.remote_id) else {
            response.status = EINVAL;
            return;
        };
        match request.arg1 {
            linux_abi::EPOLL_CTL_ADD => {
                if epoll.interests.contains_key(&request.fd) {
                    response.status = EEXIST;
                    return;
                }
                let Some(interest) = epoll_interest_from_request(request) else {
                    response.status = EINVAL;
                    return;
                };
                epoll.interests.insert(request.fd, interest);
            }
            linux_abi::EPOLL_CTL_MOD => {
                if !epoll.interests.contains_key(&request.fd) {
                    response.status = ENOENT;
                    return;
                }
                let Some(interest) = epoll_interest_from_request(request) else {
                    response.status = EINVAL;
                    return;
                };
                epoll.interests.insert(request.fd, interest);
            }
            linux_abi::EPOLL_CTL_DEL => {
                if epoll.interests.remove(&request.fd).is_none() {
                    response.status = ENOENT;
                }
            }
            _ => response.status = EINVAL,
        }
    }

    fn vfs_epoll_wait(&mut self, request: &VfsIpcRequest, response: &mut VfsIpcResponse) {
        const EPOLL_EVENT_SIZE: usize = size_of::<linux_abi::LinuxEpollEvent>();
        let Some(epoll) = self.epolls.get(&request.remote_id) else {
            response.status = EINVAL;
            return;
        };
        let maxevents = request.arg1 as usize;
        let max_payload_events = response.payload.len() / EPOLL_EVENT_SIZE;
        let mut written = 0usize;
        for interest in epoll.interests.values() {
            if written >= maxevents || written >= max_payload_events {
                break;
            }
            let events = poll_ready_bits(interest.events);
            if events == 0 {
                continue;
            }
            let offset = written * EPOLL_EVENT_SIZE;
            response.payload[offset..offset + 4].copy_from_slice(&events.to_le_bytes());
            response.payload[offset + 4..offset + 12].copy_from_slice(&interest.data.to_le_bytes());
            written += 1;
        }
        response.value = written as u64;
        response.payload_len = (written * EPOLL_EVENT_SIZE) as u32;
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

fn poll_ready_bits(requested: u32) -> u32 {
    requested
        & (linux_abi::EPOLLIN
            | linux_abi::EPOLLPRI
            | linux_abi::EPOLLOUT
            | linux_abi::EPOLLERR
            | linux_abi::EPOLLHUP)
}

fn epoll_interest_from_request(request: &VfsIpcRequest) -> Option<EpollInterest> {
    const EPOLL_EVENT_SIZE: usize = size_of::<linux_abi::LinuxEpollEvent>();
    if request.payload_len as usize != EPOLL_EVENT_SIZE {
        return None;
    }
    Some(EpollInterest {
        events: u32::from_le_bytes(request.payload[0..4].try_into().ok()?),
        data: u64::from_le_bytes(request.payload[4..12].try_into().ok()?),
    })
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
pub(crate) struct DirEntry {
    pub(crate) name: String,
    pub(crate) kind: RemoteKind,
}

impl DirEntry {
    pub(crate) fn new(name: &str, kind: RemoteKind) -> Self {
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

fn is_input_device_node(path: &str) -> bool {
    matches!(path, "/dev/input0" | "/dev/input/event0")
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
