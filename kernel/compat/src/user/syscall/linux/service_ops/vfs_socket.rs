//! Linux VFS/socket descriptor operations over service-owned open descriptions.
//!
//! - **Owner:** Compat owns fd and copyin/copyout semantics; `vfsd`/`netd` own
//!   namespace, cursor, socket, and provider policy.
//! - **Boundary:** User fds, buffers, flags, addresses, and service envelopes
//!   are untrusted.
//! - **Lifecycle:** Resolve an exact open description, prepare remote mutation,
//!   copy/commit, and settle dup/fork/exec/final-close references.
//! - **Concurrency:** Descriptor-table mutation and service calls are separated;
//!   provider restart is bound by epoch and finite deadline.
//! - **Failure:** Short I/O, copy failure, timeout, restart, capacity, and
//!   close races preserve cursor/reference ownership and Linux errno.
//! - **Forbidden:** No raw fd as provider token, partial handle installation,
//!   synchronous call under table lock, or polling fallback.
//! - **Evidence:** `vfs-open-description`.
// RING3-MIGRATION-REFERENCE START: vfsd/netd fd-usercopy substrate exception.
// vfsd/netd own file/socket policy. Ring0 keeps current-process user-copy,
// fd-table mutation, and remote-handle/socket-token installation substrate.
use super::*;
use alloc::collections::BTreeMap;
use nucleus_core::util::{
    lockdep::{LockClass, TrackedSpinLock},
    ring::RingBuffer,
};
use rustos_user_abi::syscall::VFS_DEVICE_ACCESS_DRM_COMPAT;

const PENDING_NETD_REF_CAPACITY: usize = 4096;
const PENDING_NETD_REF_STORAGE_CAPACITY: usize = PENDING_NETD_REF_CAPACITY + 1;
const REMOTE_VFS_OPEN_DESCRIPTION_CAPACITY: usize = 32 * 1024;
// DUP/CLOSE/ACK change a service-owned open-description reference. They are
// persistent control mutations, not non-consuming readiness observations.
// One attempt therefore owns the complete state-mutation lease; fragmenting
// this into 5-6 ms calls lets netd commit after each reply capability is revoked.
const NETD_REF_OPERATION_TIMEOUT_MS: u64 =
    rustos_user_abi::performance::IPC_STATE_MUTATION_HARD_LIMIT_MS;
// A final CLOSE is the exception: the local descriptor is already gone and
// nobody consumes the result, so a wedged provider must not hold close()/exec
// for the whole lease. Its uncertain outcome is replayed by housekeeping,
// which does use the lease.
const NETD_REF_CLOSE_FOREGROUND_TIMEOUT_MS: u64 =
    rustos_user_abi::performance::IPC_INTERACTIVE_CONTROL_HARD_LIMIT_MS;

const fn netd_ref_foreground_timeout_ms(op: u16) -> u64 {
    if op == SYSCALL_OFFLOAD_OP_LINUX_CLOSE {
        NETD_REF_CLOSE_FOREGROUND_TIMEOUT_MS
    } else {
        NETD_REF_OPERATION_TIMEOUT_MS
    }
}
const NETD_REF_OPERATION_ATTEMPTS: usize = 3;
const NETD_MAINTENANCE_ATTEMPTS_PER_CALL: usize = 1;

#[derive(Clone, Copy)]
struct PendingNetdRef {
    op: u16,
    socket_token: u64,
    operation_hi: u64,
    operation_lo: u64,
    acknowledge_only: bool,
}

#[derive(Clone, Copy)]
struct RemoteVfsRefSlot {
    remote_id: u64,
    references: u64,
    state: u8,
}

impl RemoteVfsRefSlot {
    const EMPTY: Self = Self {
        remote_id: 0,
        references: 0,
        state: 0,
    };

    const OCCUPIED: u8 = 1;
    const TOMBSTONE: u8 = 2;
}

struct RemoteVfsRefRegistry {
    slots: [RemoteVfsRefSlot; REMOTE_VFS_OPEN_DESCRIPTION_CAPACITY],
    len: usize,
}

impl RemoteVfsRefRegistry {
    const fn new() -> Self {
        Self {
            slots: [RemoteVfsRefSlot::EMPTY; REMOTE_VFS_OPEN_DESCRIPTION_CAPACITY],
            len: 0,
        }
    }

    fn probe_start(remote_id: u64) -> usize {
        let mut value = remote_id;
        value ^= value >> 33;
        value = value.wrapping_mul(0xff51_afd7_ed55_8ccd);
        value ^= value >> 33;
        value = value.wrapping_mul(0xc4ce_b9fe_1a85_ec53);
        value ^= value >> 33;
        value as usize & (REMOTE_VFS_OPEN_DESCRIPTION_CAPACITY - 1)
    }

    fn find_index(&self, remote_id: u64) -> Option<usize> {
        let start = Self::probe_start(remote_id);
        for offset in 0..REMOTE_VFS_OPEN_DESCRIPTION_CAPACITY {
            let index = (start + offset) & (REMOTE_VFS_OPEN_DESCRIPTION_CAPACITY - 1);
            let slot = self.slots[index];
            if slot.state == 0 {
                return None;
            }
            if slot.state == RemoteVfsRefSlot::OCCUPIED && slot.remote_id == remote_id {
                return Some(index);
            }
        }
        None
    }

    fn insert(&mut self, remote_id: u64) -> Result<(), i64> {
        if self.find_index(remote_id).is_some() {
            return Err(LINUX_EEXIST);
        }
        if self.len == REMOTE_VFS_OPEN_DESCRIPTION_CAPACITY {
            return Err(LINUX_ENOSPC);
        }
        let start = Self::probe_start(remote_id);
        let mut first_tombstone = None;
        for offset in 0..REMOTE_VFS_OPEN_DESCRIPTION_CAPACITY {
            let index = (start + offset) & (REMOTE_VFS_OPEN_DESCRIPTION_CAPACITY - 1);
            let slot = self.slots[index];
            if slot.state == RemoteVfsRefSlot::OCCUPIED {
                if slot.remote_id == remote_id {
                    return Err(LINUX_EEXIST);
                }
                continue;
            }
            if slot.state == RemoteVfsRefSlot::TOMBSTONE {
                first_tombstone.get_or_insert(index);
                continue;
            }
            let target = first_tombstone.unwrap_or(index);
            self.slots[target] = RemoteVfsRefSlot {
                remote_id,
                references: 1,
                state: RemoteVfsRefSlot::OCCUPIED,
            };
            self.len += 1;
            return Ok(());
        }

        if let Some(target) = first_tombstone {
            self.slots[target] = RemoteVfsRefSlot {
                remote_id,
                references: 1,
                state: RemoteVfsRefSlot::OCCUPIED,
            };
            self.len += 1;
            return Ok(());
        }
        Err(LINUX_ENOSPC)
    }

    fn acquire(&mut self, remote_id: u64) -> Result<(), i64> {
        let index = self.find_index(remote_id).ok_or(LINUX_EBADF)?;
        self.slots[index].references = self.slots[index]
            .references
            .checked_add(1)
            .ok_or(LINUX_EOVERFLOW)?;
        Ok(())
    }

    fn release(&mut self, remote_id: u64) -> Result<bool, i64> {
        let index = self.find_index(remote_id).ok_or(LINUX_EBADF)?;
        let slot = &mut self.slots[index];
        if slot.references == 0 {
            return Err(LINUX_ESTALE);
        }
        slot.references -= 1;
        if slot.references != 0 {
            return Ok(false);
        }
        *slot = RemoteVfsRefSlot {
            state: RemoteVfsRefSlot::TOMBSTONE,
            ..RemoteVfsRefSlot::EMPTY
        };
        self.len -= 1;
        Ok(true)
    }
}

static PENDING_NETD_REFS: TrackedSpinLock<
    RingBuffer<PendingNetdRef, PENDING_NETD_REF_STORAGE_CAPACITY>,
    { LockClass::NetdDeferredRef as u8 },
> = TrackedSpinLock::new(RingBuffer::new());

/// Kernel fd tables are authoritative for descriptor references. vfsd owns
/// open-file state, but sees only initial open and final release.
static REMOTE_VFS_DESCRIPTOR_REFS: TrackedSpinLock<
    RemoteVfsRefRegistry,
    { LockClass::RemoteVfsRegistry as u8 },
> = TrackedSpinLock::new(RemoteVfsRefRegistry::new());

fn register_remote_vfs_open_description(remote_id: u64) -> Result<(), i64> {
    if remote_id == 0 {
        return Err(LINUX_EINVAL);
    }
    REMOTE_VFS_DESCRIPTOR_REFS.lock().insert(remote_id)
}

fn acquire_remote_vfs_descriptor_ref(remote_id: u64) -> Result<(), i64> {
    REMOTE_VFS_DESCRIPTOR_REFS.lock().acquire(remote_id)
}

fn release_remote_vfs_descriptor_ref(remote_id: u64) -> Result<bool, i64> {
    REMOTE_VFS_DESCRIPTOR_REFS.lock().release(remote_id)
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
    let kind = match response.handle_kind {
        VFS_IPC_HANDLE_KIND_FILE => multitask::RemoteVfsHandleKind::File,
        VFS_IPC_HANDLE_KIND_DIR => multitask::RemoteVfsHandleKind::Directory,
        VFS_IPC_HANDLE_KIND_DEVICE => multitask::RemoteVfsHandleKind::Device,
        _ => {
            let _ = call_vfs_remote_close_bounded(remote_id);
            return linux_errno(LINUX_EINVAL);
        }
    };
    let handle_path = match response
        .payload
        .get(..response.payload_len as usize)
        .map(core::str::from_utf8)
    {
        Some(Ok(path)) if !path.is_empty() && path.starts_with('/') => {
            alloc::string::String::from(path)
        }
        _ => {
            let _ = call_vfs_remote_close_bounded(remote_id);
            return linux_errno(LINUX_EINVAL);
        }
    };
    // vfsd owns namespace resolution and declares whether the resolved node is
    // a device. Compat performs only the handle-transfer substrate after that
    // service decision; it must not classify caller path strings in ring0.
    if kind == multitask::RemoteVfsHandleKind::Device
        && response.aux != u64::from(VFS_DEVICE_ACCESS_DRM_COMPAT)
    {
        let _ = call_vfs_remote_close_bounded(remote_id);
        return match open_device_via_devmgrd(handle_path.as_str(), flags) {
            Ok(fd) => fd,
            Err(errno) => linux_errno(errno),
        };
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
    let Some(remote_handle) = multitask::RemoteVfsHandle::new(
        remote_id,
        kind,
        handle_path,
        response.value,
        response.aux as u16,
    ) else {
        release_service_handle_refs_bounded(&[ServiceHandleRef::RemoteVfs(remote_id)]);
        return linux_errno(LINUX_EOVERFLOW);
    };
    let handle = multitask::KernelHandle::RemoteVfs(remote_handle);
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
    // Status flags and pipe sizing live on the kernel-local open description.
    if let Some(local) = super::local_fd::current_local_fd(fd)
        && let Some(result) = super::local_fd::fcntl(&local, cmd, arg)
    {
        return result;
    }
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
            if matches!(cmd, linux_abi::F_GETFL | linux_abi::F_SETFL)
                && let Some(remote) = current_remote_vfs_handle(fd)
            {
                let mut request = new_vfs_request(VFS_IPC_OP_FCNTL);
                request.fd = fd;
                request.remote_id = remote.remote_id();
                request.arg0 = cmd;
                request.arg1 = arg;
                let value = match call_pinned_remote_vfs_request(&request).and_then(|response| {
                    ensure_vfs_status(&response)?;
                    Ok(response.value)
                }) {
                    Ok(value) => value,
                    Err(errno) => return linux_errno(errno),
                };
                if cmd == linux_abi::F_SETFL {
                    let _ =
                        multitask::with_current_user_process_state_mut(|_, _, process_state| {
                            if let Some(entry) = process_state.handles_mut().get_entry_mut(fd) {
                                entry.set_status_flags(value);
                            }
                        });
                    return 0;
                }
                return value;
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
    if let Some(handle_ref) = source_ref.as_ref() {
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
            if let Some(handle_ref) = source_ref.as_ref() {
                release_service_handle_refs_bounded(core::slice::from_ref(handle_ref));
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
mod tests;

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
        let nonblocking = status_flags & linux_abi::O_NONBLOCK != 0;
        return match crate::user::sysops::console::read_into_current_process(
            user_ptr,
            user_len,
            nonblocking,
        ) {
            Ok(0) if nonblocking => linux_errno(LINUX_EAGAIN),
            Ok(read) => read as u64,
            Err(err) => linux_errno(address_space_error_to_linux_errno(err)),
        };
    }
    if let Some(result) = super::local_fd::read(fd, user_ptr, user_len) {
        return result;
    }
    if current_socket_fd(fd) {
        // read(fd, ..., 0) is non-consuming even on a datagram socket.
        return socket_read_with_length(user_len, || super::vfs_meta::syscall_linux_net6(
            SYSCALL_OFFLOAD_OP_LINUX_RECVFROM, fd, user_ptr, user_len, 0, 0, 0,
        ));
    }
    if let Some(mut memfd) = current_memfd_handle(fd) {
        return super::local_memfd_io::read(&mut memfd, user_ptr, user_len);
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
    if let Some(result) = super::local_fd::not_seekable(fd) {
        return result;
    }
    if let Some(memfd) = current_memfd_handle(fd) {
        let Ok(offset) = usize::try_from(offset) else {
            return linux_errno(LINUX_EINVAL);
        };
        return super::local_memfd_io::read_at(&memfd, user_ptr, user_len, offset);
    }
    if let Some(multitask::KernelHandle::DeviceBuffer(buffer)) = current_kernel_handle(fd) {
        return super::local_memfd_io::read_device_buffer_at(&buffer, user_ptr, user_len, offset);
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
    if let Some(result) = super::local_fd::write(fd, user_ptr, user_len) {
        return result;
    }
    if current_socket_fd(fd) {
        // A compositor writing events to a client whose receive buffer is full
        // is the textbook Wayland head-of-line block - one slow client stops
        // every other client and the compositor with it, which is what
        // `V5-WAYLAND-HOL-013` names. The bound that prevents it is applied in
        // `syscall_linux_net6_with_timeout` for every socket data operation,
        // not just this one.
        return super::vfs_meta::syscall_linux_net6(
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
        return super::local_memfd_io::write(&mut memfd, user_ptr, user_len);
    }
    let Some(remote) = current_remote_vfs_handle(fd) else {
        return linux_errno(LINUX_EBADF);
    };
    write_remote_vfs(fd, remote.remote_id(), user_ptr, user_len)
}

pub fn syscall_linux_vfs_writev(fd: u64, iov_ptr: u64, iovcnt: u64) -> u64 {
    if let Some(snapshot) = super::vfs_meta::snapshot_socket(fd) {
        let bytes = match read_socket_writev_payload(iov_ptr, iovcnt, snapshot.is_datagram()) {
            Ok(bytes) => bytes,
            Err(errno) => return linux_errno(errno),
        };
        if snapshot.is_datagram() && iovcnt != 0 {
            return match call_netd_socket_payload(&snapshot, fd, SYSCALL_OFFLOAD_OP_LINUX_SENDTO, &bytes) {
                Ok(sent) if sent == bytes.len() as u64 => sent,
                Ok(_) => linux_errno(LINUX_EIO),
                Err(errno) => linux_errno(errno),
            };
        }
        if bytes.is_empty() {
            return 0;
        }
        let mut written = 0usize;
        while written < bytes.len() {
            let chunk = &bytes[written..];
            let count = match call_netd_socket_payload(&snapshot, fd, SYSCALL_OFFLOAD_OP_LINUX_SENDTO, chunk) {
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
    // Foreground final close: the fd is already retired and nobody consumes the
    // result, so keep the short rail; a timeout is replayed by housekeeping
    // under the state-mutation lease.
    call_vfs_ipc_request_with_timeout(&request,
        rustos_user_abi::performance::IPC_INTERACTIVE_CONTROL_HARD_LIMIT_MS)
        .and_then(|response| ensure_vfs_status(&response))
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
    // A cursor read spans prepare, user copy and settle; hold the description
    // so no other thread or unsettled mutation interleaves a cursor operation.
    let _cursor = if offset.is_none() {
        match acquire_vfs_cursor_gate(remote_id) {
            Ok(gate) => Some(gate),
            Err(errno) => return linux_errno(errno),
        }
    } else { None };
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
            if offset.is_none()
                && let Err(errno) = settle_vfs_cursor_mutation(&request, true)
            {
                return if copied > 0 {
                    copied as u64
                } else {
                    linux_errno(errno)
                };
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
        if offset.is_none()
            && let Err(errno) = settle_vfs_cursor_mutation(&request, true)
        {
            return if copied > 0 {
                copied as u64
            } else {
                linux_errno(errno)
            };
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
        let _cursor = match acquire_vfs_cursor_gate(remote_id) {
            Ok(gate) => gate,
            Err(errno) => return linux_errno(errno),
        };
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
            // A provider cannot have accepted more than it was offered.
            if response.value > chunk_len as u64 {
                return Err(LINUX_EINVAL);
            }
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

fn new_netd_socket_request(op: u16, socket_token: u64, deadline_ns: u64) -> NetdIpcRequest {
    let mut operation = [0_u8; 16];
    nucleus_core::util::random::Random::new().fill_bytes(&mut operation);
    let mut request = NetdIpcRequest {
        version: NETD_IPC_ABI_VERSION,
        op,
        socket_token,
        deadline_ns,
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
    let deadline_ns = super::vfs_meta::netd_deadline_after_class(
        ipc_ops::ServiceIpcClass::StateMutation,
        Some(netd_ref_foreground_timeout_ms(op)),
    );
    let request = new_netd_socket_request(op, socket_token, deadline_ns);
    let mut last = LINUX_ETIMEDOUT;
    for attempt in 0..NETD_REF_OPERATION_ATTEMPTS {
        let Some(timeout_ms) = super::vfs_meta::netd_deadline_remaining_ms(deadline_ns) else {
            break;
        };
        match call_netd_ipc_request_with_timeout(&request, timeout_ms) {
            Ok(response) => {
                // The provider operation is already committed. Acknowledgement
                // is maintenance and must not add another full IPC deadline to
                // close/dup latency; preserve it as an exact replayable item.
                // The provider operation is committed and the local descriptor
                // is already released, so a full replay queue is a maintenance
                // backlog and not the caller's failure. Propagating `ENOSPC`
                // here makes `close` fail, which POSIX does not allow to leave
                // the descriptor open and which callers treat as fatal:
                // `wayland-server` dropped a live client on exactly this,
                // retiring WayClick's surface mid-session.
                if enqueue_pending_netd_ref(PendingNetdRef {
                    op,
                    socket_token,
                    operation_hi: request.operation_hi,
                    operation_lo: request.operation_lo,
                    acknowledge_only: true,
                })
                .is_err()
                {
                    crate::debug::record_milestone(
                        crate::debug::LogCategory::Service,
                        "netd-ref-ack-dropped",
                        socket_token,
                        u64::from(op),
                    );
                }
                return Ok(response.value);
            }
            Err(errno)
                if attempt + 1 < NETD_REF_OPERATION_ATTEMPTS
                    && super::ipc_helpers::deadline::retryable_early_service_transport_error(
                        errno,
                    ) =>
            {
                last = errno;
                multitask::cond_resched();
            }
            Err(errno) => return Err(errno),
        }
    }
    // A full replay queue must not replace the transport error that actually
    // ended the retries. `last` is what the caller can act on; `ENOSPC` from
    // the queue is an internal capacity fact the caller cannot distinguish
    // from a terminal condition.
    if enqueue_pending_netd_ref(PendingNetdRef {
        op,
        socket_token,
        operation_hi: request.operation_hi,
        operation_lo: request.operation_lo,
        acknowledge_only: false,
    })
    .is_err()
    {
        crate::debug::record_milestone(
            crate::debug::LogCategory::Service,
            "netd-ref-replay-dropped",
            socket_token,
            u64::from(op),
        );
    }
    Err(last)
}

fn enqueue_pending_netd_ref(pending: PendingNetdRef) -> Result<(), i64> {
    let mut queue = PENDING_NETD_REFS.lock();
    if queue.any(|entry| {
        entry.operation_hi == pending.operation_hi && entry.operation_lo == pending.operation_lo
    }) {
        return Ok(());
    }
    if queue.len() >= PENDING_NETD_REF_CAPACITY || !queue.push(pending) {
        return Err(LINUX_ENOSPC);
    }
    Ok(())
}

pub(super) fn service_deferred_netd_refs() -> usize {
    drain_pending_netd_refs()
}

/// Releases the single-drainer claim on every exit path.
struct NetdRefDrainClaim;

impl Drop for NetdRefDrainClaim {
    fn drop(&mut self) {
        // ORDERING: release publishes the drainer's queue mutations to the
        // next claimant, which acquires the flag before touching the queue.
        NETD_REF_DRAINING.store(false, core::sync::atomic::Ordering::Release);
    }
}

/// At most one drainer owns a popped replay item at a time. The ring keeps one
/// spare slot beyond the enqueue cap precisely so a single popped item can be
/// pushed back to the front after producers refilled the queue; a second
/// concurrent drainer would break that bound and the push-back assertions.
static NETD_REF_DRAINING: core::sync::atomic::AtomicBool =
    core::sync::atomic::AtomicBool::new(false);

fn drain_pending_netd_refs() -> usize {
    // ORDERING: acquire pairs with the claim release above, so the next
    // drainer observes the queue exactly as the previous one left it.
    if NETD_REF_DRAINING
        .compare_exchange(
            false,
            true,
            // ORDERING: acquire pairs with the drop-time release above.
            core::sync::atomic::Ordering::Acquire,
            // ORDERING: a lost claim touches nothing the winner published and
            // returns immediately, so the failure load needs no ordering.
            core::sync::atomic::Ordering::Relaxed,
        )
        .is_err()
    {
        return 0;
    }
    let _claim = NetdRefDrainClaim;
    let mut attempted = 0usize;
    for _ in 0..NETD_MAINTENANCE_ATTEMPTS_PER_CALL {
        let Some(pending) = PENDING_NETD_REFS.lock().pop() else {
            return attempted;
        };
        attempted += 1;
        let mut request = new_netd_socket_request(
            pending.op,
            pending.socket_token,
            super::vfs_meta::netd_deadline_after_class(
                ipc_ops::ServiceIpcClass::StateMutation,
                Some(rustos_user_abi::performance::IPC_STATE_MUTATION_HARD_LIMIT_MS),
            ),
        );
        request.operation_hi = pending.operation_hi;
        request.operation_lo = pending.operation_lo;
        let completed = if pending.acknowledge_only {
            send_netd_ref_ack_with_timeout(
                &request,
                rustos_user_abi::performance::IPC_STATE_MUTATION_HARD_LIMIT_MS,
            )
            .is_ok()
        } else {
            match call_netd_ipc_request_with_timeout(
                &request,
                rustos_user_abi::performance::IPC_STATE_MUTATION_HARD_LIMIT_MS,
            ) {
                Ok(_) => {
                    // Split operation replay and its acknowledgement across
                    // yielded housekeeping turns. This is not charged to a
                    // foreground syscall and must outlive a normal SMP IPC
                    // round trip rather than cancel/retry it every 1 ms.
                    let mut acknowledge = pending;
                    acknowledge.acknowledge_only = true;
                    assert!(
                        PENDING_NETD_REFS.lock().push_front(acknowledge),
                        "popped deferred netd reference must have retry capacity"
                    );
                    return attempted;
                }
                Err(errno) if errno == LINUX_EBADF => true,
                Err(_) => false,
            }
        };
        if !completed {
            assert!(
                PENDING_NETD_REFS.lock().push_front(pending),
                "popped deferred netd reference must have retry capacity"
            );
            return attempted;
        }
    }
    attempted
}

fn send_netd_ref_ack_with_timeout(request: &NetdIpcRequest, timeout_ms: u64) -> Result<(), i64> {
    let mut ack = new_netd_socket_request(
        NETD_IPC_OP_REF_ACK,
        request.socket_token,
        super::vfs_meta::netd_deadline_after_class(
            ipc_ops::ServiceIpcClass::StateMutation,
            Some(timeout_ms),
        ),
    );
    ack.arg0 = request.op as u64;
    ack.operation_hi = request.operation_hi;
    ack.operation_lo = request.operation_lo;
    let response = call_netd_ipc_request_with_timeout(&ack, timeout_ms)?;
    if response.value != 0 || response.payload_len != 0 {
        return Err(LINUX_EINVAL);
    }
    Ok(())
}

/// One socket's answer inside a batched netd readiness query.
pub type NetdPollOutcome = Result<(u32, u64), i64>;

/// Evaluate every `(socket_token, events)` pair with one netd round trip.
///
/// The transport result is `Err` only when the exchange itself failed (timeout,
/// revoked endpoint, malformed reply); a single socket that netd could not
/// evaluate is that entry's `Err(errno)` and never fails its neighbours. A
/// successful entry carries `(revents, generation)` with a non-zero generation.
pub fn poll_netd_socket_batch(
    queries: &[(u64, u32)],
    timeout_ms: u64,
) -> Result<Vec<NetdPollOutcome>, i64> {
    const QUERY: usize = size_of::<NetdPollQueryWire>();
    const RESULT: usize = size_of::<NetdPollResultWire>();
    if queries.is_empty() {
        return Ok(Vec::new());
    }
    if queries.len() > NETD_POLL_BATCH_MAX {
        return Err(LINUX_EINVAL);
    }
    let mut request = new_netd_socket_request(
        NETD_IPC_OP_POLL_BATCH,
        0,
        super::vfs_meta::netd_deadline_after_class(
            ipc_ops::ServiceIpcClass::ReadinessQuery,
            Some(timeout_ms),
        ),
    );
    for (index, (token, events)) in queries.iter().copied().enumerate() {
        let offset = index * QUERY;
        request.payload[offset..offset + 8].copy_from_slice(&token.to_le_bytes());
        request.payload[offset + 8..offset + 12].copy_from_slice(&events.to_le_bytes());
        request.payload[offset + 12..offset + 16].copy_from_slice(&0_u32.to_le_bytes());
    }
    request.payload_len = (queries.len() * QUERY) as u32;
    let response = call_netd_ipc_request_with_timeout(&request, timeout_ms)?;
    if response.value != queries.len() as u64
        || response.payload_len as usize != queries.len() * RESULT
    {
        return Err(LINUX_EINVAL);
    }
    let mut outcomes = Vec::with_capacity(queries.len());
    for index in 0..queries.len() {
        let bytes = &response.payload[index * RESULT..(index + 1) * RESULT];
        let generation = u64::from_le_bytes(bytes[0..8].try_into().map_err(|_| LINUX_EINVAL)?);
        let revents = u32::from_le_bytes(bytes[8..12].try_into().map_err(|_| LINUX_EINVAL)?);
        let status = i32::from_le_bytes(bytes[12..16].try_into().map_err(|_| LINUX_EINVAL)?);
        outcomes.push(match status {
            0 if generation != 0 => Ok((revents, generation)),
            0 => return Err(LINUX_EINVAL),
            errno if errno > 0 => Err(i64::from(errno)),
            _ => return Err(LINUX_EINVAL),
        });
    }
    Ok(outcomes)
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ServiceHandleRef {
    Socket(u64),
    RemoteVfs(u64),
    Epoll(multitask::EpollHandle),
    Input(u64),
    Pipe(multitask::PipeHandle),
    EventFd(multitask::EventFdHandle),
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
        if let Err(errno) = acquire_service_handle_ref(&handle_ref) {
            release_service_handle_refs(&acquired);
            return Err(errno);
        }
        acquired.push(handle_ref);
    }
    Ok(acquired)
}

pub fn release_all_service_handle_refs(process_id: u64) {
    // Callers have already sealed the process as exiting, which revokes its
    // visibility; the teardown accessor is the one path that still reaches the
    // handle table, so every open-description reference is released exactly
    // once here instead of leaking with the process object.
    let closed = multitask::with_process_state_for_teardown_mut(process_id, |state| {
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
    for handle_ref in refs.iter().rev() {
        match handle_ref {
            ServiceHandleRef::Socket(token) => {
                if call_netd_socket_token_op(SYSCALL_OFFLOAD_OP_LINUX_CLOSE, *token) == Ok(0) {
                    let _ = purge_vfs_epoll_object(WAITSET_PROVIDER_NETD, *token);
                }
            }
            ServiceHandleRef::RemoteVfs(remote_id) => {
                if release_remote_vfs_descriptor_ref(*remote_id) == Ok(true) {
                    let _ = call_vfs_remote_close(*remote_id);
                    let _ = purge_vfs_epoll_object(WAITSET_PROVIDER_VFSD, *remote_id);
                }
            }
            ServiceHandleRef::Epoll(epoll) => {
                if epoll.release_descriptor_reference() {
                    let _ = retire_vfs_epoll_bounded(epoll.token_id());
                }
            }
            ServiceHandleRef::Input(token) => {
                if super::super::broker_ops::waitset_broker_ops::release_input_open_description(
                    *token,
                ) == Ok(true)
                {
                    let _ = purge_vfs_epoll_object(WAITSET_PROVIDER_INPUTD, *token);
                }
            }
            ServiceHandleRef::Pipe(pipe) => super::local_fd::release_pipe(pipe, false),
            ServiceHandleRef::EventFd(eventfd) => super::local_fd::release_eventfd(eventfd, false),
        }
    }
}

pub fn release_service_handle_refs_bounded(refs: &[ServiceHandleRef]) {
    for handle_ref in refs.iter().rev() {
        match handle_ref {
            ServiceHandleRef::Socket(token) => {
                if call_netd_socket_token_op_bounded(SYSCALL_OFFLOAD_OP_LINUX_CLOSE, *token)
                    == Ok(0)
                {
                    let _ = purge_vfs_epoll_object_bounded(WAITSET_PROVIDER_NETD, *token);
                }
            }
            ServiceHandleRef::RemoteVfs(remote_id) => {
                if release_remote_vfs_descriptor_ref(*remote_id) == Ok(true) {
                    let _ = call_vfs_remote_close_bounded(*remote_id);
                    let _ = purge_vfs_epoll_object_bounded(WAITSET_PROVIDER_VFSD, *remote_id);
                }
            }
            ServiceHandleRef::Epoll(epoll) => {
                if epoll.release_descriptor_reference() {
                    let _ = retire_vfs_epoll(epoll.token_id());
                }
            }
            ServiceHandleRef::Input(token) => {
                if super::super::broker_ops::waitset_broker_ops::release_input_open_description(
                    *token,
                ) == Ok(true)
                {
                    let _ = purge_vfs_epoll_object_bounded(WAITSET_PROVIDER_INPUTD, *token);
                }
            }
            ServiceHandleRef::Pipe(pipe) => super::local_fd::release_pipe(pipe, true),
            ServiceHandleRef::EventFd(eventfd) => super::local_fd::release_eventfd(eventfd, true),
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
        multitask::KernelHandle::Epoll(epoll) => Some(ServiceHandleRef::Epoll(epoll.clone())),
        multitask::KernelHandle::Pipe(pipe) => Some(ServiceHandleRef::Pipe(pipe.clone())),
        multitask::KernelHandle::EventFd(eventfd) => {
            Some(ServiceHandleRef::EventFd(eventfd.clone()))
        }
        multitask::KernelHandle::Device(device)
            if device.device_id() == kernel_object::api::device::DeviceId::Input =>
        {
            Some(ServiceHandleRef::Input(device.token_id()))
        }
        _ => None,
    }
}

pub fn acquire_service_handle_ref(handle_ref: &ServiceHandleRef) -> Result<(), i64> {
    match handle_ref {
        ServiceHandleRef::Socket(token) => {
            call_netd_socket_token_op(SYSCALL_OFFLOAD_OP_LINUX_DUP, *token).map(|_| ())
        }
        ServiceHandleRef::RemoteVfs(remote_id) => acquire_remote_vfs_descriptor_ref(*remote_id),
        ServiceHandleRef::Epoll(epoll) => epoll
            .try_acquire_descriptor_reference()
            .then_some(())
            .ok_or(LINUX_EBADF),
        ServiceHandleRef::Input(token) => {
            super::super::broker_ops::waitset_broker_ops::acquire_input_open_description(*token)
        }
        ServiceHandleRef::Pipe(pipe) => pipe
            .try_acquire_descriptor_reference()
            .then_some(())
            .ok_or(LINUX_EBADF),
        ServiceHandleRef::EventFd(eventfd) => eventfd
            .try_acquire_descriptor_reference()
            .then_some(())
            .ok_or(LINUX_EBADF),
    }
}

fn call_netd_socket_payload(snapshot: &super::vfs_meta::SocketSnapshot, fd: u64, op: u16, payload: &[u8]) -> Result<u64, i64> {
    let token = snapshot.token();
    let status_flags = snapshot.status_flags;
    if payload.len() > NETD_IPC_PAYLOAD_CAPACITY {
        return Err(LINUX_EINVAL);
    }
    let timeout = (status_flags & linux_abi::O_NONBLOCK != 0)
        .then_some(rustos_user_abi::performance::IPC_INTERACTIVE_CONTROL_HARD_LIMIT_MS);
    let class = timeout.map_or(ipc_ops::ServiceIpcClass::BulkData, |_| super::ipc_helpers::deadline::netd_timeout_class(op));
    let mut request = new_netd_socket_request(
        op, token, super::vfs_meta::netd_deadline_after_class(class, timeout),
    );
    request.arg0 = fd;
    request.status_flags = status_flags;
    populate_writev_socket_payload(&mut request, snapshot, payload)?;
    // Plain Unix-stream writev bytes must advance the same cursor as sendmsg,
    // or later SCM_RIGHTS tickets become bound to the wrong byte offsets.
    let stream = match &snapshot.handle {
        multitask::KernelHandle::Socket(socket) => Some(socket.begin_stream_send(payload.len()).ok_or(LINUX_EAGAIN)?),
        _ => None,
    };
    let response = if status_flags & linux_abi::O_NONBLOCK != 0 {
        call_netd_ipc_request_with_timeout(&request, rustos_user_abi::performance::IPC_INTERACTIVE_CONTROL_HARD_LIMIT_MS)
            .map_err(|errno| socket_writev_errno(errno, true))?
    } else {
        call_netd_ipc_request(&request)?
    };
    if response.value > payload.len() as u64 { return Err(LINUX_EIO); }
    if let Some(stream) = stream {
        if !stream.commit_send(response.value as usize) { return Err(LINUX_ESTALE); }
    }
    Ok(response.value)
}

fn socket_writev_errno(errno: i64, nonblocking: bool) -> i64 {
    if nonblocking && errno == LINUX_ETIMEDOUT { LINUX_EAGAIN } else { errno }
}

fn socket_read_with_length(len: u64, read: impl FnOnce() -> u64) -> u64 {
    if len == 0 { 0 } else { read() }
}

fn populate_writev_socket_payload(request: &mut NetdIpcRequest, snapshot: &super::vfs_meta::SocketSnapshot, payload: &[u8]) -> Result<(), i64> {
    if snapshot.is_datagram() {
        request.payload_len = rustos_user_abi::syscall::netd_message::encode(&mut request.payload, payload, &[], 0, &[])
            .ok_or(LINUX_EMSGSIZE)? as u32;
    } else {
        request.payload.get_mut(..payload.len()).ok_or(LINUX_EMSGSIZE)?.copy_from_slice(payload);
        request.payload_len = payload.len() as u32;
    }
    Ok(())
}

#[cfg(test)]
mod datagram_regression_tests {
    use super::*;
    use rustos_user_abi::syscall::netd_message;

    #[test]
    fn nonblocking_writev_timeout_is_retryable_but_blocking_timeout_is_preserved() {
        assert_eq!(socket_writev_errno(LINUX_ETIMEDOUT, true), LINUX_EAGAIN);
        assert_eq!(socket_writev_errno(LINUX_ETIMEDOUT, false), LINUX_ETIMEDOUT);
        assert_eq!(socket_writev_errno(LINUX_EIO, true), LINUX_EIO);
    }

    #[test]
    fn zero_length_read_does_not_dispatch_or_consume_a_queued_packet() {
        let mut queued = true;
        assert_eq!(socket_read_with_length(0, || { queued = false; 9 }), 0);
        assert!(queued);
        assert_eq!(socket_read_with_length(1, || { queued = false; 1 }), 1);
        assert!(!queued);
    }

    #[test]
    fn writev_data_cannot_inject_a_datagram_envelope_and_empty_payloads_are_messages() {
        let snapshot = super::super::vfs_meta::SocketSnapshot {
            handle: multitask::KernelHandle::InetSocket(multitask::InetSocketHandle::from_token(9, linux_abi::AF_INET, linux_abi::SOCK_DGRAM, 17)), status_flags: 0,
        };
        let mut injected = [0; 64];
        let len = netd_message::encode(&mut injected, b"query", &[], 0, &[7; 16]).unwrap();
        for payload in [&injected[..len], &b""[..]] {
            let mut request = NetdIpcRequest::default();
            populate_writev_socket_payload(&mut request, &snapshot, payload).unwrap();
            let message = netd_message::decode(&request.payload[..request.payload_len as usize]).unwrap();
            assert_eq!(message.data, payload);
            assert!(message.address.is_empty());
        }
    }
}

fn read_socket_writev_payload(iov_ptr: u64, iovcnt: u64, datagram: bool) -> Result<Vec<u8>, i64> {
    if iovcnt == 0 { return Ok(Vec::new()); }
    let iovecs = super::vfs_meta::read_current_iovecs_payload(iov_ptr, iovcnt)?;
    let mut total = 0_usize;
    for iov in &iovecs {
        total = total.checked_add(usize::try_from(iov.iov_len).map_err(|_| LINUX_EINVAL)?)
            .filter(|total| *total <= isize::MAX as usize).ok_or(LINUX_EINVAL)?;
    }
    let capacity = NETD_IPC_PAYLOAD_CAPACITY - if datagram {
        rustos_user_abi::syscall::netd_message::HEADER_SIZE
    } else { 0 };
    if datagram && total > capacity { return Err(LINUX_EMSGSIZE); }
    // Streams may return a bounded prefix; datagrams are never split.
    let mut payload = Vec::with_capacity(total.min(capacity));
    for iov in iovecs {
        let len = (iov.iov_len as usize).min(capacity - payload.len());
        if len != 0 {
            let start = payload.len();
            payload.resize(start + len, 0);
            usermem::copy_from_current_user_exact(iov.iov_base, &mut payload[start..])
                .map_err(address_space_error_to_linux_errno)?;
        }
        if payload.len() == capacity { break; }
    }
    Ok(payload)
}
// RING3-MIGRATION-REFERENCE END: vfsd/netd fd-usercopy substrate exception.
