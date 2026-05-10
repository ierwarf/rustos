use super::*;
use core::{mem::size_of, slice};

use rustos_user_abi::syscall::{
    LINUX_CPUSET_BYTES, LINUX_RLIMIT_SIZE, LINUX_STAT_SIZE, LINUX_STATX_SIZE, LINUX_UTSNAME_SIZE,
    LinuxSyscallOffloadRequest, LinuxSyscallOffloadResponse, RustosProcCommitBrokerArgs,
    RustosVfsMountBrokerArgs, SYSCALL_OFFLOAD_OP_LINUX_ACCESS, SYSCALL_OFFLOAD_OP_LINUX_CHDIR,
    SYSCALL_OFFLOAD_OP_LINUX_CLOSE, SYSCALL_OFFLOAD_OP_LINUX_DUP, SYSCALL_OFFLOAD_OP_LINUX_FCNTL,
    SYSCALL_OFFLOAD_OP_LINUX_GETCWD, SYSCALL_OFFLOAD_OP_LINUX_GETDENTS64,
    SYSCALL_OFFLOAD_OP_LINUX_GETEGID, SYSCALL_OFFLOAD_OP_LINUX_GETEUID,
    SYSCALL_OFFLOAD_OP_LINUX_GETGID, SYSCALL_OFFLOAD_OP_LINUX_GETUID,
    SYSCALL_OFFLOAD_OP_LINUX_MKDIR, SYSCALL_OFFLOAD_OP_LINUX_MOUNT,
    SYSCALL_OFFLOAD_OP_LINUX_NEWFSTATAT, SYSCALL_OFFLOAD_OP_LINUX_OPENAT,
    SYSCALL_OFFLOAD_OP_LINUX_PRLIMIT64, SYSCALL_OFFLOAD_OP_LINUX_READLINKAT,
    SYSCALL_OFFLOAD_OP_LINUX_SCHED_GETAFFINITY, SYSCALL_OFFLOAD_OP_LINUX_SETGID,
    SYSCALL_OFFLOAD_OP_LINUX_SETUID, SYSCALL_OFFLOAD_OP_LINUX_STATX,
    SYSCALL_OFFLOAD_OP_LINUX_UMOUNT2, SYSCALL_OFFLOAD_OP_LINUX_UNAME,
    SYSCALL_OFFLOAD_PATH_CAPACITY, SYSCALL_OFFLOAD_PAYLOAD_CAPACITY,
};

pub(super) const VFSD_DUP_MODE_DUP: u32 = 0;
pub(super) const VFSD_DUP_MODE_DUP2: u32 = 1;
pub(super) const VFSD_DUP_MODE_DUP3: u32 = 2;

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
    match linux_ops::setuid_authorized(uid) {
        Ok(result) => result,
        Err(err) => linux_errno(linux_sysop_error_to_errno(err)),
    }
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
    match linux_ops::setgid_authorized(gid) {
        Ok(result) => result,
        Err(err) => linux_errno(linux_sysop_error_to_errno(err)),
    }
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

pub(super) fn call_vfs_openat_with_fd(
    dirfd: u64,
    flags: u64,
    mode: u32,
    absolute_path: &str,
) -> Result<u64, i64> {
    let path_bytes = absolute_path.as_bytes();
    if path_bytes.len() > SYSCALL_OFFLOAD_PATH_CAPACITY {
        return Err(LINUX_EINVAL);
    }
    let mut request = LinuxSyscallOffloadRequest {
        op: SYSCALL_OFFLOAD_OP_LINUX_OPENAT,
        dirfd,
        flags,
        mask: mode,
        path_len: path_bytes.len() as u32,
        ..LinuxSyscallOffloadRequest::default()
    };
    populate_offload_identity(&mut request);
    request.path[..path_bytes.len()].copy_from_slice(path_bytes);

    let (response, mut entries) = ipc_ops::call_service_endpoint_with_received_entries(
        linux_abi::IPC_SERVICE_VFSD,
        as_bytes(&request),
        1,
    )?;
    let response = decode_offload_response(&request, response.as_slice())?;
    if response.status != 0 {
        return Err(response.status.unsigned_abs() as i64);
    }
    if response.payload_len != 0 || entries.len() != 1 {
        return Err(LINUX_EINVAL);
    }

    let entry = entries.pop().ok_or(LINUX_EINVAL)?;
    let Some(fd) = multitask::with_current_user_process_state_mut(|_, _, process_state| {
        Ok(process_state.handles_mut().install_transferred(entry))
    }) else {
        return Err(LINUX_EINVAL);
    };
    fd
}

pub(super) fn call_vfs_close_fd(fd: u64) -> Result<(), i64> {
    let mut request = new_offload_request(SYSCALL_OFFLOAD_OP_LINUX_CLOSE);
    request.dirfd = fd;
    let response = call_vfs_offload_request(&request)?;
    if response.status != 0 {
        return Err(response.status.unsigned_abs() as i64);
    }
    if response.payload_len != 0 {
        return Err(LINUX_EINVAL);
    }
    Ok(())
}

pub(super) fn call_vfs_dup_fd(oldfd: u64, newfd: u64, flags: u64, mode: u32) -> Result<u64, i64> {
    let mut request = new_offload_request(SYSCALL_OFFLOAD_OP_LINUX_DUP);
    request.dirfd = oldfd;
    request.arg0 = newfd;
    request.arg1 = flags;
    request.mask = mode;
    let response = call_vfs_offload_request(&request)?;
    if response.status != 0 {
        return Err(response.status.unsigned_abs() as i64);
    }
    if response.payload_len as usize != size_of::<u64>() {
        return Err(LINUX_EINVAL);
    }
    let mut bytes = [0_u8; size_of::<u64>()];
    bytes.copy_from_slice(&response.payload[..size_of::<u64>()]);
    Ok(u64::from_ne_bytes(bytes))
}

pub(super) fn call_vfs_getdents64(fd: u64, user_ptr: u64, user_len: u64) -> Result<u64, i64> {
    let mut request = new_offload_request(SYSCALL_OFFLOAD_OP_LINUX_GETDENTS64);
    request.dirfd = fd;
    request.arg0 = user_ptr;
    request.arg1 = user_len;
    let response = call_vfs_offload_request(&request)?;
    if response.status != 0 {
        return Err(response.status.unsigned_abs() as i64);
    }
    if response.payload_len as usize != size_of::<u64>() {
        return Err(LINUX_EINVAL);
    }
    let mut bytes = [0_u8; size_of::<u64>()];
    bytes.copy_from_slice(&response.payload[..size_of::<u64>()]);
    Ok(u64::from_ne_bytes(bytes))
}

pub(super) fn call_vfs_fcntl(fd: u64, cmd: u64, arg: u64) -> Result<u64, i64> {
    let mut request = new_offload_request(SYSCALL_OFFLOAD_OP_LINUX_FCNTL);
    request.dirfd = fd;
    request.arg0 = cmd;
    request.arg1 = arg;
    let response = call_vfs_offload_request(&request)?;
    if response.status != 0 {
        return Err(response.status.unsigned_abs() as i64);
    }
    if response.payload_len as usize != size_of::<u64>() {
        return Err(LINUX_EINVAL);
    }
    let mut bytes = [0_u8; size_of::<u64>()];
    bytes.copy_from_slice(&response.payload[..size_of::<u64>()]);
    Ok(u64::from_ne_bytes(bytes))
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
    if !current_process_is_vfsd() {
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
    if !current_process_is_vfsd() {
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
    if !current_process_is_vfsd() {
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
    if !current_process_is_vfsd() {
        return linux_errno(LINUX_EACCES);
    }
    match linux_ops::fcntl_for_process(process_id, fd, cmd, arg) {
        Ok(result) => result,
        Err(err) => linux_errno(linux_sysop_error_to_errno(err)),
    }
}

pub(super) fn syscall_linux_rustos_vfs_mount_broker(args_ptr: u64) -> u64 {
    if !current_process_is_vfsd() {
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
    if !current_process_is_vfsd() {
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

pub(super) fn syscall_linux_rustos_proc_prepare_broker(_args_ptr: u64) -> u64 {
    if !current_process_is_loaderd() {
        return linux_errno(LINUX_EACCES);
    }
    linux_errno(LINUX_EOPNOTSUPP)
}

pub(super) fn syscall_linux_rustos_proc_map_file_broker(_args_ptr: u64) -> u64 {
    if !current_process_is_loaderd() {
        return linux_errno(LINUX_EACCES);
    }
    linux_errno(LINUX_EOPNOTSUPP)
}

pub(super) fn syscall_linux_rustos_proc_map_zeroed_broker(_args_ptr: u64) -> u64 {
    if !current_process_is_loaderd() {
        return linux_errno(LINUX_EACCES);
    }
    linux_errno(LINUX_EOPNOTSUPP)
}

pub(super) fn syscall_linux_rustos_proc_commit_broker(args_ptr: u64) -> u64 {
    if !current_process_is_loaderd() {
        return linux_errno(LINUX_EACCES);
    }
    let args = match usermem::read_current_user_struct::<RustosProcCommitBrokerArgs>(args_ptr) {
        Ok(args) => args,
        Err(err) => return linux_errno(address_space_error_to_linux_errno(err)),
    };
    if args.prepare_handle != 0
        || args.reserved0 != 0
        || args.exec_path_ptr == 0
        || args.exec_path_len == 0
    {
        return linux_errno(LINUX_EINVAL);
    }
    match linux_ops::spawn_exec(
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

pub(super) fn syscall_linux_rustos_proc_abort_broker(_args_ptr: u64) -> u64 {
    if !current_process_is_loaderd() {
        return linux_errno(LINUX_EACCES);
    }
    0
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
    if ipc_ops::service_endpoint(linux_abi::IPC_SERVICE_VFSD).is_none() {
        return true;
    }
    current_process_is_policy_service()
}

fn current_process_is_policy_service() -> bool {
    multitask::with_current_user_process_state(|_, _, process_state| {
        process_path_is_one_of(
            process_state.exec_path(),
            &[
                "services/initd/initd.elf",
                "services/syscalld/syscalld.elf",
                "services/vfsd/vfsd.elf",
                "services/netd/netd.elf",
                "services/devmgrd/devmgrd.elf",
                "services/driverd/driverd.elf",
                "services/loaderd/loaderd.elf",
            ],
        )
    })
    .unwrap_or(false)
}

fn current_process_is_vfsd() -> bool {
    multitask::with_current_user_process_state(|_, _, process_state| {
        process_path_is(process_state.exec_path(), "services/vfsd/vfsd.elf")
    })
    .unwrap_or(false)
}

fn current_process_is_loaderd() -> bool {
    multitask::with_current_user_process_state(|_, _, process_state| {
        process_path_is(process_state.exec_path(), "services/loaderd/loaderd.elf")
    })
    .unwrap_or(false)
}

fn process_path_is(actual: &str, expected: &str) -> bool {
    actual == expected || actual.strip_prefix('/') == Some(expected)
}

fn process_path_is_one_of(actual: &str, expected: &[&str]) -> bool {
    expected.iter().any(|path| process_path_is(actual, path))
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
