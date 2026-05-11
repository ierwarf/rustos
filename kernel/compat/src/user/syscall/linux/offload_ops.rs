use super::*;
use alloc::{boxed::Box, collections::BTreeMap, string::String, vec::Vec};
use core::sync::atomic::{AtomicU64, Ordering};
use core::{mem::size_of, slice};

use lazy_static::lazy_static;
use rustos_user_abi::syscall::{
    BLOCK_BROKER_ABI_VERSION, BLOCK_BROKER_MAX_IO_BYTES, BLOCK_BROKER_OP_BOOT_INFO,
    BLOCK_BROKER_OP_BOOT_READ, DRIVER_BROKER_ALIAS_CAPACITY, DRIVER_BROKER_NAME_CAPACITY,
    DRIVER_BROKER_PATH_CAPACITY, LINUX_CPUSET_BYTES, LINUX_RLIMIT_SIZE, LINUX_STAT_SIZE,
    LINUX_STATX_SIZE, LINUX_UTSNAME_SIZE, LinuxSyscallOffloadRequest, LinuxSyscallOffloadResponse,
    PROC_BROKER_ABI_VERSION, PROC_BROKER_FORMAT_ELF64, PROC_BROKER_FORMAT_PE64,
    PROC_BROKER_MAP_EXEC, PROC_BROKER_MAP_PRIVATE, PROC_BROKER_MAP_READ, PROC_BROKER_MAP_WRITE,
    RustosBlockBrokerArgs, RustosDeviceIoctlBrokerArgs, RustosDriverLoadModuleBrokerArgs,
    RustosDriverProbeAliasBrokerArgs, RustosDriverProviderActiveBrokerArgs, RustosNetBrokerArgs,
    RustosProcAbortBrokerArgs, RustosProcCommitBrokerArgs, RustosProcMapFileBrokerArgs,
    RustosProcMapZeroedBrokerArgs, RustosProcPrepareBrokerArgs, RustosVfsMountBrokerArgs,
    SYSCALL_OFFLOAD_OP_LINUX_ACCEPT, SYSCALL_OFFLOAD_OP_LINUX_ACCESS,
    SYSCALL_OFFLOAD_OP_LINUX_BIND, SYSCALL_OFFLOAD_OP_LINUX_CHDIR, SYSCALL_OFFLOAD_OP_LINUX_CLOSE,
    SYSCALL_OFFLOAD_OP_LINUX_CONNECT, SYSCALL_OFFLOAD_OP_LINUX_DUP, SYSCALL_OFFLOAD_OP_LINUX_FCNTL,
    SYSCALL_OFFLOAD_OP_LINUX_GETCWD, SYSCALL_OFFLOAD_OP_LINUX_GETDENTS64,
    SYSCALL_OFFLOAD_OP_LINUX_GETEGID, SYSCALL_OFFLOAD_OP_LINUX_GETEUID,
    SYSCALL_OFFLOAD_OP_LINUX_GETGID, SYSCALL_OFFLOAD_OP_LINUX_GETUID,
    SYSCALL_OFFLOAD_OP_LINUX_IOCTL, SYSCALL_OFFLOAD_OP_LINUX_LISTEN,
    SYSCALL_OFFLOAD_OP_LINUX_MKDIR, SYSCALL_OFFLOAD_OP_LINUX_MOUNT,
    SYSCALL_OFFLOAD_OP_LINUX_NEWFSTATAT, SYSCALL_OFFLOAD_OP_LINUX_OPENAT,
    SYSCALL_OFFLOAD_OP_LINUX_PRLIMIT64, SYSCALL_OFFLOAD_OP_LINUX_READLINKAT,
    SYSCALL_OFFLOAD_OP_LINUX_SCHED_GETAFFINITY, SYSCALL_OFFLOAD_OP_LINUX_SETGID,
    SYSCALL_OFFLOAD_OP_LINUX_SETUID, SYSCALL_OFFLOAD_OP_LINUX_SOCKET,
    SYSCALL_OFFLOAD_OP_LINUX_SOCKETPAIR, SYSCALL_OFFLOAD_OP_LINUX_STATX,
    SYSCALL_OFFLOAD_OP_LINUX_UMOUNT2, SYSCALL_OFFLOAD_OP_LINUX_UNAME,
    SYSCALL_OFFLOAD_PATH_CAPACITY, SYSCALL_OFFLOAD_PAYLOAD_CAPACITY, VFS_IPC_ABI_VERSION,
    VFS_IPC_HANDLE_KIND_DEVICE, VFS_IPC_HANDLE_KIND_DIR, VFS_IPC_HANDLE_KIND_FILE,
    VFS_IPC_OP_CLOSE, VFS_IPC_OP_DUP, VFS_IPC_OP_FCNTL, VFS_IPC_OP_FSTAT, VFS_IPC_OP_FTRUNCATE,
    VFS_IPC_OP_GETDENTS64, VFS_IPC_OP_LSEEK, VFS_IPC_OP_OPENAT, VFS_IPC_OP_PREAD64,
    VFS_IPC_OP_READ, VFS_IPC_OP_WRITE, VFS_IPC_PATH_CAPACITY, VFS_IPC_PAYLOAD_CAPACITY,
    VFS_IPC_REQUEST_PAYLOAD_CAPACITY, VfsIpcRequest, VfsIpcResponse,
};
use spin::Mutex;

pub(super) const VFSD_DUP_MODE_DUP: u32 = 0;
pub(super) const VFSD_DUP_MODE_DUP2: u32 = 1;
pub(super) const VFSD_DUP_MODE_DUP3: u32 = 2;
const MAX_PROC_PREPARES: usize = 64;
const MAX_PROC_PREPARE_MAPPINGS: usize = 128;
const PROC_MAP_FLAGS_MASK: u64 =
    PROC_BROKER_MAP_READ | PROC_BROKER_MAP_WRITE | PROC_BROKER_MAP_EXEC | PROC_BROKER_MAP_PRIVATE;

static NEXT_PROC_PREPARE_HANDLE: AtomicU64 = AtomicU64::new(1);

#[derive(Clone)]
struct ProcPrepareRecord {
    owner_pid: u64,
    format: u16,
    flags: u32,
    mappings: Vec<ProcPrepareMapping>,
}

#[derive(Clone)]
enum ProcPrepareMapping {
    File {
        file: multitask::VfsFileHandle,
        file_offset: u64,
        target_addr: u64,
        file_len: u64,
        mem_len: u64,
        flags: u64,
    },
    RemoteFile {
        remote_id: u64,
        remote_len: u64,
        file_offset: u64,
        target_addr: u64,
        file_len: u64,
        mem_len: u64,
        flags: u64,
    },
    Zeroed {
        target_addr: u64,
        mem_len: u64,
        flags: u64,
    },
}

lazy_static! {
    static ref PROC_PREPARES: Mutex<BTreeMap<u64, ProcPrepareRecord>> = Mutex::new(BTreeMap::new());
}

pub(super) fn syscall_linux_access(path_ptr: u64, mode: u64) -> u64 {
    syscall_linux_faccessat(linux_abi::AT_FDCWD as u64, path_ptr, mode, 0)
}

pub(super) fn syscall_linux_mkdir(path_ptr: u64, _mode: u64) -> u64 {
    let absolute_path = match linux_ops::resolve_readlinkat_absolute_path_for_current_process(
        linux_abi::AT_FDCWD as u64,
        path_ptr,
    ) {
        Ok(path) => path,
        Err(err) => return linux_errno(linux_sysop_error_to_errno(err)),
    };
    let response = match call_path_offload(
        SYSCALL_OFFLOAD_OP_LINUX_MKDIR,
        linux_abi::AT_FDCWD as u64,
        0,
        0,
        &absolute_path,
    ) {
        Ok(response) => response,
        Err(errno) => return linux_errno(errno),
    };
    if response.status != 0 {
        return linux_errno(response.status.unsigned_abs() as i64);
    }
    if response.payload_len != 0 {
        return linux_errno(LINUX_EINVAL);
    }
    0
}

pub(super) fn syscall_linux_chdir(path_ptr: u64) -> u64 {
    let absolute_path = match linux_ops::resolve_readlinkat_absolute_path_for_current_process(
        linux_abi::AT_FDCWD as u64,
        path_ptr,
    ) {
        Ok(path) => path,
        Err(err) => return linux_errno(linux_sysop_error_to_errno(err)),
    };
    if current_process_may_bootstrap_policy_service() {
        let Some(snapshot) = multitask::current_user_snapshot() else {
            return linux_errno(LINUX_EINVAL);
        };
        return match linux_ops::chdir_absolute_path_for_process(
            snapshot.process_id(),
            &absolute_path,
        ) {
            Ok(()) => 0,
            Err(err) => linux_errno(linux_sysop_error_to_errno(err)),
        };
    }
    let response = match call_path_offload(
        SYSCALL_OFFLOAD_OP_LINUX_CHDIR,
        linux_abi::AT_FDCWD as u64,
        0,
        0,
        &absolute_path,
    ) {
        Ok(response) => response,
        Err(errno) => return linux_errno(errno),
    };
    if response.status != 0 {
        return linux_errno(response.status.unsigned_abs() as i64);
    }
    if response.payload_len != 0 {
        return linux_errno(LINUX_EINVAL);
    }
    0
}

pub(super) fn syscall_linux_statx(
    dirfd: u64,
    path_ptr: u64,
    flags: u64,
    mask: u64,
    statx_ptr: u64,
) -> u64 {
    if let Err(err) = usermem::validate_current_user_write_buffer(statx_ptr, LINUX_STATX_SIZE) {
        return linux_errno(address_space_error_to_linux_errno(err));
    }

    let absolute_path =
        match linux_ops::resolve_statx_absolute_path_for_current_process(dirfd, path_ptr, flags) {
            Ok(path) => path,
            Err(err) => return linux_errno(linux_sysop_error_to_errno(err)),
        };
    if current_process_may_bootstrap_policy_service() {
        let statx = match linux_ops::statx_for_absolute_path(
            &absolute_path,
            u32::try_from(mask).unwrap_or(u32::MAX),
        ) {
            Ok(statx) => statx,
            Err(err) => return linux_errno(linux_sysop_error_to_errno(err)),
        };
        let bytes = unsafe {
            slice::from_raw_parts(
                (&statx as *const linux_abi::LinuxStatx).cast::<u8>(),
                size_of::<linux_abi::LinuxStatx>(),
            )
        };
        return match usermem::write_current_user_bytes(statx_ptr, bytes) {
            Ok(()) => 0,
            Err(err) => linux_errno(address_space_error_to_linux_errno(err)),
        };
    }
    let path_bytes = absolute_path.as_bytes();
    if path_bytes.len() > SYSCALL_OFFLOAD_PATH_CAPACITY {
        return linux_errno(LINUX_EINVAL);
    }
    let mask = u32::try_from(mask).unwrap_or(u32::MAX);
    let mut request = LinuxSyscallOffloadRequest {
        dirfd,
        flags,
        mask,
        path_len: path_bytes.len() as u32,
        ..LinuxSyscallOffloadRequest::default()
    };
    populate_offload_identity(&mut request);
    request.path[..path_bytes.len()].copy_from_slice(path_bytes);

    let response = match call_vfs_offload_request(&request) {
        Ok(response) => response,
        Err(errno) => return linux_errno(errno),
    };
    if response.version != rustos_user_abi::syscall::SYSCALL_OFFLOAD_ABI_VERSION
        || response.op != SYSCALL_OFFLOAD_OP_LINUX_STATX
        || response.reserved0 != 0
    {
        return linux_errno(LINUX_EINVAL);
    }
    if response.status != 0 {
        return linux_errno(response.status.unsigned_abs() as i64);
    }
    if response.payload_len as usize != LINUX_STATX_SIZE {
        return linux_errno(LINUX_EINVAL);
    }
    match usermem::write_current_user_bytes(statx_ptr, &response.payload[..LINUX_STATX_SIZE]) {
        Ok(()) => 0,
        Err(err) => linux_errno(address_space_error_to_linux_errno(err)),
    }
}

pub(super) fn syscall_linux_newfstatat(
    dirfd: u64,
    path_ptr: u64,
    stat_ptr: u64,
    flags: u64,
) -> u64 {
    if let Err(err) = usermem::validate_current_user_write_buffer(stat_ptr, LINUX_STAT_SIZE) {
        return linux_errno(address_space_error_to_linux_errno(err));
    }
    let absolute_path = match linux_ops::resolve_newfstatat_absolute_path_for_current_process(
        dirfd, path_ptr, flags,
    ) {
        Ok(path) => path,
        Err(err) => return linux_errno(linux_sysop_error_to_errno(err)),
    };
    if current_process_may_bootstrap_policy_service() {
        let stat = match linux_ops::stat_for_absolute_path(&absolute_path) {
            Ok(stat) => stat,
            Err(err) => return linux_errno(linux_sysop_error_to_errno(err)),
        };
        let bytes = unsafe {
            slice::from_raw_parts(
                (&stat as *const linux_abi::LinuxStat).cast::<u8>(),
                size_of::<linux_abi::LinuxStat>(),
            )
        };
        return match usermem::write_current_user_bytes(stat_ptr, bytes) {
            Ok(()) => 0,
            Err(err) => linux_errno(address_space_error_to_linux_errno(err)),
        };
    }
    let response = match call_path_offload(
        SYSCALL_OFFLOAD_OP_LINUX_NEWFSTATAT,
        dirfd,
        flags,
        0,
        &absolute_path,
    ) {
        Ok(response) => response,
        Err(errno) => return linux_errno(errno),
    };
    if response.status != 0 {
        return linux_errno(response.status.unsigned_abs() as i64);
    }
    if response.payload_len as usize != LINUX_STAT_SIZE {
        return linux_errno(LINUX_EINVAL);
    }
    match usermem::write_current_user_bytes(stat_ptr, &response.payload[..LINUX_STAT_SIZE]) {
        Ok(()) => 0,
        Err(err) => linux_errno(address_space_error_to_linux_errno(err)),
    }
}

pub(super) fn syscall_linux_readlinkat(
    dirfd: u64,
    path_ptr: u64,
    user_ptr: u64,
    user_len: u64,
) -> u64 {
    let Ok(user_len) = usize::try_from(user_len) else {
        return linux_errno(LINUX_EINVAL);
    };
    if user_len != 0 {
        let validate_len = user_len.min(SYSCALL_OFFLOAD_PAYLOAD_CAPACITY);
        if let Err(err) = usermem::validate_current_user_write_buffer(user_ptr, validate_len) {
            return linux_errno(address_space_error_to_linux_errno(err));
        }
    }
    let absolute_path =
        match linux_ops::resolve_readlinkat_absolute_path_for_current_process(dirfd, path_ptr) {
            Ok(path) => path,
            Err(err) => return linux_errno(linux_sysop_error_to_errno(err)),
        };
    if user_len == 0 {
        return 0;
    }
    let max_copy = user_len.min(SYSCALL_OFFLOAD_PAYLOAD_CAPACITY);
    if current_process_may_bootstrap_policy_service() {
        let target = match linux_ops::readlink_for_absolute_path(&absolute_path, max_copy) {
            Ok(target) => target,
            Err(err) => return linux_errno(linux_sysop_error_to_errno(err)),
        };
        return match usermem::write_current_user_bytes(user_ptr, target.as_slice()) {
            Ok(()) => target.len() as u64,
            Err(err) => linux_errno(address_space_error_to_linux_errno(err)),
        };
    }
    let response = match call_path_offload(
        SYSCALL_OFFLOAD_OP_LINUX_READLINKAT,
        dirfd,
        0,
        max_copy as u32,
        &absolute_path,
    ) {
        Ok(response) => response,
        Err(errno) => return linux_errno(errno),
    };
    if response.status != 0 {
        return linux_errno(response.status.unsigned_abs() as i64);
    }
    let payload_len = response.payload_len as usize;
    if payload_len > max_copy {
        return linux_errno(LINUX_EINVAL);
    }
    match usermem::write_current_user_bytes(user_ptr, &response.payload[..payload_len]) {
        Ok(()) => payload_len as u64,
        Err(err) => linux_errno(address_space_error_to_linux_errno(err)),
    }
}

pub(super) fn syscall_linux_faccessat(dirfd: u64, path_ptr: u64, mode: u64, flags: u64) -> u64 {
    let absolute_path = match linux_ops::resolve_faccessat_absolute_path_for_current_process(
        dirfd, path_ptr, flags,
    ) {
        Ok(path) => path,
        Err(err) => return linux_errno(linux_sysop_error_to_errno(err)),
    };
    let mode = u32::try_from(mode).unwrap_or(u32::MAX);
    if current_process_may_bootstrap_policy_service() {
        let Some(snapshot) = multitask::current_user_snapshot() else {
            return linux_errno(LINUX_EINVAL);
        };
        return match linux_ops::check_access_for_absolute_path_and_process(
            snapshot.process_id(),
            &absolute_path,
            mode as u64,
        ) {
            Ok(()) => 0,
            Err(err) => linux_errno(linux_sysop_error_to_errno(err)),
        };
    }
    let response = match call_path_offload(
        SYSCALL_OFFLOAD_OP_LINUX_ACCESS,
        dirfd,
        flags,
        mode,
        &absolute_path,
    ) {
        Ok(response) => response,
        Err(errno) => return linux_errno(errno),
    };
    if response.status != 0 {
        return linux_errno(response.status.unsigned_abs() as i64);
    }
    if response.payload_len != 0 {
        return linux_errno(LINUX_EINVAL);
    }
    0
}

pub(super) fn syscall_linux_rustos_statx_metadata(
    path_ptr: u64,
    path_len: u64,
    mask: u64,
    out_ptr: u64,
) -> u64 {
    let Ok(path_len) = usize::try_from(path_len) else {
        return linux_errno(LINUX_EINVAL);
    };
    if path_len == 0 || path_len > SYSCALL_OFFLOAD_PATH_CAPACITY {
        return linux_errno(LINUX_EINVAL);
    }
    let mut path = alloc::vec![0_u8; path_len];
    if let Err(err) = usermem::copy_from_current_user_exact(path_ptr, &mut path) {
        return linux_errno(address_space_error_to_linux_errno(err));
    }
    let Ok(path) = core::str::from_utf8(path.as_slice()) else {
        return linux_errno(LINUX_EINVAL);
    };
    let statx =
        match linux_ops::statx_for_absolute_path(path, u32::try_from(mask).unwrap_or(u32::MAX)) {
            Ok(statx) => statx,
            Err(err) => return linux_errno(linux_sysop_error_to_errno(err)),
        };
    let bytes = unsafe {
        slice::from_raw_parts(
            (&statx as *const linux_abi::LinuxStatx).cast::<u8>(),
            size_of::<linux_abi::LinuxStatx>(),
        )
    };
    match usermem::write_current_user_bytes(out_ptr, bytes) {
        Ok(()) => 0,
        Err(err) => linux_errno(address_space_error_to_linux_errno(err)),
    }
}

pub(super) fn syscall_linux_rustos_stat_metadata(
    path_ptr: u64,
    path_len: u64,
    out_ptr: u64,
) -> u64 {
    let path = match copy_metadata_path(path_ptr, path_len) {
        Ok(path) => path,
        Err(errno) => return linux_errno(errno),
    };
    let stat = match linux_ops::stat_for_absolute_path(&path) {
        Ok(stat) => stat,
        Err(err) => return linux_errno(linux_sysop_error_to_errno(err)),
    };
    let bytes = unsafe {
        slice::from_raw_parts(
            (&stat as *const linux_abi::LinuxStat).cast::<u8>(),
            size_of::<linux_abi::LinuxStat>(),
        )
    };
    match usermem::write_current_user_bytes(out_ptr, bytes) {
        Ok(()) => 0,
        Err(err) => linux_errno(address_space_error_to_linux_errno(err)),
    }
}

pub(super) fn syscall_linux_rustos_readlink_metadata(
    path_ptr: u64,
    path_len: u64,
    out_ptr: u64,
    out_len: u64,
) -> u64 {
    let path = match copy_metadata_path(path_ptr, path_len) {
        Ok(path) => path,
        Err(errno) => return linux_errno(errno),
    };
    let Ok(out_len) = usize::try_from(out_len) else {
        return linux_errno(LINUX_EINVAL);
    };
    if out_len > SYSCALL_OFFLOAD_PAYLOAD_CAPACITY {
        return linux_errno(LINUX_EINVAL);
    }
    let target = match linux_ops::readlink_for_absolute_path(&path, out_len) {
        Ok(target) => target,
        Err(err) => return linux_errno(linux_sysop_error_to_errno(err)),
    };
    match usermem::write_current_user_bytes(out_ptr, target.as_slice()) {
        Ok(()) => target.len() as u64,
        Err(err) => linux_errno(address_space_error_to_linux_errno(err)),
    }
}

pub(super) fn syscall_linux_rustos_access_metadata(
    path_ptr: u64,
    path_len: u64,
    mode: u64,
    process_id: u64,
) -> u64 {
    let path = match copy_metadata_path(path_ptr, path_len) {
        Ok(path) => path,
        Err(errno) => return linux_errno(errno),
    };
    match linux_ops::check_access_for_absolute_path_and_process(process_id, &path, mode) {
        Ok(()) => 0,
        Err(err) => linux_errno(linux_sysop_error_to_errno(err)),
    }
}

pub(super) fn syscall_linux_rustos_getcwd_metadata(
    process_id: u64,
    out_ptr: u64,
    out_len: u64,
) -> u64 {
    let Ok(out_len) = usize::try_from(out_len) else {
        return linux_errno(LINUX_EINVAL);
    };
    if out_len > SYSCALL_OFFLOAD_PAYLOAD_CAPACITY {
        return linux_errno(LINUX_EINVAL);
    }
    let cwd = match linux_ops::cwd_for_process(process_id) {
        Ok(cwd) => cwd,
        Err(err) => return linux_errno(linux_sysop_error_to_errno(err)),
    };
    let required_len = match cwd.len().checked_add(1) {
        Some(len) => len,
        None => return linux_errno(LINUX_EINVAL),
    };
    if required_len > out_len {
        return linux_errno(LINUX_EINVAL);
    }
    let mut bytes = alloc::vec::Vec::with_capacity(required_len);
    bytes.extend_from_slice(cwd.as_bytes());
    bytes.push(0);
    match usermem::write_current_user_bytes(out_ptr, bytes.as_slice()) {
        Ok(()) => required_len as u64,
        Err(err) => linux_errno(address_space_error_to_linux_errno(err)),
    }
}

pub(super) fn syscall_linux_rustos_chdir_metadata(
    path_ptr: u64,
    path_len: u64,
    process_id: u64,
) -> u64 {
    let path = match copy_metadata_path(path_ptr, path_len) {
        Ok(path) => path,
        Err(errno) => return linux_errno(errno),
    };
    match linux_ops::chdir_absolute_path_for_process(process_id, &path) {
        Ok(()) => 0,
        Err(err) => linux_errno(linux_sysop_error_to_errno(err)),
    }
}

pub(super) fn syscall_linux_getcwd(user_ptr: u64, user_len: u64) -> u64 {
    let Ok(user_len) = usize::try_from(user_len) else {
        return linux_errno(LINUX_EINVAL);
    };
    if user_len != 0 {
        let validate_len = user_len.min(SYSCALL_OFFLOAD_PAYLOAD_CAPACITY);
        if let Err(err) = usermem::validate_current_user_write_buffer(user_ptr, validate_len) {
            return linux_errno(address_space_error_to_linux_errno(err));
        }
    }
    if current_process_may_bootstrap_policy_service() {
        let Some(snapshot) = multitask::current_user_snapshot() else {
            return linux_errno(LINUX_EINVAL);
        };
        let cwd = match linux_ops::cwd_for_process(snapshot.process_id()) {
            Ok(cwd) => cwd,
            Err(err) => return linux_errno(linux_sysop_error_to_errno(err)),
        };
        let required_len = match cwd.len().checked_add(1) {
            Some(len) => len,
            None => return linux_errno(LINUX_EINVAL),
        };
        if user_len < required_len {
            return linux_errno(LINUX_EINVAL);
        }
        let mut bytes = alloc::vec::Vec::with_capacity(required_len);
        bytes.extend_from_slice(cwd.as_bytes());
        bytes.push(0);
        return match usermem::write_current_user_bytes(user_ptr, bytes.as_slice()) {
            Ok(()) => required_len as u64,
            Err(err) => linux_errno(address_space_error_to_linux_errno(err)),
        };
    }
    let response = match call_path_offload(
        SYSCALL_OFFLOAD_OP_LINUX_GETCWD,
        linux_abi::AT_FDCWD as u64,
        0,
        0,
        "",
    ) {
        Ok(response) => response,
        Err(errno) => return linux_errno(errno),
    };
    if response.status != 0 {
        return linux_errno(response.status.unsigned_abs() as i64);
    }
    let payload_len = response.payload_len as usize;
    if payload_len == 0 || payload_len > SYSCALL_OFFLOAD_PAYLOAD_CAPACITY {
        return linux_errno(LINUX_EINVAL);
    }
    if user_len < payload_len {
        return linux_errno(LINUX_EINVAL);
    }
    match usermem::write_current_user_bytes(user_ptr, &response.payload[..payload_len]) {
        Ok(()) => payload_len as u64,
        Err(err) => linux_errno(address_space_error_to_linux_errno(err)),
    }
}

pub(super) fn syscall_linux_uname(buf_ptr: u64) -> u64 {
    if let Err(err) =
        usermem::validate_current_user_write_buffer(buf_ptr, size_of::<linux_abi::LinuxUtsName>())
    {
        return linux_errno(address_space_error_to_linux_errno(err));
    }
    let request = new_offload_request(SYSCALL_OFFLOAD_OP_LINUX_UNAME);
    let response = match call_offload_request(&request) {
        Ok(response) => response,
        Err(errno) => return linux_errno(errno),
    };
    if response.status != 0 {
        return linux_errno(response.status.unsigned_abs() as i64);
    }
    if response.payload_len as usize != LINUX_UTSNAME_SIZE
        || response.payload_len as usize != size_of::<linux_abi::LinuxUtsName>()
    {
        return linux_errno(LINUX_EINVAL);
    }
    match usermem::write_current_user_bytes(buf_ptr, &response.payload[..LINUX_UTSNAME_SIZE]) {
        Ok(()) => 0,
        Err(err) => linux_errno(address_space_error_to_linux_errno(err)),
    }
}

pub(super) fn syscall_linux_prlimit64(
    pid: u64,
    resource: u64,
    new_limit_ptr: u64,
    old_limit_ptr: u64,
) -> u64 {
    let mut request = new_offload_request(SYSCALL_OFFLOAD_OP_LINUX_PRLIMIT64);
    request.dirfd = pid;
    request.flags = resource;
    if new_limit_ptr != 0 {
        let mut new_limit = [0_u8; LINUX_RLIMIT_SIZE];
        if let Err(err) = usermem::copy_from_current_user_exact(new_limit_ptr, &mut new_limit) {
            return linux_errno(address_space_error_to_linux_errno(err));
        }
        request.mask |= 0x1;
        request.path_len = LINUX_RLIMIT_SIZE as u32;
        request.path[..LINUX_RLIMIT_SIZE].copy_from_slice(&new_limit);
    }
    if old_limit_ptr != 0 {
        if let Err(err) =
            usermem::validate_current_user_write_buffer(old_limit_ptr, LINUX_RLIMIT_SIZE)
        {
            return linux_errno(address_space_error_to_linux_errno(err));
        }
        request.mask |= 0x2;
    }
    let response = match call_offload_request(&request) {
        Ok(response) => response,
        Err(errno) => return linux_errno(errno),
    };
    if response.status != 0 {
        return linux_errno(response.status.unsigned_abs() as i64);
    }
    if old_limit_ptr != 0 {
        if response.payload_len as usize != LINUX_RLIMIT_SIZE {
            return linux_errno(LINUX_EINVAL);
        }
        if let Err(err) =
            usermem::write_current_user_bytes(old_limit_ptr, &response.payload[..LINUX_RLIMIT_SIZE])
        {
            return linux_errno(address_space_error_to_linux_errno(err));
        }
    } else if response.payload_len != 0 {
        return linux_errno(LINUX_EINVAL);
    }
    0
}

pub(super) fn syscall_linux_sched_getaffinity(pid: u64, user_len: u64, mask_ptr: u64) -> u64 {
    let Ok(user_len) = usize::try_from(user_len) else {
        return linux_errno(LINUX_EINVAL);
    };
    if user_len == 0 {
        return linux_errno(LINUX_EINVAL);
    }
    let validate_len = user_len.min(LINUX_CPUSET_BYTES);
    if let Err(err) = usermem::validate_current_user_write_buffer(mask_ptr, validate_len) {
        return linux_errno(address_space_error_to_linux_errno(err));
    }
    let mut request = new_offload_request(SYSCALL_OFFLOAD_OP_LINUX_SCHED_GETAFFINITY);
    request.dirfd = pid;
    request.flags = user_len as u64;
    let response = match call_offload_request(&request) {
        Ok(response) => response,
        Err(errno) => return linux_errno(errno),
    };
    if response.status != 0 {
        return linux_errno(response.status.unsigned_abs() as i64);
    }
    let payload_len = response.payload_len as usize;
    if payload_len == 0 || payload_len > validate_len {
        return linux_errno(LINUX_EINVAL);
    }
    match usermem::write_current_user_bytes(mask_ptr, &response.payload[..payload_len]) {
        Ok(()) => payload_len as u64,
        Err(err) => linux_errno(address_space_error_to_linux_errno(err)),
    }
}

pub(super) fn syscall_linux_getuid() -> u64 {
    syscall_linux_id_getter(SYSCALL_OFFLOAD_OP_LINUX_GETUID)
}

pub(super) fn syscall_linux_getgid() -> u64 {
    syscall_linux_id_getter(SYSCALL_OFFLOAD_OP_LINUX_GETGID)
}

pub(super) fn syscall_linux_geteuid() -> u64 {
    syscall_linux_id_getter(SYSCALL_OFFLOAD_OP_LINUX_GETEUID)
}

pub(super) fn syscall_linux_getegid() -> u64 {
    syscall_linux_id_getter(SYSCALL_OFFLOAD_OP_LINUX_GETEGID)
}

pub(super) fn syscall_linux_setuid(uid: u64) -> u64 {
    let Ok(uid) = u32::try_from(uid) else {
        return linux_errno(LINUX_EINVAL);
    };
    let mut request = new_offload_request(SYSCALL_OFFLOAD_OP_LINUX_SETUID);
    request.mask = uid;
    let response = match call_offload_request(&request) {
        Ok(response) => response,
        Err(errno) => return linux_errno(errno),
    };
    if response.status != 0 {
        return linux_errno(response.status.unsigned_abs() as i64);
    }
    if response.payload_len != 0 {
        return linux_errno(LINUX_EINVAL);
    }
    0
}

pub(super) fn syscall_linux_setgid(gid: u64) -> u64 {
    let Ok(gid) = u32::try_from(gid) else {
        return linux_errno(LINUX_EINVAL);
    };
    let mut request = new_offload_request(SYSCALL_OFFLOAD_OP_LINUX_SETGID);
    request.mask = gid;
    let response = match call_offload_request(&request) {
        Ok(response) => response,
        Err(errno) => return linux_errno(errno),
    };
    if response.status != 0 {
        return linux_errno(response.status.unsigned_abs() as i64);
    }
    if response.payload_len != 0 {
        return linux_errno(LINUX_EINVAL);
    }
    0
}

fn syscall_linux_id_getter(op: u16) -> u64 {
    let request = new_offload_request(op);
    let response = match call_offload_request(&request) {
        Ok(response) => response,
        Err(errno) => return linux_errno(errno),
    };
    if response.status != 0 {
        return linux_errno(response.status.unsigned_abs() as i64);
    }
    if response.payload_len as usize != size_of::<u32>() {
        return linux_errno(LINUX_EINVAL);
    }
    u32::from_le_bytes([
        response.payload[0],
        response.payload[1],
        response.payload[2],
        response.payload[3],
    ]) as u64
}

fn as_bytes<T>(value: &T) -> &[u8] {
    unsafe { slice::from_raw_parts((value as *const T).cast::<u8>(), size_of::<T>()) }
}

fn read_unaligned<T: Copy + Default>(bytes: &[u8]) -> T {
    let mut value = T::default();
    let dest =
        unsafe { slice::from_raw_parts_mut((&mut value as *mut T).cast::<u8>(), size_of::<T>()) };
    dest.copy_from_slice(&bytes[..size_of::<T>()]);
    value
}

fn write_current_user_u64(ptr: u64, value: u64) -> Result<(), paging::AddressSpaceError> {
    usermem::write_current_user_bytes(ptr, value.to_le_bytes().as_slice())
}

fn storage_error_to_linux_errno(err: storage_core::StorageError) -> i64 {
    match err {
        storage_core::StorageError::InvalidInput => LINUX_EINVAL,
        storage_core::StorageError::NotPresent => LINUX_ENODEV,
        storage_core::StorageError::Unsupported => LINUX_EOPNOTSUPP,
        storage_core::StorageError::UnexpectedEof => LINUX_EIO,
        storage_core::StorageError::Interrupted => LINUX_EINTR,
        storage_core::StorageError::Timeout => LINUX_ETIMEDOUT,
        storage_core::StorageError::DeviceFault | storage_core::StorageError::WriteZero => {
            LINUX_EIO
        }
    }
}

pub(super) fn call_vfs_path_policy(
    op: u16,
    dirfd: u64,
    flags: u64,
    arg0: u32,
    absolute_path: &str,
) -> Result<(), i64> {
    if current_process_may_bootstrap_policy_service() {
        return Ok(());
    }
    let response = call_path_offload(op, dirfd, flags, arg0, absolute_path)?;
    if response.status != 0 {
        return Err(response.status.unsigned_abs() as i64);
    }
    if response.payload_len != 0 {
        return Err(LINUX_EINVAL);
    }
    Ok(())
}

pub(super) fn call_service_policy(
    service_id: u64,
    op: u16,
    dirfd: u64,
    flags: u64,
    arg0: u32,
) -> Result<(), i64> {
    if current_process_may_bootstrap_policy_service() {
        return Ok(());
    }
    let mut request = new_offload_request(op);
    request.dirfd = dirfd;
    request.flags = flags;
    request.mask = arg0;
    let response = call_service_offload_request(service_id, &request)?;
    if response.status != 0 {
        return Err(response.status.unsigned_abs() as i64);
    }
    if response.payload_len != 0 {
        return Err(LINUX_EINVAL);
    }
    Ok(())
}

pub(super) fn call_device_ioctl(fd: u64, request_code: u64, arg: u64) -> Result<u64, i64> {
    if current_process_may_bootstrap_policy_service()
        || !ipc_ops::service_registered(linux_abi::IPC_SERVICE_DEVMGRD)
    {
        return linux_ops::ioctl(fd, request_code, arg).map_err(linux_sysop_error_to_errno);
    }
    let mut request = new_offload_request(SYSCALL_OFFLOAD_OP_LINUX_IOCTL);
    request.dirfd = fd;
    request.flags = request_code;
    request.arg1 = arg;
    let response = call_service_offload_request(linux_abi::IPC_SERVICE_DEVMGRD, &request)?;
    if response.status != 0 {
        return Err(response.status.unsigned_abs() as i64);
    }
    if response.payload_len as usize != size_of::<u64>() {
        return Err(LINUX_EINVAL);
    }
    let mut bytes = [0_u8; size_of::<u64>()];
    bytes.copy_from_slice(&response.payload[..size_of::<u64>()]);
    Ok(u64::from_le_bytes(bytes))
}

pub(super) fn call_net_broker(
    op: u16,
    arg0: u64,
    arg1: u64,
    arg2: u64,
    arg3: u64,
) -> Result<u64, i64> {
    let Some(snapshot) = multitask::current_user_snapshot() else {
        return Err(LINUX_EINVAL);
    };
    if current_process_may_bootstrap_policy_service()
        || !ipc_ops::service_registered(linux_abi::IPC_SERVICE_NETD)
    {
        return call_legacy_net_operation(op, arg0, arg1, arg2, arg3);
    }
    let mut request = new_offload_request(op);
    request.dirfd = arg0;
    request.flags = arg1;
    request.arg0 = arg2;
    request.arg1 = arg3;
    let response = call_service_offload_request(linux_abi::IPC_SERVICE_NETD, &request)?;
    if response.status != 0 {
        return Err(response.status.unsigned_abs() as i64);
    }
    if response.payload_len as usize != size_of::<u64>() {
        return Err(LINUX_EINVAL);
    }
    let mut bytes = [0_u8; size_of::<u64>()];
    bytes.copy_from_slice(&response.payload[..size_of::<u64>()]);
    let _ = snapshot;
    Ok(u64::from_le_bytes(bytes))
}

fn call_legacy_net_operation(
    op: u16,
    arg0: u64,
    arg1: u64,
    arg2: u64,
    arg3: u64,
) -> Result<u64, i64> {
    match op {
        SYSCALL_OFFLOAD_OP_LINUX_SOCKET => linux_ops::socket(arg0, arg1, arg2),
        SYSCALL_OFFLOAD_OP_LINUX_SOCKETPAIR => {
            linux_ops::socketpair(arg0, arg1, arg2, arg3).map(|()| 0)
        }
        SYSCALL_OFFLOAD_OP_LINUX_BIND => linux_ops::bind(arg0, arg1, arg2).map(|()| 0),
        SYSCALL_OFFLOAD_OP_LINUX_LISTEN => linux_ops::listen(arg0, arg1).map(|()| 0),
        SYSCALL_OFFLOAD_OP_LINUX_ACCEPT => linux_ops::accept4(arg0, arg1, arg2, arg3),
        SYSCALL_OFFLOAD_OP_LINUX_CONNECT => linux_ops::connect(arg0, arg1, arg2).map(|()| 0),
        _ => Err(linux_ops::LinuxSysopError::InvalidArgument),
    }
    .map_err(linux_sysop_error_to_errno)
}

pub(super) fn call_vfs_openat_with_fd(
    dirfd: u64,
    flags: u64,
    mode: u32,
    absolute_path: &str,
) -> Result<u64, i64> {
    let mut request = new_vfs_request(VFS_IPC_OP_OPENAT);
    request.dirfd = dirfd;
    request.arg0 = flags;
    request.arg1 = u64::from(mode);
    populate_vfs_path(&mut request, absolute_path)?;
    let response = call_vfs_ipc_request(&request)?;
    if response.status != 0 {
        return Err(response.status.unsigned_abs() as i64);
    }
    if response.remote_id == 0 {
        return Err(LINUX_EINVAL);
    }
    let kind = match response.handle_kind {
        VFS_IPC_HANDLE_KIND_FILE => multitask::RemoteVfsHandleKind::File,
        VFS_IPC_HANDLE_KIND_DIR => multitask::RemoteVfsHandleKind::Directory,
        VFS_IPC_HANDLE_KIND_DEVICE => multitask::RemoteVfsHandleKind::Device,
        _ => return Err(LINUX_EINVAL),
    };
    let handle = multitask::KernelHandle::RemoteVfs(multitask::RemoteVfsHandle::new(
        response.remote_id,
        kind,
        alloc::string::String::from(absolute_path),
        response.value,
    ));
    let Some(fd) = multitask::with_current_user_process_state_mut(|_, _, process_state| {
        Ok(process_state
            .handles_mut()
            .install_with_open_flags(handle, flags))
    }) else {
        return Err(LINUX_EINVAL);
    };
    fd
}

pub(super) fn call_vfs_close_fd(fd: u64) -> Result<(), i64> {
    if let Some(remote) = current_remote_vfs_handle(fd)? {
        let mut request = new_vfs_request(VFS_IPC_OP_CLOSE);
        request.fd = fd;
        request.remote_id = remote.remote_id();
        let response = call_vfs_ipc_request(&request)?;
        if response.status != 0 {
            return Err(response.status.unsigned_abs() as i64);
        }
    }
    linux_ops::close(fd).map_err(linux_sysop_error_to_errno)?;
    Ok(())
}

pub(super) fn call_vfs_dup_fd(oldfd: u64, newfd: u64, flags: u64, mode: u32) -> Result<u64, i64> {
    if let Some(remote) = current_remote_vfs_handle(oldfd)? {
        let mut request = new_vfs_request(VFS_IPC_OP_DUP);
        request.fd = oldfd;
        request.remote_id = remote.remote_id();
        let response = call_vfs_ipc_request(&request)?;
        if response.status != 0 {
            return Err(response.status.unsigned_abs() as i64);
        }
    }
    match mode {
        VFSD_DUP_MODE_DUP => linux_ops::dup(oldfd),
        VFSD_DUP_MODE_DUP2 => linux_ops::dup2(oldfd, newfd),
        VFSD_DUP_MODE_DUP3 => linux_ops::dup3(oldfd, newfd, flags),
        _ => Err(linux_ops::LinuxSysopError::InvalidArgument),
    }
    .map_err(linux_sysop_error_to_errno)
}

pub(super) fn call_vfs_getdents64(fd: u64, user_ptr: u64, user_len: u64) -> Result<u64, i64> {
    let Some(remote) = current_remote_vfs_handle(fd)? else {
        return linux_ops::getdents64(fd, user_ptr, user_len)
            .map(|read| read as u64)
            .map_err(linux_sysop_error_to_errno);
    };
    let user_len_usize = usize::try_from(user_len).map_err(|_| LINUX_EINVAL)?;
    usermem::validate_current_user_write_buffer(user_ptr, user_len_usize)
        .map_err(address_space_error_to_linux_errno)?;
    let mut request = new_vfs_request(VFS_IPC_OP_GETDENTS64);
    request.fd = fd;
    request.remote_id = remote.remote_id();
    request.arg1 = user_len;
    let response = call_vfs_ipc_request(&request)?;
    if response.status != 0 {
        return Err(response.status.unsigned_abs() as i64);
    }
    let len = response.payload_len as usize;
    if len > user_len_usize || len > response.payload.len() {
        return Err(LINUX_EINVAL);
    }
    usermem::write_current_user_bytes(user_ptr, &response.payload[..len])
        .map_err(address_space_error_to_linux_errno)?;
    Ok(len as u64)
}

pub(super) fn call_vfs_fcntl(fd: u64, cmd: u64, arg: u64) -> Result<u64, i64> {
    if current_remote_vfs_handle(fd)?.is_none() {
        return linux_ops::fcntl(fd, cmd, arg).map_err(linux_sysop_error_to_errno);
    }
    let mut request = new_vfs_request(VFS_IPC_OP_FCNTL);
    request.fd = fd;
    request.arg0 = cmd;
    request.arg1 = arg;
    let response = call_vfs_ipc_request(&request)?;
    if response.status != 0 {
        return Err(response.status.unsigned_abs() as i64);
    }
    linux_ops::fcntl(fd, cmd, arg).map_err(linux_sysop_error_to_errno)
}

pub(super) fn call_remote_vfs_read(
    fd: u64,
    user_ptr: u64,
    user_len: u64,
) -> Result<Option<u64>, i64> {
    call_remote_vfs_read_common(fd, user_ptr, user_len, None)
}

pub(super) fn call_remote_vfs_pread64(
    fd: u64,
    user_ptr: u64,
    user_len: u64,
    offset: u64,
) -> Result<Option<u64>, i64> {
    call_remote_vfs_read_common(fd, user_ptr, user_len, Some(offset))
}

fn call_remote_vfs_read_common(
    fd: u64,
    user_ptr: u64,
    user_len: u64,
    offset: Option<u64>,
) -> Result<Option<u64>, i64> {
    let Some(remote) = current_remote_vfs_handle(fd)? else {
        return Ok(None);
    };
    let user_len = usize::try_from(user_len).map_err(|_| LINUX_EINVAL)?;
    if user_len == 0 {
        return Ok(Some(0));
    }
    usermem::validate_current_user_write_buffer(user_ptr, user_len)
        .map_err(address_space_error_to_linux_errno)?;
    let mut total = 0usize;
    while total < user_len {
        let chunk_len = (user_len - total).min(VFS_IPC_PAYLOAD_CAPACITY);
        let mut request = new_vfs_request(if offset.is_some() {
            VFS_IPC_OP_PREAD64
        } else {
            VFS_IPC_OP_READ
        });
        request.fd = fd;
        request.remote_id = remote.remote_id();
        request.arg0 = offset.unwrap_or(0).saturating_add(total as u64);
        request.arg1 = chunk_len as u64;
        let response = call_vfs_ipc_request(&request)?;
        if response.status != 0 {
            return Err(response.status.unsigned_abs() as i64);
        }
        let read = response.payload_len as usize;
        if read > chunk_len || read > response.payload.len() {
            return Err(LINUX_EINVAL);
        }
        if read == 0 {
            break;
        }
        let dest = user_ptr.checked_add(total as u64).ok_or(LINUX_EINVAL)?;
        usermem::write_current_user_bytes(dest, &response.payload[..read])
            .map_err(address_space_error_to_linux_errno)?;
        total += read;
        if read < chunk_len {
            break;
        }
    }
    Ok(Some(total as u64))
}

pub(super) fn call_remote_vfs_write(
    fd: u64,
    user_ptr: u64,
    user_len: u64,
) -> Result<Option<u64>, i64> {
    let Some(remote) = current_remote_vfs_handle(fd)? else {
        return Ok(None);
    };
    let user_len = usize::try_from(user_len).map_err(|_| LINUX_EINVAL)?;
    if user_len == 0 {
        return Ok(Some(0));
    }
    let chunk_len = user_len.min(VFS_IPC_REQUEST_PAYLOAD_CAPACITY);
    let mut request = new_vfs_request(VFS_IPC_OP_WRITE);
    request.fd = fd;
    request.remote_id = remote.remote_id();
    request.payload_len = chunk_len as u32;
    usermem::copy_from_current_user_exact(user_ptr, &mut request.payload[..chunk_len])
        .map_err(address_space_error_to_linux_errno)?;
    let response = call_vfs_ipc_request(&request)?;
    if response.status != 0 {
        return Err(response.status.unsigned_abs() as i64);
    }
    Ok(Some(response.value))
}

pub(super) fn call_remote_vfs_lseek(fd: u64, offset: i64, whence: u64) -> Result<Option<u64>, i64> {
    let Some(remote) = current_remote_vfs_handle(fd)? else {
        return Ok(None);
    };
    let mut request = new_vfs_request(VFS_IPC_OP_LSEEK);
    request.fd = fd;
    request.remote_id = remote.remote_id();
    request.arg0 = offset as u64;
    request.arg1 = whence;
    let response = call_vfs_ipc_request(&request)?;
    if response.status != 0 {
        return Err(response.status.unsigned_abs() as i64);
    }
    Ok(Some(response.value))
}

pub(super) fn call_remote_vfs_fstat(fd: u64, stat_ptr: u64) -> Result<Option<()>, i64> {
    let Some(remote) = current_remote_vfs_handle(fd)? else {
        return Ok(None);
    };
    usermem::validate_current_user_write_buffer(stat_ptr, LINUX_STAT_SIZE)
        .map_err(address_space_error_to_linux_errno)?;
    let mut request = new_vfs_request(VFS_IPC_OP_FSTAT);
    request.fd = fd;
    request.remote_id = remote.remote_id();
    let response = call_vfs_ipc_request(&request)?;
    if response.status != 0 {
        return Err(response.status.unsigned_abs() as i64);
    }
    if response.payload_len as usize != LINUX_STAT_SIZE {
        return Err(LINUX_EINVAL);
    }
    usermem::write_current_user_bytes(stat_ptr, &response.payload[..LINUX_STAT_SIZE])
        .map_err(address_space_error_to_linux_errno)?;
    Ok(Some(()))
}

pub(super) fn call_remote_vfs_ftruncate(fd: u64, len: u64) -> Result<Option<()>, i64> {
    let Some(remote) = current_remote_vfs_handle(fd)? else {
        return Ok(None);
    };
    let mut request = new_vfs_request(VFS_IPC_OP_FTRUNCATE);
    request.fd = fd;
    request.remote_id = remote.remote_id();
    request.arg0 = len;
    let response = call_vfs_ipc_request(&request)?;
    if response.status != 0 {
        return Err(response.status.unsigned_abs() as i64);
    }
    Ok(Some(()))
}

pub(crate) fn call_remote_vfs_read_bytes(
    remote_id: u64,
    offset: u64,
    len: usize,
) -> Result<Vec<u8>, i64> {
    let mut bytes = Vec::new();
    while bytes.len() < len {
        let chunk_len = (len - bytes.len()).min(VFS_IPC_PAYLOAD_CAPACITY);
        let mut request = new_vfs_request(VFS_IPC_OP_PREAD64);
        request.remote_id = remote_id;
        request.arg0 = offset.saturating_add(bytes.len() as u64);
        request.arg1 = chunk_len as u64;
        let response = call_vfs_ipc_request(&request)?;
        if response.status != 0 {
            return Err(response.status.unsigned_abs() as i64);
        }
        let read = response.payload_len as usize;
        if read > chunk_len || read > response.payload.len() {
            return Err(LINUX_EINVAL);
        }
        if read == 0 {
            break;
        }
        bytes.extend_from_slice(&response.payload[..read]);
        if read < chunk_len {
            break;
        }
    }
    Ok(bytes)
}

pub(super) fn call_vfs_mount(
    source_ptr: u64,
    target_path: &str,
    fstype_ptr: u64,
    flags: u64,
    data_ptr: u64,
) -> Result<(), i64> {
    let path_bytes = target_path.as_bytes();
    if path_bytes.len() > SYSCALL_OFFLOAD_PATH_CAPACITY {
        return Err(LINUX_EINVAL);
    }
    let mut request = LinuxSyscallOffloadRequest {
        op: SYSCALL_OFFLOAD_OP_LINUX_MOUNT,
        flags,
        arg0: source_ptr,
        arg1: fstype_ptr,
        dirfd: data_ptr,
        path_len: path_bytes.len() as u32,
        ..LinuxSyscallOffloadRequest::default()
    };
    populate_offload_identity(&mut request);
    request.path[..path_bytes.len()].copy_from_slice(path_bytes);

    let response = call_vfs_offload_request(&request)?;
    if response.status != 0 {
        return Err(response.status.unsigned_abs() as i64);
    }
    if response.payload_len != 0 {
        return Err(LINUX_EINVAL);
    }
    Ok(())
}

pub(super) fn call_vfs_umount2(target_path: &str, flags: u64) -> Result<(), i64> {
    let path_bytes = target_path.as_bytes();
    if path_bytes.len() > SYSCALL_OFFLOAD_PATH_CAPACITY {
        return Err(LINUX_EINVAL);
    }
    let mut request = LinuxSyscallOffloadRequest {
        op: SYSCALL_OFFLOAD_OP_LINUX_UMOUNT2,
        flags,
        path_len: path_bytes.len() as u32,
        ..LinuxSyscallOffloadRequest::default()
    };
    populate_offload_identity(&mut request);
    request.path[..path_bytes.len()].copy_from_slice(path_bytes);

    let response = call_vfs_offload_request(&request)?;
    if response.status != 0 {
        return Err(response.status.unsigned_abs() as i64);
    }
    if response.payload_len != 0 {
        return Err(LINUX_EINVAL);
    }
    Ok(())
}

pub(super) fn syscall_linux_rustos_fd_close_broker(process_id: u64, fd: u64) -> u64 {
    if !current_process_has_vfs_broker_capability() {
        return linux_errno(LINUX_EACCES);
    }
    match linux_ops::close_for_process(process_id, fd) {
        Ok(()) => 0,
        Err(err) => linux_errno(linux_sysop_error_to_errno(err)),
    }
}

pub(super) fn syscall_linux_rustos_fd_dup_broker(
    process_id: u64,
    oldfd: u64,
    newfd: u64,
    flags: u64,
    mode: u64,
) -> u64 {
    if !current_process_has_vfs_broker_capability() {
        return linux_errno(LINUX_EACCES);
    }
    let result = match u32::try_from(mode).unwrap_or(u32::MAX) {
        VFSD_DUP_MODE_DUP => linux_ops::dup_for_process(process_id, oldfd),
        VFSD_DUP_MODE_DUP2 => linux_ops::dup2_for_process(process_id, oldfd, newfd),
        VFSD_DUP_MODE_DUP3 => linux_ops::dup3_for_process(process_id, oldfd, newfd, flags),
        _ => Err(linux_ops::LinuxSysopError::InvalidArgument),
    };
    match result {
        Ok(fd) => fd,
        Err(err) => linux_errno(linux_sysop_error_to_errno(err)),
    }
}

pub(super) fn syscall_linux_rustos_fd_getdents64_broker(
    process_id: u64,
    fd: u64,
    user_ptr: u64,
    user_len: u64,
) -> u64 {
    if !current_process_has_vfs_broker_capability() {
        return linux_errno(LINUX_EACCES);
    }
    match linux_ops::getdents64_for_process(process_id, fd, user_ptr, user_len) {
        Ok(read) => read as u64,
        Err(err) => linux_errno(linux_sysop_error_to_errno(err)),
    }
}

pub(super) fn syscall_linux_rustos_fd_fcntl_broker(
    process_id: u64,
    fd: u64,
    cmd: u64,
    arg: u64,
) -> u64 {
    if !current_process_has_vfs_broker_capability() {
        return linux_errno(LINUX_EACCES);
    }
    match linux_ops::fcntl_for_process(process_id, fd, cmd, arg) {
        Ok(result) => result,
        Err(err) => linux_errno(linux_sysop_error_to_errno(err)),
    }
}

pub(super) fn syscall_linux_rustos_vfs_mount_broker(args_ptr: u64) -> u64 {
    if !current_process_has_vfs_broker_capability() {
        return linux_errno(LINUX_EACCES);
    }
    let args = match usermem::read_current_user_struct::<RustosVfsMountBrokerArgs>(args_ptr) {
        Ok(args) => args,
        Err(err) => return linux_errno(address_space_error_to_linux_errno(err)),
    };
    if args.reserved0 != 0 {
        return linux_errno(LINUX_EINVAL);
    }
    let target_path = match copy_metadata_path(args.target_path_ptr, args.target_path_len) {
        Ok(path) => path,
        Err(errno) => return linux_errno(errno),
    };
    match linux_ops::mount_for_process(
        args.process_id,
        args.source_ptr,
        &target_path,
        args.fstype_ptr,
        args.flags,
        args.data_ptr,
    ) {
        Ok(()) => 0,
        Err(err) => linux_errno(linux_sysop_error_to_errno(err)),
    }
}

pub(super) fn syscall_linux_rustos_vfs_umount_broker(
    process_id: u64,
    target_path_ptr: u64,
    target_path_len: u64,
    flags: u64,
) -> u64 {
    if !current_process_has_vfs_broker_capability() {
        return linux_errno(LINUX_EACCES);
    }
    let target_path = match copy_metadata_path(target_path_ptr, target_path_len) {
        Ok(path) => path,
        Err(errno) => return linux_errno(errno),
    };
    match linux_ops::umount2_for_process(process_id, &target_path, flags) {
        Ok(()) => 0,
        Err(err) => linux_errno(linux_sysop_error_to_errno(err)),
    }
}

pub(super) fn syscall_linux_rustos_proc_prepare_broker(args_ptr: u64) -> u64 {
    if !current_process_has_loader_broker_capability() {
        return linux_errno(LINUX_EACCES);
    }
    let args = match usermem::read_current_user_struct::<RustosProcPrepareBrokerArgs>(args_ptr) {
        Ok(args) => args,
        Err(err) => return linux_errno(address_space_error_to_linux_errno(err)),
    };
    if args.abi_version != PROC_BROKER_ABI_VERSION
        || args.reserved0 != 0
        || !matches!(
            args.format,
            PROC_BROKER_FORMAT_ELF64 | PROC_BROKER_FORMAT_PE64
        )
    {
        return linux_errno(LINUX_EINVAL);
    }
    let Some(owner_pid) = multitask::current_user_process_id() else {
        return linux_errno(LINUX_EINVAL);
    };
    let mut prepares = PROC_PREPARES.lock();
    if prepares.len() >= MAX_PROC_PREPARES {
        return linux_errno(LINUX_EAGAIN);
    }
    let handle = NEXT_PROC_PREPARE_HANDLE.fetch_add(1, Ordering::Relaxed);
    if handle == 0 || prepares.contains_key(&handle) {
        return linux_errno(LINUX_EAGAIN);
    }
    prepares.insert(
        handle,
        ProcPrepareRecord {
            owner_pid,
            format: args.format,
            flags: args.flags,
            mappings: Vec::new(),
        },
    );
    handle
}

pub(super) fn syscall_linux_rustos_proc_map_file_broker(args_ptr: u64) -> u64 {
    if !current_process_has_loader_broker_capability() {
        return linux_errno(LINUX_EACCES);
    }
    let args = match usermem::read_current_user_struct::<RustosProcMapFileBrokerArgs>(args_ptr) {
        Ok(args) => args,
        Err(err) => return linux_errno(address_space_error_to_linux_errno(err)),
    };
    if args.reserved0 != 0 || args.fd == 0 || args.file_len > args.mem_len || args.mem_len == 0 {
        return linux_errno(LINUX_EINVAL);
    }
    if let Err(errno) = validate_proc_mapping_range(
        args.target_addr,
        args.mem_len,
        args.flags,
        args.file_len != 0,
    ) {
        return linux_errno(errno);
    }
    let mapping = match current_remote_vfs_handle(args.fd) {
        Ok(Some(remote)) => {
            if remote.kind() != multitask::RemoteVfsHandleKind::File {
                return linux_errno(LINUX_EBADF);
            }
            if args
                .file_offset
                .checked_add(args.file_len)
                .is_none_or(|end| end > remote.len())
            {
                return linux_errno(LINUX_EINVAL);
            }
            ProcPrepareMapping::RemoteFile {
                remote_id: remote.remote_id(),
                remote_len: remote.len(),
                file_offset: args.file_offset,
                target_addr: args.target_addr,
                file_len: args.file_len,
                mem_len: args.mem_len,
                flags: args.flags,
            }
        }
        Ok(None) => {
            let file = match current_process_vfs_file_handle(args.fd) {
                Ok(file) => file,
                Err(errno) => return linux_errno(errno),
            };
            if args
                .file_offset
                .checked_add(args.file_len)
                .is_none_or(|end| end > file.len() as u64)
            {
                return linux_errno(LINUX_EINVAL);
            }
            ProcPrepareMapping::File {
                file,
                file_offset: args.file_offset,
                target_addr: args.target_addr,
                file_len: args.file_len,
                mem_len: args.mem_len,
                flags: args.flags,
            }
        }
        Err(errno) => return linux_errno(errno),
    };
    match push_current_process_prepare_mapping(args.prepare_handle, mapping) {
        Ok(()) => 0,
        Err(errno) => linux_errno(errno),
    }
}

pub(super) fn syscall_linux_rustos_proc_map_zeroed_broker(args_ptr: u64) -> u64 {
    if !current_process_has_loader_broker_capability() {
        return linux_errno(LINUX_EACCES);
    }
    let args = match usermem::read_current_user_struct::<RustosProcMapZeroedBrokerArgs>(args_ptr) {
        Ok(args) => args,
        Err(err) => return linux_errno(address_space_error_to_linux_errno(err)),
    };
    if args.reserved0 != 0 || args.mem_len == 0 {
        return linux_errno(LINUX_EINVAL);
    }
    if let Err(errno) =
        validate_proc_mapping_range(args.target_addr, args.mem_len, args.flags, false)
    {
        return linux_errno(errno);
    }
    match push_current_process_prepare_mapping(
        args.prepare_handle,
        ProcPrepareMapping::Zeroed {
            target_addr: args.target_addr,
            mem_len: args.mem_len,
            flags: args.flags,
        },
    ) {
        Ok(()) => 0,
        Err(errno) => linux_errno(errno),
    }
}

pub(super) fn syscall_linux_rustos_proc_commit_broker(args_ptr: u64) -> u64 {
    if !current_process_has_loader_broker_capability() {
        return linux_errno(LINUX_EACCES);
    }
    let args = match usermem::read_current_user_struct::<RustosProcCommitBrokerArgs>(args_ptr) {
        Ok(args) => args,
        Err(err) => return linux_errno(address_space_error_to_linux_errno(err)),
    };
    if args.prepare_handle == 0
        || args.reserved0 != 0
        || args.exec_path_ptr == 0
        || args.exec_path_len == 0
    {
        return linux_errno(LINUX_EINVAL);
    }
    let Some(prepare) = take_current_process_prepare(args.prepare_handle) else {
        return linux_errno(LINUX_EINVAL);
    };
    let address_space = match build_address_space_from_prepare(&prepare) {
        Ok(address_space) => address_space,
        Err(errno) => return linux_errno(errno),
    };
    match linux_ops::spawn_prepared_exec(
        address_space,
        args.exec_path_ptr,
        args.argv_ptr,
        args.envp_ptr,
        args.flags,
        args.console_session,
        args.weight_micros,
    ) {
        Ok(pid) => pid,
        Err(err) => linux_errno(linux_sysop_error_to_errno(err)),
    }
}

fn build_address_space_from_prepare(
    prepare: &ProcPrepareRecord,
) -> Result<paging::ProcessAddressSpace, i64> {
    let mut address_space =
        paging::ProcessAddressSpace::new().map_err(address_space_error_to_linux_errno)?;
    for mapping in &prepare.mappings {
        match mapping {
            ProcPrepareMapping::File {
                file,
                file_offset,
                target_addr,
                file_len,
                mem_len,
                flags,
            } => {
                map_prepared_range(&mut address_space, *target_addr, *mem_len, *flags)?;
                if *file_len != 0 {
                    initialize_prepared_file_bytes(
                        &address_space,
                        file,
                        *file_offset,
                        *target_addr,
                        *file_len,
                    )?;
                }
            }
            ProcPrepareMapping::RemoteFile {
                remote_id,
                remote_len,
                file_offset,
                target_addr,
                file_len,
                mem_len,
                flags,
            } => {
                if file_offset
                    .checked_add(*file_len)
                    .is_none_or(|end| end > *remote_len)
                {
                    return Err(LINUX_EINVAL);
                }
                map_prepared_range(&mut address_space, *target_addr, *mem_len, *flags)?;
                if *file_len != 0 {
                    initialize_prepared_remote_file_bytes(
                        &address_space,
                        *remote_id,
                        *file_offset,
                        *target_addr,
                        *file_len,
                    )?;
                }
            }
            ProcPrepareMapping::Zeroed {
                target_addr,
                mem_len,
                flags,
            } => {
                map_prepared_range(&mut address_space, *target_addr, *mem_len, *flags)?;
            }
        }
    }
    Ok(address_space)
}

fn map_prepared_range(
    address_space: &mut paging::ProcessAddressSpace,
    target_addr: u64,
    mem_len: u64,
    flags: u64,
) -> Result<(), i64> {
    let page_count = usize::try_from(mem_len / 4096).map_err(|_| LINUX_EOVERFLOW)?;
    let page_flags = proc_mapping_page_flags(flags);
    address_space
        .map_zeroed_user_pages_at(x86_64::VirtAddr::new(target_addr), page_count, page_flags)
        .map(|_| ())
        .map_err(address_space_error_to_linux_errno)
}

fn initialize_prepared_file_bytes(
    address_space: &paging::ProcessAddressSpace,
    file: &multitask::VfsFileHandle,
    file_offset: u64,
    target_addr: u64,
    file_len: u64,
) -> Result<(), i64> {
    let mut current_file_offset = usize::try_from(file_offset).map_err(|_| LINUX_EOVERFLOW)?;
    let mut current_user_addr = target_addr;
    let mut remaining = usize::try_from(file_len).map_err(|_| LINUX_EOVERFLOW)?;
    let mut chunk = alloc::vec![0_u8; 64 * 1024];
    while remaining != 0 {
        let chunk_len = remaining.min(chunk.len());
        let read = file.read_at(current_file_offset, &mut chunk[..chunk_len]);
        if read != chunk_len {
            return Err(LINUX_ENOEXEC);
        }
        address_space
            .initialize_user_bytes(
                x86_64::VirtAddr::new(current_user_addr),
                &chunk[..chunk_len],
            )
            .map_err(address_space_error_to_linux_errno)?;
        current_file_offset = current_file_offset
            .checked_add(chunk_len)
            .ok_or(LINUX_EOVERFLOW)?;
        current_user_addr = current_user_addr
            .checked_add(chunk_len as u64)
            .ok_or(LINUX_EOVERFLOW)?;
        remaining -= chunk_len;
    }
    Ok(())
}

fn initialize_prepared_remote_file_bytes(
    address_space: &paging::ProcessAddressSpace,
    remote_id: u64,
    file_offset: u64,
    target_addr: u64,
    file_len: u64,
) -> Result<(), i64> {
    let mut current_file_offset = file_offset;
    let mut current_user_addr = target_addr;
    let mut remaining = usize::try_from(file_len).map_err(|_| LINUX_EOVERFLOW)?;
    while remaining != 0 {
        let chunk_len = remaining.min(VFS_IPC_PAYLOAD_CAPACITY);
        let bytes = call_remote_vfs_read_bytes(remote_id, current_file_offset, chunk_len)?;
        if bytes.len() != chunk_len {
            return Err(LINUX_ENOEXEC);
        }
        address_space
            .initialize_user_bytes(x86_64::VirtAddr::new(current_user_addr), bytes.as_slice())
            .map_err(address_space_error_to_linux_errno)?;
        current_file_offset = current_file_offset
            .checked_add(chunk_len as u64)
            .ok_or(LINUX_EOVERFLOW)?;
        current_user_addr = current_user_addr
            .checked_add(chunk_len as u64)
            .ok_or(LINUX_EOVERFLOW)?;
        remaining -= chunk_len;
    }
    Ok(())
}

fn proc_mapping_page_flags(flags: u64) -> crate::memory::PageTableFlags {
    let mut page_flags = crate::memory::PageTableFlags::empty();
    if flags & PROC_BROKER_MAP_WRITE != 0 {
        page_flags |= crate::memory::PageTableFlags::WRITABLE;
    }
    if flags & PROC_BROKER_MAP_EXEC == 0 {
        page_flags |= crate::memory::PageTableFlags::NO_EXECUTE;
    }
    page_flags
}

pub(super) fn syscall_linux_rustos_proc_abort_broker(args_ptr: u64) -> u64 {
    if !current_process_has_loader_broker_capability() {
        return linux_errno(LINUX_EACCES);
    }
    let args = match usermem::read_current_user_struct::<RustosProcAbortBrokerArgs>(args_ptr) {
        Ok(args) => args,
        Err(err) => return linux_errno(address_space_error_to_linux_errno(err)),
    };
    if args.reserved0 != 0 || args.prepare_handle == 0 {
        return linux_errno(LINUX_EINVAL);
    }
    match take_current_process_prepare(args.prepare_handle) {
        Some(_) => 0,
        None => linux_errno(LINUX_EINVAL),
    }
}

pub(super) fn syscall_linux_rustos_device_ioctl_broker(args_ptr: u64) -> u64 {
    if !current_process_has_device_broker_capability() {
        return linux_errno(LINUX_EACCES);
    }
    let args = match usermem::read_current_user_struct::<RustosDeviceIoctlBrokerArgs>(args_ptr) {
        Ok(args) => args,
        Err(err) => return linux_errno(address_space_error_to_linux_errno(err)),
    };
    if args.reserved0 != 0 || args.process_id == 0 {
        return linux_errno(LINUX_EINVAL);
    }
    match linux_ops::ioctl_for_process(args.process_id, args.fd, args.request, args.arg) {
        Ok(value) => value,
        Err(err) => linux_errno(linux_sysop_error_to_errno(err)),
    }
}

pub(super) fn syscall_linux_rustos_net_broker(args_ptr: u64) -> u64 {
    if !current_process_has_net_broker_capability() {
        return linux_errno(LINUX_EACCES);
    }
    let args = match usermem::read_current_user_struct::<RustosNetBrokerArgs>(args_ptr) {
        Ok(args) => args,
        Err(err) => return linux_errno(address_space_error_to_linux_errno(err)),
    };
    if args.reserved0 != 0 || args.reserved1 != 0 || args.process_id == 0 {
        return linux_errno(LINUX_EINVAL);
    }
    let result = match args.op {
        SYSCALL_OFFLOAD_OP_LINUX_SOCKET => {
            linux_ops::socket_for_process(args.process_id, args.arg0, args.arg1, args.arg2)
        }
        SYSCALL_OFFLOAD_OP_LINUX_SOCKETPAIR => linux_ops::socketpair_for_process(
            args.process_id,
            args.arg0,
            args.arg1,
            args.arg2,
            args.arg3,
        )
        .map(|()| 0),
        SYSCALL_OFFLOAD_OP_LINUX_BIND => {
            linux_ops::bind_for_process(args.process_id, args.arg0, args.arg1, args.arg2)
                .map(|()| 0)
        }
        SYSCALL_OFFLOAD_OP_LINUX_LISTEN => {
            linux_ops::listen_for_process(args.process_id, args.arg0, args.arg1).map(|()| 0)
        }
        SYSCALL_OFFLOAD_OP_LINUX_ACCEPT => linux_ops::accept4_for_process(
            args.process_id,
            args.arg0,
            args.arg1,
            args.arg2,
            args.arg3,
        ),
        SYSCALL_OFFLOAD_OP_LINUX_CONNECT => {
            linux_ops::connect_for_process(args.process_id, args.arg0, args.arg1, args.arg2)
                .map(|()| 0)
        }
        _ => Err(linux_ops::LinuxSysopError::InvalidArgument),
    };
    match result {
        Ok(value) => value,
        Err(err) => linux_errno(linux_sysop_error_to_errno(err)),
    }
}

pub(super) fn syscall_linux_rustos_driver_load_module_broker(args_ptr: u64) -> u64 {
    if !current_process_has_driver_broker_capability() {
        return linux_errno(LINUX_EACCES);
    }
    let args = match usermem::read_current_user_struct::<RustosDriverLoadModuleBrokerArgs>(args_ptr)
    {
        Ok(args) => args,
        Err(err) => return linux_errno(address_space_error_to_linux_errno(err)),
    };
    if args.reserved0 != 0 {
        return linux_errno(LINUX_EINVAL);
    }
    let name = match copy_current_user_bounded_string(
        args.name_ptr,
        args.name_len,
        DRIVER_BROKER_NAME_CAPACITY,
    ) {
        Ok(name) if !name.is_empty() => name,
        _ => return linux_errno(LINUX_EINVAL),
    };
    let image_path = match copy_current_user_bounded_string(
        args.path_ptr,
        args.path_len,
        DRIVER_BROKER_PATH_CAPACITY,
    ) {
        Ok(path) if !path.is_empty() => path,
        _ => return linux_errno(LINUX_EINVAL),
    };
    let linux_driver_names = match copy_current_user_bounded_string(
        args.linux_driver_names_ptr,
        args.linux_driver_names_len,
        DRIVER_BROKER_PATH_CAPACITY,
    ) {
        Ok(names) => names,
        Err(errno) => return linux_errno(errno),
    };

    let leaked_name: &'static str = Box::leak(name.into_boxed_str());
    let leaked_path: &'static str = Box::leak(image_path.into_boxed_str());
    let leaked_linux_driver_names: &'static str = Box::leak(linux_driver_names.into_boxed_str());
    match kernel_io_manager::api::driver::load_module_image_from_policy(
        leaked_name,
        args.class,
        args.bus,
        leaked_path,
        leaked_linux_driver_names,
    ) {
        Ok(()) => 0,
        Err(error) => {
            debug::println!(
                "driver broker load failed: name={} class={} bus={} path={} error={}",
                leaked_name,
                args.class,
                args.bus,
                leaked_path,
                error,
            );
            linux_errno(LINUX_ENOEXEC)
        }
    }
}

pub(super) fn syscall_linux_rustos_driver_probe_alias_broker(args_ptr: u64) -> u64 {
    if !current_process_has_driver_broker_capability() {
        return linux_errno(LINUX_EACCES);
    }
    let args = match usermem::read_current_user_struct::<RustosDriverProbeAliasBrokerArgs>(args_ptr)
    {
        Ok(args) => args,
        Err(err) => return linux_errno(address_space_error_to_linux_errno(err)),
    };
    if args.reserved0 != 0 {
        return linux_errno(LINUX_EINVAL);
    }
    let alias = match copy_current_user_bounded_string(
        args.alias_ptr,
        args.alias_len,
        DRIVER_BROKER_ALIAS_CAPACITY,
    ) {
        Ok(alias) if !alias.is_empty() => alias,
        _ => return linux_errno(LINUX_EINVAL),
    };
    u64::from(
        kernel_io_manager::api::driver::device_alias_present_from_policy(
            alias.as_str(),
            args.class,
            args.bus,
        ),
    )
}

pub(super) fn syscall_linux_rustos_driver_provider_active_broker(args_ptr: u64) -> u64 {
    if !current_process_has_driver_broker_capability() {
        return linux_errno(LINUX_EACCES);
    }
    let args =
        match usermem::read_current_user_struct::<RustosDriverProviderActiveBrokerArgs>(args_ptr) {
            Ok(args) => args,
            Err(err) => return linux_errno(address_space_error_to_linux_errno(err)),
        };
    if args.reserved0 != 0 {
        return linux_errno(LINUX_EINVAL);
    }
    let provider_group = match copy_current_user_bounded_string(
        args.provider_group_ptr,
        args.provider_group_len,
        DRIVER_BROKER_NAME_CAPACITY,
    ) {
        Ok(provider_group) if !provider_group.is_empty() => provider_group,
        _ => return linux_errno(LINUX_EINVAL),
    };
    u64::from(
        kernel_io_manager::api::driver::provider_group_active_from_policy(provider_group.as_str()),
    )
}

fn current_process_vfs_file_handle(fd: u64) -> Result<multitask::VfsFileHandle, i64> {
    let Some(result) = multitask::with_current_user_process_state(|_, _, process_state| {
        let Some(entry) = process_state.handles().get_entry(fd) else {
            return Err(LINUX_EBADF);
        };
        match entry.handle() {
            multitask::KernelHandle::VfsFile(file) => Ok(file.clone()),
            _ => Err(LINUX_EBADF),
        }
    }) else {
        return Err(LINUX_EINVAL);
    };
    result
}

pub(super) fn syscall_linux_rustos_block_broker(args_ptr: u64) -> u64 {
    if !current_process_has_vfs_broker_capability() {
        return linux_errno(LINUX_EACCES);
    }
    let args = match usermem::read_current_user_struct::<RustosBlockBrokerArgs>(args_ptr) {
        Ok(args) => args,
        Err(err) => return linux_errno(address_space_error_to_linux_errno(err)),
    };
    if args.abi_version != BLOCK_BROKER_ABI_VERSION || args.reserved0 != 0 {
        return linux_errno(LINUX_EINVAL);
    }
    match args.op {
        BLOCK_BROKER_OP_BOOT_INFO => {
            if args.out_logical_block_size_ptr == 0 || args.out_block_count_ptr == 0 {
                return linux_errno(LINUX_EINVAL);
            }
            let Some(descriptor) = kernel_io_manager::api::boot_volume_descriptor() else {
                return linux_errno(LINUX_ENODEV);
            };
            if let Err(err) = write_current_user_u64(
                args.out_logical_block_size_ptr,
                descriptor.logical_block_size as u64,
            ) {
                return linux_errno(address_space_error_to_linux_errno(err));
            }
            if let Err(err) =
                write_current_user_u64(args.out_block_count_ptr, descriptor.block_count)
            {
                return linux_errno(address_space_error_to_linux_errno(err));
            }
            0
        }
        BLOCK_BROKER_OP_BOOT_READ => {
            let len = match usize::try_from(args.buffer_len) {
                Ok(len) => len,
                Err(_) => return linux_errno(LINUX_EINVAL),
            };
            if len == 0 || len > BLOCK_BROKER_MAX_IO_BYTES || args.buffer_ptr == 0 {
                return linux_errno(LINUX_EINVAL);
            }
            let Some(descriptor) = kernel_io_manager::api::boot_volume_descriptor() else {
                return linux_errno(LINUX_ENODEV);
            };
            let block_size = descriptor.logical_block_size;
            if block_size == 0
                || len % block_size != 0
                || args.block_count != (len / block_size) as u64
                || args
                    .lba
                    .checked_add(args.block_count)
                    .is_none_or(|end| end > descriptor.block_count)
            {
                return linux_errno(LINUX_EINVAL);
            }
            if let Err(err) = usermem::validate_current_user_write_buffer(args.buffer_ptr, len) {
                return linux_errno(address_space_error_to_linux_errno(err));
            }
            let mut bytes = alloc::vec![0_u8; len];
            if let Err(err) = kernel_io_manager::api::read_boot_volume_blocks(args.lba, &mut bytes)
            {
                return linux_errno(storage_error_to_linux_errno(err));
            }
            match usermem::write_current_user_bytes(args.buffer_ptr, bytes.as_slice()) {
                Ok(()) => 0,
                Err(err) => linux_errno(address_space_error_to_linux_errno(err)),
            }
        }
        _ => linux_errno(LINUX_EINVAL),
    }
}

fn push_current_process_prepare_mapping(
    handle: u64,
    mapping: ProcPrepareMapping,
) -> Result<(), i64> {
    if handle == 0 {
        return Err(LINUX_EINVAL);
    }
    let owner_pid = multitask::current_user_process_id().ok_or(LINUX_EINVAL)?;
    let mut prepares = PROC_PREPARES.lock();
    let Some(record) = prepares.get_mut(&handle) else {
        return Err(LINUX_EINVAL);
    };
    if record.owner_pid != owner_pid {
        return Err(LINUX_EINVAL);
    }
    if record.mappings.len() >= MAX_PROC_PREPARE_MAPPINGS {
        return Err(LINUX_E2BIG);
    }
    if record
        .mappings
        .iter()
        .any(|existing| proc_mappings_overlap(existing, &mapping))
    {
        return Err(LINUX_EINVAL);
    }
    record.mappings.push(mapping);
    Ok(())
}

fn validate_proc_mapping_range(
    target_addr: u64,
    mem_len: u64,
    flags: u64,
    requires_file_data: bool,
) -> Result<(), i64> {
    if mem_len == 0
        || target_addr & 0xfff != 0
        || mem_len & 0xfff != 0
        || flags & !PROC_MAP_FLAGS_MASK != 0
        || flags & (PROC_BROKER_MAP_READ | PROC_BROKER_MAP_WRITE | PROC_BROKER_MAP_EXEC) == 0
        || flags & PROC_BROKER_MAP_PRIVATE == 0
    {
        return Err(LINUX_EINVAL);
    }
    if requires_file_data && flags & PROC_BROKER_MAP_READ == 0 {
        return Err(LINUX_EINVAL);
    }
    let end = target_addr.checked_add(mem_len).ok_or(LINUX_EOVERFLOW)?;
    if target_addr < crate::memory::paging::USER_SPACE_BASE
        || end > crate::memory::paging::USER_SPACE_END_EXCLUSIVE
    {
        return Err(LINUX_EINVAL);
    }
    Ok(())
}

fn proc_mappings_overlap(left: &ProcPrepareMapping, right: &ProcPrepareMapping) -> bool {
    let (left_start, left_len) = proc_mapping_range(left);
    let (right_start, right_len) = proc_mapping_range(right);
    let Some(left_end) = left_start.checked_add(left_len) else {
        return true;
    };
    let Some(right_end) = right_start.checked_add(right_len) else {
        return true;
    };
    left_start < right_end && right_start < left_end
}

fn proc_mapping_range(mapping: &ProcPrepareMapping) -> (u64, u64) {
    match mapping {
        ProcPrepareMapping::File {
            target_addr,
            mem_len,
            ..
        }
        | ProcPrepareMapping::RemoteFile {
            target_addr,
            mem_len,
            ..
        }
        | ProcPrepareMapping::Zeroed {
            target_addr,
            mem_len,
            ..
        } => (*target_addr, *mem_len),
    }
}

fn take_current_process_prepare(handle: u64) -> Option<ProcPrepareRecord> {
    if handle == 0 {
        return None;
    }
    let owner_pid = multitask::current_user_process_id()?;
    let mut prepares = PROC_PREPARES.lock();
    if prepares
        .get(&handle)
        .is_none_or(|record| record.owner_pid != owner_pid)
    {
        return None;
    }
    prepares.remove(&handle)
}

fn call_path_offload(
    op: u16,
    dirfd: u64,
    flags: u64,
    arg0: u32,
    absolute_path: &str,
) -> Result<LinuxSyscallOffloadResponse, i64> {
    let path_bytes = absolute_path.as_bytes();
    if path_bytes.len() > SYSCALL_OFFLOAD_PATH_CAPACITY {
        return Err(LINUX_EINVAL);
    }
    let mut request = LinuxSyscallOffloadRequest {
        op,
        dirfd,
        flags,
        mask: arg0,
        path_len: path_bytes.len() as u32,
        ..LinuxSyscallOffloadRequest::default()
    };
    populate_offload_identity(&mut request);
    request.path[..path_bytes.len()].copy_from_slice(path_bytes);

    call_vfs_offload_request(&request)
}

fn new_offload_request(op: u16) -> LinuxSyscallOffloadRequest {
    let mut request = LinuxSyscallOffloadRequest {
        op,
        ..LinuxSyscallOffloadRequest::default()
    };
    populate_offload_identity(&mut request);
    request
}

fn new_vfs_request(op: u16) -> VfsIpcRequest {
    let mut request = VfsIpcRequest {
        op,
        ..VfsIpcRequest::default()
    };
    populate_vfs_identity(&mut request);
    request
}

fn populate_vfs_path(request: &mut VfsIpcRequest, path: &str) -> Result<(), i64> {
    let path_bytes = path.as_bytes();
    if path_bytes.len() > VFS_IPC_PATH_CAPACITY {
        return Err(LINUX_EINVAL);
    }
    request.path_len = path_bytes.len() as u32;
    request.path[..path_bytes.len()].copy_from_slice(path_bytes);
    Ok(())
}

fn populate_vfs_identity(request: &mut VfsIpcRequest) {
    if let Some(snapshot) = multitask::current_user_snapshot() {
        request.pid = snapshot.process_id();
        request.tid = snapshot.thread_id();
        request.session_handle = snapshot.console_session().raw();
    }
    if let Some(security) = multitask::with_current_process_credentials(|security| security) {
        request.uid = security.uid();
        request.gid = security.gid();
        request.euid = security.euid();
        request.egid = security.egid();
    }
}

fn call_vfs_ipc_request(request: &VfsIpcRequest) -> Result<VfsIpcResponse, i64> {
    let response = ipc_ops::call_service_endpoint(linux_abi::IPC_SERVICE_VFSD, as_bytes(request))?;
    if response.len() != size_of::<VfsIpcResponse>() {
        return Err(LINUX_EINVAL);
    }
    let response = read_unaligned::<VfsIpcResponse>(response.as_slice());
    if response.version != VFS_IPC_ABI_VERSION
        || response.op != request.op
        || response.reserved0 != 0
    {
        return Err(LINUX_EINVAL);
    }
    Ok(response)
}

fn current_remote_vfs_handle(fd: u64) -> Result<Option<multitask::RemoteVfsHandle>, i64> {
    if fd < 3 {
        return Ok(None);
    }
    let Some(result) = multitask::with_current_user_process_state(|_, _, process_state| {
        Ok(match process_state.handles().get(fd) {
            Some(multitask::KernelHandle::RemoteVfs(handle)) => Some(handle.clone()),
            Some(_) => None,
            None => return Err(LINUX_EBADF),
        })
    }) else {
        return Err(LINUX_EINVAL);
    };
    result
}

fn call_offload_request(
    request: &LinuxSyscallOffloadRequest,
) -> Result<LinuxSyscallOffloadResponse, i64> {
    let response = ipc_ops::call_linux_syscall_endpoint(as_bytes(request))?;
    decode_offload_response(request, response.as_slice())
}

fn call_vfs_offload_request(
    request: &LinuxSyscallOffloadRequest,
) -> Result<LinuxSyscallOffloadResponse, i64> {
    call_service_offload_request(linux_abi::IPC_SERVICE_VFSD, request)
}

fn call_service_offload_request(
    service_id: u64,
    request: &LinuxSyscallOffloadRequest,
) -> Result<LinuxSyscallOffloadResponse, i64> {
    let response = ipc_ops::call_service_endpoint(service_id, as_bytes(request))?;
    decode_offload_response(request, response.as_slice())
}

fn decode_offload_response(
    request: &LinuxSyscallOffloadRequest,
    response: &[u8],
) -> Result<LinuxSyscallOffloadResponse, i64> {
    if response.len() != size_of::<LinuxSyscallOffloadResponse>() {
        return Err(LINUX_EINVAL);
    }
    let response = read_unaligned::<LinuxSyscallOffloadResponse>(response);
    if response.version != rustos_user_abi::syscall::SYSCALL_OFFLOAD_ABI_VERSION
        || response.op != request.op
        || response.reserved0 != 0
    {
        return Err(LINUX_EINVAL);
    }
    Ok(response)
}

fn populate_offload_identity(request: &mut LinuxSyscallOffloadRequest) {
    if let Some(snapshot) = multitask::current_user_snapshot() {
        request.pid = snapshot.process_id();
        request.tid = snapshot.thread_id();
        request.session_handle = snapshot.console_session().raw();
    }
    if let Some(security) = multitask::with_current_process_credentials(|security| security) {
        request.uid = security.uid();
        request.gid = security.gid();
        request.euid = security.euid();
        request.egid = security.egid();
    }
}

pub(super) fn current_process_may_bootstrap_policy_service() -> bool {
    if !ipc_ops::service_registered(linux_abi::IPC_SERVICE_VFSD) {
        return true;
    }
    current_process_has_policy_service_capability()
}

fn current_process_has_policy_service_capability() -> bool {
    ipc_ops::current_process_has_any_service_capability(
        rustos_user_abi::syscall::IPC_SERVICE_CAP_BOOTSTRAP_POLICY,
    )
}

fn current_process_has_vfs_broker_capability() -> bool {
    ipc_ops::current_process_has_service_capability(
        rustos_user_abi::syscall::IPC_SERVICE_CAP_VFS_POLICY,
    )
}

fn current_process_has_loader_broker_capability() -> bool {
    ipc_ops::current_process_has_service_capability(
        rustos_user_abi::syscall::IPC_SERVICE_CAP_PROCESS_LOADER,
    )
}

fn current_process_has_device_broker_capability() -> bool {
    ipc_ops::current_process_has_service_capability(
        rustos_user_abi::syscall::IPC_SERVICE_CAP_DEVICE_POLICY,
    )
}

fn current_process_has_net_broker_capability() -> bool {
    ipc_ops::current_process_has_service_capability(
        rustos_user_abi::syscall::IPC_SERVICE_CAP_NET_POLICY,
    )
}

fn current_process_has_driver_broker_capability() -> bool {
    ipc_ops::current_process_has_service_capability(
        rustos_user_abi::syscall::IPC_SERVICE_CAP_DRIVER_POLICY,
    )
}

fn copy_current_user_bounded_string(ptr: u64, len: u64, capacity: usize) -> Result<String, i64> {
    let len = usize::try_from(len).map_err(|_| LINUX_EINVAL)?;
    if len > capacity || (len != 0 && ptr == 0) {
        return Err(LINUX_EINVAL);
    }
    if len == 0 {
        return Ok(String::new());
    }
    let mut bytes = alloc::vec![0_u8; len];
    usermem::copy_from_current_user_exact(ptr, &mut bytes)
        .map_err(address_space_error_to_linux_errno)?;
    let text = core::str::from_utf8(bytes.as_slice()).map_err(|_| LINUX_EINVAL)?;
    Ok(String::from(text))
}

fn copy_metadata_path(path_ptr: u64, path_len: u64) -> Result<alloc::string::String, i64> {
    let Ok(path_len) = usize::try_from(path_len) else {
        return Err(LINUX_EINVAL);
    };
    if path_len == 0 || path_len > SYSCALL_OFFLOAD_PATH_CAPACITY {
        return Err(LINUX_EINVAL);
    }
    let mut path = alloc::vec![0_u8; path_len];
    usermem::copy_from_current_user_exact(path_ptr, &mut path)
        .map_err(address_space_error_to_linux_errno)?;
    let path = core::str::from_utf8(path.as_slice()).map_err(|_| LINUX_EINVAL)?;
    Ok(alloc::string::String::from(path))
}
