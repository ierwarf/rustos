// RING3-MIGRATION-REFERENCE START: vfsd is the ring3 owner for VFS policy.
// Marker is restored to close historical audits; this file is not ring0 debt.
#![no_std]
#![no_main]

extern crate alloc;

use alloc::collections::BTreeMap;
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
    CommercialMaxProtocolRequest, CommercialMaxProtocolResponse, ServiceCheckpointRecordWire,
    VfsIpcRequest, VfsIpcResponse, WaitSetInterestWire, WaitSetSignalBrokerArgs,
    COMMERCIAL_MAX_PROTOCOL_ABI_VERSION, COMMERCIAL_MAX_PROTOCOL_MAX_DESCRIPTORS,
    COMMERCIAL_MAX_PROTOCOL_ROOTD_SUPERVISOR, COMMERCIAL_MAX_PROTOCOL_VFSD,
    COMMERCIAL_MAX_ROOTD_OP_SERVICE_CHECKPOINT_COMPACT,
    COMMERCIAL_MAX_ROOTD_OP_SERVICE_CHECKPOINT_MUTATE,
    COMMERCIAL_MAX_ROOTD_OP_SERVICE_CHECKPOINT_SCAN, COMMERCIAL_MAX_VFSD_OP_DIRECTORY_CURSOR,
    COMMERCIAL_MAX_VFSD_OP_FD_TABLE_PLAN, COMMERCIAL_MAX_VFSD_OP_FILE_CURSOR,
    COMMERCIAL_MAX_VFSD_OP_METADATA_POLICY, COMMERCIAL_MAX_VFSD_OP_MOUNT_GRAPH,
    COMMERCIAL_MAX_VFSD_OP_PATH_RESOLVE, IPC_SERVICE_ROOTD, IPC_SERVICE_VFSD, LINUX_STATX_SIZE,
    LINUX_STAT_SIZE, SERVICE_CHECKPOINT_FLAG_TOMBSTONE, SERVICE_CHECKPOINT_VALUE_CAPACITY,
    SYSCALL_OFFLOAD_ABI_VERSION, SYSCALL_OFFLOAD_OP_LINUX_ACCESS, SYSCALL_OFFLOAD_OP_LINUX_CHDIR,
    SYSCALL_OFFLOAD_OP_LINUX_CLOSE, SYSCALL_OFFLOAD_OP_LINUX_DUP, SYSCALL_OFFLOAD_OP_LINUX_FCNTL,
    SYSCALL_OFFLOAD_OP_LINUX_GETCWD, SYSCALL_OFFLOAD_OP_LINUX_GETDENTS64,
    SYSCALL_OFFLOAD_OP_LINUX_MKDIR, SYSCALL_OFFLOAD_OP_LINUX_MOUNT,
    SYSCALL_OFFLOAD_OP_LINUX_NEWFSTATAT, SYSCALL_OFFLOAD_OP_LINUX_OPENAT,
    SYSCALL_OFFLOAD_OP_LINUX_READLINKAT, SYSCALL_OFFLOAD_OP_LINUX_STATX,
    SYSCALL_OFFLOAD_OP_LINUX_UMOUNT2, SYSCALL_OFFLOAD_OP_LINUX_UNLINKAT,
    SYS_RUSTOS_WAITSET_SIGNAL_BROKER, VFS_CURSOR_SETTLE_CANCEL, VFS_CURSOR_SETTLE_COMMIT,
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
use storage_fat::{FatDirEntry, FatDisk, FatNodeKind, FatVolume};
use vfsd::{
    cacheable_metadata_errno, checked_next_generation, checked_seek_position, checkpoint_path_key,
    mkdir_policy, persistent_mutation_status, unlink_policy, valid_checkpoint_record,
    OpenDescriptionCheckpointWire, SeekPositionError, WaitSetInterestKey, WaitSetInterestRecord,
    WaitSetRegistry, WaitSetRegistryError, VFSD_CHECKPOINT_HANDLE_TAG,
    VFSD_OPEN_CHECKPOINT_VERSION, VFSD_OPEN_MUTATION_FCNTL, VFSD_OPEN_MUTATION_GETDENTS,
    VFSD_OPEN_MUTATION_LSEEK, VFSD_OPEN_MUTATION_OPEN, VFSD_OPEN_MUTATION_READ,
    VFSD_OPEN_MUTATION_STABLE, VFSD_OPEN_MUTATION_STAGING,
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
pub(crate) const EAGAIN: i32 = 11;
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
        size_of::<CommercialMaxProtocolRequest>(),
        size_of::<LinuxSyscallOffloadRequest>(),
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
        let reply = if received as usize == size_of::<VfsIpcRequest>() {
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

impl VfsState {
    fn new() -> Self {
        Self {
            volume: None,
            cwd: BTreeMap::new(),
            handles: BTreeMap::new(),
            metadata_cache: BTreeMap::new(),
            dir_entries_cache: BTreeMap::new(),
            epolls: WaitSetRegistry::default(),
            checkpoint_revisions: BTreeMap::new(),
            checkpoint_operations: BTreeMap::new(),
            checkpoint_records: BTreeMap::new(),
            readiness_generation: 1,
            next_handle: 1,
            mount_generation: 1,
            cache_generation: 1,
        }
    }

    fn restore_waitset_checkpoint(&mut self) -> Result<(), i32> {
        let mut cursor = 0_u64;
        let mut records = Vec::new();
        loop {
            let response = call_rootd_checkpoint(
                COMMERCIAL_MAX_ROOTD_OP_SERVICE_CHECKPOINT_SCAN,
                cursor,
                None,
            )?;
            let wire_size = size_of::<ServiceCheckpointRecordWire>();
            let record_count = response.payload_len as usize / wire_size;
            if response.payload_len as usize % wire_size != 0
                || response.value1 as usize != record_count
                || cursor
                    .checked_add(record_count as u64)
                    .is_none_or(|next| response.value0 != next)
            {
                return Err(EIO);
            }
            for offset in (0..response.payload_len as usize).step_by(wire_size) {
                let record = unsafe {
                    core::ptr::read_unaligned(
                        response.payload[offset..]
                            .as_ptr()
                            .cast::<ServiceCheckpointRecordWire>(),
                    )
                };
                let key = checkpoint_revision_key(&record);
                if !valid_checkpoint_record(&record)
                    || self
                        .checkpoint_revisions
                        .insert(key, record.revision)
                        .is_some()
                    || self
                        .checkpoint_operations
                        .insert(key, (record.operation_hi, record.operation_lo))
                        .is_some()
                    || self.checkpoint_records.insert(key, record).is_some()
                {
                    return Err(EIO);
                }
                records.push(record);
            }
            if response.value1 == 0 {
                break;
            }
            if response.value0 == cursor {
                return Err(EIO);
            }
            cursor = response.value0;
        }

        for record in records.iter().filter(|record| {
            record.parent_hi == 0
                && record.parent_lo == 0
                && record.key_lo == CHECKPOINT_EPOLL_TAG
                && record.flags & SERVICE_CHECKPOINT_FLAG_TOMBSTONE == 0
        }) {
            if record.value_len != 8 {
                return Err(EIO);
            }
            let refs = u64::from_le_bytes(record.value[..8].try_into().map_err(|_| EIO)?);
            self.epolls.restore(record.key_hi, refs).map_err(|_| EIO)?;
        }
        for record in records.iter().filter(|record| {
            record.parent_lo == CHECKPOINT_EPOLL_TAG
                && record.flags & SERVICE_CHECKPOINT_FLAG_TOMBSTONE == 0
        }) {
            if record.value_len as usize != size_of::<WaitSetInterestWire>() {
                return Err(EIO);
            }
            let wire = unsafe {
                core::ptr::read_unaligned(record.value.as_ptr().cast::<WaitSetInterestWire>())
            };
            let interest = waitset_interest_from_wire(&wire).ok_or(EIO)?;
            let (key_hi, key_lo) = checkpoint_interest_key(&interest);
            if record.key_hi != key_hi || record.key_lo != key_lo {
                return Err(EIO);
            }
            self.epolls
                .add(record.parent_hi, interest)
                .map_err(|_| EIO)?;
        }
        self.restore_open_descriptions(&records)?;
        Ok(())
    }

    fn restore_open_descriptions(
        &mut self,
        records: &[ServiceCheckpointRecordWire],
    ) -> Result<(), i32> {
        for record in records.iter().filter(|record| {
            record.parent_hi == 0
                && record.parent_lo == 0
                && record.key_lo == VFSD_CHECKPOINT_HANDLE_TAG
                && record.flags & SERVICE_CHECKPOINT_FLAG_TOMBSTONE == 0
        }) {
            if record.value_len as usize != size_of::<OpenDescriptionCheckpointWire>() {
                return Err(EIO);
            }
            let wire = unsafe {
                core::ptr::read_unaligned(
                    record
                        .value
                        .as_ptr()
                        .cast::<OpenDescriptionCheckpointWire>(),
                )
            };
            if !wire.valid(VFS_IPC_PATH_CAPACITY) {
                return Err(EIO);
            }
            // A staging parent is a recoverable interrupted OPEN. It remains
            // unavailable until an exact request completes every path chunk
            // and advances the parent to OPEN.
            if wire.last_mutation == VFSD_OPEN_MUTATION_STAGING {
                continue;
            }
            let path_len = wire.path_len as usize;
            let chunk_count = path_len.div_ceil(SERVICE_CHECKPOINT_VALUE_CAPACITY);
            let mut path_bytes = Vec::with_capacity(path_len);
            for chunk_index in 0..chunk_count {
                let (key_hi, key_lo) =
                    checkpoint_path_key(record.key_hi, chunk_index).ok_or(EIO)?;
                let child = records
                    .iter()
                    .find(|candidate| {
                        candidate.parent_hi == record.key_hi
                            && candidate.parent_lo == VFSD_CHECKPOINT_HANDLE_TAG
                            && candidate.key_hi == key_hi
                            && candidate.key_lo == key_lo
                            && candidate.flags & SERVICE_CHECKPOINT_FLAG_TOMBSTONE == 0
                    })
                    .ok_or(EIO)?;
                let expected = (path_len - path_bytes.len()).min(SERVICE_CHECKPOINT_VALUE_CAPACITY);
                if child.value_len as usize != expected {
                    return Err(EIO);
                }
                path_bytes.extend_from_slice(&child.value[..expected]);
            }
            let live_children = records
                .iter()
                .filter(|candidate| {
                    candidate.parent_hi == record.key_hi
                        && candidate.parent_lo == VFSD_CHECKPOINT_HANDLE_TAG
                        && candidate.flags & SERVICE_CHECKPOINT_FLAG_TOMBSTONE == 0
                })
                .count();
            if live_children != chunk_count || path_bytes.len() != path_len {
                return Err(EIO);
            }
            let path = str::from_utf8(&path_bytes).map_err(|_| EIO)?.to_string();
            if !path.starts_with('/') || path_inode(path.as_bytes()) != wire.content_identity {
                return Err(EIO);
            }
            let kind = remote_kind_from_u16(wire.kind).ok_or(EIO)?;
            if self
                .handles
                .insert(
                    record.key_hi,
                    RemoteHandle {
                        kind,
                        path,
                        cursor: wire.cursor,
                        len: wire.len,
                        refs: u64::from(wire.refs),
                        status_flags: wire.status_flags,
                        last_mutation: wire.last_mutation,
                        last_start: wire.last_start,
                        last_result: wire.last_result,
                    },
                )
                .is_some()
            {
                return Err(EIO);
            }
        }
        Ok(())
    }

    fn compact_checkpoint_proof(&mut self, proof: ServiceCheckpointRecordWire) -> Result<(), i32> {
        call_rootd_checkpoint(
            COMMERCIAL_MAX_ROOTD_OP_SERVICE_CHECKPOINT_COMPACT,
            0,
            Some(&proof),
        )?;
        self.forget_compacted_record(proof);
        Ok(())
    }

    fn forget_compacted_record(&mut self, proof: ServiceCheckpointRecordWire) {
        let parent_key = checkpoint_revision_key(&proof);
        self.checkpoint_revisions.retain(|key, _| {
            !(*key == parent_key || (key.0, key.1) == (proof.key_hi, proof.key_lo))
        });
        self.checkpoint_operations.retain(|key, _| {
            !(*key == parent_key || (key.0, key.1) == (proof.key_hi, proof.key_lo))
        });
        self.checkpoint_records.retain(|key, _| {
            !(*key == parent_key || (key.0, key.1) == (proof.key_hi, proof.key_lo))
        });
    }

    fn checkpoint_mutate(
        &mut self,
        request: &VfsIpcRequest,
        record: ServiceCheckpointRecordWire,
    ) -> Result<bool, i32> {
        self.checkpoint_mutate_with_operation(record, request.operation_hi, request.operation_lo)
    }

    fn checkpoint_mutate_with_operation(
        &mut self,
        mut record: ServiceCheckpointRecordWire,
        operation_hi: u64,
        operation_lo: u64,
    ) -> Result<bool, i32> {
        let key = checkpoint_revision_key(&record);
        if let Some(current) = self.checkpoint_records.get(&key).copied() {
            if (current.operation_hi, current.operation_lo) == (operation_hi, operation_lo) {
                record.revision = current.revision;
                record.operation_hi = operation_hi;
                record.operation_lo = operation_lo;
                return if record == current {
                    Ok(true)
                } else {
                    Err(EIO)
                };
            }
        }
        let current = self.checkpoint_revisions.get(&key).copied().unwrap_or(0);
        record.revision = current.checked_add(1).ok_or(EOVERFLOW)?;
        record.operation_hi = operation_hi;
        record.operation_lo = operation_lo;
        let response = call_rootd_checkpoint(
            COMMERCIAL_MAX_ROOTD_OP_SERVICE_CHECKPOINT_MUTATE,
            0,
            Some(&record),
        )?;
        if response.payload_len != 0 || response.value0 != record.revision || response.value1 > 1 {
            return Err(EIO);
        }
        self.checkpoint_revisions.insert(key, record.revision);
        self.checkpoint_operations
            .insert(key, (record.operation_hi, record.operation_lo));
        self.checkpoint_records.insert(key, record);
        Ok(response.value1 == 1)
    }

    fn checkpoint_operation_replayed(
        &self,
        request: &VfsIpcRequest,
        key: CheckpointRevisionKey,
    ) -> bool {
        self.checkpoint_operations.get(&key).copied()
            == Some((request.operation_hi, request.operation_lo))
    }

    fn checkpoint_open_description(
        &mut self,
        request: &VfsIpcRequest,
        remote_id: u64,
        handle: &RemoteHandle,
    ) -> Result<(), i32> {
        let path_bytes = handle.path.as_bytes();
        if path_bytes.is_empty() || path_bytes.len() > VFS_IPC_PATH_CAPACITY {
            return Err(EINVAL);
        }
        let chunk_count = path_bytes.len().div_ceil(SERVICE_CHECKPOINT_VALUE_CAPACITY);
        let staging = checkpoint_handle_record(
            remote_id,
            handle,
            VFSD_OPEN_MUTATION_STAGING,
            request.arg0,
            0,
        )?;
        let (operation_hi, operation_lo) = checkpoint_suboperation(request, 0);
        self.checkpoint_mutate_with_operation(staging, operation_hi, operation_lo)?;
        for chunk_index in 0..chunk_count {
            let start = chunk_index * SERVICE_CHECKPOINT_VALUE_CAPACITY;
            let end = (start + SERVICE_CHECKPOINT_VALUE_CAPACITY).min(path_bytes.len());
            let record = checkpoint_path_record(remote_id, chunk_index, &path_bytes[start..end])?;
            let (operation_hi, operation_lo) =
                checkpoint_suboperation(request, chunk_index as u64 + 1);
            self.checkpoint_mutate_with_operation(record, operation_hi, operation_lo)?;
        }
        let final_record =
            checkpoint_handle_record(remote_id, handle, VFSD_OPEN_MUTATION_OPEN, request.arg0, 0)?;
        let (operation_hi, operation_lo) = checkpoint_suboperation(request, chunk_count as u64 + 1);
        self.checkpoint_mutate_with_operation(final_record, operation_hi, operation_lo)?;
        Ok(())
    }

    fn checkpoint_handle_state(
        &mut self,
        request: &VfsIpcRequest,
        remote_id: u64,
        handle: &RemoteHandle,
        mutation: u16,
        last_start: u64,
        last_result: u64,
    ) -> Result<bool, i32> {
        let record =
            checkpoint_handle_record(remote_id, handle, mutation, last_start, last_result)?;
        self.checkpoint_mutate(request, record)
    }

    fn checkpoint_close_description(
        &mut self,
        request: &VfsIpcRequest,
        remote_id: u64,
    ) -> Result<ServiceCheckpointRecordWire, i32> {
        let tombstone = checkpoint_handle_tombstone(remote_id)?;
        self.checkpoint_mutate(request, tombstone)?;
        let key = checkpoint_revision_key(&tombstone);
        self.checkpoint_records.get(&key).copied().ok_or(EIO)
    }

    fn invalidate_caches_if_remounted(&mut self) {
        if self.cache_generation != self.mount_generation {
            self.metadata_cache.clear();
            self.dir_entries_cache.clear();
            self.cache_generation = self.mount_generation;
        }
    }

    fn advance_mount_generation(&mut self) -> Result<(), i32> {
        self.mount_generation = checked_next_generation(self.mount_generation).ok_or(EOVERFLOW)?;
        Ok(())
    }

    fn handle_linux_request(
        &mut self,
        request: &LinuxSyscallOffloadRequest,
    ) -> LinuxSyscallOffloadResponse {
        let mut response = linux_response_for_op(request.op);
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
            // Stateful fd operations require the versioned VFS IPC request's
            // operation identity. The retired generic offload envelope has no
            // such field and cannot provide crash-safe open descriptions.
            SYSCALL_OFFLOAD_OP_LINUX_OPENAT
            | SYSCALL_OFFLOAD_OP_LINUX_GETDENTS64
            | SYSCALL_OFFLOAD_OP_LINUX_CLOSE
            | SYSCALL_OFFLOAD_OP_LINUX_DUP
            | SYSCALL_OFFLOAD_OP_LINUX_FCNTL => response.status = EOPNOTSUPP,
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

    fn handle_vfs_request(&mut self, request: &VfsIpcRequest, response: &mut VfsIpcResponse) {
        if let Err(errno) = validate_vfs_request(request) {
            response.status = errno;
            return;
        }
        match request.op {
            VFS_IPC_OP_OPENAT => self.vfs_openat(request, response),
            VFS_IPC_OP_CLOSE => self.vfs_close(request, response),
            VFS_IPC_OP_DUP => response.status = EOPNOTSUPP,
            VFS_IPC_OP_READ => self.vfs_read(request, response, None),
            VFS_IPC_OP_PREAD64 => self.vfs_read(request, response, Some(request.arg0)),
            VFS_IPC_OP_WRITE => {
                response.status = persistent_mutation_status(request.op).unwrap_or(EINVAL)
            }
            VFS_IPC_OP_LSEEK => self.vfs_lseek(request, response),
            VFS_IPC_OP_FSTAT => self.vfs_fstat(request, response),
            VFS_IPC_OP_FTRUNCATE => {
                response.status = persistent_mutation_status(request.op).unwrap_or(EINVAL)
            }
            VFS_IPC_OP_GETDENTS64 => self.vfs_getdents64(request, response),
            VFS_IPC_OP_FCNTL => self.vfs_fcntl(request, response),
            VFS_IPC_OP_CURSOR_SETTLE => self.vfs_cursor_settle(request, response),
            VFS_IPC_OP_CHECKPOINT_ACK => self.vfs_checkpoint_ack(request, response),
            VFS_IPC_OP_STATX => self.vfs_path_statx(request, response),
            VFS_IPC_OP_NEWFSTATAT => self.vfs_path_stat(request, response),
            VFS_IPC_OP_READLINKAT => response.status = ENOENT,
            VFS_IPC_OP_ACCESS => self.vfs_access(request, response),
            VFS_IPC_OP_GETCWD => self.vfs_getcwd(request, response),
            VFS_IPC_OP_CHDIR => self.vfs_chdir(request, response),
            VFS_IPC_OP_MKDIR => self.vfs_mkdir(request, response),
            VFS_IPC_OP_MOUNT => self.linux_mount_vfs(response),
            VFS_IPC_OP_UMOUNT2 => self.linux_umount_vfs(response),
            VFS_IPC_OP_UNLINKAT => self.vfs_unlinkat(request, response),
            VFS_IPC_OP_POLL_QUERY => self.vfs_poll_query(request, response),
            _ => response.status = EINVAL,
        }
    }

    fn vfs_poll_query(&mut self, request: &VfsIpcRequest, response: &mut VfsIpcResponse) {
        let mut mutated = false;
        match request.arg0 {
            VFS_POLL_QUERY_POLL => self.vfs_poll_once(request, response),
            VFS_POLL_QUERY_EPOLL_CREATE => {
                let key = checkpoint_epoll_key(request.remote_id);
                if self.checkpoint_operation_replayed(request, key) {
                    return;
                }
                let mut candidate = self.epolls.clone();
                if let Err(err) = candidate.create(request.remote_id) {
                    response.status = waitset_registry_status(err);
                    return;
                }
                let record = checkpoint_epoll_record(request.remote_id, 1, false);
                if let Err(errno) = self.checkpoint_mutate(request, record) {
                    response.status = errno;
                    return;
                }
                self.epolls = candidate;
                mutated = true;
            }
            VFS_POLL_QUERY_EPOLL_CTL => {
                self.vfs_epoll_ctl(request, response);
                mutated = response.status == 0;
            }
            VFS_POLL_QUERY_EPOLL_SNAPSHOT => self.vfs_epoll_snapshot(request, response),
            VFS_POLL_QUERY_EPOLL_REF | VFS_POLL_QUERY_EPOLL_UNREF => {
                let key = checkpoint_epoll_key(request.remote_id);
                if self.checkpoint_operation_replayed(request, key) {
                    return;
                }
                let refs = match self.epolls.refs(request.remote_id) {
                    Ok(refs) => refs,
                    Err(err) => {
                        response.status = waitset_registry_status(err);
                        return;
                    }
                };
                let mut candidate = self.epolls.clone();
                let (next_refs, tombstone) = if request.arg0 == VFS_POLL_QUERY_EPOLL_REF {
                    if let Err(err) = candidate.acquire(request.remote_id) {
                        response.status = waitset_registry_status(err);
                        return;
                    }
                    (refs.checked_add(1).unwrap_or(0), false)
                } else {
                    if let Err(err) = candidate.release(request.remote_id) {
                        response.status = waitset_registry_status(err);
                        return;
                    }
                    (refs.saturating_sub(1), refs == 1)
                };
                if next_refs == 0 && !tombstone {
                    response.status = EOVERFLOW;
                    return;
                }
                let record = checkpoint_epoll_record(request.remote_id, next_refs, tombstone);
                if let Err(errno) = self.checkpoint_mutate(request, record) {
                    response.status = errno;
                    return;
                }
                self.epolls = candidate;
                mutated = true;
            }
            VFS_POLL_QUERY_EPOLL_PURGE_OBJECT => {
                let Ok(provider) = u16::try_from(request.arg1) else {
                    response.status = EINVAL;
                    return;
                };
                if provider == 0 || request.arg2 == 0 {
                    response.status = EINVAL;
                    return;
                }
                let interests = self.epolls.matching_interests(provider, request.arg2);
                let mut candidate = self.epolls.clone();
                mutated = candidate.purge(provider, request.arg2);
                for (token, interest) in interests {
                    let record = checkpoint_interest_record(token, interest, true);
                    let key = checkpoint_revision_key(&record);
                    if !self.checkpoint_operation_replayed(request, key) {
                        if let Err(errno) = self.checkpoint_mutate(request, record) {
                            response.status = errno;
                            return;
                        }
                    }
                }
                self.epolls = candidate;
            }
            _ => response.status = EINVAL,
        }
        if mutated {
            self.advance_readiness_generation();
        }
    }

    fn vfs_poll_once(&mut self, request: &VfsIpcRequest, response: &mut VfsIpcResponse) {
        const POLLFD_SIZE: usize = size_of::<linux_abi::LinuxPollFd>();
        let len = request.payload_len as usize;
        if !len.is_multiple_of(POLLFD_SIZE) || len > response.payload.len() {
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
        let Some(interest) = epoll_interest_from_request(request) else {
            response.status = EINVAL;
            return;
        };
        let tombstone = request.arg1 == linux_abi::EPOLL_CTL_DEL;
        let checkpoint = checkpoint_interest_record(request.remote_id, interest, tombstone);
        let checkpoint_key = checkpoint_revision_key(&checkpoint);
        if self.checkpoint_operation_replayed(request, checkpoint_key) {
            return;
        }
        let mut candidate = self.epolls.clone();
        let result = match request.arg1 {
            linux_abi::EPOLL_CTL_ADD => candidate.add(request.remote_id, interest),
            linux_abi::EPOLL_CTL_MOD => candidate.modify(request.remote_id, interest),
            linux_abi::EPOLL_CTL_DEL => candidate.delete(request.remote_id, interest.key),
            _ => {
                response.status = EINVAL;
                return;
            }
        };
        if let Err(err) = result {
            response.status = waitset_registry_status(err);
            return;
        }
        if let Err(errno) = self.checkpoint_mutate(request, checkpoint) {
            response.status = errno;
            return;
        }
        self.epolls = candidate;
    }

    fn vfs_epoll_snapshot(&mut self, request: &VfsIpcRequest, response: &mut VfsIpcResponse) {
        let maxevents = request.arg1 as usize;
        let wire_size = size_of::<WaitSetInterestWire>();
        let capacity = response.payload.len() / wire_size;
        if maxevents == 0 || maxevents > WAITSET_MAX_INTERESTS || maxevents > capacity {
            response.status = EINVAL;
            return;
        }
        let interests = match self.epolls.snapshot(request.remote_id, maxevents) {
            Ok(interests) => interests,
            Err(err) => {
                response.status = waitset_registry_status(err);
                return;
            }
        };
        for (written, interest) in interests.iter().enumerate() {
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
                    wire_size,
                )
            };
            let offset = written * wire_size;
            response.payload[offset..offset + wire_size].copy_from_slice(bytes);
        }
        response.value = interests.len() as u64;
        response.aux = self.readiness_generation;
        response.payload_len = (interests.len() * wire_size) as u32;
    }

    fn advance_readiness_generation(&mut self) {
        self.readiness_generation = self
            .readiness_generation
            .checked_add(1)
            .expect("vfsd readiness generation exhausted");
        #[cfg(not(test))]
        {
            let args = WaitSetSignalBrokerArgs {
                abi_version: WAITSET_ABI_VERSION,
                provider: WAITSET_PROVIDER_VFSD,
                flags: 0,
                object_id: WAITSET_GLOBAL_OBJECT_ID,
                generation: self.readiness_generation,
                reserved0: 0,
            };
            let status = unsafe {
                rustos_svc_runtime::syscall::syscall1(
                    SYS_RUSTOS_WAITSET_SIGNAL_BROKER,
                    (&args as *const WaitSetSignalBrokerArgs) as u64,
                )
            };
            if status < 0 {
                panic!("vfsd readiness generation publication failed");
            }
        }
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

    fn linux_mount(&mut self, response: &mut LinuxSyscallOffloadResponse) {
        response.status = match self.advance_mount_generation() {
            Ok(()) => 0,
            Err(errno) => errno,
        };
    }

    fn linux_umount2(&mut self, response: &mut LinuxSyscallOffloadResponse) {
        response.status = match self.advance_mount_generation() {
            Ok(()) => 0,
            Err(errno) => errno,
        };
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
        let resolved_path;
        let path = if path.starts_with('/') {
            path
        } else {
            resolved_path = match self.resolve_path(request, request.pid, request.dirfd, path) {
                Ok(path) => path,
                Err(errno) => {
                    response.status = errno;
                    return;
                }
            };
            resolved_path.as_str()
        };
        match self.open_remote_checkpointed(request, path, request.arg0) {
            Ok((id, handle)) => {
                response.remote_id = id;
                response.handle_kind = handle_kind_u16(handle.kind);
                response.value = handle.len;
                response.aux = device_access_for_path(handle.path.as_str());
                write_vfs_payload_bytes(response, handle.path.as_bytes());
            }
            Err(errno) => response.status = errno,
        }
    }

    fn vfs_close(&mut self, request: &VfsIpcRequest, response: &mut VfsIpcResponse) {
        let key = checkpoint_handle_key(request.remote_id);
        let Some(handle) = self.handles.get(&request.remote_id).cloned() else {
            let Some(record) = self.checkpoint_records.get(&key).copied() else {
                response.status = EBADF;
                return;
            };
            if record.flags & SERVICE_CHECKPOINT_FLAG_TOMBSTONE != 0 {
                if (record.operation_hi, record.operation_lo)
                    != (request.operation_hi, request.operation_lo)
                {
                    response.status = EBADF;
                }
                return;
            }
            if let Err(errno) = self.checkpoint_close_description(request, request.remote_id) {
                response.status = errno;
            }
            return;
        };
        if cursor_mutation_prepared(&handle) {
            response.status = EBUSY;
            return;
        }
        match self.checkpoint_close_description(request, request.remote_id) {
            Ok(_) => {
                self.handles.remove(&request.remote_id);
            }
            Err(errno) => response.status = errno,
        }
    }

    fn vfs_read(
        &mut self,
        request: &VfsIpcRequest,
        response: &mut VfsIpcResponse,
        offset: Option<u64>,
    ) {
        let len = (request.arg1 as usize).min(VFS_IPC_PAYLOAD_CAPACITY);
        match self.read_remote_into(request, offset, len, &mut response.payload) {
            Ok(read) => {
                response.payload_len = read as u32;
                response.value = read as u64;
            }
            Err(errno) => response.status = errno,
        }
    }

    fn vfs_lseek(&mut self, request: &VfsIpcRequest, response: &mut VfsIpcResponse) {
        let Some(handle) = self.handles.get(&request.remote_id).cloned() else {
            response.status = EBADF;
            return;
        };
        if self.checkpoint_operation_replayed(request, checkpoint_handle_key(request.remote_id)) {
            if handle.last_mutation != VFSD_OPEN_MUTATION_LSEEK
                || handle.last_start != request.arg0
                || handle.last_result != request.arg1
            {
                response.status = EIO;
                return;
            }
            response.value = handle.cursor;
            return;
        }
        if cursor_mutation_prepared(&handle) {
            response.status = EBUSY;
            return;
        }
        let next = match checked_seek_position(
            handle.cursor,
            handle.len,
            request.arg0 as i64,
            request.arg1,
        ) {
            Ok(next) => next,
            Err(SeekPositionError::InvalidWhence | SeekPositionError::Negative) => {
                response.status = EINVAL;
                return;
            }
            Err(SeekPositionError::Overflow) => {
                response.status = EOVERFLOW;
                return;
            }
        };
        let mut candidate = handle;
        candidate.cursor = next;
        candidate.last_mutation = VFSD_OPEN_MUTATION_LSEEK;
        candidate.last_start = request.arg0;
        candidate.last_result = request.arg1;
        if let Err(errno) = self.checkpoint_handle_state(
            request,
            request.remote_id,
            &candidate,
            VFSD_OPEN_MUTATION_LSEEK,
            request.arg0,
            request.arg1,
        ) {
            response.status = errno;
            return;
        }
        response.value = candidate.cursor;
        self.handles.insert(request.remote_id, candidate);
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
        let Some(handle) = self.handles.get(&request.remote_id).cloned() else {
            response.status = EBADF;
            return;
        };
        let requested = (request.arg1 as usize).min(response.payload.len());
        let replayed =
            self.checkpoint_operation_replayed(request, checkpoint_handle_key(request.remote_id));
        if cursor_mutation_prepared(&handle) && !replayed {
            response.status = EBUSY;
            return;
        }
        let start = if replayed {
            let (recorded_requested, _) = unpack_u32_pair(handle.last_result);
            if handle.last_mutation != VFSD_OPEN_MUTATION_GETDENTS
                || recorded_requested as usize != requested
            {
                response.status = EIO;
                return;
            }
            handle.last_start
        } else {
            handle.cursor
        };
        let start_index = match usize::try_from(start) {
            Ok(start) => start,
            Err(_) => {
                response.status = EOVERFLOW;
                return;
            }
        };
        let (written, consumed) = match self.render_getdents_payload(
            request.remote_id,
            start_index,
            requested,
            &mut response.payload,
        ) {
            Ok(result) => result,
            Err(errno) => {
                response.status = errno;
                return;
            }
        };
        if replayed {
            let (_, recorded_consumed) = unpack_u32_pair(handle.last_result);
            if consumed != recorded_consumed as usize {
                response.status = EIO;
                return;
            }
        } else {
            let mut candidate = handle;
            candidate.cursor = match candidate.cursor.checked_add(consumed as u64) {
                Some(cursor) => cursor,
                None => {
                    response.status = EOVERFLOW;
                    return;
                }
            };
            candidate.last_mutation = VFSD_OPEN_MUTATION_GETDENTS;
            candidate.last_start = start;
            candidate.last_result = match pack_u32_pair(requested, consumed) {
                Ok(result) => result,
                Err(errno) => {
                    response.status = errno;
                    return;
                }
            };
            if let Err(errno) = self.checkpoint_handle_state(
                request,
                request.remote_id,
                &candidate,
                VFSD_OPEN_MUTATION_GETDENTS,
                start,
                candidate.last_result,
            ) {
                response.status = errno;
                return;
            }
            self.handles.insert(request.remote_id, candidate);
        }
        response.payload_len = written as u32;
        response.value = written as u64;
    }

    fn vfs_fcntl(&mut self, request: &VfsIpcRequest, response: &mut VfsIpcResponse) {
        const F_SETFL_MUTABLE_MASK: u64 = linux_abi::O_APPEND | linux_abi::O_NONBLOCK;
        let Some(handle) = self.handles.get(&request.remote_id).cloned() else {
            response.status = EBADF;
            return;
        };
        if cursor_mutation_prepared(&handle)
            && !self
                .checkpoint_operation_replayed(request, checkpoint_handle_key(request.remote_id))
        {
            response.status = EBUSY;
            return;
        }
        match request.arg0 {
            linux_abi::F_GETFL => response.value = handle.status_flags,
            linux_abi::F_SETFL => {
                if self.checkpoint_operation_replayed(
                    request,
                    checkpoint_handle_key(request.remote_id),
                ) {
                    if handle.last_mutation != VFSD_OPEN_MUTATION_FCNTL
                        || handle.last_start != request.arg0
                        || handle.last_result != request.arg1
                    {
                        response.status = EIO;
                        return;
                    }
                    response.value = handle.status_flags;
                    return;
                }
                let mut candidate = handle;
                candidate.status_flags = (candidate.status_flags & !F_SETFL_MUTABLE_MASK)
                    | (request.arg1 & F_SETFL_MUTABLE_MASK);
                candidate.last_mutation = VFSD_OPEN_MUTATION_FCNTL;
                candidate.last_start = request.arg0;
                candidate.last_result = request.arg1;
                if let Err(errno) = self.checkpoint_handle_state(
                    request,
                    request.remote_id,
                    &candidate,
                    VFSD_OPEN_MUTATION_FCNTL,
                    request.arg0,
                    request.arg1,
                ) {
                    response.status = errno;
                    return;
                }
                response.value = candidate.status_flags;
                self.handles.insert(request.remote_id, candidate);
            }
            _ => response.status = EINVAL,
        }
    }

    fn vfs_cursor_settle(&mut self, request: &VfsIpcRequest, response: &mut VfsIpcResponse) {
        let Some(handle) = self.handles.get(&request.remote_id).cloned() else {
            response.status = EBADF;
            return;
        };
        if self.checkpoint_operation_replayed(request, checkpoint_handle_key(request.remote_id)) {
            if handle.last_mutation != VFSD_OPEN_MUTATION_STABLE {
                response.status = EIO;
            } else {
                response.value = handle.cursor;
            }
            return;
        }
        if !cursor_mutation_prepared(&handle)
            || self
                .checkpoint_operations
                .get(&checkpoint_handle_key(request.remote_id))
                .copied()
                != Some((request.arg0, request.arg1))
            || !matches!(
                request.arg2,
                VFS_CURSOR_SETTLE_COMMIT | VFS_CURSOR_SETTLE_CANCEL
            )
            || request.arg3 != 0
            || request.path_len != 0
            || request.payload_len != 0
        {
            response.status = EINVAL;
            return;
        }
        let mut candidate = handle;
        if request.arg2 == VFS_CURSOR_SETTLE_CANCEL {
            candidate.cursor = candidate.last_start;
        }
        candidate.last_mutation = VFSD_OPEN_MUTATION_STABLE;
        candidate.last_start = 0;
        candidate.last_result = 0;
        if let Err(errno) = self.checkpoint_handle_state(
            request,
            request.remote_id,
            &candidate,
            VFSD_OPEN_MUTATION_STABLE,
            0,
            0,
        ) {
            response.status = errno;
            return;
        }
        response.value = candidate.cursor;
        self.handles.insert(request.remote_id, candidate);
    }

    fn vfs_checkpoint_ack(&mut self, request: &VfsIpcRequest, response: &mut VfsIpcResponse) {
        let Ok(original_op) = u16::try_from(request.arg2) else {
            response.status = EINVAL;
            return;
        };
        if (request.arg0 == 0 && request.arg1 == 0)
            || request.fd != 0
            || request.path_len != 0
            || request.payload_len != 0
            || !matches!(original_op, VFS_IPC_OP_CLOSE | VFS_IPC_OP_POLL_QUERY)
            || (original_op == VFS_IPC_OP_CLOSE && request.arg3 != 0)
            || (original_op == VFS_IPC_OP_POLL_QUERY
                && !matches!(
                    request.arg3,
                    VFS_POLL_QUERY_EPOLL_UNREF
                        | VFS_POLL_QUERY_EPOLL_CTL
                        | VFS_POLL_QUERY_EPOLL_PURGE_OBJECT
                ))
        {
            response.status = EINVAL;
            return;
        }
        let proofs = self
            .checkpoint_records
            .values()
            .copied()
            .filter(|record| {
                record.flags & SERVICE_CHECKPOINT_FLAG_TOMBSTONE != 0
                    && (record.operation_hi, record.operation_lo) == (request.arg0, request.arg1)
                    && (original_op != VFS_IPC_OP_CLOSE
                        || (record.parent_hi == 0
                            && record.parent_lo == 0
                            && record.key_hi == request.remote_id
                            && record.key_lo == VFSD_CHECKPOINT_HANDLE_TAG))
            })
            .collect::<Vec<_>>();
        for proof in proofs {
            if let Err(errno) = self.compact_checkpoint_proof(proof) {
                response.status = errno;
                return;
            }
        }
    }

    fn vfs_path_statx(&mut self, request: &VfsIpcRequest, response: &mut VfsIpcResponse) {
        let Some(path) = vfs_request_path(request) else {
            response.status = EINVAL;
            return;
        };
        let path = match self.resolve_path(request, request.pid, request.dirfd, path) {
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
        let path = match self.resolve_path(request, request.pid, request.dirfd, path) {
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
        response.status = match self.resolve_path(request, request.pid, request.dirfd, path) {
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
        let path = match self.resolve_path(request, request.pid, request.dirfd, path) {
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
        let path = match self.resolve_path(request, request.pid, request.dirfd, path) {
            Ok(path) => path,
            Err(errno) => {
                response.status = errno;
                return;
            }
        };
        response.status = mkdir_policy(path.as_str(), request.euid);
    }

    fn linux_mount_vfs(&mut self, response: &mut VfsIpcResponse) {
        response.status = match self.advance_mount_generation() {
            Ok(()) => 0,
            Err(errno) => errno,
        };
    }

    fn linux_umount_vfs(&mut self, response: &mut VfsIpcResponse) {
        response.status = match self.advance_mount_generation() {
            Ok(()) => 0,
            Err(errno) => errno,
        };
    }

    fn vfs_unlinkat(&mut self, request: &VfsIpcRequest, response: &mut VfsIpcResponse) {
        let Some(path) = vfs_request_path(request) else {
            response.status = EINVAL;
            return;
        };
        let path = match self.resolve_path(request, request.pid, request.dirfd, path) {
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

    fn resolve_path(
        &mut self,
        request: &VfsIpcRequest,
        pid: u64,
        dirfd: u64,
        path: &str,
    ) -> Result<String, i32> {
        if path.is_empty() || path.len() > VFS_IPC_PATH_CAPACITY {
            return Err(EINVAL);
        }
        let base = if path.starts_with('/') {
            "/".to_string()
        } else if is_at_fdcwd(dirfd) {
            if let Some(cwd) = self.cwd.get(&pid) {
                cwd.clone()
            } else {
                let mut clean_relative = !path.starts_with('/');
                for component in path.split('/') {
                    if component.is_empty() || component == "." || component == ".." {
                        clean_relative = false;
                        break;
                    }
                }
                if clean_relative {
                    let mut resolved = String::with_capacity(path.len() + 1);
                    unsafe {
                        let bytes = resolved.as_mut_vec();
                        bytes.push(b'/');
                        for byte in path.as_bytes() {
                            bytes.push(*byte);
                        }
                    }
                    return Ok(resolved);
                }
                return normalize_absolute_path("/", path);
            }
        } else {
            let base_handle_id = if request.remote_id != 0 {
                request.remote_id
            } else {
                dirfd
            };
            let handle = self.handles.get(&base_handle_id).ok_or(EBADF)?;
            if handle.kind != RemoteKind::Directory {
                return Err(ENOTDIR);
            }
            handle.path.clone()
        };
        normalize_absolute_path(base.as_str(), path)
    }

    fn open_remote_checkpointed(
        &mut self,
        request: &VfsIpcRequest,
        path: &str,
        flags: u64,
    ) -> Result<(u64, RemoteHandle), i32> {
        let id = request.arg3;
        if id == 0 {
            return Err(EINVAL);
        }
        if let Some(handle) = self.handles.get(&id).cloned() {
            let chunk_count = handle
                .path
                .len()
                .div_ceil(SERVICE_CHECKPOINT_VALUE_CAPACITY);
            let (operation_hi, operation_lo) =
                checkpoint_suboperation(request, chunk_count as u64 + 1);
            let key = checkpoint_handle_key(id);
            let Some(current) = self.checkpoint_records.get(&key).copied() else {
                return Err(EIO);
            };
            let mut expected =
                checkpoint_handle_record(id, &handle, VFSD_OPEN_MUTATION_OPEN, flags, 0)?;
            expected.revision = current.revision;
            expected.operation_hi = operation_hi;
            expected.operation_lo = operation_lo;
            return if current == expected && handle.path == path {
                Ok((id, handle))
            } else {
                Err(EINVAL)
            };
        }

        let metadata = self.metadata(path).map_err(|errno| {
            debug_line(&format!(
                "vfsd: open failed stage=metadata errno={errno} path={path}"
            ));
            errno
        })?;
        if flags & O_DIRECTORY != 0 && metadata.kind != RemoteKind::Directory {
            return Err(ENOTDIR);
        }
        if flags & (O_CREAT | O_TRUNC) != 0 {
            return Err(EROFS);
        }
        let handle = RemoteHandle {
            kind: metadata.kind,
            path: path.to_string(),
            cursor: 0,
            len: metadata.len,
            refs: 1,
            status_flags: flags & !linux_abi::O_CLOEXEC,
            last_mutation: VFSD_OPEN_MUTATION_OPEN,
            last_start: flags,
            last_result: 0,
        };
        self.checkpoint_open_description(request, id, &handle)
            .map_err(|errno| {
                debug_line(&format!(
                    "vfsd: open failed stage=checkpoint errno={errno} path={path}"
                ));
                errno
            })?;
        if self.handles.insert(id, handle.clone()).is_some() {
            debug_line(&format!(
                "vfsd: open failed stage=install errno={EIO} path={path}"
            ));
            return Err(EIO);
        }
        Ok((id, handle))
    }

    fn read_remote_into(
        &mut self,
        request: &VfsIpcRequest,
        offset: Option<u64>,
        len: usize,
        dest: &mut [u8],
    ) -> Result<usize, i32> {
        let id = request.remote_id;
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
        if offset.is_none()
            && self.checkpoint_operation_replayed(request, checkpoint_handle_key(id))
        {
            let handle = self.handles.get(&id).ok_or(EBADF)?;
            if handle.last_mutation != VFSD_OPEN_MUTATION_READ {
                return Err(EIO);
            }
            let (requested, replay_len) = unpack_u32_pair(handle.last_result);
            if requested as usize != len
                || replay_len as usize > len
                || handle.last_start > file_len
            {
                return Err(EIO);
            }
            if replay_len == 0 {
                return Ok(0);
            }
            let replay_len = replay_len as usize;
            let read = self.read_file_slice_into(
                path.as_str(),
                handle.last_start,
                &mut dest[..replay_len],
            )?;
            return (read == replay_len).then_some(read).ok_or(EIO);
        }
        if offset.is_none() && self.handles.get(&id).is_some_and(cursor_mutation_prepared) {
            return Err(EBUSY);
        }
        let available = file_len.saturating_sub(start);
        let len = len.min(available as usize).min(dest.len());
        let read = if len == 0 {
            0
        } else {
            self.read_file_slice_into(path.as_str(), start, &mut dest[..len])?
        };
        if offset.is_none() {
            let mut candidate = self.handles.get(&id).cloned().ok_or(EBADF)?;
            candidate.cursor = candidate.cursor.checked_add(read as u64).ok_or(EOVERFLOW)?;
            candidate.last_mutation = VFSD_OPEN_MUTATION_READ;
            candidate.last_start = start;
            candidate.last_result = pack_u32_pair(len, read)?;
            self.checkpoint_handle_state(
                request,
                id,
                &candidate,
                VFSD_OPEN_MUTATION_READ,
                start,
                candidate.last_result,
            )?;
            self.handles.insert(id, candidate);
        }
        Ok(read)
    }

    fn read_file_slice_into(
        &mut self,
        path: &str,
        start: u64,
        dest: &mut [u8],
    ) -> Result<usize, i32> {
        match early_system::read(path, start, dest)? {
            Some(read) => Ok(read),
            None => self
                .volume()?
                .read_file_range_into(path, start, dest)
                .map_err(map_fat_error),
        }
    }

    fn render_getdents_payload(
        &mut self,
        id: u64,
        cursor: usize,
        user_len: usize,
        payload: &mut [u8],
    ) -> Result<(usize, usize), i32> {
        if user_len < 24 {
            return Err(EINVAL);
        }
        let path = {
            let Some(handle) = self.handles.get(&id) else {
                return Err(EBADF);
            };
            if handle.kind != RemoteKind::Directory {
                return Err(ENOTDIR);
            }
            handle.path.clone()
        };
        let entries = match self.dir_entries(path.as_str()) {
            Ok(entries) => entries,
            Err(errno) => return Err(errno),
        };
        let mut written = 0usize;
        let mut consumed = 0usize;
        for (index, entry) in entries.iter().enumerate().skip(cursor) {
            let record = encode_dirent(entry, index + 1);
            if written + record.len() > user_len.min(payload.len()) {
                if written == 0 {
                    return Err(EINVAL);
                }
                break;
            }
            payload[written..written + record.len()].copy_from_slice(record.as_slice());
            written += record.len();
            consumed += 1;
        }
        Ok((written, consumed))
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
        if let Some(len) = early_system::file_len(path)? {
            let metadata = Metadata {
                kind: RemoteKind::File,
                len,
                inode: path_inode(path.as_bytes()),
            };
            self.metadata_cache.insert(path.to_string(), Ok(metadata));
            return Ok(metadata);
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
        if result.is_ok()
            || result
                .as_ref()
                .is_err_and(|errno| cacheable_metadata_errno(*errno))
        {
            self.metadata_cache.insert(path.to_string(), result);
        }
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
            let device = BootBlockDevice::open().map_err(|errno| {
                debug_line(&format!(
                    "vfsd: volume unavailable stage=block-info errno={errno}"
                ));
                errno
            })?;
            let disk = FatDisk::new(device);
            self.volume = Some(FatVolume::from_disk(disk).map_err(map_fat_error).map_err(
                |errno| {
                    debug_line(&format!(
                        "vfsd: volume unavailable stage=fat-admission errno={errno}"
                    ));
                    errno
                },
            )?);
        }
        Ok(self.volume.as_ref().expect("volume initialized"))
    }
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
}
// RING3-MIGRATION-REFERENCE END: vfsd ring3-owned VFS policy.
