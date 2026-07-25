// RING3-MIGRATION-REFERENCE START: vfsd is the ring3 owner for VFS policy.
// Marker is restored to close historical audits; this file is not ring0 debt.
#![no_std]
#![no_main]

extern crate alloc;

use alloc::collections::BTreeMap;
use alloc::ffi::CString;
use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec::Vec;
use core::mem::{size_of, MaybeUninit};
#[cfg(all(not(test), not(clippy)))]
use core::panic::PanicInfo;
use core::str;

use rustos_svc_runtime::ipc;
use rustos_user_abi::linux as linux_abi;
use rustos_user_abi::syscall::{
    CommercialMaxCapabilityLeaseWire, CommercialMaxProtocolDescriptorWire,
    CommercialMaxProtocolRequest, CommercialMaxProtocolResponse, IpcReplyWithHandlesArgs,
    ServiceCheckpointRecordWire, VfsExecutableSnapshotRequest, VfsExecutableSnapshotResponse,
    VfsIpcRequest, VfsIpcResponse, WaitSetInterestWire, WaitSetSignalBrokerArgs,
    COMMERCIAL_MAX_PROTOCOL_ABI_VERSION, COMMERCIAL_MAX_PROTOCOL_MAX_DESCRIPTORS,
    COMMERCIAL_MAX_PROTOCOL_ROOTD_SUPERVISOR, COMMERCIAL_MAX_PROTOCOL_VFSD,
    COMMERCIAL_MAX_ROOTD_OP_SERVICE_CHECKPOINT_COMPACT,
    COMMERCIAL_MAX_ROOTD_OP_SERVICE_CHECKPOINT_MUTATE,
    COMMERCIAL_MAX_ROOTD_OP_SERVICE_CHECKPOINT_SCAN, COMMERCIAL_MAX_VFSD_OP_DIRECTORY_CURSOR,
    COMMERCIAL_MAX_VFSD_OP_FD_TABLE_PLAN, COMMERCIAL_MAX_VFSD_OP_FILE_CURSOR,
    COMMERCIAL_MAX_VFSD_OP_METADATA_POLICY, COMMERCIAL_MAX_VFSD_OP_MOUNT_GRAPH,
    COMMERCIAL_MAX_VFSD_OP_PATH_RESOLVE, IPC_SERVICE_LOADERD, IPC_SERVICE_ROOTD, IPC_SERVICE_VFSD,
    LINUX_STATX_SIZE, LINUX_STAT_SIZE, SERVICE_CHECKPOINT_FLAG_TOMBSTONE,
    SERVICE_CHECKPOINT_VALUE_CAPACITY, SYSCALL_OFFLOAD_ABI_VERSION,
    SYSCALL_OFFLOAD_OP_LINUX_ACCESS, SYSCALL_OFFLOAD_OP_LINUX_CHDIR,
    SYSCALL_OFFLOAD_OP_LINUX_CLOSE, SYSCALL_OFFLOAD_OP_LINUX_DUP, SYSCALL_OFFLOAD_OP_LINUX_FCNTL,
    SYSCALL_OFFLOAD_OP_LINUX_GETCWD, SYSCALL_OFFLOAD_OP_LINUX_GETDENTS64,
    SYSCALL_OFFLOAD_OP_LINUX_MKDIR, SYSCALL_OFFLOAD_OP_LINUX_MOUNT,
    SYSCALL_OFFLOAD_OP_LINUX_NEWFSTATAT, SYSCALL_OFFLOAD_OP_LINUX_OPENAT,
    SYSCALL_OFFLOAD_OP_LINUX_READLINKAT, SYSCALL_OFFLOAD_OP_LINUX_STATX,
    SYSCALL_OFFLOAD_OP_LINUX_UMOUNT2, SYSCALL_OFFLOAD_OP_LINUX_UNLINKAT,
    SYS_RUSTOS_IPC_REPLY_WITH_HANDLES, SYS_RUSTOS_WAITSET_SIGNAL_BROKER, VFS_CURSOR_SETTLE_CANCEL,
    VFS_CURSOR_SETTLE_COMMIT, VFS_EXECUTABLE_SNAPSHOT_ABI_VERSION, VFS_EXECUTABLE_SNAPSHOT_OP_OPEN,
    VFS_IPC_ABI_VERSION, VFS_IPC_OP_ACCESS, VFS_IPC_OP_CHDIR, VFS_IPC_OP_CHECKPOINT_ACK,
    VFS_IPC_OP_CLOSE, VFS_IPC_OP_CURSOR_SETTLE, VFS_IPC_OP_DUP, VFS_IPC_OP_FCNTL, VFS_IPC_OP_FSTAT,
    VFS_IPC_OP_FTRUNCATE, VFS_IPC_OP_GETCWD, VFS_IPC_OP_GETDENTS64, VFS_IPC_OP_LSEEK,
    VFS_IPC_OP_MKDIR, VFS_IPC_OP_MOUNT, VFS_IPC_OP_NEWFSTATAT, VFS_IPC_OP_OPENAT,
    VFS_IPC_OP_POLL_QUERY, VFS_IPC_OP_PREAD64, VFS_IPC_OP_READ, VFS_IPC_OP_READLINKAT,
    VFS_IPC_OP_STATX, VFS_IPC_OP_UMOUNT2, VFS_IPC_OP_UNLINKAT, VFS_IPC_OP_WRITE,
    VFS_IPC_PATH_CAPACITY, VFS_IPC_PAYLOAD_CAPACITY, VFS_POLL_QUERY_EPOLL_CREATE,
    VFS_POLL_QUERY_EPOLL_CTL, VFS_POLL_QUERY_EPOLL_PURGE_OBJECT, VFS_POLL_QUERY_EPOLL_REF,
    VFS_POLL_QUERY_EPOLL_SNAPSHOT, VFS_POLL_QUERY_EPOLL_UNREF, VFS_POLL_QUERY_POLL,
    WAITSET_ABI_VERSION, WAITSET_GLOBAL_OBJECT_ID, WAITSET_MAX_INTERESTS, WAITSET_PROVIDER_VFSD,
};
use storage_fat::{FatDirEntry, FatNodeKind, FatVolume};
use vfsd::{
    cacheable_metadata_errno, checked_next_generation, checked_seek_position, checkpoint_path_key,
    mkdir_policy, persistent_mutation_status, should_materialize_file_cache, unlink_policy,
    valid_checkpoint_record, OpenDescriptionCheckpointWire, SeekPositionError, WaitSetInterestKey,
    WaitSetInterestRecord, WaitSetRegistry, WaitSetRegistryError, FILE_BYTES_CACHE_BUDGET_BYTES,
    VFSD_CHECKPOINT_HANDLE_TAG, VFSD_OPEN_CHECKPOINT_VERSION, VFSD_OPEN_MUTATION_FCNTL,
    VFSD_OPEN_MUTATION_GETDENTS, VFSD_OPEN_MUTATION_LSEEK, VFSD_OPEN_MUTATION_OPEN,
    VFSD_OPEN_MUTATION_READ, VFSD_OPEN_MUTATION_STABLE, VFSD_OPEN_MUTATION_STAGING,
};

mod block;
mod devmgrd;
mod early_system;
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
    linux_request_path, map_fat_error, normalize_absolute_path, path_inode, vfs_request_path,
    write_payload_bytes, write_vfs_payload_bytes,
};

// Linux errno constants (x86_64)
pub(crate) const ENOENT: i32 = 2;
pub(crate) const EIO: i32 = 5;
pub(crate) const ENOEXEC: i32 = 8;
pub(crate) const EAGAIN: i32 = 11;
pub(crate) const ENOMEM: i32 = 12;
pub(crate) const EBUSY: i32 = 16;
pub(crate) const EACCES: i32 = 13;
pub(crate) const EBADF: i32 = 9;
pub(crate) const EEXIST: i32 = 17;
pub(crate) const ENODEV: i32 = 19;
pub(crate) const ENOTDIR: i32 = 20;
pub(crate) const EISDIR: i32 = 21;
pub(crate) const EINVAL: i32 = 22;
pub(crate) const ENOSPC: i32 = 28;
pub(crate) const EROFS: i32 = 30;
pub(crate) const ENOSYS: i32 = 38;
pub(crate) const EOVERFLOW: i32 = 75;
pub(crate) const EOPNOTSUPP: i32 = 95;

const fn max_usize(a: usize, b: usize) -> usize {
    if a > b {
        a
    } else {
        b
    }
}

const VFS_RECV_BYTES: usize = max_usize(
    size_of::<VfsIpcRequest>(),
    max_usize(
        size_of::<VfsExecutableSnapshotRequest>(),
        max_usize(
            size_of::<CommercialMaxProtocolRequest>(),
            size_of::<LinuxSyscallOffloadRequest>(),
        ),
    ),
);

#[repr(align(8))]
// Tuple storage is accessed through raw IPC buffers; the wrapper's alignment
// rather than field reads is its purpose.
#[allow(dead_code)]
struct VfsRecvBuffer([u8; VFS_RECV_BYTES]);

static mut VFS_RESPONSE_SLOT: VfsIpcResponse = VfsIpcResponse {
    version: 0,
    op: 0,
    status: 0,
    handle_kind: 0,
    reserved0: 0,
    payload_len: 0,
    remote_id: 0,
    value: 0,
    aux: 0,
    payload: [0; VFS_IPC_PAYLOAD_CAPACITY],
};

const EXECUTABLE_SNAPSHOT_MAX_BYTES: u64 = 128 * 1024 * 1024;
const EXECUTABLE_SNAPSHOT_CACHE_BUDGET_BYTES: usize = 64 * 1024 * 1024;
const EXECUTABLE_SNAPSHOT_WRITE_CHUNK_BYTES: usize = 256 * 1024;

#[derive(Clone, Copy)]
struct ExecutableSnapshot {
    fd: i32,
    file_bytes: u64,
}

struct ExecutableSnapshotOpen {
    fd: i32,
    file_bytes: u64,
    close_after_reply: bool,
}

fn linux_response_for_op(op: u16) -> LinuxSyscallOffloadResponse {
    let mut response = MaybeUninit::<LinuxSyscallOffloadResponse>::uninit();
    let ptr = response.as_mut_ptr().cast::<u8>();
    for offset in 0..size_of::<LinuxSyscallOffloadResponse>() {
        unsafe { core::ptr::write_volatile(ptr.add(offset), 0) };
    }
    let mut response = unsafe { response.assume_init() };
    response.version = SYSCALL_OFFLOAD_ABI_VERSION;
    response.op = op;
    response
}

fn reset_vfs_response_slot(op: u16) -> *mut VfsIpcResponse {
    let response = core::ptr::addr_of_mut!(VFS_RESPONSE_SLOT);
    let ptr = response.cast::<u8>();
    unsafe {
        core::ptr::write_bytes(ptr, 0, size_of::<VfsIpcResponse>());
        (*response).version = VFS_IPC_ABI_VERSION;
        (*response).op = op;
    }
    response
}

// Linux lseek whence constants
// Linux open flags (x86_64)
const O_CREAT: u64 = 0o100;
const O_TRUNC: u64 = 0o1000;
const O_DIRECTORY: u64 = 0o200000;
const CHECKPOINT_EPOLL_TAG: u64 = 1;
type CheckpointRevisionKey = (u64, u64, u64, u64);

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

#[cfg(all(not(test), not(clippy)))]
#[panic_handler]
fn panic(_info: &PanicInfo<'_>) -> ! {
    loop {}
}

#[no_mangle]
pub extern "C" fn rust_eh_personality() {}

fn service_main() {
    let mut state = VfsState::new();
    if state.restore_waitset_checkpoint().is_err() {
        ipc::debug_line("vfsd: checkpoint replay failed");
        return;
    }
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
    serve(endpoint as u64, state);
}

fn serve(endpoint: u64, mut state: VfsState) {
    loop {
        let mut request = MaybeUninit::<VfsRecvBuffer>::uninit();
        let mut reply_cap = 0_u64;
        let mut sender_pid = 0_u64;
        let mut sender_tid = 0_u64;
        let received = unsafe {
            ipc::recv_with_sender(
                endpoint,
                request.as_mut_ptr().cast::<u8>(),
                VFS_RECV_BYTES,
                &mut reply_cap as *mut u64,
                &mut sender_pid as *mut u64,
                &mut sender_tid as *mut u64,
            )
        };
        if received < 0 {
            rustos_svc_runtime::syscall::sleep_millis(1);
            continue;
        }

        let request = unsafe {
            core::slice::from_raw_parts(request.as_ptr() as *const u8, received as usize)
        };
        let reply = if received as usize == size_of::<VfsExecutableSnapshotRequest>() {
            let request = unsafe { &*request.as_ptr().cast::<VfsExecutableSnapshotRequest>() };
            reply_executable_snapshot(&mut state, reply_cap, sender_pid, sender_tid, request)
        } else if received as usize == size_of::<VfsIpcRequest>() {
            let request = unsafe { &*request.as_ptr().cast::<VfsIpcRequest>() };
            let response = reset_vfs_response_slot(request.op);
            if request.pid != sender_pid || request.tid != sender_tid {
                unsafe { (*response).status = EACCES };
            } else {
                state.handle_vfs_request(request, unsafe { &mut *response });
            }
            unsafe {
                ipc::reply(
                    reply_cap,
                    response.cast::<u8>(),
                    size_of::<VfsIpcResponse>(),
                )
            }
        } else if received as usize == size_of::<CommercialMaxProtocolRequest>() {
            let request = unsafe { &*request.as_ptr().cast::<CommercialMaxProtocolRequest>() };
            let response = if request.header.subject_pid != sender_pid
                || request.header.subject_tid != sender_tid
            {
                CommercialMaxProtocolResponse {
                    header: request.header,
                    status: EACCES,
                    ..CommercialMaxProtocolResponse::default()
                }
            } else {
                state.handle_commercial_request(request)
            };
            unsafe {
                ipc::reply(
                    reply_cap,
                    (&response as *const CommercialMaxProtocolResponse).cast::<u8>(),
                    size_of::<CommercialMaxProtocolResponse>(),
                )
            }
        } else if received as usize == size_of::<LinuxSyscallOffloadRequest>() {
            let request = unsafe { &*request.as_ptr().cast::<LinuxSyscallOffloadRequest>() };
            let response = if request.pid != sender_pid || request.tid != sender_tid {
                LinuxSyscallOffloadResponse {
                    op: request.op,
                    status: EACCES,
                    ..LinuxSyscallOffloadResponse::default()
                }
            } else {
                state.handle_linux_request(request)
            };
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

fn reply_executable_snapshot(
    state: &mut VfsState,
    reply_cap: u64,
    sender_pid: u64,
    sender_tid: u64,
    request: &VfsExecutableSnapshotRequest,
) -> i64 {
    let mut response = VfsExecutableSnapshotResponse {
        version: VFS_EXECUTABLE_SNAPSHOT_ABI_VERSION,
        op: VFS_EXECUTABLE_SNAPSHOT_OP_OPEN,
        ..VfsExecutableSnapshotResponse::default()
    };
    if request.requester_pid != sender_pid
        || request.requester_tid != sender_tid
        || rustos_svc_runtime::ipc::validate_service_owner(IPC_SERVICE_LOADERD, sender_pid) < 0
    {
        ipc::debug_line("vfsd: executable snapshot rejected stage=identity");
        response.status = EACCES;
        return unsafe {
            ipc::reply(
                reply_cap,
                (&response as *const VfsExecutableSnapshotResponse).cast::<u8>(),
                size_of::<VfsExecutableSnapshotResponse>(),
            )
        };
    }

    let snapshot = match state.open_executable_snapshot(request) {
        Ok(snapshot) => snapshot,
        Err(errno) => {
            ipc::debug_line(
                format!("vfsd: executable snapshot rejected stage=open errno={errno}").as_str(),
            );
            response.status = errno;
            return unsafe {
                ipc::reply(
                    reply_cap,
                    (&response as *const VfsExecutableSnapshotResponse).cast::<u8>(),
                    size_of::<VfsExecutableSnapshotResponse>(),
                )
            };
        }
    };
    response.file_bytes = snapshot.file_bytes;
    response.mount_generation = state.mount_generation;
    let send_fd = snapshot.fd as u64;
    let args = IpcReplyWithHandlesArgs {
        reply_cap,
        response_ptr: (&response as *const VfsExecutableSnapshotResponse) as u64,
        response_len: size_of::<VfsExecutableSnapshotResponse>() as u64,
        send_fds_ptr: (&send_fd as *const u64) as u64,
        send_fd_count: 1,
        reserved0: 0,
        reserved1: 0,
    };
    let reply = unsafe {
        rustos_svc_runtime::syscall::syscall1(
            SYS_RUSTOS_IPC_REPLY_WITH_HANDLES,
            (&args as *const IpcReplyWithHandlesArgs) as u64,
        )
    };
    if snapshot.close_after_reply {
        close_fd(snapshot.fd);
    }
    reply
}

struct VfsState {
    volume: Option<FatVolume<BootBlockDevice>>,
    cwd: BTreeMap<u64, String>,
    handles: BTreeMap<u64, RemoteHandle>,
    /// Positive + negative metadata cache. `Ok(_)` is a resolved entry;
    /// `Err(errno)` is a negative cache (e.g. ENOENT) so back-to-back stat()s
    /// of common missing libc paths return without touching FAT. The whole map
    /// is dropped whenever `mount_generation` changes.
    metadata_cache: BTreeMap<String, Result<Metadata, i32>>,
    /// Cached directory listings, keyed by absolute path. Linux startup
    /// re-reads `/`, `/dev`, library directories, etc.; FAT traversal per call
    /// is expensive enough to dominate boot time when libc walks PATH.
    dir_entries_cache: BTreeMap<String, Vec<DirEntry>>,
    /// Complete bytes for bounded read-only files on the current mount
    /// generation. Persistent mutation is not implemented by vfsd, and every
    /// remount invalidates this cache before the replacement volume is used.
    file_bytes_cache: BTreeMap<String, Vec<u8>>,
    file_bytes_cache_bytes: usize,
    /// Terminally sealed executable images owned by the current mount
    /// generation. Handle transfer duplicates the descriptor into loaderd;
    /// keeping one vfsd reference lets common interpreters/DLLs be reused
    /// without re-reading the storage DVM on every process launch.
    executable_snapshot_cache: BTreeMap<String, ExecutableSnapshot>,
    executable_snapshot_cache_bytes: usize,
    epolls: WaitSetRegistry,
    checkpoint_revisions: BTreeMap<CheckpointRevisionKey, u64>,
    checkpoint_operations: BTreeMap<CheckpointRevisionKey, (u64, u64)>,
    checkpoint_records: BTreeMap<CheckpointRevisionKey, ServiceCheckpointRecordWire>,
    readiness_generation: u64,
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
    status_flags: u64,
    last_mutation: u16,
    last_start: u64,
    last_result: u64,
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

include!("state_checkpoint.rs");
include!("state_requests.rs");
include!("state_files.rs");
include!("state_storage.rs");

fn create_terminally_sealed_snapshot(_path: &str, bytes: &[u8]) -> Result<i32, i32> {
    let name = CString::new("vfsd-executable-snapshot").map_err(|_| EINVAL)?;
    let fd = unsafe {
        rustos_svc_runtime::syscall::syscall2(
            linux_abi::SYS_MEMFD_CREATE,
            name.as_ptr() as u64,
            u64::from(linux_abi::MFD_CLOEXEC | linux_abi::MFD_ALLOW_SEALING),
        )
    } as i32;
    if fd < 0 {
        return Err(-fd);
    }

    let result = (|| {
        let mut written = 0_usize;
        while written < bytes.len() {
            let end = written
                .saturating_add(EXECUTABLE_SNAPSHOT_WRITE_CHUNK_BYTES)
                .min(bytes.len());
            let status = unsafe {
                rustos_svc_runtime::syscall::syscall3(
                    linux_abi::SYS_WRITE,
                    fd as u64,
                    bytes[written..end].as_ptr() as u64,
                    (end - written) as u64,
                )
            };
            if status < 0 {
                return Err((-status) as i32);
            }
            let count = usize::try_from(status).map_err(|_| EOVERFLOW)?;
            if count == 0 || count > end - written {
                return Err(EIO);
            }
            written = written.checked_add(count).ok_or(EOVERFLOW)?;
        }

        let seals = linux_abi::F_SEAL_WRITE
            | linux_abi::F_SEAL_GROW
            | linux_abi::F_SEAL_SHRINK
            | linux_abi::F_SEAL_SEAL;
        let status = unsafe {
            rustos_svc_runtime::syscall::syscall3(
                linux_abi::SYS_FCNTL,
                fd as u64,
                linux_abi::F_ADD_SEALS as u64,
                seals as u64,
            )
        };
        if status < 0 {
            return Err((-status) as i32);
        }
        Ok(())
    })();
    if let Err(errno) = result {
        close_fd(fd);
        return Err(errno);
    }
    Ok(fd)
}

fn close_fd(fd: i32) {
    if fd >= 0 {
        unsafe {
            let _ = rustos_svc_runtime::syscall::syscall1(linux_abi::SYS_CLOSE, fd as u64);
        }
    }
}

fn copy_file_cache_range(bytes: &[u8], start: u64, dest: &mut [u8]) -> usize {
    let Ok(start) = usize::try_from(start) else {
        return 0;
    };
    let Some(source) = bytes.get(start..) else {
        return 0;
    };
    let count = source.len().min(dest.len());
    dest[..count].copy_from_slice(&source[..count]);
    count
}

fn call_rootd_checkpoint(
    op: u16,
    cursor: u64,
    record: Option<&ServiceCheckpointRecordWire>,
) -> Result<CommercialMaxProtocolResponse, i32> {
    let endpoint = ipc::lookup_service_endpoint(IPC_SERVICE_ROOTD);
    if endpoint < 0 {
        return Err((-endpoint) as i32);
    }
    let pid = unsafe { rustos_svc_runtime::syscall::syscall0(linux_abi::SYS_GETPID) };
    let tid = unsafe { rustos_svc_runtime::syscall::syscall0(linux_abi::SYS_GETTID) };
    if pid <= 0 || tid <= 0 {
        return Err(EIO);
    }
    let mut request = CommercialMaxProtocolRequest::default();
    request.header.version = COMMERCIAL_MAX_PROTOCOL_ABI_VERSION;
    request.header.protocol = COMMERCIAL_MAX_PROTOCOL_ROOTD_SUPERVISOR;
    request.header.op = op;
    request.header.subject_pid = pid as u64;
    request.header.subject_tid = tid as u64;
    request.arg0 = IPC_SERVICE_VFSD;
    request.arg1 = cursor;
    if let Some(record) = record {
        let bytes = unsafe {
            core::slice::from_raw_parts(
                (record as *const ServiceCheckpointRecordWire).cast::<u8>(),
                size_of::<ServiceCheckpointRecordWire>(),
            )
        };
        request.payload_len = bytes.len() as u32;
        request.payload[..bytes.len()].copy_from_slice(bytes);
    }
    let mut response = CommercialMaxProtocolResponse::default();
    let received = unsafe {
        ipc::call(
            endpoint as u64,
            (&request as *const CommercialMaxProtocolRequest).cast::<u8>(),
            size_of::<CommercialMaxProtocolRequest>(),
            (&mut response as *mut CommercialMaxProtocolResponse).cast::<u8>(),
            size_of::<CommercialMaxProtocolResponse>(),
        )
    };
    let valid_envelope = response.is_valid_envelope_for(&request);
    if received as usize != size_of::<CommercialMaxProtocolResponse>()
        || !valid_envelope
        || response.descriptor_count != 0
    {
        debug_line(&format!(
            "vfsd: checkpoint transport failed op={op} received={received} expected={} envelope={} descriptors={}",
            size_of::<CommercialMaxProtocolResponse>(),
            u8::from(valid_envelope),
            response.descriptor_count
        ));
        return Err(EIO);
    }
    if response.status != 0 {
        debug_line(&format!(
            "vfsd: checkpoint rejected op={op} errno={} key={:016x}:{:016x}",
            response.status,
            record.map_or(0, |value| value.key_hi),
            record.map_or(0, |value| value.key_lo)
        ));
        return Err(response.status);
    }
    Ok(response)
}

fn debug_line(message: &str) {
    let bytes = message.as_bytes();
    let len = bytes.len().min(1023);
    let mut line = [0_u8; 1024];
    line[..len].copy_from_slice(&bytes[..len]);
    line[len] = b'\n';
    let _ = unsafe {
        rustos_svc_runtime::syscall::syscall2(
            linux_abi::SYS_RUSTOS_DEBUG_PRINT,
            line.as_ptr() as u64,
            (len + 1) as u64,
        )
    };
}

fn checkpoint_revision_key(record: &ServiceCheckpointRecordWire) -> CheckpointRevisionKey {
    (
        record.parent_hi,
        record.parent_lo,
        record.key_hi,
        record.key_lo,
    )
}

fn checkpoint_suboperation(request: &VfsIpcRequest, ordinal: u64) -> (u64, u64) {
    let mut hi = request.operation_hi ^ 0x5646_5343_484b_0000_u64.rotate_left(ordinal as u32);
    let mut lo = request
        .operation_lo
        .wrapping_add(ordinal.wrapping_mul(0x9e37_79b9_7f4a_7c15));
    if hi == 0 && lo == 0 {
        lo = 1;
    }
    // Keep both assignments explicit so future wire changes cannot silently
    // drop half of the base operation identity.
    hi ^= ordinal.rotate_left(17);
    if hi == 0 && lo == 0 {
        lo = 1;
    }
    (hi, lo)
}

fn checkpoint_handle_record(
    remote_id: u64,
    handle: &RemoteHandle,
    last_mutation: u16,
    last_start: u64,
    last_result: u64,
) -> Result<ServiceCheckpointRecordWire, i32> {
    if remote_id == 0
        || handle.refs != 1
        || handle.path.is_empty()
        || handle.path.len() > VFS_IPC_PATH_CAPACITY
    {
        return Err(EINVAL);
    }
    let mut record = ServiceCheckpointRecordWire {
        key_hi: remote_id,
        key_lo: VFSD_CHECKPOINT_HANDLE_TAG,
        ..ServiceCheckpointRecordWire::default()
    };
    let path_len = u32::try_from(handle.path.len()).map_err(|_| EOVERFLOW)?;
    let refs = u32::try_from(handle.refs).map_err(|_| EOVERFLOW)?;
    let wire = OpenDescriptionCheckpointWire {
        version: VFSD_OPEN_CHECKPOINT_VERSION,
        kind: handle_kind_u16(handle.kind),
        last_mutation,
        reserved0: 0,
        path_len,
        refs,
        cursor: handle.cursor,
        len: handle.len,
        status_flags: handle.status_flags,
        content_identity: path_inode(handle.path.as_bytes()),
        last_start,
        last_result,
    };
    let bytes = unsafe {
        core::slice::from_raw_parts(
            (&wire as *const OpenDescriptionCheckpointWire).cast::<u8>(),
            size_of::<OpenDescriptionCheckpointWire>(),
        )
    };
    record.value_len = bytes.len() as u32;
    record.value[..bytes.len()].copy_from_slice(bytes);
    Ok(record)
}

fn checkpoint_handle_tombstone(remote_id: u64) -> Result<ServiceCheckpointRecordWire, i32> {
    if remote_id == 0 {
        return Err(EINVAL);
    }
    Ok(ServiceCheckpointRecordWire {
        key_hi: remote_id,
        key_lo: VFSD_CHECKPOINT_HANDLE_TAG,
        flags: SERVICE_CHECKPOINT_FLAG_TOMBSTONE,
        ..ServiceCheckpointRecordWire::default()
    })
}

fn checkpoint_path_record(
    remote_id: u64,
    chunk_index: usize,
    bytes: &[u8],
) -> Result<ServiceCheckpointRecordWire, i32> {
    if bytes.is_empty() || bytes.len() > SERVICE_CHECKPOINT_VALUE_CAPACITY {
        return Err(EINVAL);
    }
    let (key_hi, key_lo) = checkpoint_path_key(remote_id, chunk_index).ok_or(EOVERFLOW)?;
    let mut record = ServiceCheckpointRecordWire {
        key_hi,
        key_lo,
        parent_hi: remote_id,
        parent_lo: VFSD_CHECKPOINT_HANDLE_TAG,
        value_len: bytes.len() as u32,
        ..ServiceCheckpointRecordWire::default()
    };
    record.value[..bytes.len()].copy_from_slice(bytes);
    Ok(record)
}

fn remote_kind_from_u16(kind: u16) -> Option<RemoteKind> {
    match kind {
        rustos_user_abi::syscall::VFS_IPC_HANDLE_KIND_FILE => Some(RemoteKind::File),
        rustos_user_abi::syscall::VFS_IPC_HANDLE_KIND_DIR => Some(RemoteKind::Directory),
        rustos_user_abi::syscall::VFS_IPC_HANDLE_KIND_DEVICE => Some(RemoteKind::Device),
        _ => None,
    }
}

fn pack_u32_pair(first: usize, second: usize) -> Result<u64, i32> {
    let first = u32::try_from(first).map_err(|_| EOVERFLOW)?;
    let second = u32::try_from(second).map_err(|_| EOVERFLOW)?;
    Ok((u64::from(first) << 32) | u64::from(second))
}

fn unpack_u32_pair(value: u64) -> (u32, u32) {
    ((value >> 32) as u32, value as u32)
}

fn cursor_mutation_prepared(handle: &RemoteHandle) -> bool {
    matches!(
        handle.last_mutation,
        VFSD_OPEN_MUTATION_READ | VFSD_OPEN_MUTATION_GETDENTS
    )
}

fn checkpoint_epoll_key(token: u64) -> CheckpointRevisionKey {
    (0, 0, token, CHECKPOINT_EPOLL_TAG)
}

fn checkpoint_handle_key(token: u64) -> CheckpointRevisionKey {
    (0, 0, token, VFSD_CHECKPOINT_HANDLE_TAG)
}

fn checkpoint_epoll_record(token: u64, refs: u64, tombstone: bool) -> ServiceCheckpointRecordWire {
    let mut record = ServiceCheckpointRecordWire {
        key_hi: token,
        key_lo: CHECKPOINT_EPOLL_TAG,
        ..ServiceCheckpointRecordWire::default()
    };
    if tombstone {
        record.flags = SERVICE_CHECKPOINT_FLAG_TOMBSTONE;
    } else {
        record.value_len = 8;
        record.value[..8].copy_from_slice(&refs.to_le_bytes());
    }
    record
}

fn checkpoint_interest_record(
    epoll_token: u64,
    interest: WaitSetInterestRecord,
    tombstone: bool,
) -> ServiceCheckpointRecordWire {
    let (key_hi, key_lo) = checkpoint_interest_key(&interest);
    let mut record = ServiceCheckpointRecordWire {
        key_hi,
        key_lo,
        parent_hi: epoll_token,
        parent_lo: CHECKPOINT_EPOLL_TAG,
        ..ServiceCheckpointRecordWire::default()
    };
    if tombstone {
        record.flags = SERVICE_CHECKPOINT_FLAG_TOMBSTONE;
        return record;
    }
    let wire = WaitSetInterestWire {
        abi_version: WAITSET_ABI_VERSION,
        provider: interest.key.provider,
        flags: 0,
        target_fd: interest.key.target_fd,
        object_id: interest.key.object_id,
        provider_epoch: interest.provider_epoch,
        events: interest.events,
        reserved0: 0,
        data: interest.data,
    };
    let bytes = unsafe {
        core::slice::from_raw_parts(
            (&wire as *const WaitSetInterestWire).cast::<u8>(),
            size_of::<WaitSetInterestWire>(),
        )
    };
    record.value_len = bytes.len() as u32;
    record.value[..bytes.len()].copy_from_slice(bytes);
    record
}

fn checkpoint_interest_key(interest: &WaitSetInterestRecord) -> (u64, u64) {
    (
        interest.key.object_id,
        (u64::from(interest.key.provider) << 48) | interest.key.target_fd,
    )
}

fn waitset_interest_from_wire(wire: &WaitSetInterestWire) -> Option<WaitSetInterestRecord> {
    if wire.abi_version != WAITSET_ABI_VERSION
        || wire.flags != 0
        || wire.reserved0 != 0
        || wire.provider == 0
        || wire.provider > rustos_user_abi::syscall::WAITSET_PROVIDER_MAX
        || wire.target_fd > u16::MAX as u64
        || wire.object_id == 0
        || wire.provider_epoch == 0
    {
        return None;
    }
    Some(WaitSetInterestRecord {
        key: WaitSetInterestKey {
            target_fd: wire.target_fd,
            provider: wire.provider,
            object_id: wire.object_id,
        },
        provider_epoch: wire.provider_epoch,
        events: wire.events,
        data: wire.data,
    })
}

fn poll_ready_bits(requested: u32) -> u32 {
    requested
        & (linux_abi::EPOLLIN
            | linux_abi::EPOLLPRI
            | linux_abi::EPOLLOUT
            | linux_abi::EPOLLERR
            | linux_abi::EPOLLHUP)
}

fn waitset_registry_status(err: WaitSetRegistryError) -> i32 {
    match err {
        WaitSetRegistryError::Exists => EEXIST,
        WaitSetRegistryError::NotFound => ENOENT,
        WaitSetRegistryError::Capacity => ENOSPC,
        WaitSetRegistryError::Overflow => EOVERFLOW,
    }
}

fn epoll_interest_from_request(request: &VfsIpcRequest) -> Option<WaitSetInterestRecord> {
    let wire_size = size_of::<WaitSetInterestWire>();
    if request.payload_len as usize != wire_size {
        return None;
    }
    let wire = unsafe {
        core::ptr::read_unaligned(request.payload.as_ptr().cast::<WaitSetInterestWire>())
    };
    if wire.target_fd != request.fd {
        return None;
    }
    waitset_interest_from_wire(&wire)
}

fn validate_commercial_request(request: &CommercialMaxProtocolRequest) -> Result<(), i32> {
    if !request.has_valid_envelope() || request.header.protocol != COMMERCIAL_MAX_PROTOCOL_VFSD {
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

fn device_access_for_path(path: &str) -> u64 {
    match path {
        "/dev/input0" => rustos_user_abi::syscall::INPUTD_ACCESS_NATIVE as u64,
        "/dev/input/event0" => rustos_user_abi::syscall::INPUTD_ACCESS_EVDEV as u64,
        "/dev/dri/card0" => rustos_user_abi::syscall::VFS_DEVICE_ACCESS_DRM_COMPAT as u64,
        _ => 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rustos_user_abi::syscall::IPC_MAX_INLINE_BYTES;

    #[test]
    fn vfs_requests_validate_inline_shape() {
        let request = VfsIpcRequest {
            operation_lo: 1,
            ..VfsIpcRequest::default()
        };
        assert_eq!(validate_vfs_request(&request), Ok(()));
        assert!(size_of::<VfsIpcRequest>() <= IPC_MAX_INLINE_BYTES);
        assert_eq!(size_of::<VfsIpcResponse>(), IPC_MAX_INLINE_BYTES);
    }

    #[test]
    fn waitset_checkpoint_keys_are_parent_scoped_and_round_trip_wire() {
        let interest = WaitSetInterestRecord {
            key: WaitSetInterestKey {
                target_fd: 7,
                provider: rustos_user_abi::syscall::WAITSET_PROVIDER_NETD,
                object_id: 0xfeed_beef,
            },
            provider_epoch: 9,
            events: linux_abi::EPOLLIN,
            data: 11,
        };
        let record = checkpoint_interest_record(44, interest, false);
        assert_eq!(record.parent_hi, 44);
        assert_eq!(record.parent_lo, CHECKPOINT_EPOLL_TAG);
        assert_eq!(record.value_len as usize, size_of::<WaitSetInterestWire>());
        let wire = unsafe {
            core::ptr::read_unaligned(record.value.as_ptr().cast::<WaitSetInterestWire>())
        };
        assert_eq!(waitset_interest_from_wire(&wire), Some(interest));
        assert_eq!(size_of::<ServiceCheckpointRecordWire>(), 128);
    }

    #[test]
    fn normalizes_cwd_relative_paths() {
        assert_eq!(
            normalize_absolute_path("/usr/lib", "../bin/app").unwrap(),
            "/usr/bin/app"
        );
    }

    #[test]
    fn cached_file_ranges_are_exact_and_eof_bounded() {
        let bytes = b"0123456789";
        let mut out = [0xaa; 4];
        assert_eq!(copy_file_cache_range(bytes, 3, &mut out), 4);
        assert_eq!(&out, b"3456");
        assert_eq!(copy_file_cache_range(bytes, 9, &mut out), 1);
        assert_eq!(out[0], b'9');
        assert_eq!(copy_file_cache_range(bytes, 10, &mut out), 0);
        assert_eq!(copy_file_cache_range(bytes, u64::MAX, &mut out), 0);
    }
}
// RING3-MIGRATION-REFERENCE END: vfsd ring3-owned VFS policy.
