// RING3-MIGRATION-REFERENCE START: vfsd/netd fd-usercopy substrate exception.
// vfsd/netd own file/socket policy. Ring0 keeps current-process user-copy,
// fd-table mutation, and remote-handle/socket-token installation substrate.
use super::*;
use alloc::collections::{BTreeMap, VecDeque};

const PENDING_NETD_REF_CAPACITY: usize = 4096;
const REMOTE_VFS_OPEN_DESCRIPTION_CAPACITY: usize = 32 * 1024;

#[derive(Clone, Copy)]
struct PendingNetdRef {
    op: u16,
    socket_token: u64,
    operation_hi: u64,
    operation_lo: u64,
    acknowledge_only: bool,
}

lazy_static::lazy_static! {
    static ref PENDING_NETD_REFS: spin::Mutex<VecDeque<PendingNetdRef>> =
        spin::Mutex::new(VecDeque::new());
    /// Kernel fd tables are authoritative for descriptor references. vfsd
    /// owns open-file state, but sees only initial open and final release.
    static ref REMOTE_VFS_DESCRIPTOR_REFS: spin::Mutex<BTreeMap<u64, u64>> =
        spin::Mutex::new(BTreeMap::new());
}

fn register_remote_vfs_open_description(remote_id: u64) -> Result<(), i64> {
    if remote_id == 0 {
        return Err(LINUX_EINVAL);
    }
    let mut refs = REMOTE_VFS_DESCRIPTOR_REFS.lock();
    if refs.contains_key(&remote_id) {
        return Err(LINUX_EEXIST);
    }
    if refs.len() == REMOTE_VFS_OPEN_DESCRIPTION_CAPACITY {
        return Err(LINUX_ENOSPC);
    }
    refs.insert(remote_id, 1);
    Ok(())
}

fn acquire_remote_vfs_descriptor_ref(remote_id: u64) -> Result<(), i64> {
    let mut refs = REMOTE_VFS_DESCRIPTOR_REFS.lock();
    let count = refs.get_mut(&remote_id).ok_or(LINUX_EBADF)?;
    *count = count.checked_add(1).ok_or(LINUX_EOVERFLOW)?;
    Ok(())
}

fn release_remote_vfs_descriptor_ref(remote_id: u64) -> Result<bool, i64> {
    let mut refs = REMOTE_VFS_DESCRIPTOR_REFS.lock();
    let count = refs.get_mut(&remote_id).ok_or(LINUX_EBADF)?;
    if *count == 0 {
        return Err(LINUX_ESTALE);
    }
    *count -= 1;
    if *count != 0 {
        return Ok(false);
    }
    refs.remove(&remote_id);
    Ok(true)
}

pub fn syscall_linux_vfs_openat(dirfd: u64, path_ptr: u64, flags: u64, mode: u64) -> u64 {
    let path = match copy_current_user_path(path_ptr, VFS_IPC_PATH_CAPACITY) {
        Ok(path) => path,
        Err(errno) => return linux_errno(errno),
    };
    if path.contains("startup-programs") || path.contains("runtime-env") {
        crate::debug::info!(
            compat,
            "bootstrap path probe: op=openat dirfd={:#x} flags={:#x} path={}",
            dirfd,
            flags,
            path
        );
    }
    if should_try_devmgrd_open(path.as_str()) {
        return match open_device_via_devmgrd(path.as_str(), flags) {
            Ok(fd) => fd,
            Err(errno) => linux_errno(errno),
        };
    }
    let mut request = new_vfs_request(VFS_IPC_OP_OPENAT);
    request.arg0 = flags;
    request.arg1 = mode;
    // The proposed remote id is a boot-entropy-backed capability. vfsd owns
    // the object state, while dup/fork/transfer preserve this exact token.
    request.arg3 = mint_service_object_id();
    if let Err(errno) = populate_vfs_path_base(&mut request, dirfd, &path) {
        return linux_errno(errno);
    }
    let response = match call_pinned_remote_vfs_request(&request) {
        Ok(response) => response,
        Err(errno) => {
            let _ = call_vfs_remote_close_bounded(request.arg3);
            return linux_errno(errno);
        }
    };
    if let Err(errno) = ensure_vfs_status(&response) {
        let _ = call_vfs_remote_close_bounded(request.arg3);
        return linux_errno(errno);
    }
    let remote_id = response.remote_id;
    if remote_id == 0 || remote_id != request.arg3 {
        let _ = call_vfs_remote_close_bounded(request.arg3);
        return linux_errno(LINUX_EINVAL);
    }
    if let Err(errno) = register_remote_vfs_open_description(remote_id) {
        // EEXIST is a capability collision and must not close the already
        // tracked description. Other local admission failures retire the new
        // provider object so they cannot leak it.
        if errno != LINUX_EEXIST {
            let _ = call_vfs_remote_close_bounded(remote_id);
        }
        return linux_errno(errno);
    }
    let kind = match response.handle_kind {
        VFS_IPC_HANDLE_KIND_FILE => multitask::RemoteVfsHandleKind::File,
        VFS_IPC_HANDLE_KIND_DIR => multitask::RemoteVfsHandleKind::Directory,
        VFS_IPC_HANDLE_KIND_DEVICE => multitask::RemoteVfsHandleKind::Device,
        _ => {
            release_service_handle_refs_bounded(&[ServiceHandleRef::RemoteVfs(remote_id)]);
            return linux_errno(LINUX_EINVAL);
        }
    };
    let len = response.payload_len as usize;
    let handle_path = match core::str::from_utf8(&response.payload[..len]) {
        Ok(path) if !path.is_empty() && path.starts_with('/') => alloc::string::String::from(path),
        _ => {
            release_service_handle_refs_bounded(&[ServiceHandleRef::RemoteVfs(remote_id)]);
            return linux_errno(LINUX_EINVAL);
        }
    };
    let handle = multitask::KernelHandle::RemoteVfs(multitask::RemoteVfsHandle::new(
        remote_id,
        kind,
        handle_path,
        response.value,
        response.aux as u16,
    ));
    let installed = multitask::with_current_user_process_state_mut(|_, _, process_state| {
        process_state
            .handles_mut()
            .install_with_open_flags(handle, flags)
    });
    match installed {
        Some(Some(fd)) => fd,
        Some(None) => {
            release_service_handle_refs_bounded(&[ServiceHandleRef::RemoteVfs(remote_id)]);
            linux_errno(LINUX_EMFILE)
        }
        None => {
            release_service_handle_refs_bounded(&[ServiceHandleRef::RemoteVfs(remote_id)]);
            linux_errno(LINUX_EINVAL)
        }
    }
}

pub fn syscall_linux_vfs_close(fd: u64) -> u64 {
    let closed_handle = multitask::with_current_user_process_state_mut(|_, _, process_state| {
        process_state.handles_mut().close(fd)
    })
    .flatten();
    let Some(closed_handle) = closed_handle else {
        return linux_errno(LINUX_EBADF);
    };
    if let Some(handle_ref) = service_handle_ref_for_handle(&closed_handle) {
        release_service_handle_refs_bounded(&[handle_ref]);
    }
    if let multitask::KernelHandle::Console(console) = closed_handle
        && console.is_last_reference()
    {
        let _ = purge_vfs_epoll_object_bounded(WAITSET_PROVIDER_SESSIOND, console.token_id());
    }
    0
}

pub fn syscall_linux_vfs_dup(oldfd: u64, newfd: u64, flags: u64, mode: VfsDupMode) -> u64 {
    if matches!(mode, VfsDupMode::Dup3) && (oldfd == newfd || flags & !linux_abi::O_CLOEXEC != 0) {
        return linux_errno(LINUX_EINVAL);
    }

    if matches!(mode, VfsDupMode::Dup2) && oldfd == newfd {
        return if current_fd_exists(oldfd) {
            oldfd
        } else {
            linux_errno(LINUX_EBADF)
        };
    }
    if matches!(mode, VfsDupMode::Dup2 | VfsDupMode::Dup3) && newfd > multitask::MAX_DYNAMIC_FD {
        return linux_errno(LINUX_EBADF);
    }

    let target = match mode {
        VfsDupMode::Dup => FdDupTarget::Minimum(0),
        VfsDupMode::Dup2 | VfsDupMode::Dup3 => FdDupTarget::Exact(newfd),
    };
    match duplicate_fd_transaction(
        oldfd,
        target,
        flags & linux_abi::O_CLOEXEC != 0,
        duplicate_install_errno(mode),
    ) {
        Ok(fd) => fd,
        Err(errno) => linux_errno(errno),
    }
}

pub fn syscall_linux_vfs_fcntl(fd: u64, cmd: u64, arg: u64) -> u64 {
    match cmd {
        linux_abi::F_DUPFD | linux_abi::F_DUPFD_CLOEXEC => {
            let Ok(min_fd) = i32::try_from(arg) else {
                return linux_errno(LINUX_EINVAL);
            };
            if min_fd < 0 {
                return linux_errno(LINUX_EINVAL);
            }
            if min_fd as u64 > multitask::MAX_DYNAMIC_FD {
                return linux_errno(LINUX_EINVAL);
            }
            let close_on_exec = cmd == linux_abi::F_DUPFD_CLOEXEC;
            match duplicate_fd_transaction(
                fd,
                FdDupTarget::Minimum(min_fd as u64),
                close_on_exec,
                LINUX_EMFILE,
            ) {
                Ok(new_fd) => new_fd,
                Err(errno) => linux_errno(errno),
            }
        }
        linux_abi::F_GETFD | linux_abi::F_SETFD | linux_abi::F_GETFL | linux_abi::F_SETFL => {
            if matches!(cmd, linux_abi::F_GETFL | linux_abi::F_SETFL) {
                if let Some(remote) = current_remote_vfs_handle(fd) {
                    let mut request = new_vfs_request(VFS_IPC_OP_FCNTL);
                    request.fd = fd;
                    request.remote_id = remote.remote_id();
                    request.arg0 = cmd;
                    request.arg1 = arg;
                    let value =
                        match call_pinned_remote_vfs_request(&request).and_then(|response| {
                            ensure_vfs_status(&response)?;
                            Ok(response.value)
                        }) {
                            Ok(value) => value,
                            Err(errno) => return linux_errno(errno),
                        };
                    if cmd == linux_abi::F_SETFL {
                        let _ = multitask::with_current_user_process_state_mut(
                            |_, _, process_state| {
                                process_state
                                    .handles_mut()
                                    .get_entry_mut(fd)
                                    .map(|entry| entry.set_status_flags(value));
                            },
                        );
                        return 0;
                    }
                    return value;
                }
            }
            match multitask::with_current_user_process_state_mut(|_, _, process_state| {
                let entry = process_state.handles_mut().get_entry_mut(fd)?;
                Some(match cmd {
                    linux_abi::F_GETFD => entry.fd_flags() as u64,
                    linux_abi::F_SETFD => {
                        entry.set_fd_flags(arg as u32);
                        0
                    }
                    linux_abi::F_GETFL => entry.status_flags(),
                    linux_abi::F_SETFL => {
                        entry.set_status_flags(arg);
                        0
                    }
                    _ => unreachable!(),
                })
            })
            .flatten()
            {
                Some(value) => value,
                None => linux_errno(LINUX_EBADF),
            }
        }
        _ => {
            if let Some(memfd) = current_memfd_handle(fd) {
                return match cmd {
                    linux_abi::F_GET_SEALS => memfd.seals() as u64,
                    linux_abi::F_ADD_SEALS => match memfd.add_seals(arg as u32) {
                        Ok(()) => 0,
                        Err(err) => linux_errno(memfd_error_to_linux_errno(err)),
                    },
                    _ => linux_errno(LINUX_EINVAL),
                };
            }
            let Some(remote) = current_remote_vfs_handle(fd) else {
                return linux_errno(LINUX_EBADF);
            };
            let mut request = new_vfs_request(VFS_IPC_OP_FCNTL);
            request.fd = fd;
            request.remote_id = remote.remote_id();
            request.arg0 = cmd;
            request.arg1 = arg;
            match call_pinned_remote_vfs_request(&request).and_then(|response| {
                ensure_vfs_status(&response)?;
                Ok(response.value)
            }) {
                Ok(value) => value,
                Err(errno) => linux_errno(errno),
            }
        }
    }
}

pub fn call_pinned_remote_vfs_request(request: &VfsIpcRequest) -> Result<VfsIpcResponse, i64> {
    if request.remote_id == 0 {
        return call_vfs_ipc_request(request);
    }
    let fd = match request.op {
        VFS_IPC_OP_OPENAT
        | VFS_IPC_OP_STATX
        | VFS_IPC_OP_NEWFSTATAT
        | VFS_IPC_OP_READLINKAT
        | VFS_IPC_OP_ACCESS
        | VFS_IPC_OP_MKDIR
        | VFS_IPC_OP_UNLINKAT => request.dirfd,
        _ => request.fd,
    };
    acquire_current_remote_vfs_ref(fd, request.remote_id)?;
    let result = call_vfs_ipc_request(request);
    release_service_handle_refs_bounded(&[ServiceHandleRef::RemoteVfs(request.remote_id)]);
    result
}

/// Atomically resolves an fd and pins its exact open description. Holding the
/// process handle-table lock across the reference increment closes the fdget
/// versus concurrent-close gap; a cloned handle value alone is not a pin.
pub fn acquire_current_remote_vfs_ref(fd: u64, expected_remote_id: u64) -> Result<(), i64> {
    multitask::with_current_user_process_state(|_, _, process_state| {
        let remote = match process_state.handles().get(fd) {
            Some(multitask::KernelHandle::RemoteVfs(remote))
                if remote.remote_id() == expected_remote_id =>
            {
                remote
            }
            _ => return Err(LINUX_EBADF),
        };
        acquire_remote_vfs_descriptor_ref(remote.remote_id())
    })
    .ok_or(LINUX_EBADF)?
}

fn duplicate_install_errno(mode: VfsDupMode) -> i64 {
    match mode {
        VfsDupMode::Dup => LINUX_EMFILE,
        VfsDupMode::Dup2 | VfsDupMode::Dup3 => LINUX_EBADF,
    }
}

#[derive(Clone, Copy)]
enum FdDupTarget {
    Minimum(u64),
    Exact(u64),
}

fn duplicate_fd_transaction(
    oldfd: u64,
    target: FdDupTarget,
    close_on_exec: bool,
    install_errno: i64,
) -> Result<u64, i64> {
    let source = multitask::with_current_user_process_state(|_, _, process_state| {
        process_state.handles().get_entry(oldfd).cloned()
    })
    .flatten()
    .ok_or(LINUX_EBADF)?;
    let source_token = source.token();
    let source_ref = service_handle_ref_for_handle(source.handle());
    if let Some(handle_ref) = source_ref {
        acquire_service_handle_ref(handle_ref)?;
    }

    let committed = multitask::with_current_user_process_state_mut(|_, _, process_state| {
        if process_state
            .handles()
            .get_entry(oldfd)
            .map(|entry| entry.token())
            != Some(source_token)
        {
            return Err(LINUX_EBADF);
        }
        match target {
            FdDupTarget::Minimum(min_fd) => process_state
                .handles_mut()
                .duplicate_min(oldfd, min_fd, close_on_exec)
                .map(|fd| (fd, None))
                .ok_or(install_errno),
            FdDupTarget::Exact(newfd) => {
                if process_state.handles().is_reserved(newfd) {
                    return Err(LINUX_EBUSY);
                }
                process_state
                    .handles_mut()
                    .duplicate_exact_with_replaced(oldfd, newfd, close_on_exec)
                    .ok_or(install_errno)
            }
        }
    })
    .unwrap_or(Err(LINUX_EBADF));

    let (fd, replaced) = match committed {
        Ok(committed) => committed,
        Err(errno) => {
            if let Some(handle_ref) = source_ref {
                release_service_handle_refs_bounded(&[handle_ref]);
            }
            return Err(errno);
        }
    };

    if let Some(replaced) = replaced {
        if let Some(handle_ref) = service_handle_ref_for_handle(&replaced) {
            release_service_handle_refs_bounded(&[handle_ref]);
        }
        if let multitask::KernelHandle::Console(console) = replaced
            && console.is_last_reference()
        {
            let _ = purge_vfs_epoll_object_bounded(WAITSET_PROVIDER_SESSIOND, console.token_id());
        }
    }
    Ok(fd)
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec;

    #[test]
    fn descriptor_exhaustion_is_not_reported_as_a_bad_source_fd() {
        assert_eq!(duplicate_install_errno(VfsDupMode::Dup), LINUX_EMFILE);
        assert_eq!(duplicate_install_errno(VfsDupMode::Dup2), LINUX_EBADF);
        assert_eq!(duplicate_install_errno(VfsDupMode::Dup3), LINUX_EBADF);
    }

    #[test]
    fn transferred_input_description_keeps_the_waitset_service_reference() {
        let device = kernel_object::api::device::DeviceHandle::from_parts_with_token(
            kernel_object::api::device::DeviceId::Input,
            kernel_object::api::device::DeviceAccessKind::Evdev,
            u64::MAX - 811,
        );
        assert_eq!(
            service_handle_ref_for_handle(&multitask::KernelHandle::Device(device)),
            Some(ServiceHandleRef::Input(device.token_id()))
        );
    }

    #[test]
    fn fork_service_refs_come_from_the_frozen_child_handle_snapshot() {
        let mut parent = multitask::HandleTable::new();
        let inherited = multitask::EpollHandle::new();
        let inherited_token = inherited.token_id();
        let fd = parent
            .install(multitask::KernelHandle::Epoll(inherited))
            .expect("epoll fd");
        let child_snapshot = parent.clone();

        assert!(parent.close(fd).is_some());
        let replacement = multitask::EpollHandle::new();
        let replacement_token = replacement.token_id();
        assert_eq!(
            parent.install(multitask::KernelHandle::Epoll(replacement)),
            Some(fd)
        );

        assert_eq!(
            service_handle_refs_from_table(&child_snapshot),
            vec![ServiceHandleRef::Epoll(inherited_token)]
        );
        assert_eq!(
            service_handle_refs_from_table(&parent),
            vec![ServiceHandleRef::Epoll(replacement_token)]
        );
    }

    #[test]
    fn exit_service_refs_come_from_the_exact_closed_handle_set() {
        let mut handles = multitask::HandleTable::new();
        let epoll = multitask::EpollHandle::new();
        let token = epoll.token_id();
        handles
            .install(multitask::KernelHandle::Epoll(epoll))
            .expect("epoll fd");

        let closed = handles.close_all();
        assert!(handles.entries_snapshot(false).is_empty());
        assert_eq!(
            service_handle_refs_from_handles(&closed),
            vec![ServiceHandleRef::Epoll(token)]
        );
    }

    #[test]
    fn remote_vfs_refs_are_local_and_provider_close_is_final_only() {
        let id = u64::MAX - 0x5f5;
        assert_eq!(register_remote_vfs_open_description(id), Ok(()));
        assert_eq!(acquire_remote_vfs_descriptor_ref(id), Ok(()));
        assert_eq!(release_remote_vfs_descriptor_ref(id), Ok(false));
        assert_eq!(release_remote_vfs_descriptor_ref(id), Ok(true));
        assert_eq!(release_remote_vfs_descriptor_ref(id), Err(LINUX_EBADF));
    }
}

pub fn syscall_linux_vfs_read(fd: u64, user_ptr: u64, user_len: u64) -> u64 {
    if let Some((console, status_flags)) = current_console_handle_and_status_flags(fd) {
        if console.stream() != multitask::ConsoleStreamKind::Input {
            return linux_errno(LINUX_EBADF);
        }
        let Ok(user_len) = usize::try_from(user_len) else {
            return linux_errno(LINUX_EINVAL);
        };
        if user_len == 0 {
            return 0;
        }
        if !current_console_session_is_system() {
            return match console_read_via_sessiond(
                user_ptr,
                user_len,
                status_flags & linux_abi::O_NONBLOCK != 0,
            ) {
                Ok(read) => read,
                Err(errno) => linux_errno(errno),
            };
        }
        return match crate::user::sysops::console::read_into_current_process(user_ptr, user_len) {
            Ok(0) if status_flags & linux_abi::O_NONBLOCK != 0 => linux_errno(LINUX_EAGAIN),
            Ok(read) => read as u64,
            Err(err) => linux_errno(address_space_error_to_linux_errno(err)),
        };
    }
    if current_socket_fd(fd) {
        return syscall_linux_net6(
            SYSCALL_OFFLOAD_OP_LINUX_RECVFROM,
            fd,
            user_ptr,
            user_len,
            0,
            0,
            0,
        );
    }
    if let Some(mut memfd) = current_memfd_handle(fd) {
        return read_local_vfs_file(user_ptr, user_len, |_, chunk| memfd.read_into(chunk));
    }
    if let Some((inputd_access, status_flags)) = current_input_device_access(fd) {
        return read_input_device_via_inputd(fd, user_ptr, user_len, inputd_access, status_flags);
    }
    let Some(remote) = current_remote_vfs_handle(fd) else {
        return linux_errno(LINUX_EBADF);
    };
    if remote.kind() == multitask::RemoteVfsHandleKind::Device
        && matches!(
            remote.device_access(),
            INPUTD_ACCESS_NATIVE | INPUTD_ACCESS_EVDEV
        )
    {
        let access = remote.device_access();
        let Some(status_flags) = current_fd_status_flags(fd) else {
            // The remote device handle and its descriptor state must be
            // observed atomically from the current process. Do not turn a
            // missing descriptor into a blocking read with synthetic flags.
            return linux_errno(LINUX_EBADF);
        };
        return read_input_device_via_inputd(fd, user_ptr, user_len, access, status_flags);
    }
    read_remote_vfs(fd, remote.remote_id(), user_ptr, user_len, None)
}

fn current_console_handle_and_status_flags(fd: u64) -> Option<(multitask::ConsoleHandle, u64)> {
    multitask::with_current_user_process_state(|_, _, process_state| {
        let entry = process_state.handles().get_entry(fd)?;
        match entry.handle() {
            multitask::KernelHandle::Console(console) => {
                Some((console.clone(), entry.status_flags()))
            }
            _ => None,
        }
    })
    .flatten()
}

pub fn syscall_linux_vfs_pread64(fd: u64, user_ptr: u64, user_len: u64, offset: u64) -> u64 {
    if offset > i64::MAX as u64 {
        return linux_errno(LINUX_EINVAL);
    }
    if let Some(memfd) = current_memfd_handle(fd) {
        let Ok(offset) = usize::try_from(offset) else {
            return linux_errno(LINUX_EINVAL);
        };
        return read_local_vfs_file(user_ptr, user_len, |copied, chunk| {
            offset
                .checked_add(copied)
                .map(|position| memfd.read_at(position, chunk))
                .unwrap_or(0)
        });
    }
    let Some(remote) = current_remote_vfs_handle(fd) else {
        return linux_errno(LINUX_EBADF);
    };
    read_remote_vfs(fd, remote.remote_id(), user_ptr, user_len, Some(offset))
}

pub fn syscall_linux_vfs_write(fd: u64, user_ptr: u64, user_len: u64) -> u64 {
    if let Some(multitask::KernelHandle::Console(console)) = current_kernel_handle(fd) {
        if !matches!(
            console.stream(),
            multitask::ConsoleStreamKind::Output | multitask::ConsoleStreamKind::Error
        ) {
            return linux_errno(LINUX_EBADF);
        }
        let Ok(user_len) = usize::try_from(user_len) else {
            return linux_errno(LINUX_EINVAL);
        };
        if user_len == 0 {
            return 0;
        }
        if !current_console_session_is_system() {
            return match console_write_via_sessiond(user_ptr, user_len) {
                Ok(written) => written,
                Err(errno) => linux_errno(errno),
            };
        }
        return match crate::user::sysops::console::write_from_current_process(user_ptr, user_len) {
            Ok(written) => written as u64,
            Err(err) => linux_errno(address_space_error_to_linux_errno(err)),
        };
    }
    if current_socket_fd(fd) {
        return syscall_linux_net6(
            SYSCALL_OFFLOAD_OP_LINUX_SENDTO,
            fd,
            user_ptr,
            user_len,
            0,
            0,
            0,
        );
    }
    if let Some(mut memfd) = current_memfd_handle(fd) {
        let Ok(user_len) = usize::try_from(user_len) else {
            return linux_errno(LINUX_EINVAL);
        };
        if user_len == 0 {
            return 0;
        }
        let mut copied = 0usize;
        let mut chunk = [0_u8; 256];
        while copied < user_len {
            let chunk_len = (user_len - copied).min(chunk.len());
            let Some(src) = user_ptr.checked_add(copied as u64) else {
                return linux_errno(LINUX_EINVAL);
            };
            if let Err(err) = usermem::copy_from_current_user_exact(src, &mut chunk[..chunk_len]) {
                return linux_errno(address_space_error_to_linux_errno(err));
            }
            let written = match memfd.write_from(&chunk[..chunk_len]) {
                Ok(written) => written,
                Err(err) => return linux_errno(memfd_error_to_linux_errno(err)),
            };
            copied += written;
            multitask::cond_resched();
            if written < chunk_len {
                break;
            }
        }
        return copied as u64;
    }
    let Some(remote) = current_remote_vfs_handle(fd) else {
        return linux_errno(LINUX_EBADF);
    };
    write_remote_vfs(fd, remote.remote_id(), user_ptr, user_len)
}

pub fn syscall_linux_vfs_writev(fd: u64, iov_ptr: u64, iovcnt: u64) -> u64 {
    if current_socket_fd(fd) {
        let bytes = match read_socket_writev_payload(iov_ptr, iovcnt) {
            Ok(bytes) => bytes,
            Err(errno) => return linux_errno(errno),
        };
        if bytes.is_empty() {
            return 0;
        }
        let mut written = 0usize;
        while written < bytes.len() {
            let chunk = &bytes[written..];
            let count = match call_netd_socket_payload(fd, SYSCALL_OFFLOAD_OP_LINUX_SENDTO, chunk) {
                Ok(count) => count as usize,
                Err(errno) => {
                    return if written == 0 {
                        linux_errno(errno)
                    } else {
                        written as u64
                    };
                }
            };
            if count == 0 {
                break;
            }
            written = written.saturating_add(count);
        }
        return written as u64;
    }
    writev_via_write(fd, iov_ptr, iovcnt)
}

pub fn is_linux_error(result: u64) -> bool {
    let signed = result as i64;
    (-4095..0).contains(&signed)
}

fn current_fd_exists(fd: u64) -> bool {
    multitask::with_current_user_process_state(|_, _, process_state| {
        process_state.handles().get_entry(fd).is_some()
    })
    .unwrap_or(false)
}

fn call_vfs_remote_close(remote_id: u64) -> Result<(), i64> {
    call_vfs_remote_close_bounded(remote_id)
}

fn call_vfs_remote_close_bounded(remote_id: u64) -> Result<(), i64> {
    let mut request = new_vfs_request(VFS_IPC_OP_CLOSE);
    request.remote_id = remote_id;
    call_vfs_ipc_request_with_timeout(&request, 16)
        .and_then(|response| ensure_vfs_status(&response))
}

fn read_local_vfs_file<F>(user_ptr: u64, user_len: u64, mut read: F) -> u64
where
    F: FnMut(usize, &mut [u8]) -> usize,
{
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
    let mut chunk = alloc::vec![0_u8; user_len.min(64 * 1024)];
    while copied < user_len {
        let chunk_len = (user_len - copied).min(chunk.len());
        let read_len = read(copied, &mut chunk[..chunk_len]);
        if read_len == 0 {
            break;
        }
        let Some(dest) = user_ptr.checked_add(copied as u64) else {
            return linux_errno(LINUX_EINVAL);
        };
        if let Err(err) = usermem::write_current_user_bytes(dest, &chunk[..read_len]) {
            return linux_errno(address_space_error_to_linux_errno(err));
        }
        copied += read_len;
        multitask::cond_resched();
        if read_len < chunk_len {
            break;
        }
    }
    copied as u64
}

fn read_remote_vfs(
    fd: u64,
    remote_id: u64,
    user_ptr: u64,
    user_len: u64,
    offset: Option<u64>,
) -> u64 {
    if let Err(errno) = acquire_current_remote_vfs_ref(fd, remote_id) {
        return linux_errno(errno);
    }
    let result = read_remote_vfs_pinned(fd, remote_id, user_ptr, user_len, offset);
    release_service_handle_refs_bounded(&[ServiceHandleRef::RemoteVfs(remote_id)]);
    result
}

fn read_remote_vfs_pinned(
    fd: u64,
    remote_id: u64,
    user_ptr: u64,
    user_len: u64,
    offset: Option<u64>,
) -> u64 {
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
        let mut request = new_vfs_request(if offset.is_some() {
            VFS_IPC_OP_PREAD64
        } else {
            VFS_IPC_OP_READ
        });
        request.fd = fd;
        request.remote_id = remote_id;
        request.arg1 = chunk_len as u64;
        if let Some(offset) = offset {
            let Some(chunk_offset) = offset.checked_add(copied as u64) else {
                return if copied > 0 {
                    copied as u64
                } else {
                    linux_errno(LINUX_EOVERFLOW)
                };
            };
            if chunk_offset > i64::MAX as u64 {
                return if copied > 0 {
                    copied as u64
                } else {
                    linux_errno(LINUX_EOVERFLOW)
                };
            }
            request.arg0 = chunk_offset;
        }
        let response = match call_vfs_ipc_request(&request) {
            Ok(response) => response,
            Err(errno) => {
                if offset.is_none() {
                    let _ = settle_vfs_cursor_mutation(&request, false);
                }
                return if copied > 0 {
                    copied as u64
                } else {
                    linux_errno(errno)
                };
            }
        };
        if let Err(errno) = ensure_vfs_status(&response) {
            return if copied > 0 {
                copied as u64
            } else {
                linux_errno(errno)
            };
        }
        let read = response.payload_len as usize;
        if read > chunk_len || read > response.payload.len() {
            if offset.is_none() {
                let _ = settle_vfs_cursor_mutation(&request, false);
            }
            return if copied > 0 {
                copied as u64
            } else {
                linux_errno(LINUX_EINVAL)
            };
        }
        if read == 0 {
            if offset.is_none() {
                if let Err(errno) = settle_vfs_cursor_mutation(&request, true) {
                    return if copied > 0 {
                        copied as u64
                    } else {
                        linux_errno(errno)
                    };
                }
            }
            break;
        }
        let Some(dest) = user_ptr.checked_add(copied as u64) else {
            if offset.is_none() {
                let _ = settle_vfs_cursor_mutation(&request, false);
            }
            return if copied > 0 {
                copied as u64
            } else {
                linux_errno(LINUX_EINVAL)
            };
        };
        if let Err(err) = usermem::write_current_user_bytes(dest, &response.payload[..read]) {
            if offset.is_none() {
                let _ = settle_vfs_cursor_mutation(&request, false);
            }
            return if copied > 0 {
                copied as u64
            } else {
                linux_errno(address_space_error_to_linux_errno(err))
            };
        }
        copied += read;
        if offset.is_none() {
            if let Err(errno) = settle_vfs_cursor_mutation(&request, true) {
                return if copied > 0 {
                    copied as u64
                } else {
                    linux_errno(errno)
                };
            }
        }
        multitask::cond_resched();
        if read < chunk_len {
            break;
        }
    }
    copied as u64
}

fn write_remote_vfs(fd: u64, remote_id: u64, user_ptr: u64, user_len: u64) -> u64 {
    let Ok(user_len) = usize::try_from(user_len) else {
        return linux_errno(LINUX_EINVAL);
    };
    if user_len == 0 {
        return 0;
    }
    if let Err(errno) = acquire_current_remote_vfs_ref(fd, remote_id) {
        return linux_errno(errno);
    }
    let result = (|| {
        let chunk_len = user_len.min(VFS_IPC_REQUEST_PAYLOAD_CAPACITY);
        let mut request = new_vfs_request(VFS_IPC_OP_WRITE);
        request.fd = fd;
        request.remote_id = remote_id;
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
    })();
    release_service_handle_refs_bounded(&[ServiceHandleRef::RemoteVfs(remote_id)]);
    result
}

fn writev_via_write(fd: u64, iov_ptr: u64, iovcnt: u64) -> u64 {
    const LINUX_UIO_MAXIOV: u64 = 1024;
    if iovcnt > LINUX_UIO_MAXIOV {
        return linux_errno(LINUX_EINVAL);
    }
    let mut total = 0_u64;
    for index in 0..iovcnt {
        let Some(entry_ptr) = iov_ptr.checked_add(index.saturating_mul(16)) else {
            return if total == 0 {
                linux_errno(LINUX_EFAULT)
            } else {
                total
            };
        };
        let iov = match usermem::read_current_user_struct::<linux_abi::LinuxIovec>(entry_ptr) {
            Ok(iov) => iov,
            Err(err) => {
                return if total == 0 {
                    linux_errno(address_space_error_to_linux_errno(err))
                } else {
                    total
                };
            }
        };
        let mut written_for_iov = 0_u64;
        while written_for_iov < iov.iov_len {
            let Some(chunk_ptr) = iov.iov_base.checked_add(written_for_iov) else {
                return if total == 0 {
                    linux_errno(LINUX_EFAULT)
                } else {
                    total
                };
            };
            let chunk_len =
                (iov.iov_len - written_for_iov).min(VFS_IPC_REQUEST_PAYLOAD_CAPACITY as u64);
            let result = syscall_linux_vfs_write(fd, chunk_ptr, chunk_len);
            if is_linux_error(result) {
                return if total == 0 { result } else { total };
            }
            if result == 0 {
                return total;
            }
            total = match total.checked_add(result) {
                Some(value) => value,
                None => return linux_errno(LINUX_EINVAL),
            };
            written_for_iov = match written_for_iov.checked_add(result) {
                Some(value) => value,
                None => return linux_errno(LINUX_EINVAL),
            };
            if result < chunk_len {
                return total;
            }
        }
    }
    total
}

fn current_socket_fd(fd: u64) -> bool {
    multitask::with_current_user_process_state(|_, _, process_state| {
        matches!(
            process_state.handles().get(fd),
            Some(multitask::KernelHandle::Socket(_) | multitask::KernelHandle::InetSocket(_))
        )
    })
    .unwrap_or(false)
}

fn current_socket_token_and_flags(fd: u64) -> Option<(u64, u64)> {
    multitask::with_current_user_process_state(|_, _, process_state| {
        let entry = process_state.handles().get_entry(fd)?;
        match entry.handle() {
            multitask::KernelHandle::Socket(socket) => {
                Some((socket.token_id(), entry.status_flags()))
            }
            multitask::KernelHandle::InetSocket(socket) => {
                Some((socket.token_id(), entry.status_flags()))
            }
            _ => None,
        }
    })
    .flatten()
}

pub fn new_netd_socket_request(op: u16, socket_token: u64) -> NetdIpcRequest {
    let mut operation = [0_u8; 16];
    nucleus_core::util::random::Random::new().fill_bytes(&mut operation);
    let mut request = NetdIpcRequest {
        version: NETD_IPC_ABI_VERSION,
        op,
        socket_token,
        operation_hi: u64::from_le_bytes(operation[..8].try_into().unwrap()),
        operation_lo: u64::from_le_bytes(operation[8..].try_into().unwrap()),
        ..NetdIpcRequest::default()
    };
    if request.operation_hi == 0 && request.operation_lo == 0 {
        request.operation_lo = 1;
    }
    if let Some(snapshot) = multitask::current_user_snapshot() {
        request.pid = snapshot.process_id();
        request.tid = snapshot.thread_id();
    }
    if let Some(security) = multitask::with_current_process_credentials(|security| security) {
        request.uid = security.uid();
        request.gid = security.gid();
        request.euid = security.euid();
        request.egid = security.egid();
    }
    request
}

pub fn call_netd_socket_token_op(op: u16, socket_token: u64) -> Result<u64, i64> {
    call_netd_socket_token_op_bounded(op, socket_token)
}

fn call_netd_socket_token_op_bounded(op: u16, socket_token: u64) -> Result<u64, i64> {
    drain_pending_netd_refs();
    let request = new_netd_socket_request(op, socket_token);
    let mut last = LINUX_ETIMEDOUT;
    for _ in 0..3 {
        match call_netd_ipc_request_with_timeout(&request, 16) {
            Ok(response) => {
                if send_netd_ref_ack(&request).is_err() {
                    enqueue_pending_netd_ref(PendingNetdRef {
                        op,
                        socket_token,
                        operation_hi: request.operation_hi,
                        operation_lo: request.operation_lo,
                        acknowledge_only: true,
                    })?;
                }
                return Ok(response.value);
            }
            Err(errno) if errno == LINUX_ETIMEDOUT => last = errno,
            Err(errno) => return Err(errno),
        }
    }
    enqueue_pending_netd_ref(PendingNetdRef {
        op,
        socket_token,
        operation_hi: request.operation_hi,
        operation_lo: request.operation_lo,
        acknowledge_only: false,
    })?;
    Err(last)
}

fn enqueue_pending_netd_ref(pending: PendingNetdRef) -> Result<(), i64> {
    let mut queue = PENDING_NETD_REFS.lock();
    if queue.iter().any(|entry| {
        entry.operation_hi == pending.operation_hi && entry.operation_lo == pending.operation_lo
    }) {
        return Ok(());
    }
    if queue.len() == PENDING_NETD_REF_CAPACITY {
        return Err(LINUX_ENOSPC);
    }
    queue.push_back(pending);
    Ok(())
}

fn drain_pending_netd_refs() {
    for _ in 0..8 {
        let Some(pending) = PENDING_NETD_REFS.lock().pop_front() else {
            return;
        };
        let mut request = new_netd_socket_request(pending.op, pending.socket_token);
        request.operation_hi = pending.operation_hi;
        request.operation_lo = pending.operation_lo;
        let completed = if pending.acknowledge_only {
            send_netd_ref_ack(&request).is_ok()
        } else {
            match call_netd_ipc_request_with_timeout(&request, 16) {
                Ok(_) => send_netd_ref_ack(&request).is_ok(),
                Err(errno) if errno == LINUX_EBADF => true,
                Err(_) => false,
            }
        };
        if !completed {
            PENDING_NETD_REFS.lock().push_front(pending);
            return;
        }
    }
}

fn send_netd_ref_ack(request: &NetdIpcRequest) -> Result<(), i64> {
    let mut ack = new_netd_socket_request(NETD_IPC_OP_REF_ACK, request.socket_token);
    ack.arg0 = request.op as u64;
    ack.operation_hi = request.operation_hi;
    ack.operation_lo = request.operation_lo;
    let response = call_netd_ipc_request_with_timeout(&ack, 16)?;
    if response.value != 0 || response.payload_len != 0 {
        return Err(LINUX_EINVAL);
    }
    Ok(())
}

pub fn poll_netd_socket_token(
    socket_token: u64,
    events: u32,
    timeout_ms: u64,
) -> Result<(u32, u64), i64> {
    let mut request = new_netd_socket_request(SYSCALL_OFFLOAD_OP_LINUX_POLL_SOCKET, socket_token);
    request.arg1 = events as u64;
    request.arg2 = NETD_POLL_MODE_QUERY;
    let response = call_netd_ipc_request_with_timeout(&request, timeout_ms)?;
    if response.payload_len != 8 {
        return Err(LINUX_EINVAL);
    }
    let generation =
        u64::from_le_bytes(response.payload[..8].try_into().map_err(|_| LINUX_EINVAL)?);
    if generation == 0 {
        return Err(LINUX_EINVAL);
    }
    Ok((response.value as u32, generation))
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ServiceHandleRef {
    Socket(u64),
    RemoteVfs(u64),
    Epoll(u64),
    Input(u64),
}

fn service_handle_refs_from_table(handles: &multitask::HandleTable) -> Vec<ServiceHandleRef> {
    handles
        .entries_snapshot(false)
        .into_iter()
        .filter_map(|(_, entry)| service_handle_ref_for_handle(entry.handle()))
        .collect()
}

fn service_handle_refs_from_handles(handles: &[multitask::KernelHandle]) -> Vec<ServiceHandleRef> {
    handles
        .iter()
        .filter_map(service_handle_ref_for_handle)
        .collect()
}

pub fn acquire_cloned_service_handle_refs(
    child_state: &multitask::UserProcessState,
) -> Result<Vec<ServiceHandleRef>, i64> {
    // The handles in `child_state` are the exact snapshot that fork will
    // publish. Never resnapshot the live parent after cloning: a sibling can
    // close/reuse an fd in between and redirect the provider refs to a
    // different open description than the one installed in the child.
    let refs = service_handle_refs_from_table(child_state.handles());
    let mut acquired = Vec::with_capacity(refs.len());
    for handle_ref in refs {
        if let Err(errno) = acquire_service_handle_ref(handle_ref) {
            release_service_handle_refs(&acquired);
            return Err(errno);
        }
        acquired.push(handle_ref);
    }
    Ok(acquired)
}

pub fn release_all_service_handle_refs(process_id: u64) {
    let closed = multitask::with_process_state_by_pid_mut(process_id, |state| {
        state.handles_mut().close_all()
    })
    .unwrap_or_default();
    // Derive provider releases from the exact handles removed under the
    // process-state mutation. A pre-close snapshot races dup/close/fd reuse
    // and can release the wrong service object or leak the one actually
    // removed during exit.
    let refs = service_handle_refs_from_handles(&closed);
    release_service_handle_refs_bounded(&refs);
    purge_closed_console_handles(
        closed
            .into_iter()
            .filter_map(|handle| match handle {
                multitask::KernelHandle::Console(console) => Some(console),
                _ => None,
            })
            .collect(),
        true,
    );
}

pub fn purge_closed_console_handles(handles: Vec<multitask::ConsoleHandle>, bounded: bool) {
    let mut descriptions = BTreeMap::new();
    for handle in handles {
        descriptions.insert(handle.token_id(), handle);
    }
    for (token, handle) in descriptions {
        if !handle.is_last_reference() {
            continue;
        }
        if bounded {
            let _ = purge_vfs_epoll_object_bounded(WAITSET_PROVIDER_SESSIOND, token);
        } else {
            let _ = purge_vfs_epoll_object(WAITSET_PROVIDER_SESSIOND, token);
        }
    }
}

pub fn release_service_handle_refs(refs: &[ServiceHandleRef]) {
    for handle_ref in refs.iter().copied().rev() {
        match handle_ref {
            ServiceHandleRef::Socket(token) => {
                if call_netd_socket_token_op(SYSCALL_OFFLOAD_OP_LINUX_CLOSE, token) == Ok(0) {
                    let _ = purge_vfs_epoll_object(WAITSET_PROVIDER_NETD, token);
                }
            }
            ServiceHandleRef::RemoteVfs(remote_id) => {
                if release_remote_vfs_descriptor_ref(remote_id) == Ok(true) {
                    let _ = call_vfs_remote_close(remote_id);
                    let _ = purge_vfs_epoll_object(WAITSET_PROVIDER_VFSD, remote_id);
                }
            }
            ServiceHandleRef::Epoll(token) => {
                let _ = update_vfs_epoll_ref_bounded(token, false);
            }
            ServiceHandleRef::Input(token) => {
                if super::super::broker_ops::waitset_broker_ops::release_input_open_description(
                    token,
                ) == Ok(true)
                {
                    let _ = purge_vfs_epoll_object(WAITSET_PROVIDER_INPUTD, token);
                }
            }
        }
    }
}

pub fn release_service_handle_refs_bounded(refs: &[ServiceHandleRef]) {
    for handle_ref in refs.iter().copied().rev() {
        match handle_ref {
            ServiceHandleRef::Socket(token) => {
                if call_netd_socket_token_op_bounded(SYSCALL_OFFLOAD_OP_LINUX_CLOSE, token) == Ok(0)
                {
                    let _ = purge_vfs_epoll_object_bounded(WAITSET_PROVIDER_NETD, token);
                }
            }
            ServiceHandleRef::RemoteVfs(remote_id) => {
                if release_remote_vfs_descriptor_ref(remote_id) == Ok(true) {
                    let _ = call_vfs_remote_close_bounded(remote_id);
                    let _ = purge_vfs_epoll_object_bounded(WAITSET_PROVIDER_VFSD, remote_id);
                }
            }
            ServiceHandleRef::Epoll(token) => {
                let _ = update_vfs_epoll_ref(token, false);
            }
            ServiceHandleRef::Input(token) => {
                if super::super::broker_ops::waitset_broker_ops::release_input_open_description(
                    token,
                ) == Ok(true)
                {
                    let _ = purge_vfs_epoll_object_bounded(WAITSET_PROVIDER_INPUTD, token);
                }
            }
        }
    }
}

pub fn service_handle_ref_for_handle(handle: &multitask::KernelHandle) -> Option<ServiceHandleRef> {
    match handle {
        multitask::KernelHandle::Socket(socket) => {
            Some(ServiceHandleRef::Socket(socket.token_id()))
        }
        multitask::KernelHandle::InetSocket(socket) => {
            Some(ServiceHandleRef::Socket(socket.token_id()))
        }
        multitask::KernelHandle::RemoteVfs(remote) => {
            Some(ServiceHandleRef::RemoteVfs(remote.remote_id()))
        }
        multitask::KernelHandle::Epoll(epoll) => Some(ServiceHandleRef::Epoll(epoll.token_id())),
        multitask::KernelHandle::Device(device)
            if device.device_id() == kernel_object::api::device::DeviceId::Input =>
        {
            Some(ServiceHandleRef::Input(device.token_id()))
        }
        _ => None,
    }
}

pub fn acquire_service_handle_ref(handle_ref: ServiceHandleRef) -> Result<(), i64> {
    match handle_ref {
        ServiceHandleRef::Socket(token) => {
            call_netd_socket_token_op(SYSCALL_OFFLOAD_OP_LINUX_DUP, token).map(|_| ())
        }
        ServiceHandleRef::RemoteVfs(remote_id) => acquire_remote_vfs_descriptor_ref(remote_id),
        ServiceHandleRef::Epoll(token) => update_vfs_epoll_ref(token, true),
        ServiceHandleRef::Input(token) => {
            super::super::broker_ops::waitset_broker_ops::acquire_input_open_description(token)
        }
    }
}

fn call_netd_socket_payload(fd: u64, op: u16, payload: &[u8]) -> Result<u64, i64> {
    let Some((token, status_flags)) = current_socket_token_and_flags(fd) else {
        return Err(LINUX_EBADF);
    };
    if payload.len() > NETD_IPC_PAYLOAD_CAPACITY {
        return Err(LINUX_EINVAL);
    }
    let mut request = new_netd_socket_request(op, token);
    request.arg0 = fd;
    request.status_flags = status_flags;
    request.payload_len = payload.len() as u32;
    request.payload[..payload.len()].copy_from_slice(payload);
    call_netd_ipc_request(&request).map(|response| response.value)
}

fn read_socket_writev_payload(iov_ptr: u64, iovcnt: u64) -> Result<Vec<u8>, i64> {
    let Ok(iovcnt) = usize::try_from(iovcnt) else {
        return Err(LINUX_EINVAL);
    };
    if iovcnt > 16 {
        return Err(LINUX_EINVAL);
    }
    let mut payload = Vec::new();
    for index in 0..iovcnt {
        let Some(entry_ptr) =
            iov_ptr.checked_add((index * size_of::<linux_abi::LinuxIovec>()) as u64)
        else {
            return Err(LINUX_EFAULT);
        };
        let iov = usermem::read_current_user_struct::<linux_abi::LinuxIovec>(entry_ptr)
            .map_err(address_space_error_to_linux_errno)?;
        let len = usize::try_from(iov.iov_len).map_err(|_| LINUX_EINVAL)?;
        if len == 0 {
            continue;
        }
        if payload.len().saturating_add(len) > NETD_IPC_PAYLOAD_CAPACITY {
            return Err(LINUX_EINVAL);
        }
        let start = payload.len();
        payload.resize(start + len, 0);
        usermem::copy_from_current_user_exact(iov.iov_base, &mut payload[start..])
            .map_err(address_space_error_to_linux_errno)?;
    }
    Ok(payload)
}
// RING3-MIGRATION-REFERENCE END: vfsd/netd fd-usercopy substrate exception.
