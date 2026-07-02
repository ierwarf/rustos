use super::*;

pub(super) fn syscall_linux_syscalld_uname(buf_ptr: u64) -> u64 {
    if let Err(err) = usermem::validate_current_user_write_buffer(buf_ptr, LINUX_UTSNAME_SIZE) {
        return linux_errno(address_space_error_to_linux_errno(err));
    }
    let response = match call_syscalld(new_syscalld_request(SYSCALL_OFFLOAD_OP_LINUX_UNAME)) {
        Ok(response) => response,
        Err(errno) => return linux_errno(errno),
    };
    if let Err(errno) = ensure_syscalld_payload(&response, LINUX_UTSNAME_SIZE) {
        return linux_errno(errno);
    }
    match usermem::write_current_user_bytes(buf_ptr, &response.payload[..LINUX_UTSNAME_SIZE]) {
        Ok(()) => 0,
        Err(err) => linux_errno(address_space_error_to_linux_errno(err)),
    }
}

pub(super) fn syscall_linux_syscalld_prlimit64(
    pid: u64,
    resource: u64,
    new_limit_ptr: u64,
    old_limit_ptr: u64,
) -> u64 {
    let mut request = new_syscalld_request(SYSCALL_OFFLOAD_OP_LINUX_PRLIMIT64);
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
    let response = match call_syscalld(request) {
        Ok(response) => response,
        Err(errno) => return linux_errno(errno),
    };
    if let Err(errno) = ensure_syscalld_status(&response) {
        return linux_errno(errno);
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

pub(super) fn syscall_linux_syscalld_sched_getaffinity(
    pid: u64,
    user_len: u64,
    mask_ptr: u64,
) -> u64 {
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
    let mut request = new_syscalld_request(SYSCALL_OFFLOAD_OP_LINUX_SCHED_GETAFFINITY);
    request.dirfd = pid;
    request.flags = user_len as u64;
    let response = match call_syscalld(request) {
        Ok(response) => response,
        Err(errno) => return linux_errno(errno),
    };
    if let Err(errno) = ensure_syscalld_status(&response) {
        return linux_errno(errno);
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

pub(super) fn syscall_linux_syscalld_id_getter(op: u16) -> u64 {
    let response = match call_syscalld(new_syscalld_request(op)) {
        Ok(response) => response,
        Err(errno) => return linux_errno(errno),
    };
    if let Err(errno) = ensure_syscalld_payload(&response, size_of::<u32>()) {
        return linux_errno(errno);
    }
    u32::from_le_bytes([
        response.payload[0],
        response.payload[1],
        response.payload[2],
        response.payload[3],
    ]) as u64
}

pub(super) fn syscall_linux_syscalld_setid(op: u16, id: u64) -> u64 {
    let Ok(id) = u32::try_from(id) else {
        return linux_errno(LINUX_EINVAL);
    };
    let mut request = new_syscalld_request(op);
    request.mask = id;
    match call_syscalld(request).and_then(|response| ensure_empty_syscalld_response(&response)) {
        Ok(()) => 0,
        Err(errno) => linux_errno(errno),
    }
}

pub(super) fn syscall_linux_syscalld_umask(new_mask: u64) -> u64 {
    let mut request = new_syscalld_request(SYSCALL_OFFLOAD_OP_LINUX_UMASK);
    request.mask = (new_mask & 0o777) as u32;
    let response = match call_syscalld(request) {
        Ok(response) => response,
        Err(errno) => return linux_errno(errno),
    };
    if let Err(errno) = ensure_syscalld_payload(&response, size_of::<u32>()) {
        return linux_errno(errno);
    }
    u32::from_le_bytes([
        response.payload[0],
        response.payload[1],
        response.payload[2],
        response.payload[3],
    ]) as u64
}

pub(super) fn syscall_linux_syscalld_u64_getter(op: u16, target_pid: u64) -> u64 {
    let mut request = new_syscalld_request(op);
    request.dirfd = target_pid;
    let response = match call_syscalld(request) {
        Ok(response) => response,
        Err(errno) => return linux_errno(errno),
    };
    if let Err(errno) = ensure_syscalld_payload(&response, size_of::<u64>()) {
        return linux_errno(errno);
    }
    let mut bytes = [0_u8; size_of::<u64>()];
    bytes.copy_from_slice(&response.payload[..size_of::<u64>()]);
    u64::from_le_bytes(bytes)
}

pub(super) fn syscall_linux_syscalld_setpgid(target_pid: u64, pgid: u64) -> u64 {
    let mut request = new_syscalld_request(SYSCALL_OFFLOAD_OP_LINUX_SETPGID);
    request.dirfd = target_pid;
    request.arg0 = pgid;
    match call_syscalld(request).and_then(|response| ensure_empty_syscalld_response(&response)) {
        Ok(()) => 0,
        Err(errno) => linux_errno(errno),
    }
}

pub(super) fn syscall_linux_syscalld_getrandom(user_ptr: u64, user_len: u64, flags: u64) -> u64 {
    let Ok(len) = usize::try_from(user_len) else {
        return linux_errno(LINUX_EINVAL);
    };
    if len == 0 {
        return 0;
    }
    let mut copied = 0usize;
    while copied < len {
        let chunk_len = (len - copied).min(SYSCALL_OFFLOAD_PAYLOAD_CAPACITY);
        let mut request = new_syscalld_request(SYSCALL_OFFLOAD_OP_LINUX_GETRANDOM);
        request.flags = chunk_len as u64;
        request.arg0 = flags;
        let response = match call_syscalld(request) {
            Ok(response) => response,
            Err(errno) => return linux_errno(errno),
        };
        if let Err(errno) = ensure_syscalld_status(&response) {
            return linux_errno(errno);
        }
        let payload_len = response.payload_len as usize;
        if payload_len == 0 || payload_len > chunk_len {
            return linux_errno(LINUX_EINVAL);
        }
        let Some(dest_ptr) = user_ptr.checked_add(copied as u64) else {
            return linux_errno(LINUX_EINVAL);
        };
        if let Err(err) =
            usermem::write_current_user_bytes(dest_ptr, &response.payload[..payload_len])
        {
            return linux_errno(address_space_error_to_linux_errno(err));
        }
        copied += payload_len;
    }
    copied as u64
}

pub(super) fn syscall_linux_wait4(pid: i64, status_ptr: u64, options: u64, rusage_ptr: u64) -> u64 {
    if status_ptr != 0 {
        if let Err(err) = usermem::validate_current_user_write_buffer(status_ptr, size_of::<i32>())
        {
            return linux_errno(address_space_error_to_linux_errno(err));
        }
    }
    if rusage_ptr != 0 {
        if let Err(err) = usermem::validate_current_user_write_buffer(
            rusage_ptr,
            size_of::<linux_abi::LinuxRusage>(),
        ) {
            return linux_errno(address_space_error_to_linux_errno(err));
        }
    }

    let mut request = new_procd_request(PROCD_OP_WAIT4);
    request.arg0 = pid as u64;
    request.arg1 = options;
    request.arg2 = status_ptr;
    request.arg3 = rusage_ptr;
    if let Err(errno) =
        call_procd(&request).and_then(|response| ensure_empty_procd_response(&response))
    {
        return linux_errno(errno);
    }

    let parent_pid = match multitask::current_user_process_id() {
        Some(pid) => pid,
        None => return linux_errno(LINUX_ENOSYS),
    };
    let nohang = options & linux_abi::WNOHANG as u64 != 0;
    loop {
        match multitask::wait_for_child(parent_pid, pid) {
            multitask::WaitChildResult::Exited {
                pid: child_pid,
                status,
            } => {
                if status_ptr != 0 {
                    if let Err(err) = usermem::write_current_user_struct(status_ptr, &status) {
                        return linux_errno(address_space_error_to_linux_errno(err));
                    }
                }
                if rusage_ptr != 0 {
                    if let Err(err) = usermem::write_current_user_struct(
                        rusage_ptr,
                        &linux_abi::LinuxRusage::default(),
                    ) {
                        return linux_errno(address_space_error_to_linux_errno(err));
                    }
                }
                return child_pid;
            }
            multitask::WaitChildResult::Pending if nohang => return 0,
            multitask::WaitChildResult::Pending => {
                multitask::yield_now();
                if let Some(state) = multitask::current_linux_thread_state() {
                    if state.pending_signals & !state.signal_mask != 0 {
                        return linux_errno(LINUX_EINTR);
                    }
                }
            }
            multitask::WaitChildResult::NoMatchingChild if nohang => return 0,
            multitask::WaitChildResult::NoMatchingChild => return linux_errno(LINUX_ECHILD),
        }
    }
}

pub(super) fn syscall_linux_memfd_create(name_ptr: u64, flags: u64) -> u64 {
    let name = match usermem::read_current_user_c_string(name_ptr, 249) {
        Ok(name) if !name.is_empty() => name,
        Ok(_) => return linux_errno(LINUX_EINVAL),
        Err(err) => return linux_errno(address_space_error_to_linux_errno(err)),
    };
    let mut request = new_syscalld_request(SYSCALL_OFFLOAD_OP_LINUX_MEMFD_CREATE);
    request.flags = flags;
    request.path_len = name.len() as u32;
    request.path[..name.len()].copy_from_slice(name.as_bytes());
    if let Err(errno) =
        call_syscalld(request).and_then(|response| ensure_empty_syscalld_response(&response))
    {
        return linux_errno(errno);
    }

    let fd_flags = if flags & linux_abi::MFD_CLOEXEC != 0 {
        multitask::FD_CLOEXEC
    } else {
        0
    };
    let handle = multitask::KernelHandle::Memfd(multitask::MemfdHandle::new(
        name,
        flags & linux_abi::MFD_ALLOW_SEALING != 0,
    ));
    match multitask::with_current_user_process_state_mut(|_, _, process_state| {
        process_state
            .handles_mut()
            .install_entry(multitask::HandleEntry::new(
                handle,
                fd_flags,
                linux_abi::O_RDWR,
            ))
    }) {
        Some(fd) => fd,
        None => linux_errno(LINUX_ENOSYS),
    }
}

pub(super) fn new_syscalld_request(op: u16) -> LinuxSyscallOffloadRequest {
    let mut request = LinuxSyscallOffloadRequest {
        op,
        ..LinuxSyscallOffloadRequest::default()
    };
    if let Some(snapshot) = multitask::current_user_snapshot() {
        request.pid = snapshot.process_id();
        request.tid = snapshot.thread_id();
        request.session_handle = snapshot.console_session().raw();
        request.parent_pid = multitask::parent_process_id_of(snapshot.process_id()).unwrap_or(0);
    }
    if let Some(security) = multitask::with_current_process_credentials(|security| security) {
        request.uid = security.uid();
        request.gid = security.gid();
        request.euid = security.euid();
        request.egid = security.egid();
    }
    request
}

pub(super) fn call_syscalld(
    request: LinuxSyscallOffloadRequest,
) -> Result<LinuxSyscallOffloadResponse, i64> {
    let response = ipc_ops::call_linux_syscall_endpoint(as_bytes(&request))?;
    if response.len() != size_of::<LinuxSyscallOffloadResponse>() {
        return Err(LINUX_EINVAL);
    }
    let response = read_unaligned::<LinuxSyscallOffloadResponse>(response.as_slice());
    if response.version != SYSCALL_OFFLOAD_ABI_VERSION
        || response.op != request.op
        || response.reserved0 != 0
    {
        return Err(LINUX_EINVAL);
    }
    Ok(response)
}

pub(super) fn ensure_empty_syscalld_response(
    response: &LinuxSyscallOffloadResponse,
) -> Result<(), i64> {
    ensure_syscalld_status(response)?;
    if response.payload_len != 0 {
        return Err(LINUX_EINVAL);
    }
    Ok(())
}

pub(super) fn ensure_syscalld_payload(
    response: &LinuxSyscallOffloadResponse,
    expected_len: usize,
) -> Result<(), i64> {
    ensure_syscalld_status(response)?;
    if response.payload_len as usize != expected_len {
        return Err(LINUX_EINVAL);
    }
    Ok(())
}

pub(super) fn ensure_syscalld_status(response: &LinuxSyscallOffloadResponse) -> Result<(), i64> {
    if response.status != 0 {
        return Err(response.status.unsigned_abs() as i64);
    }
    Ok(())
}

pub(super) fn as_bytes<T>(value: &T) -> &[u8] {
    unsafe { slice::from_raw_parts((value as *const T).cast::<u8>(), size_of::<T>()) }
}

pub(super) fn read_unaligned<T: Copy>(bytes: &[u8]) -> T {
    assert!(bytes.len() >= size_of::<T>());
    unsafe { bytes.as_ptr().cast::<T>().read_unaligned() }
}
