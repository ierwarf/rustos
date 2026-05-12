use super::*;

pub(super) fn syscall_linux_sched_yield() -> u64 {
    multitask::yield_now();
    0
}

pub(super) fn syscall_linux_getpid() -> u64 {
    multitask::current_user_process_id().unwrap_or(0)
}

pub(super) fn syscall_linux_gettid() -> u64 {
    multitask::current_user_thread_id().unwrap_or(0)
}

pub(super) fn syscall_linux_rt_sigaction(
    signal: u64,
    action_ptr: u64,
    old_action_ptr: u64,
    _sigset_size: u64,
) -> u64 {
    let mut request = new_syscalld_request(SYSCALL_OFFLOAD_OP_LINUX_RT_SIGACTION);
    request.arg0 = signal;
    if action_ptr != 0 {
        let action = match usermem::read_current_user_struct::<LinuxSigActionWire>(action_ptr) {
            Ok(action) => action,
            Err(err) => return linux_errno(address_space_error_to_linux_errno(err)),
        };
        request.mask |= 0x1;
        request.path_len = LINUX_SIGACTION_SIZE as u32;
        request.path[..LINUX_SIGACTION_SIZE].copy_from_slice(as_bytes(&action));
    }
    if old_action_ptr != 0 {
        if let Err(err) =
            usermem::validate_current_user_write_buffer(old_action_ptr, LINUX_SIGACTION_SIZE)
        {
            return linux_errno(address_space_error_to_linux_errno(err));
        }
        request.mask |= 0x2;
    }
    let response = match call_syscalld(request) {
        Ok(response) => response,
        Err(errno) => return linux_errno(errno),
    };
    if old_action_ptr != 0 {
        if let Err(errno) = ensure_syscalld_payload(&response, LINUX_SIGACTION_SIZE) {
            return linux_errno(errno);
        }
        match usermem::write_current_user_bytes(
            old_action_ptr,
            &response.payload[..LINUX_SIGACTION_SIZE],
        ) {
            Ok(()) => 0,
            Err(err) => linux_errno(address_space_error_to_linux_errno(err)),
        }
    } else {
        match ensure_empty_syscalld_response(&response) {
            Ok(()) => 0,
            Err(errno) => linux_errno(errno),
        }
    }
}

pub(super) fn syscall_linux_rt_sigprocmask(
    how: u64,
    set_ptr: u64,
    oldset_ptr: u64,
    _sigset_size: u64,
) -> u64 {
    let mut request = new_syscalld_request(SYSCALL_OFFLOAD_OP_LINUX_RT_SIGPROCMASK);
    request.arg0 = how;
    if set_ptr != 0 {
        let mut set = [0_u8; 8];
        if let Err(err) = usermem::copy_from_current_user_exact(set_ptr, &mut set) {
            return linux_errno(address_space_error_to_linux_errno(err));
        }
        request.mask |= 0x1;
        request.path_len = 8;
        request.path[..8].copy_from_slice(&set);
    }
    if oldset_ptr != 0 {
        if let Err(err) = usermem::validate_current_user_write_buffer(oldset_ptr, 8) {
            return linux_errno(address_space_error_to_linux_errno(err));
        }
        request.mask |= 0x2;
    }
    let response = match call_syscalld(request) {
        Ok(response) => response,
        Err(errno) => return linux_errno(errno),
    };
    if oldset_ptr != 0 {
        if let Err(errno) = ensure_syscalld_payload(&response, 8) {
            return linux_errno(errno);
        }
        match usermem::write_current_user_bytes(oldset_ptr, &response.payload[..8]) {
            Ok(()) => 0,
            Err(err) => linux_errno(address_space_error_to_linux_errno(err)),
        }
    } else {
        match ensure_empty_syscalld_response(&response) {
            Ok(()) => 0,
            Err(errno) => linux_errno(errno),
        }
    }
}

pub(super) fn syscall_linux_nanosleep(request_ptr: u64, _remaining_ptr: u64) -> u64 {
    let ts = match usermem::read_current_user_struct::<LinuxTimespecWire>(request_ptr) {
        Ok(ts) => ts,
        Err(err) => return linux_errno(address_space_error_to_linux_errno(err)),
    };
    let mut request = new_syscalld_request(SYSCALL_OFFLOAD_OP_LINUX_NANOSLEEP);
    request.path_len = LINUX_TIMESPEC_SIZE as u32;
    request.path[..LINUX_TIMESPEC_SIZE].copy_from_slice(as_bytes(&ts));
    match call_syscalld(request).and_then(|response| ensure_empty_syscalld_response(&response)) {
        Ok(()) => 0,
        Err(errno) => linux_errno(errno),
    }
}

pub(super) fn syscall_linux_clock_gettime(clock_id: u64, timespec_ptr: u64) -> u64 {
    if let Err(err) = usermem::validate_current_user_write_buffer(timespec_ptr, LINUX_TIMESPEC_SIZE)
    {
        return linux_errno(address_space_error_to_linux_errno(err));
    }
    let mut request = new_syscalld_request(SYSCALL_OFFLOAD_OP_LINUX_CLOCK_GETTIME);
    request.arg0 = clock_id;
    let response = match call_syscalld(request) {
        Ok(response) => response,
        Err(errno) => return linux_errno(errno),
    };
    if let Err(errno) = ensure_syscalld_payload(&response, LINUX_TIMESPEC_SIZE) {
        return linux_errno(errno);
    }
    match usermem::write_current_user_bytes(timespec_ptr, &response.payload[..LINUX_TIMESPEC_SIZE])
    {
        Ok(()) => 0,
        Err(err) => linux_errno(address_space_error_to_linux_errno(err)),
    }
}

pub(super) fn syscall_linux_clock_nanosleep(
    clock_id: u64,
    flags: u64,
    request_ptr: u64,
    _remaining_ptr: u64,
) -> u64 {
    let ts = match usermem::read_current_user_struct::<LinuxTimespecWire>(request_ptr) {
        Ok(ts) => ts,
        Err(err) => return linux_errno(address_space_error_to_linux_errno(err)),
    };
    let mut request = new_syscalld_request(SYSCALL_OFFLOAD_OP_LINUX_CLOCK_NANOSLEEP);
    request.arg0 = clock_id;
    request.arg1 = flags;
    request.path_len = LINUX_TIMESPEC_SIZE as u32;
    request.path[..LINUX_TIMESPEC_SIZE].copy_from_slice(as_bytes(&ts));
    match call_syscalld(request).and_then(|response| ensure_empty_syscalld_response(&response)) {
        Ok(()) => 0,
        Err(errno) => linux_errno(errno),
    }
}

pub(super) fn syscall_linux_clone(_frame: &SyscallFrame) -> u64 {
    linux_errno(LINUX_ENOSYS)
}

pub(super) fn syscall_linux_clone3(_frame: &SyscallFrame) -> u64 {
    linux_errno(LINUX_ENOSYS)
}

pub(super) fn syscall_linux_futex(
    _frame: &SyscallFrame,
    _uaddr: u64,
    _op: u64,
    _val: u64,
    _timeout_ptr: u64,
    _uaddr2: u64,
    _val3: u64,
) -> u64 {
    linux_errno(LINUX_ENOSYS)
}

pub(super) fn syscall_linux_arch_prctl(_code: u64, _arg: u64) -> u64 {
    match _code {
        linux_abi::ARCH_SET_FS => {
            if _arg != 0
                && !(paging::USER_SPACE_BASE..paging::USER_SPACE_END_EXCLUSIVE).contains(&_arg)
            {
                return linux_errno(LINUX_EINVAL);
            }
            let Some(result) =
                multitask::with_current_user_linux_state_mut(|_, _, abi, _, _, thread_state| {
                    if abi != crate::user::abi::UserAbi::Linux {
                        return false;
                    }
                    let Some(state) = thread_state.as_mut() else {
                        return false;
                    };
                    state.fs_base = _arg;
                    x86_64::registers::model_specific::FsBase::write(x86_64::VirtAddr::new(_arg));
                    true
                })
            else {
                return linux_errno(LINUX_ENOSYS);
            };
            if result { 0 } else { linux_errno(LINUX_ENOSYS) }
        }
        linux_abi::ARCH_GET_FS => {
            let fs = x86_64::registers::model_specific::FsBase::read().as_u64();
            match usermem::write_current_user_bytes(_arg, &fs.to_le_bytes()) {
                Ok(()) => 0,
                Err(err) => linux_errno(address_space_error_to_linux_errno(err)),
            }
        }
        _ => linux_errno(LINUX_EINVAL),
    }
}

pub(super) fn syscall_linux_set_tid_address(user_ptr: u64) -> u64 {
    let Some(result) =
        multitask::with_current_user_linux_state_mut(|_, tid, abi, _, _, thread_state| {
            if abi != crate::user::abi::UserAbi::Linux {
                return None;
            }
            let state = thread_state.as_mut()?;
            state.clear_child_tid = user_ptr;
            Some(tid)
        })
    else {
        return linux_errno(LINUX_ENOSYS);
    };
    result.unwrap_or_else(|| linux_errno(LINUX_ENOSYS))
}

pub(super) fn syscall_linux_tgkill(_tgid: u64, _tid: u64, _signal: u64) -> u64 {
    linux_errno(LINUX_ENOSYS)
}

pub(super) fn syscall_linux_set_robust_list(head_ptr: u64, len: u64) -> u64 {
    let mut request = new_syscalld_request(SYSCALL_OFFLOAD_OP_LINUX_SET_ROBUST_LIST);
    request.arg0 = head_ptr;
    request.arg1 = len;
    match call_syscalld(request).and_then(|response| ensure_empty_syscalld_response(&response)) {
        Ok(()) => 0,
        Err(errno) => linux_errno(errno),
    }
}

pub(super) fn syscall_linux_get_robust_list(pid: u64, head_ptr_ptr: u64, len_ptr: u64) -> u64 {
    let mut request = new_syscalld_request(SYSCALL_OFFLOAD_OP_LINUX_GET_ROBUST_LIST);
    request.arg0 = pid;
    let response = match call_syscalld(request) {
        Ok(response) => response,
        Err(errno) => return linux_errno(errno),
    };
    if let Err(errno) = ensure_syscalld_payload(&response, 16) {
        return linux_errno(errno);
    }
    if let Err(err) = usermem::write_current_user_bytes(head_ptr_ptr, &response.payload[..8]) {
        return linux_errno(address_space_error_to_linux_errno(err));
    }
    if let Err(err) = usermem::write_current_user_bytes(len_ptr, &response.payload[8..16]) {
        return linux_errno(address_space_error_to_linux_errno(err));
    }
    0
}

pub(super) fn syscall_linux_rseq(area_ptr: u64, len: u64, flags: u64, signature: u64) -> u64 {
    let mut request = new_syscalld_request(SYSCALL_OFFLOAD_OP_LINUX_RSEQ);
    request.arg0 = area_ptr;
    request.arg1 = len;
    request.arg2 = flags;
    request.arg3 = signature;
    match call_syscalld(request).and_then(|response| ensure_empty_syscalld_response(&response)) {
        Ok(()) => 0,
        Err(errno) => linux_errno(errno),
    }
}

pub(super) fn syscall_linux_vfs_openat(dirfd: u64, path_ptr: u64, flags: u64, mode: u64) -> u64 {
    let path = match copy_current_user_path(path_ptr, VFS_IPC_PATH_CAPACITY) {
        Ok(path) => path,
        Err(errno) => return linux_errno(errno),
    };
    let mut request = new_vfs_request(VFS_IPC_OP_OPENAT);
    request.dirfd = dirfd;
    request.arg0 = flags;
    request.arg1 = mode;
    if let Err(errno) = populate_vfs_path(&mut request, &path) {
        return linux_errno(errno);
    }
    let response = match call_vfs_ipc_request(&request) {
        Ok(response) => response,
        Err(_) if !ipc_ops::service_registered(linux_abi::IPC_SERVICE_VFSD) => {
            return bootstrap_openat(path.as_str(), flags);
        }
        Err(errno) => return linux_errno(errno),
    };
    if let Err(errno) = ensure_vfs_status(&response) {
        return linux_errno(errno);
    }
    let kind = match response.handle_kind {
        VFS_IPC_HANDLE_KIND_FILE => multitask::RemoteVfsHandleKind::File,
        VFS_IPC_HANDLE_KIND_DIR => multitask::RemoteVfsHandleKind::Directory,
        VFS_IPC_HANDLE_KIND_DEVICE => multitask::RemoteVfsHandleKind::Device,
        _ => return linux_errno(LINUX_EINVAL),
    };
    let handle = multitask::KernelHandle::RemoteVfs(multitask::RemoteVfsHandle::new(
        response.remote_id,
        kind,
        String::from(path.as_str()),
        response.value,
    ));
    match multitask::with_current_user_process_state_mut(|_, _, process_state| {
        process_state
            .handles_mut()
            .install_with_open_flags(handle, flags)
    }) {
        Some(fd) => fd,
        None => linux_errno(LINUX_EINVAL),
    }
}

pub(super) fn syscall_linux_vfs_close(fd: u64) -> u64 {
    if let Some(remote) = current_remote_vfs_handle(fd) {
        let mut request = new_vfs_request(VFS_IPC_OP_CLOSE);
        request.fd = fd;
        request.remote_id = remote.remote_id();
        if let Err(errno) =
            call_vfs_ipc_request(&request).and_then(|response| ensure_vfs_status(&response))
        {
            return linux_errno(errno);
        }
    }
    match multitask::with_current_user_process_state_mut(|_, _, process_state| {
        process_state.handles_mut().close(fd).is_some()
    }) {
        Some(true) => 0,
        _ => linux_errno(LINUX_EBADF),
    }
}

pub(super) fn syscall_linux_vfs_dup(oldfd: u64, newfd: u64, flags: u64, mode: VfsDupMode) -> u64 {
    if let Some(remote) = current_remote_vfs_handle(oldfd) {
        let mut request = new_vfs_request(VFS_IPC_OP_DUP);
        request.fd = oldfd;
        request.remote_id = remote.remote_id();
        if let Err(errno) =
            call_vfs_ipc_request(&request).and_then(|response| ensure_vfs_status(&response))
        {
            return linux_errno(errno);
        }
    }
    let close_on_exec = flags & linux_abi::O_CLOEXEC != 0;
    let result = multitask::with_current_user_process_state_mut(|_, _, process_state| match mode {
        VfsDupMode::Dup => process_state.handles_mut().duplicate_min(oldfd, 0, false),
        VfsDupMode::Dup2 if oldfd == newfd => {
            process_state.handles().get_entry(oldfd).map(|_| oldfd)
        }
        VfsDupMode::Dup2 => process_state
            .handles_mut()
            .duplicate_exact(oldfd, newfd, false),
        VfsDupMode::Dup3 => {
            if oldfd == newfd || flags & !linux_abi::O_CLOEXEC != 0 {
                None
            } else {
                process_state
                    .handles_mut()
                    .duplicate_exact(oldfd, newfd, close_on_exec)
            }
        }
    });
    match result.flatten() {
        Some(fd) => fd,
        None => linux_errno(LINUX_EBADF),
    }
}

pub(super) fn syscall_linux_vfs_read(fd: u64, user_ptr: u64, user_len: u64) -> u64 {
    if let Some(mut file) = current_vfs_file_handle(fd) {
        let Ok(user_len) = usize::try_from(user_len) else {
            return linux_errno(LINUX_EINVAL);
        };
        if user_len == 0 {
            return 0;
        }
        if let Err(err) = usermem::validate_current_user_write_buffer(user_ptr, user_len) {
            return linux_errno(address_space_error_to_linux_errno(err));
        }
        let mut copied = 0usize;
        let mut chunk = [0_u8; 256];
        while copied < user_len {
            let chunk_len = (user_len - copied).min(chunk.len());
            let read = file.read_into(&mut chunk[..chunk_len]);
            if read == 0 {
                break;
            }
            let Some(dest) = user_ptr.checked_add(copied as u64) else {
                return linux_errno(LINUX_EINVAL);
            };
            if let Err(err) = usermem::write_current_user_bytes(dest, &chunk[..read]) {
                return linux_errno(address_space_error_to_linux_errno(err));
            }
            copied += read;
            if read < chunk_len {
                break;
            }
        }
        return copied as u64;
    }
    let Some(remote) = current_remote_vfs_handle(fd) else {
        return linux_errno(LINUX_EBADF);
    };
    let Ok(user_len) = usize::try_from(user_len) else {
        return linux_errno(LINUX_EINVAL);
    };
    if user_len == 0 {
        return 0;
    }
    if let Err(err) = usermem::validate_current_user_write_buffer(user_ptr, user_len) {
        return linux_errno(address_space_error_to_linux_errno(err));
    }
    let mut copied = 0usize;
    while copied < user_len {
        let chunk_len = (user_len - copied).min(VFS_IPC_PAYLOAD_CAPACITY);
        let mut request = new_vfs_request(VFS_IPC_OP_READ);
        request.fd = fd;
        request.remote_id = remote.remote_id();
        request.arg1 = chunk_len as u64;
        let response = match call_vfs_ipc_request(&request) {
            Ok(response) => response,
            Err(errno) => return linux_errno(errno),
        };
        if let Err(errno) = ensure_vfs_status(&response) {
            return linux_errno(errno);
        }
        let read = response.payload_len as usize;
        if read > chunk_len {
            return linux_errno(LINUX_EINVAL);
        }
        if read == 0 {
            break;
        }
        let Some(dest) = user_ptr.checked_add(copied as u64) else {
            return linux_errno(LINUX_EINVAL);
        };
        if let Err(err) = usermem::write_current_user_bytes(dest, &response.payload[..read]) {
            return linux_errno(address_space_error_to_linux_errno(err));
        }
        copied += read;
        if read < chunk_len {
            break;
        }
    }
    copied as u64
}

pub(super) fn syscall_linux_vfs_pread64(fd: u64, user_ptr: u64, user_len: u64, offset: u64) -> u64 {
    let Some(remote) = current_remote_vfs_handle(fd) else {
        return linux_errno(LINUX_EBADF);
    };
    let Ok(user_len) = usize::try_from(user_len) else {
        return linux_errno(LINUX_EINVAL);
    };
    if user_len == 0 {
        return 0;
    }
    if let Err(err) = usermem::validate_current_user_write_buffer(user_ptr, user_len) {
        return linux_errno(address_space_error_to_linux_errno(err));
    }
    let mut copied = 0usize;
    while copied < user_len {
        let chunk_len = (user_len - copied).min(VFS_IPC_PAYLOAD_CAPACITY);
        let mut request = new_vfs_request(VFS_IPC_OP_PREAD64);
        request.fd = fd;
        request.remote_id = remote.remote_id();
        request.arg0 = offset.saturating_add(copied as u64);
        request.arg1 = chunk_len as u64;
        let response = match call_vfs_ipc_request(&request) {
            Ok(response) => response,
            Err(errno) => return linux_errno(errno),
        };
        if let Err(errno) = ensure_vfs_status(&response) {
            return linux_errno(errno);
        }
        let read = response.payload_len as usize;
        if read > chunk_len {
            return linux_errno(LINUX_EINVAL);
        }
        if read == 0 {
            break;
        }
        let Some(dest) = user_ptr.checked_add(copied as u64) else {
            return linux_errno(LINUX_EINVAL);
        };
        if let Err(err) = usermem::write_current_user_bytes(dest, &response.payload[..read]) {
            return linux_errno(address_space_error_to_linux_errno(err));
        }
        copied += read;
        if read < chunk_len {
            break;
        }
    }
    copied as u64
}

pub(super) fn syscall_linux_vfs_write(fd: u64, user_ptr: u64, user_len: u64) -> u64 {
    let Some(remote) = current_remote_vfs_handle(fd) else {
        return linux_errno(LINUX_EBADF);
    };
    let Ok(user_len) = usize::try_from(user_len) else {
        return linux_errno(LINUX_EINVAL);
    };
    if user_len == 0 {
        return 0;
    }
    let chunk_len = user_len.min(VFS_IPC_REQUEST_PAYLOAD_CAPACITY);
    let mut request = new_vfs_request(VFS_IPC_OP_WRITE);
    request.fd = fd;
    request.remote_id = remote.remote_id();
    request.payload_len = chunk_len as u32;
    if let Err(err) =
        usermem::copy_from_current_user_exact(user_ptr, &mut request.payload[..chunk_len])
    {
        return linux_errno(address_space_error_to_linux_errno(err));
    }
    match call_vfs_ipc_request(&request).and_then(|response| {
        ensure_vfs_status(&response)?;
        Ok(response.value)
    }) {
        Ok(written) => written,
        Err(errno) => linux_errno(errno),
    }
}

pub(super) fn syscall_linux_vfs_lseek(fd: u64, offset: i64, whence: u64) -> u64 {
    if let Some(mut file) = current_vfs_file_handle(fd) {
        let whence = match whence {
            value if value == linux_abi::SEEK_SET => multitask::FileHandleSeekWhence::Start,
            value if value == linux_abi::SEEK_CUR => multitask::FileHandleSeekWhence::Current,
            value if value == linux_abi::SEEK_END => multitask::FileHandleSeekWhence::End,
            _ => return linux_errno(LINUX_EINVAL),
        };
        return match file.seek(offset, whence) {
            Ok(pos) => pos,
            Err(_) => linux_errno(LINUX_EINVAL),
        };
    }
    let Some(remote) = current_remote_vfs_handle(fd) else {
        return linux_errno(LINUX_EBADF);
    };
    let mut request = new_vfs_request(VFS_IPC_OP_LSEEK);
    request.fd = fd;
    request.remote_id = remote.remote_id();
    request.arg0 = offset as u64;
    request.arg1 = whence;
    match call_vfs_ipc_request(&request).and_then(|response| {
        ensure_vfs_status(&response)?;
        Ok(response.value)
    }) {
        Ok(value) => value,
        Err(errno) => linux_errno(errno),
    }
}

pub(super) fn syscall_linux_vfs_fstat(fd: u64, stat_ptr: u64) -> u64 {
    if let Some(file) = current_vfs_file_handle(fd) {
        return write_bootstrap_stat(
            stat_ptr,
            crate::vfs_core::path_inode(file.path().as_bytes()),
            file.len() as u64,
        );
    }
    let Some(remote) = current_remote_vfs_handle(fd) else {
        return linux_errno(LINUX_EBADF);
    };
    if let Err(err) = usermem::validate_current_user_write_buffer(stat_ptr, LINUX_STAT_SIZE) {
        return linux_errno(address_space_error_to_linux_errno(err));
    }
    let mut request = new_vfs_request(VFS_IPC_OP_FSTAT);
    request.fd = fd;
    request.remote_id = remote.remote_id();
    let response = match call_vfs_ipc_request(&request) {
        Ok(response) => response,
        Err(errno) => return linux_errno(errno),
    };
    if let Err(errno) = ensure_vfs_status(&response) {
        return linux_errno(errno);
    }
    if response.payload_len as usize != LINUX_STAT_SIZE {
        return linux_errno(LINUX_EINVAL);
    }
    match usermem::write_current_user_bytes(stat_ptr, &response.payload[..LINUX_STAT_SIZE]) {
        Ok(()) => 0,
        Err(err) => linux_errno(address_space_error_to_linux_errno(err)),
    }
}

pub(super) fn syscall_linux_vfs_ftruncate(fd: u64, len: u64) -> u64 {
    let Some(remote) = current_remote_vfs_handle(fd) else {
        return linux_errno(LINUX_EBADF);
    };
    let mut request = new_vfs_request(VFS_IPC_OP_FTRUNCATE);
    request.fd = fd;
    request.remote_id = remote.remote_id();
    request.arg0 = len;
    match call_vfs_ipc_request(&request).and_then(|response| ensure_vfs_status(&response)) {
        Ok(()) => 0,
        Err(errno) => linux_errno(errno),
    }
}

pub(super) fn syscall_linux_vfs_getdents64(fd: u64, user_ptr: u64, user_len: u64) -> u64 {
    let Some(remote) = current_remote_vfs_handle(fd) else {
        return linux_errno(LINUX_EBADF);
    };
    let Ok(user_len) = usize::try_from(user_len) else {
        return linux_errno(LINUX_EINVAL);
    };
    if let Err(err) = usermem::validate_current_user_write_buffer(user_ptr, user_len) {
        return linux_errno(address_space_error_to_linux_errno(err));
    }
    let mut request = new_vfs_request(VFS_IPC_OP_GETDENTS64);
    request.fd = fd;
    request.remote_id = remote.remote_id();
    request.arg1 = user_len as u64;
    let response = match call_vfs_ipc_request(&request) {
        Ok(response) => response,
        Err(errno) => return linux_errno(errno),
    };
    if let Err(errno) = ensure_vfs_status(&response) {
        return linux_errno(errno);
    }
    let len = response.payload_len as usize;
    if len > user_len || len > response.payload.len() {
        return linux_errno(LINUX_EINVAL);
    }
    match usermem::write_current_user_bytes(user_ptr, &response.payload[..len]) {
        Ok(()) => len as u64,
        Err(err) => linux_errno(address_space_error_to_linux_errno(err)),
    }
}

pub(super) fn syscall_linux_vfs_fcntl(fd: u64, cmd: u64, arg: u64) -> u64 {
    let Some(remote) = current_remote_vfs_handle(fd) else {
        return linux_errno(LINUX_EBADF);
    };
    let mut request = new_vfs_request(VFS_IPC_OP_FCNTL);
    request.fd = fd;
    request.remote_id = remote.remote_id();
    request.arg0 = cmd;
    request.arg1 = arg;
    match call_vfs_ipc_request(&request).and_then(|response| {
        ensure_vfs_status(&response)?;
        Ok(response.value)
    }) {
        Ok(value) => value,
        Err(errno) => linux_errno(errno),
    }
}

pub(super) fn syscall_linux_vfs_statx(
    dirfd: u64,
    path_ptr: u64,
    flags: u64,
    mask: u64,
    statx_ptr: u64,
) -> u64 {
    let path = match copy_current_user_path(path_ptr, VFS_IPC_PATH_CAPACITY) {
        Ok(path) => path,
        Err(errno) => return linux_errno(errno),
    };
    if let Err(err) = usermem::validate_current_user_write_buffer(statx_ptr, LINUX_STATX_SIZE) {
        return linux_errno(address_space_error_to_linux_errno(err));
    }
    let mut request = new_vfs_request(VFS_IPC_OP_STATX);
    request.dirfd = dirfd;
    request.flags = flags as u32;
    request.arg1 = mask;
    if let Err(errno) = populate_vfs_path(&mut request, &path) {
        return linux_errno(errno);
    }
    let response = match call_vfs_ipc_request(&request) {
        Ok(response) => response,
        Err(_) if !ipc_ops::service_registered(linux_abi::IPC_SERVICE_VFSD) => {
            return bootstrap_stat_path(path.as_str(), statx_ptr, true);
        }
        Err(errno) => return linux_errno(errno),
    };
    if let Err(errno) = ensure_vfs_status(&response) {
        return linux_errno(errno);
    }
    if response.payload_len as usize != LINUX_STATX_SIZE {
        return linux_errno(LINUX_EINVAL);
    }
    match usermem::write_current_user_bytes(statx_ptr, &response.payload[..LINUX_STATX_SIZE]) {
        Ok(()) => 0,
        Err(err) => linux_errno(address_space_error_to_linux_errno(err)),
    }
}

pub(super) fn syscall_linux_vfs_newfstatat(
    dirfd: u64,
    path_ptr: u64,
    stat_ptr: u64,
    flags: u64,
) -> u64 {
    let path = match copy_current_user_path(path_ptr, VFS_IPC_PATH_CAPACITY) {
        Ok(path) => path,
        Err(errno) => return linux_errno(errno),
    };
    if let Err(err) = usermem::validate_current_user_write_buffer(stat_ptr, LINUX_STAT_SIZE) {
        return linux_errno(address_space_error_to_linux_errno(err));
    }
    let mut request = new_vfs_request(VFS_IPC_OP_NEWFSTATAT);
    request.dirfd = dirfd;
    request.flags = flags as u32;
    if let Err(errno) = populate_vfs_path(&mut request, &path) {
        return linux_errno(errno);
    }
    let response = match call_vfs_ipc_request(&request) {
        Ok(response) => response,
        Err(_) if !ipc_ops::service_registered(linux_abi::IPC_SERVICE_VFSD) => {
            return bootstrap_stat_path(path.as_str(), stat_ptr, false);
        }
        Err(errno) => return linux_errno(errno),
    };
    if let Err(errno) = ensure_vfs_status(&response) {
        return linux_errno(errno);
    }
    if response.payload_len as usize != LINUX_STAT_SIZE {
        return linux_errno(LINUX_EINVAL);
    }
    match usermem::write_current_user_bytes(stat_ptr, &response.payload[..LINUX_STAT_SIZE]) {
        Ok(()) => 0,
        Err(err) => linux_errno(address_space_error_to_linux_errno(err)),
    }
}

pub(super) fn syscall_linux_vfs_readlinkat(
    dirfd: u64,
    path_ptr: u64,
    user_ptr: u64,
    user_len: u64,
) -> u64 {
    let path = match copy_current_user_path(path_ptr, VFS_IPC_PATH_CAPACITY) {
        Ok(path) => path,
        Err(errno) => return linux_errno(errno),
    };
    let Ok(user_len) = usize::try_from(user_len) else {
        return linux_errno(LINUX_EINVAL);
    };
    if user_len == 0 {
        return 0;
    }
    if let Err(err) = usermem::validate_current_user_write_buffer(user_ptr, user_len) {
        return linux_errno(address_space_error_to_linux_errno(err));
    }
    let mut request = new_vfs_request(VFS_IPC_OP_READLINKAT);
    request.dirfd = dirfd;
    request.arg0 = user_len as u64;
    if let Err(errno) = populate_vfs_path(&mut request, &path) {
        return linux_errno(errno);
    }
    let response = match call_vfs_ipc_request(&request) {
        Ok(response) => response,
        Err(errno) => return linux_errno(errno),
    };
    if let Err(errno) = ensure_vfs_status(&response) {
        return linux_errno(errno);
    }
    let len = (response.payload_len as usize).min(user_len);
    match usermem::write_current_user_bytes(user_ptr, &response.payload[..len]) {
        Ok(()) => len as u64,
        Err(err) => linux_errno(address_space_error_to_linux_errno(err)),
    }
}

pub(super) fn syscall_linux_vfs_access(dirfd: u64, path_ptr: u64, mode: u64, flags: u64) -> u64 {
    let path = match copy_current_user_path(path_ptr, VFS_IPC_PATH_CAPACITY) {
        Ok(path) => path,
        Err(errno) => return linux_errno(errno),
    };
    let mut request = new_vfs_request(VFS_IPC_OP_ACCESS);
    request.dirfd = dirfd;
    request.flags = flags as u32;
    request.arg0 = mode;
    if let Err(errno) = populate_vfs_path(&mut request, &path) {
        return linux_errno(errno);
    }
    match call_vfs_ipc_request(&request).and_then(|response| ensure_vfs_status(&response)) {
        Ok(()) => 0,
        Err(_) if !ipc_ops::service_registered(linux_abi::IPC_SERVICE_VFSD) => {
            match bootstrap_read_file(path.as_str()) {
                Ok(_) => 0,
                Err(errno) => linux_errno(errno),
            }
        }
        Err(errno) => linux_errno(errno),
    }
}

fn current_vfs_file_handle(fd: u64) -> Option<multitask::VfsFileHandle> {
    multitask::with_current_user_process_state(|_, _, process_state| {
        match process_state.handles().get(fd) {
            Some(multitask::KernelHandle::VfsFile(file)) => Some(file.clone()),
            _ => None,
        }
    })
    .flatten()
}

fn bootstrap_openat(path: &str, flags: u64) -> u64 {
    if flags & linux_abi::O_DIRECTORY != 0 || flags & linux_abi::O_ACCMODE != linux_abi::O_RDONLY {
        return linux_errno(LINUX_ENOSYS);
    }
    let bytes = match bootstrap_read_file(path) {
        Ok(bytes) => bytes,
        Err(errno) => return linux_errno(errno),
    };
    let boot_path = bootstrap_path(path).unwrap_or(path);
    let handle = multitask::KernelHandle::VfsFile(multitask::VfsFileHandle::read_only_memory(
        String::from(boot_path),
        bytes,
    ));
    multitask::with_current_user_process_state_mut(|_, _, process_state| {
        process_state
            .handles_mut()
            .install_with_open_flags(handle, flags)
    })
    .unwrap_or_else(|| linux_errno(LINUX_EINVAL))
}

fn bootstrap_stat_path(path: &str, user_ptr: u64, statx: bool) -> u64 {
    let bytes = match bootstrap_read_file(path) {
        Ok(bytes) => bytes,
        Err(errno) => return linux_errno(errno),
    };
    let inode = crate::vfs_core::path_inode(path.as_bytes());
    if statx {
        write_bootstrap_statx(user_ptr, inode, bytes.len() as u64)
    } else {
        write_bootstrap_stat(user_ptr, inode, bytes.len() as u64)
    }
}

fn bootstrap_read_file(path: &str) -> Result<alloc::vec::Vec<u8>, i64> {
    let path = bootstrap_path(path).ok_or(LINUX_ENOSYS)?;
    crate::storage::boot_volume::read_file_to_vec(path).map_err(|_| LINUX_ENOENT)
}

fn bootstrap_path(path: &str) -> Option<&str> {
    let path = path.strip_prefix('/').unwrap_or(path);
    if path.is_empty() || path.contains("..") {
        return None;
    }
    (path.starts_with("services/")
        || path.starts_with("applications/")
        || path.starts_with("lib/")
        || path.starts_with("lib64/"))
    .then_some(path)
}

fn write_bootstrap_stat(user_ptr: u64, inode: u64, len: u64) -> u64 {
    if let Err(err) = usermem::validate_current_user_write_buffer(user_ptr, LINUX_STAT_SIZE) {
        return linux_errno(address_space_error_to_linux_errno(err));
    }
    let mut out = [0_u8; LINUX_STAT_SIZE];
    out[8..16].copy_from_slice(&inode.max(1).to_ne_bytes());
    out[16..24].copy_from_slice(&1_u64.to_ne_bytes());
    out[24..28].copy_from_slice(&(0o100000_u32 | 0o555).to_ne_bytes());
    out[48..56].copy_from_slice(&(len as i64).to_ne_bytes());
    out[56..64].copy_from_slice(&4096_i64.to_ne_bytes());
    out[64..72].copy_from_slice(&(len.div_ceil(512) as i64).to_ne_bytes());
    match usermem::write_current_user_bytes(user_ptr, &out) {
        Ok(()) => 0,
        Err(err) => linux_errno(address_space_error_to_linux_errno(err)),
    }
}

fn write_bootstrap_statx(user_ptr: u64, inode: u64, len: u64) -> u64 {
    if let Err(err) = usermem::validate_current_user_write_buffer(user_ptr, LINUX_STATX_SIZE) {
        return linux_errno(address_space_error_to_linux_errno(err));
    }
    let mut out = [0_u8; LINUX_STATX_SIZE];
    out[0..4].copy_from_slice(&0x17ff_u32.to_ne_bytes());
    out[4..8].copy_from_slice(&4096_u32.to_ne_bytes());
    out[16..20].copy_from_slice(&1_u32.to_ne_bytes());
    out[28..30].copy_from_slice(&(0o100000_u16 | 0o555).to_ne_bytes());
    out[40..48].copy_from_slice(&inode.max(1).to_ne_bytes());
    out[48..56].copy_from_slice(&len.to_ne_bytes());
    out[56..64].copy_from_slice(&len.div_ceil(512).to_ne_bytes());
    match usermem::write_current_user_bytes(user_ptr, &out) {
        Ok(()) => 0,
        Err(err) => linux_errno(address_space_error_to_linux_errno(err)),
    }
}

pub(super) fn syscall_linux_vfs_getcwd(user_ptr: u64, user_len: u64) -> u64 {
    let Ok(user_len) = usize::try_from(user_len) else {
        return linux_errno(LINUX_EINVAL);
    };
    if user_len == 0 {
        return linux_errno(LINUX_EINVAL);
    }
    if let Err(err) = usermem::validate_current_user_write_buffer(user_ptr, user_len) {
        return linux_errno(address_space_error_to_linux_errno(err));
    }
    let request = new_vfs_request(VFS_IPC_OP_GETCWD);
    let response = match call_vfs_ipc_request(&request) {
        Ok(response) => response,
        Err(errno) => return linux_errno(errno),
    };
    if let Err(errno) = ensure_vfs_status(&response) {
        return linux_errno(errno);
    }
    let len = response.payload_len as usize;
    if len.checked_add(1).map_or(true, |needed| needed > user_len) {
        return linux_errno(LINUX_ERANGE);
    }
    if let Err(err) = usermem::write_current_user_bytes(user_ptr, &response.payload[..len]) {
        return linux_errno(address_space_error_to_linux_errno(err));
    }
    if let Err(err) = usermem::write_current_user_bytes(user_ptr + len as u64, &[0]) {
        return linux_errno(address_space_error_to_linux_errno(err));
    }
    (len + 1) as u64
}

pub(super) fn syscall_linux_vfs_chdir(dirfd: u64, path_ptr: u64) -> u64 {
    let path = match copy_current_user_path(path_ptr, VFS_IPC_PATH_CAPACITY) {
        Ok(path) => path,
        Err(errno) => return linux_errno(errno),
    };
    let mut request = new_vfs_request(VFS_IPC_OP_CHDIR);
    request.dirfd = dirfd;
    if let Err(errno) = populate_vfs_path(&mut request, &path) {
        return linux_errno(errno);
    }
    match call_vfs_ipc_request(&request).and_then(|response| ensure_vfs_status(&response)) {
        Ok(()) => 0,
        Err(errno) => linux_errno(errno),
    }
}

pub(super) fn syscall_linux_vfs_mkdir(dirfd: u64, path_ptr: u64, mode: u64) -> u64 {
    let path = match copy_current_user_path(path_ptr, VFS_IPC_PATH_CAPACITY) {
        Ok(path) => path,
        Err(errno) => return linux_errno(errno),
    };
    let mut request = new_vfs_request(VFS_IPC_OP_MKDIR);
    request.dirfd = dirfd;
    request.arg0 = mode;
    if let Err(errno) = populate_vfs_path(&mut request, &path) {
        return linux_errno(errno);
    }
    match call_vfs_ipc_request(&request).and_then(|response| ensure_vfs_status(&response)) {
        Ok(()) => 0,
        Err(errno) => linux_errno(errno),
    }
}

pub(super) fn syscall_linux_vfs_unlinkat(dirfd: u64, path_ptr: u64, flags: u64) -> u64 {
    let path = match copy_current_user_path(path_ptr, VFS_IPC_PATH_CAPACITY) {
        Ok(path) => path,
        Err(errno) => return linux_errno(errno),
    };
    let mut request = new_vfs_request(VFS_IPC_OP_UNLINKAT);
    request.dirfd = dirfd;
    request.flags = flags as u32;
    if let Err(errno) = populate_vfs_path(&mut request, &path) {
        return linux_errno(errno);
    }
    match call_vfs_ipc_request(&request).and_then(|response| ensure_vfs_status(&response)) {
        Ok(()) => 0,
        Err(errno) => linux_errno(errno),
    }
}

pub(super) fn syscall_linux_vfs_mount(
    _source_ptr: u64,
    target_ptr: u64,
    _fstype_ptr: u64,
    flags: u64,
) -> u64 {
    let path = match copy_current_user_path(target_ptr, VFS_IPC_PATH_CAPACITY) {
        Ok(path) => path,
        Err(errno) => return linux_errno(errno),
    };
    let mut request = new_vfs_request(VFS_IPC_OP_MOUNT);
    request.flags = flags as u32;
    if let Err(errno) = populate_vfs_path(&mut request, &path) {
        return linux_errno(errno);
    }
    match call_vfs_ipc_request(&request).and_then(|response| ensure_vfs_status(&response)) {
        Ok(()) => 0,
        Err(errno) => linux_errno(errno),
    }
}

pub(super) fn syscall_linux_vfs_umount2(target_ptr: u64, flags: u64) -> u64 {
    let path = match copy_current_user_path(target_ptr, VFS_IPC_PATH_CAPACITY) {
        Ok(path) => path,
        Err(errno) => return linux_errno(errno),
    };
    let mut request = new_vfs_request(VFS_IPC_OP_UMOUNT2);
    request.flags = flags as u32;
    if let Err(errno) = populate_vfs_path(&mut request, &path) {
        return linux_errno(errno);
    }
    match call_vfs_ipc_request(&request).and_then(|response| ensure_vfs_status(&response)) {
        Ok(()) => 0,
        Err(errno) => linux_errno(errno),
    }
}

pub(super) fn syscall_linux_ioctl(_fd: u64, _request: u64, _arg: u64) -> u64 {
    linux_errno(LINUX_ENOSYS)
}

pub(super) fn syscall_linux_net4(op: u16, arg0: u64, arg1: u64, arg2: u64, arg3: u64) -> u64 {
    syscall_linux_net6(op, arg0, arg1, arg2, arg3, 0, 0)
}

pub(super) fn syscall_linux_net6(
    op: u16,
    arg0: u64,
    arg1: u64,
    arg2: u64,
    arg3: u64,
    arg4: u64,
    arg5: u64,
) -> u64 {
    let mut request = new_syscalld_request(op);
    request.dirfd = arg0;
    request.flags = arg1;
    request.arg0 = arg2;
    request.arg1 = arg3;
    request.arg2 = arg4;
    request.arg3 = arg5;
    match call_service_offload_request(IPC_SERVICE_NETD, &request).and_then(|response| {
        ensure_service_response(&response, op)?;
        ensure_syscalld_payload(&response, size_of::<u64>())?;
        let mut bytes = [0_u8; size_of::<u64>()];
        bytes.copy_from_slice(&response.payload[..size_of::<u64>()]);
        Ok(u64::from_le_bytes(bytes))
    }) {
        Ok(value) => value,
        Err(errno) => linux_errno(errno),
    }
}

pub(super) fn syscall_linux_loader_spawn_exec(
    path_ptr: u64,
    argv_ptr: u64,
    envp_ptr: u64,
    flags: u64,
    console_session: u64,
    weight_micros: u64,
) -> u64 {
    let exec_path = match copy_current_user_path(path_ptr, LOADER_SPAWN_EXEC_PATH_CAPACITY) {
        Ok(path) => path,
        Err(errno) => return linux_errno(errno),
    };
    let mut request = LoaderSpawnRequest {
        flags: flags as u32,
        console_session,
        weight_micros,
        ..LoaderSpawnRequest::default()
    };
    if exec_path.len() > request.exec_path.len() {
        return linux_errno(LINUX_EINVAL);
    }
    request.exec_path_len = exec_path.len() as u32;
    request.exec_path[..exec_path.len()].copy_from_slice(exec_path.as_bytes());
    if let Err(errno) = copy_string_vector(
        argv_ptr,
        LOADER_SPAWN_MAX_ARG_COUNT,
        &mut request.argv_bytes,
        &mut request.argv_bytes_len,
        &mut request.argv_count,
    ) {
        return linux_errno(errno);
    }
    if let Err(errno) = copy_string_vector(
        envp_ptr,
        LOADER_SPAWN_MAX_ENV_COUNT,
        &mut request.env_bytes,
        &mut request.env_bytes_len,
        &mut request.env_count,
    ) {
        return linux_errno(errno);
    }
    let response = match call_loaderd(&request) {
        Ok(response) => response,
        Err(errno) if can_bootstrap_spawn_direct(exec_path.as_str()) => {
            return match spawn_bootstrap_exec_direct(
                exec_path.as_str(),
                flags,
                console_session,
                weight_micros,
            ) {
                Ok(pid) => pid,
                Err(fallback_errno) => linux_errno(if fallback_errno == LINUX_ENOENT {
                    errno
                } else {
                    fallback_errno
                }),
            };
        }
        Err(errno) => return linux_errno(errno),
    };
    if response.version != LOADER_REQUEST_ABI_VERSION
        || response.op != LOADER_OP_SPAWN_EXEC
        || response.reserved0 != 0
    {
        return linux_errno(LINUX_EINVAL);
    }
    if response.status != 0 {
        return linux_errno(response.status.unsigned_abs() as i64);
    }
    response.pid as u64
}

fn can_bootstrap_spawn_direct(exec_path: &str) -> bool {
    let path = exec_path.strip_prefix('/').unwrap_or(exec_path);
    matches!(
        path,
        "services/syscalld/syscalld.elf"
            | "services/vfsd/vfsd.elf"
            | "services/loaderd/loaderd.elf"
    )
}

fn spawn_bootstrap_exec_direct(
    exec_path: &str,
    flags: u64,
    console_session: u64,
    weight_micros: u64,
) -> Result<u64, i64> {
    let current_is_admin =
        multitask::with_current_process_credentials(|security| security.is_logical_admin())
            .unwrap_or(false);
    if !current_is_admin {
        return Err(LINUX_EPERM);
    }

    let loaded = crate::user::console_host::load_executable_image_by_path(exec_path, None)
        .map_err(console_host_error_to_linux_errno)?;
    let session = if console_session == 0 {
        multitask::current_user_snapshot()
            .map(|snapshot| snapshot.console_session())
            .unwrap_or(crate::io::session::ConsoleSessionHandle::SYSTEM)
    } else {
        crate::io::session::ConsoleSessionHandle::from_raw(console_session)
    };
    let logical_admin = flags & 0x1 != 0;
    let program = crate::user::console_host::ConsoleProgramSpec::new(
        &loaded.bytes,
        loaded.path,
        weight_micros,
    )
    .with_logical_admin(logical_admin);
    crate::user::console_host::spawn_program_in_session(session, program)
        .map(|spawned| spawned.pid)
        .map_err(console_host_error_to_linux_errno)
}

fn console_host_error_to_linux_errno(error: crate::user::console_host::ConsoleHostError) -> i64 {
    match error {
        crate::user::console_host::ConsoleHostError::BootstrapBlocked => LINUX_EAGAIN,
        crate::user::console_host::ConsoleHostError::Load { error, .. } => match error {
            crate::vfs::VfsError::BadFileDescriptor => LINUX_EBADF,
            crate::vfs::VfsError::InvalidArgument => LINUX_EINVAL,
            crate::vfs::VfsError::NotFound => LINUX_ENOENT,
            crate::vfs::VfsError::NotDirectory => LINUX_ENOTDIR,
            crate::vfs::VfsError::PermissionDenied => LINUX_EACCES,
            crate::vfs::VfsError::ReadOnlyFilesystem => LINUX_EROFS,
            crate::vfs::VfsError::Unsupported => LINUX_ENOSYS,
        },
        crate::user::console_host::ConsoleHostError::Spawn { .. } => LINUX_ENOEXEC,
    }
}

pub(super) fn copy_string_vector(
    vector_ptr: u64,
    max_count: usize,
    dest: &mut [u8],
    dest_len: &mut u32,
    dest_count: &mut u16,
) -> Result<(), i64> {
    *dest_len = 0;
    *dest_count = 0;
    if vector_ptr == 0 {
        return Ok(());
    }
    let mut cursor = vector_ptr;
    let mut offset = 0usize;
    for count in 0..max_count {
        let mut ptr_bytes = [0_u8; size_of::<u64>()];
        usermem::copy_from_current_user_exact(cursor, &mut ptr_bytes)
            .map_err(address_space_error_to_linux_errno)?;
        let value_ptr = u64::from_ne_bytes(ptr_bytes);
        if value_ptr == 0 {
            *dest_len = offset as u32;
            *dest_count = count as u16;
            return Ok(());
        }
        let value = usermem::read_current_user_c_string(value_ptr, dest.len())
            .map_err(address_space_error_to_linux_errno)?;
        let needed = value.len().checked_add(1).ok_or(LINUX_E2BIG)?;
        if offset.checked_add(needed).ok_or(LINUX_E2BIG)? > dest.len() {
            return Err(LINUX_E2BIG);
        }
        dest[offset..offset + value.len()].copy_from_slice(value.as_bytes());
        offset += value.len();
        dest[offset] = 0;
        offset += 1;
        cursor = cursor
            .checked_add(size_of::<u64>() as u64)
            .ok_or(LINUX_EINVAL)?;
    }
    Err(LINUX_E2BIG)
}

pub(super) fn new_vfs_request(op: u16) -> VfsIpcRequest {
    let mut request = VfsIpcRequest {
        op,
        ..VfsIpcRequest::default()
    };
    populate_vfs_identity(&mut request);
    request
}

pub(super) fn populate_vfs_identity(request: &mut VfsIpcRequest) {
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

pub(super) fn populate_vfs_path(request: &mut VfsIpcRequest, path: &str) -> Result<(), i64> {
    let bytes = path.as_bytes();
    if bytes.len() > VFS_IPC_PATH_CAPACITY {
        return Err(LINUX_EINVAL);
    }
    request.path_len = bytes.len() as u32;
    request.path[..bytes.len()].copy_from_slice(bytes);
    Ok(())
}

pub(super) fn call_vfs_ipc_request(request: &VfsIpcRequest) -> Result<VfsIpcResponse, i64> {
    let response = ipc_ops::call_service_endpoint(IPC_SERVICE_VFSD, as_bytes(request))?;
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

pub(super) fn call_service_offload_request(
    service_id: u64,
    request: &LinuxSyscallOffloadRequest,
) -> Result<LinuxSyscallOffloadResponse, i64> {
    let response = ipc_ops::call_service_endpoint(service_id, as_bytes(request))?;
    if response.len() != size_of::<LinuxSyscallOffloadResponse>() {
        return Err(LINUX_EINVAL);
    }
    Ok(read_unaligned::<LinuxSyscallOffloadResponse>(
        response.as_slice(),
    ))
}

pub(super) fn call_loaderd(request: &LoaderSpawnRequest) -> Result<LoaderSpawnResponse, i64> {
    let response = ipc_ops::call_service_endpoint(IPC_SERVICE_LOADERD, as_bytes(request))?;
    if response.len() != size_of::<LoaderSpawnResponse>() {
        return Err(LINUX_EINVAL);
    }
    Ok(read_unaligned::<LoaderSpawnResponse>(response.as_slice()))
}

pub(super) fn ensure_vfs_status(response: &VfsIpcResponse) -> Result<(), i64> {
    if response.status != 0 {
        return Err(response.status.unsigned_abs() as i64);
    }
    Ok(())
}

pub(super) fn ensure_service_response(
    response: &LinuxSyscallOffloadResponse,
    op: u16,
) -> Result<(), i64> {
    if response.version != SYSCALL_OFFLOAD_ABI_VERSION
        || response.op != op
        || response.reserved0 != 0
    {
        return Err(LINUX_EINVAL);
    }
    ensure_syscalld_status(response)
}

pub(super) fn current_remote_vfs_handle(fd: u64) -> Option<multitask::RemoteVfsHandle> {
    multitask::with_current_user_process_state(|_, _, process_state| {
        match process_state.handles().get(fd) {
            Some(multitask::KernelHandle::RemoteVfs(remote)) => Some(remote.clone()),
            _ => None,
        }
    })
    .flatten()
}

pub(super) fn copy_current_user_path(ptr: u64, capacity: usize) -> Result<String, i64> {
    if ptr == 0 {
        return Err(LINUX_EFAULT);
    }
    let path = usermem::read_current_user_c_string(ptr, capacity)
        .map_err(address_space_error_to_linux_errno)?;
    if path.is_empty() || path.len() > capacity {
        return Err(LINUX_EINVAL);
    }
    Ok(path)
}
