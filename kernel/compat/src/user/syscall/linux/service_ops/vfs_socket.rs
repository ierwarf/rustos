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
use rustos_user_abi::syscall::{
    VFS_DEVICE_ACCESS_DRM_COMPAT, VFS_IPC_HANDLE_KIND_INOTIFY, VFS_IPC_OP_CHMOD,
    VFS_IPC_OP_CHOWN, VFS_IPC_OP_FSYNC, VFS_IPC_OP_INOTIFY_ADD_WATCH, VFS_IPC_OP_INOTIFY_INIT,
    VFS_IPC_OP_INOTIFY_RM_WATCH, VFS_IPC_OP_LINKAT, VFS_IPC_OP_MKNOD, VFS_IPC_OP_PWRITE64,
    VFS_IPC_OP_RENAMEAT2, VFS_IPC_OP_SET_ATTRS, VFS_IPC_OP_SYMLINKAT,
    VFS_LINKAT_FLAG_SOURCE_DESCRIPTION,
};

const VFS_DEVICE_ACCESS_NULL: u16 = VFS_DEVICE_ACCESS_DRM_COMPAT + 1;
const VFS_DEVICE_ACCESS_URANDOM: u16 = VFS_DEVICE_ACCESS_DRM_COMPAT + 2;
const VFS_DEVICE_ACCESS_ZERO: u16 = VFS_DEVICE_ACCESS_DRM_COMPAT + 3;
const VFS_DEVICE_ACCESS_FULL: u16 = VFS_DEVICE_ACCESS_DRM_COMPAT + 4;

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

fn call_remote_vfs_open_and_check(
    request: &VfsIpcRequest,
    minted_id: u64,
) -> LinuxResult<alloc::boxed::Box<VfsIpcResponse>> {
    match call_vfs_checked(request) {
        Ok(response) => Ok(response),
        Err(errno) => {
            let _ = call_vfs_remote_close_bounded(minted_id);
            Err(errno)
        }
    }
}

pub fn syscall_linux_vfs_openat(dirfd: u64, path_ptr: u64, flags: u64, mode: u64) -> u64 {
    linux_result((|| {
        let path = copy_current_user_path(path_ptr, VFS_IPC_PATH_CAPACITY)?;
        syscall_linux_vfs_openat_str(dirfd, path.as_str(), flags, mode)
    })())
}

pub fn syscall_linux_vfs_openat_str(
    dirfd: u64,
    path: &str,
    flags: u64,
    mode: u64,
) -> Result<u64, i64> {
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
    populate_vfs_path_base(&mut request, dirfd, path)?;
    let response = call_remote_vfs_open_and_check(&request, request.arg3)?;
    // vfsd creates no description for a kernel-fact `/proc/<pid>` node; the
    // delegation in `aux` makes this front-end build the snapshot itself.
    if let Some(result) = proc_identity::complete_open(response.aux, flags) {
        return result;
    }
    install_remote_vfs_response(response, request.arg3, flags, true)
}

fn install_remote_vfs_response(
    response: alloc::boxed::Box<VfsIpcResponse>,
    expected_id: u64,
    flags: u64,
    allow_device_handoff: bool,
) -> Result<u64, i64> {
    let remote_id = response.remote_id;
    if remote_id == 0 || remote_id != expected_id {
        let _ = call_vfs_remote_close_bounded(expected_id);
        return Err(LINUX_EINVAL);
    }
    let kind = match response.handle_kind {
        VFS_IPC_HANDLE_KIND_FILE => multitask::RemoteVfsHandleKind::File,
        VFS_IPC_HANDLE_KIND_DIR => multitask::RemoteVfsHandleKind::Directory,
        VFS_IPC_HANDLE_KIND_DEVICE => multitask::RemoteVfsHandleKind::Device,
        VFS_IPC_HANDLE_KIND_INOTIFY => multitask::RemoteVfsHandleKind::Inotify,
        _ => {
            let _ = call_vfs_remote_close_bounded(remote_id);
            return Err(LINUX_EINVAL);
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
            return Err(LINUX_EINVAL);
        }
    };
    // vfsd owns namespace resolution and declares whether the resolved node is
    // a device. Compat performs only the handle-transfer substrate after that
    // service decision; it must not classify caller path strings in ring0.
    if kind == multitask::RemoteVfsHandleKind::Device
        && !matches!(
            response.aux as u16,
            VFS_DEVICE_ACCESS_DRM_COMPAT
                | VFS_DEVICE_ACCESS_NULL
                | VFS_DEVICE_ACCESS_URANDOM
                | VFS_DEVICE_ACCESS_ZERO
                | VFS_DEVICE_ACCESS_FULL
        )
    {
        let _ = call_vfs_remote_close_bounded(remote_id);
        return if allow_device_handoff {
            open_device_via_devmgrd(handle_path.as_str(), flags)
        } else {
            Err(LINUX_EOPNOTSUPP)
        };
    }
    if let Err(errno) = register_remote_vfs_open_description(remote_id) {
        // EEXIST is a capability collision and must not close the already
        // tracked description. Other local admission failures retire the new
        // provider object so they cannot leak it.
        if errno != LINUX_EEXIST {
            let _ = call_vfs_remote_close_bounded(remote_id);
        }
        return Err(errno);
    }
    let registered = RemoteVfsRefGuard::from_registered(remote_id);
    let vfsd_epoch = ipc_ops::service_endpoint_epoch(IPC_SERVICE_VFSD).unwrap_or(0);
    let remote_handle = multitask::RemoteVfsHandle::new_with_epoch(
        remote_id,
        kind,
        handle_path,
        response.value,
        response.aux as u16,
        vfsd_epoch,
    )
    .ok_or(LINUX_EOVERFLOW)?;
    let handle = multitask::KernelHandle::RemoteVfs(remote_handle);
    let installed = multitask::with_current_user_process_state_mut(|_, _, process_state| {
        process_state
            .handles_mut()
            .install_with_open_flags(handle, flags)
    });
    match installed {
        Some(Some(fd)) => {
            registered.commit();
            Ok(fd)
        }
        Some(None) => Err(LINUX_EMFILE),
        None => Err(LINUX_EINVAL),
    }
}

/// Reopen a pinned provider object, never its recorded pathname.
pub(super) fn reopen_remote_vfs(fd: u64, remote_id: u64, flags: u64) -> Result<u64, i64> {
    let new_id = mint_service_object_id();
    let mut request = new_vfs_request(VFS_IPC_OP_FCNTL);
    request.fd = fd;
    request.remote_id = remote_id;
    request.arg0 = rustos_user_abi::syscall::VFS_FCNTL_REOPEN;
    request.arg1 = flags;
    request.arg2 = new_id;
    let response = call_remote_vfs_open_and_check(&request, new_id)?;
    install_remote_vfs_response(response, new_id, flags, false)
}

pub fn syscall_linux_vfs_close(fd: u64) -> u64 {
    linux_unit_result((|| {
        let (closed_handle, release_now) =
            multitask::with_current_user_process_state_mut(|_, _, process_state| {
                process_state.close_handle_respecting_fork_pin(fd)
            })
            .flatten()
            .ok_or(LINUX_EBADF)?;
        // POSIX record locks go with any close of the file, before the description
        // reference (which may be the last) is dropped.
        super::super::file_lock_ops::note_fd_closed(&closed_handle);
        if !release_now {
            return Ok(());
        }
        if let multitask::KernelHandle::RemoteVfs(remote) = &closed_handle {
            let id = remote.remote_id();
            if release_remote_vfs_descriptor_ref(id)? {
                // Path-backed files can be renamed only after vfsd retires
                // the old description. Unlike background exit cleanup, the
                // explicit close must acknowledge that mutation to the caller.
                let mut request = new_vfs_request(VFS_IPC_OP_CLOSE);
                request.remote_id = id;
                let result = call_vfs_ipc_request(&request).and_then(|r| ensure_vfs_status(&r));
                purge_kernel_epoll_object(WAITSET_PROVIDER_VFSD, id);
                return result;
            }
            return Ok(());
        }
        release_closed_handles(core::iter::once(closed_handle));
        Ok(())
    })())
}

pub(in crate::user::syscall::linux) fn syscall_linux_vfs_dup(
    oldfd: u64,
    newfd: u64,
    flags: u64,
    mode: VfsDupMode,
) -> u64 {
    linux_result((|| {
        if matches!(mode, VfsDupMode::Dup3)
            && (oldfd == newfd || flags & !linux_abi::O_CLOEXEC != 0)
        {
            return Err(LINUX_EINVAL);
        }
        if matches!(mode, VfsDupMode::Dup2) && oldfd == newfd {
            return current_fd_exists(oldfd).then_some(oldfd).ok_or(LINUX_EBADF);
        }
        if matches!(mode, VfsDupMode::Dup2 | VfsDupMode::Dup3)
            && newfd > multitask::MAX_DYNAMIC_FD
        {
            return Err(LINUX_EBADF);
        }
        let target = match mode {
            VfsDupMode::Dup => FdDupTarget::Minimum(0),
            VfsDupMode::Dup2 | VfsDupMode::Dup3 => FdDupTarget::Exact(newfd),
        };
        let close_on_exec =
            matches!(mode, VfsDupMode::Dup3) && (flags & linux_abi::O_CLOEXEC != 0);
        duplicate_fd_transaction(oldfd, target, close_on_exec, duplicate_install_errno(mode))
    })())
}

fn validate_fcntl_dupfd_min(arg: u64) -> LinuxResult<u64> {
    if arg > multitask::MAX_DYNAMIC_FD {
        return Err(LINUX_EINVAL);
    }
    Ok(arg)
}

fn new_vfs_fcntl_request(
    fd: u64,
    remote_id: u64,
    cmd: u64,
    arg: u64,
) -> alloc::boxed::Box<VfsIpcRequest> {
    let mut request = new_vfs_request(VFS_IPC_OP_FCNTL);
    request.fd = fd;
    request.remote_id = remote_id;
    request.arg0 = cmd;
    request.arg1 = arg;
    request
}

fn call_vfs_fd_fcntl(fd: u64, remote_id: u64, cmd: u64, arg: u64) -> LinuxResult<u64> {
    let request = new_vfs_fcntl_request(fd, remote_id, cmd, arg);
    Ok(call_vfs_checked(&request)?.value)
}

fn fcntl_non_local(fd: u64, cmd: u64, arg: u64) -> LinuxResult<u64> {
    match cmd {
        linux_abi::F_DUPFD | linux_abi::F_DUPFD_CLOEXEC => {
            if !current_fd_exists(fd) {
                return Err(LINUX_EBADF);
            }
            let min_fd = validate_fcntl_dupfd_min(arg)?;
            duplicate_fd_transaction(
                fd,
                FdDupTarget::Minimum(min_fd),
                cmd == linux_abi::F_DUPFD_CLOEXEC,
                LINUX_EMFILE,
            )
        }
        linux_abi::F_GETFD | linux_abi::F_SETFD | linux_abi::F_GETFL | linux_abi::F_SETFL => {
            if matches!(cmd, linux_abi::F_GETFL | linux_abi::F_SETFL)
                && let Some(remote) = current_remote_vfs_handle(fd)
            {
                let remote_id = remote.remote_id();
                if cmd == linux_abi::F_GETFL {
                    return call_vfs_fd_fcntl(fd, remote_id, cmd, arg);
                }
                let _pin = RemoteVfsRefGuard::acquire(fd, remote_id)?;
                let _cursor = acquire_vfs_cursor_gate(remote_id)?;
                let request = new_vfs_fcntl_request(fd, remote_id, cmd, arg);
                let response = call_vfs_ipc_request(&request)?;
                ensure_vfs_status(&response)?;
                let _ = multitask::with_current_user_process_state_mut(|_, _, process_state| {
                    if let Some(entry) = process_state.handles_mut().get_entry_mut(fd)
                        && matches!(
                            entry.handle(),
                            multitask::KernelHandle::RemoteVfs(current)
                                if current.remote_id() == remote_id
                        )
                    {
                        entry.set_status_flags(response.value);
                    }
                });
                return Ok(0);
            }
            multitask::with_current_user_process_state_mut(|_, _, process_state| {
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
            .ok_or(LINUX_EBADF)
        }
        _ => {
            if let Some((memfd, rights)) = current_memfd_handle_with_rights(fd) {
                return match cmd {
                    linux_abi::F_GET_SEALS => Ok(memfd.seals() as u64),
                    linux_abi::F_ADD_SEALS => {
                        if !rights.allows_write() {
                            return Err(LINUX_EPERM);
                        }
                        let seals = u32::try_from(arg).map_err(|_| LINUX_EINVAL)?;
                        memfd
                            .add_seals(seals)
                            .map_err(memfd_error_to_linux_errno)?;
                        Ok(0)
                    }
                    _ => Err(LINUX_EINVAL),
                };
            }
            if let Some(remote) = current_remote_vfs_handle(fd) {
                return call_vfs_fd_fcntl(fd, remote.remote_id(), cmd, arg);
            }
            Err(if current_fd_exists(fd) {
                LINUX_EINVAL
            } else {
                LINUX_EBADF
            })
        }
    }
}

pub fn syscall_linux_vfs_fcntl(fd: u64, cmd: u64, arg: u64) -> u64 {
    // Advisory record locks (F_GETLK/F_SETLK[W], F_OFD_*): vfsd owns the policy.
    if let Some(result) = super::super::file_lock_ops::fcntl(fd, cmd, arg) {
        return result;
    }
    // Status flags and pipe sizing live on the kernel-local open description.
    if let Some(local) = super::local_fd::current_local_fd(fd)
        && let Some(result) = super::local_fd::fcntl(&local, cmd, arg)
    {
        return result;
    }
    linux_result(fcntl_non_local(fd, cmd, arg))
}

/// Pins a current-process remote VFS open description across blocking/IPC
/// operations and releases the bounded descriptor reference on drop.
pub(super) struct RemoteVfsRefGuard {
    remote_id: u64,
}

impl RemoteVfsRefGuard {
    /// Atomically verifies `fd` still names `remote_id` and pins a reference.
    pub(super) fn acquire(fd: u64, remote_id: u64) -> LinuxResult<Self> {
        acquire_current_remote_vfs_ref(fd, remote_id)?;
        Ok(Self { remote_id })
    }

    /// Resolves `fd` to a `RemoteVfsHandle` and pins its open description.
    pub(super) fn acquire_fd(fd: u64) -> LinuxResult<(multitask::RemoteVfsHandle, Self)> {
        let remote = current_remote_vfs_handle(fd).ok_or(LINUX_EBADF)?;
        let guard = Self::acquire(fd, remote.remote_id())?;
        Ok((remote, guard))
    }

    /// Adopts a freshly registered open-description reference; drops it unless
    /// [`Self::commit`] transfers custody to the descriptor table.
    const fn from_registered(remote_id: u64) -> Self {
        Self { remote_id }
    }

    fn commit(self) {
        core::mem::forget(self);
    }
}

impl Drop for RemoteVfsRefGuard {
    fn drop(&mut self) {
        release_service_handle_refs_bounded(&[ServiceHandleRef::RemoteVfs(self.remote_id)]);
    }
}

fn pinned_remote_vfs_fd(request: &VfsIpcRequest) -> u64 {
    match request.op {
        VFS_IPC_OP_OPENAT
        | VFS_IPC_OP_STATX
        | VFS_IPC_OP_NEWFSTATAT
        | VFS_IPC_OP_READLINKAT
        | VFS_IPC_OP_ACCESS
        | VFS_IPC_OP_CHDIR
        | VFS_IPC_OP_MKDIR
        | VFS_IPC_OP_UNLINKAT
        | VFS_IPC_OP_RENAMEAT2
        | VFS_IPC_OP_SYMLINKAT
        | VFS_IPC_OP_MKNOD
        | VFS_IPC_OP_CHMOD
        | VFS_IPC_OP_CHOWN => request.dirfd,
        VFS_IPC_OP_SET_ATTRS if request.path_len != 0 => request.dirfd,
        VFS_IPC_OP_LINKAT if request.flags & VFS_LINKAT_FLAG_SOURCE_DESCRIPTION == 0 => {
            request.dirfd
        }
        _ => request.fd,
    }
}

pub fn call_pinned_remote_vfs_request(
    request: &VfsIpcRequest,
) -> Result<alloc::boxed::Box<VfsIpcResponse>, i64> {
    if request.remote_id == 0 {
        return call_vfs_ipc_request(request);
    }
    let fd = pinned_remote_vfs_fd(request);
    let _pin = RemoteVfsRefGuard::acquire(fd, request.remote_id)?;
    call_vfs_ipc_request(request)
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

struct ServiceRefGuard<'a>(Option<&'a ServiceHandleRef>);

impl<'a> ServiceRefGuard<'a> {
    fn acquire(handle_ref: Option<&'a ServiceHandleRef>) -> LinuxResult<Self> {
        if let Some(handle_ref) = handle_ref {
            acquire_service_handle_ref(handle_ref)?;
        }
        Ok(Self(handle_ref))
    }

    fn commit(mut self) {
        self.0 = None;
    }
}

impl Drop for ServiceRefGuard<'_> {
    fn drop(&mut self) {
        if let Some(handle_ref) = self.0 {
            release_service_handle_refs_bounded(core::slice::from_ref(handle_ref));
        }
    }
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
    let source_guard = ServiceRefGuard::acquire(source_ref.as_ref())?;

    let (fd, replaced) = multitask::with_current_user_process_state_mut(|_, _, process_state| {
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
                    .map(|(fd, replaced)| {
                        (
                            fd,
                            replaced.map(|handle| process_state.defer_handle_drop_if_pinned(handle)),
                        )
                    })
                    .ok_or(install_errno)
            }
        }
    })
    .unwrap_or(Err(LINUX_EBADF))?;
    source_guard.commit();

    if let Some((replaced, release_now)) = replaced {
        super::super::file_lock_ops::note_fd_closed(&replaced);
        if release_now {
            release_closed_handles(core::iter::once(replaced));
        }
    }
    Ok(fd)
}

#[cfg(test)]
mod tests;

const MAX_RW_COUNT: usize = 0x7fff_f000;

#[inline]
fn partial_or_err(completed: usize, errno: i64) -> LinuxResult<u64> {
    if completed > 0 {
        Ok(completed as u64)
    } else {
        Err(errno)
    }
}

fn current_handle_and_rights(
    fd: u64,
) -> Option<(multitask::KernelHandle, kernel_object::api::HandleRights, u64)> {
    multitask::with_current_user_process_state(|_, _, process_state| {
        let entry = process_state.handles().get_entry(fd)?;
        Some((entry.handle().clone(), entry.rights(), entry.status_flags()))
    })
    .flatten()
}

fn read_console_fd(
    console: &multitask::ConsoleHandle,
    status_flags: u64,
    user_ptr: u64,
    user_len: u64,
) -> LinuxResult<u64> {
    if console.stream() != multitask::ConsoleStreamKind::Input {
        return Err(LINUX_EBADF);
    }
    let user_len = usize::try_from(user_len).map_err(|_| LINUX_EINVAL)?;
    if user_len == 0 {
        return Ok(0);
    }
    let nonblocking = status_flags & linux_abi::O_NONBLOCK != 0;
    if !current_console_session_is_system() {
        return console_read_via_sessiond(user_ptr, user_len, nonblocking);
    }
    match crate::user::sysops::console::read_into_current_process(user_ptr, user_len, nonblocking) {
        Ok(0) if nonblocking => Err(LINUX_EAGAIN),
        Ok(read) => Ok(read as u64),
        Err(err) => Err(address_space_error_to_linux_errno(err)),
    }
}

fn write_console_fd(
    console: &multitask::ConsoleHandle,
    user_ptr: u64,
    user_len: u64,
) -> LinuxResult<u64> {
    if !matches!(
        console.stream(),
        multitask::ConsoleStreamKind::Output | multitask::ConsoleStreamKind::Error
    ) {
        return Err(LINUX_EBADF);
    }
    let user_len = usize::try_from(user_len).map_err(|_| LINUX_EINVAL)?;
    if user_len == 0 {
        return Ok(0);
    }
    if !current_console_session_is_system() {
        return console_write_via_sessiond(user_ptr, user_len);
    }
    crate::user::sysops::console::write_from_current_process(user_ptr, user_len)
        .map(|written| written as u64)
        .map_err(address_space_error_to_linux_errno)
}

pub fn syscall_linux_vfs_read(fd: u64, user_ptr: u64, user_len: u64) -> u64 {
    if let Some(result) = super::local_fd::read(fd, user_ptr, user_len) {
        return result;
    }
    linux_result((|| {
        let (handle, rights, status_flags) = current_handle_and_rights(fd).ok_or(LINUX_EBADF)?;
        match handle {
            multitask::KernelHandle::Console(console) => {
                read_console_fd(&console, status_flags, user_ptr, user_len)
            }
            multitask::KernelHandle::InetSocket(_) => kernel_vfs_result(socket_read_with_length(
                user_len,
                || {
                    super::vfs_meta::syscall_linux_net6(
                        SYSCALL_OFFLOAD_OP_LINUX_RECVFROM,
                        fd,
                        user_ptr,
                        user_len,
                        0,
                        0,
                        0,
                    )
                },
            )),
            multitask::KernelHandle::Memfd(mut memfd) => {
                if !rights.allows_read() {
                    return Err(LINUX_EBADF);
                }
                kernel_vfs_result(super::local_memfd_io::read(&mut memfd, user_ptr, user_len))
            }
            multitask::KernelHandle::Device(device)
                if device.device_id() == kernel_object::api::device::DeviceId::Input =>
            {
                if !rights.allows_read() {
                    return Err(LINUX_EBADF);
                }
                let access = match device.access_kind() {
                    kernel_object::api::device::DeviceAccessKind::Native => INPUTD_ACCESS_NATIVE,
                    kernel_object::api::device::DeviceAccessKind::Evdev => INPUTD_ACCESS_EVDEV,
                };
                kernel_vfs_result(read_input_device_via_inputd(
                    fd,
                    user_ptr,
                    user_len,
                    access,
                    status_flags,
                ))
            }
            multitask::KernelHandle::RemoteVfs(remote) => {
                if !rights.allows_read() {
                    // A write-only or O_PATH description cannot be read.
                    return Err(LINUX_EBADF);
                }
                if remote.kind() == multitask::RemoteVfsHandleKind::Device {
                    match remote.device_access() {
                        access @ (INPUTD_ACCESS_NATIVE | INPUTD_ACCESS_EVDEV) => {
                            return kernel_vfs_result(read_input_device_via_inputd(
                                fd,
                                user_ptr,
                                user_len,
                                access,
                                status_flags,
                            ));
                        }
                        access => {
                            if let Some(result) = try_read_virtual_device(access, user_ptr, user_len)
                            {
                                return result;
                            }
                        }
                    }
                }
                if remote.kind() == multitask::RemoteVfsHandleKind::Inotify {
                    return read_inotify_vfs(fd, remote.remote_id(), user_ptr, user_len);
                }
                read_remote_vfs(fd, &remote, user_ptr, user_len, None)
            }
            _ => Err(LINUX_EBADF),
        }
    })())
}

fn zero_fill_current_user(user_ptr: u64, user_len: u64) -> LinuxResult<u64> {
    const ZEROS: [u8; 4096] = [0u8; 4096];
    let len = usize::try_from(user_len)
        .map(|len| len.min(MAX_RW_COUNT))
        .map_err(|_| LINUX_EINVAL)?;
    let mut written = 0usize;
    while written < len {
        let chunk = (len - written).min(ZEROS.len());
        let Some(dest_ptr) = user_ptr.checked_add(written as u64) else {
            return partial_or_err(written, LINUX_EFAULT);
        };
        if let Err(errno) = write_user_bytes_as(dest_ptr, &ZEROS[..chunk]) {
            return partial_or_err(written, errno);
        }
        written += chunk;
    }
    Ok(written as u64)
}

fn try_write_virtual_device(
    access: u16,
    user_ptr: u64,
    user_len: u64,
) -> Option<LinuxResult<u64>> {
    if !matches!(
        access,
        VFS_DEVICE_ACCESS_NULL
            | VFS_DEVICE_ACCESS_URANDOM
            | VFS_DEVICE_ACCESS_ZERO
            | VFS_DEVICE_ACCESS_FULL
    ) {
        return None;
    }
    let len = match usize::try_from(user_len).map(|len| len.min(MAX_RW_COUNT)) {
        Ok(len) => len,
        Err(_) => return Some(Err(LINUX_EINVAL)),
    };
    if len > 0 {
        let mut probe = [0u8; 1];
        if let Err(errno) = read_user_bytes_as(user_ptr, &mut probe) {
            return Some(Err(errno));
        }
        if access == VFS_DEVICE_ACCESS_FULL {
            return Some(Err(LINUX_ENOSPC));
        }
    }
    Some(Ok(len as u64))
}

fn try_read_virtual_device(
    access: u16,
    user_ptr: u64,
    user_len: u64,
) -> Option<LinuxResult<u64>> {
    match access {
        VFS_DEVICE_ACCESS_NULL => {
            let Ok(len) = usize::try_from(user_len) else {
                return Some(Err(LINUX_EINVAL));
            };
            if len > 0
                && let Err(errno) = validate_user_write_as(user_ptr, 1)
            {
                return Some(Err(errno));
            }
            Some(Ok(0))
        }
        VFS_DEVICE_ACCESS_ZERO | VFS_DEVICE_ACCESS_FULL => {
            Some(zero_fill_current_user(user_ptr, user_len))
        }
        VFS_DEVICE_ACCESS_URANDOM => Some(kernel_vfs_result(
            super::super::syscalld_ops::syscall_linux_syscalld_getrandom(user_ptr, user_len, 0),
        )),
        _ => None,
    }
}

pub fn syscall_linux_vfs_pread64(fd: u64, user_ptr: u64, user_len: u64, offset: u64) -> u64 {
    linux_result((|| {
        if offset > i64::MAX as u64 {
            return Err(LINUX_EINVAL);
        }
        if let Some(result) = super::local_fd::not_seekable(fd) {
            return kernel_vfs_result(result);
        }
        let (handle, rights, _) = current_handle_and_rights(fd).ok_or(LINUX_EBADF)?;
        validate_handle_entry_for_io(&handle, rights, false, true)?;
        match handle {
            multitask::KernelHandle::Memfd(memfd) => {
                let offset = usize::try_from(offset).map_err(|_| LINUX_EINVAL)?;
                kernel_vfs_result(super::local_memfd_io::read_at(
                    &memfd, user_ptr, user_len, offset,
                ))
            }
            multitask::KernelHandle::DeviceBuffer(buffer) => kernel_vfs_result(
                super::local_memfd_io::read_device_buffer_at(&buffer, user_ptr, user_len, offset),
            ),
            multitask::KernelHandle::RemoteVfs(remote) => {
                if remote.kind() == multitask::RemoteVfsHandleKind::Device
                    && let Some(result) =
                        try_read_virtual_device(remote.device_access(), user_ptr, user_len)
                {
                    return result;
                }
                read_remote_vfs(fd, &remote, user_ptr, user_len, Some(offset))
            }
            _ => Err(LINUX_EBADF),
        }
    })())
}

pub fn kernel_vfs_result(raw: u64) -> Result<u64, i64> {
    let signed = raw as i64;
    if (-4095..=-1).contains(&signed) {
        Err(-signed)
    } else {
        Ok(raw)
    }
}

pub fn kernel_vfs_dup(oldfd: u64) -> Result<u64, i64> {
    kernel_vfs_result(syscall_linux_vfs_dup(oldfd, 0, 0, VfsDupMode::Dup))
}

pub fn kernel_vfs_read_user(
    fd: u64,
    user_ptr: u64,
    user_len: u64,
    offset: Option<u64>,
) -> Result<u64, i64> {
    let raw = match offset {
        Some(pos) => syscall_linux_vfs_pread64(fd, user_ptr, user_len, pos),
        None => syscall_linux_vfs_read(fd, user_ptr, user_len),
    };
    kernel_vfs_result(raw)
}

pub fn kernel_vfs_write_user(
    fd: u64,
    user_ptr: u64,
    user_len: u64,
    offset: Option<u64>,
) -> Result<u64, i64> {
    let raw = match offset {
        Some(pos) => syscall_linux_vfs_pwrite64(fd, user_ptr, user_len, pos),
        None => syscall_linux_vfs_write(fd, user_ptr, user_len),
    };
    kernel_vfs_result(raw)
}

pub fn kernel_vfs_pread_bytes(fd: u64, offset: u64, dest: &mut [u8]) -> Result<usize, i64> {
    if dest.is_empty() {
        return Ok(0);
    }
    if offset > i64::MAX as u64 {
        return Err(LINUX_EINVAL);
    }
    if let Some((memfd, rights)) = current_memfd_handle_with_rights(fd) {
        if !rights.allows_read() {
            return Err(LINUX_EBADF);
        }
        let pos = usize::try_from(offset).map_err(|_| LINUX_EINVAL)?;
        return Ok(memfd.read_at(pos, dest));
    }
    let (remote, _pin) = RemoteVfsRefGuard::acquire_fd(fd)?;
    kernel_vfs_pread_bytes_pinned(fd, remote.remote_id(), offset, dest)
}

fn read_vfs_inline_chunks(
    fd: u64,
    remote_id: u64,
    len: usize,
    offset: Option<u64>,
    yield_between_chunks: bool,
    mut deliver: impl FnMut(usize, &[u8]) -> LinuxResult<()>,
) -> LinuxResult<usize> {
    let mut copied = 0usize;
    while copied < len {
        let chunk_len = (len - copied).min(VFS_IPC_PAYLOAD_CAPACITY);
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
                return if copied > 0 { Ok(copied) } else { Err(LINUX_EOVERFLOW) };
            };
            if chunk_offset > i64::MAX as u64 {
                return if copied > 0 { Ok(copied) } else { Err(LINUX_EOVERFLOW) };
            }
            request.arg0 = chunk_offset;
        }
        let response = match call_vfs_ipc_request(&request).and_then(|response| {
            ensure_vfs_status(&response)?;
            Ok(response)
        }) {
            Ok(response) => response,
            Err(errno) => return if copied > 0 { Ok(copied) } else { Err(errno) },
        };
        let read = response.payload_len as usize;
        if read > chunk_len || read > response.payload.len() {
            return if copied > 0 { Ok(copied) } else { Err(LINUX_EINVAL) };
        }
        if read == 0 {
            break;
        }
        if let Err(errno) = deliver(copied, &response.payload[..read]) {
            return if copied > 0 { Ok(copied) } else { Err(errno) };
        }
        copied += read;
        if yield_between_chunks {
            multitask::cond_resched();
        }
        if read < chunk_len {
            break;
        }
    }
    Ok(copied)
}

pub(super) fn kernel_vfs_pread_bytes_pinned(
    fd: u64,
    remote_id: u64,
    offset: u64,
    dest: &mut [u8],
) -> Result<usize, i64> {
    read_vfs_inline_chunks(fd, remote_id, dest.len(), Some(offset), false, |copied, bytes| {
        dest[copied..copied + bytes.len()].copy_from_slice(bytes);
        Ok(())
    })
}

pub fn syscall_linux_vfs_write(fd: u64, user_ptr: u64, user_len: u64) -> u64 {
    if let Some(result) = super::local_fd::write(fd, user_ptr, user_len) {
        return result;
    }
    linux_result((|| {
        let (handle, rights, _) = current_handle_and_rights(fd).ok_or(LINUX_EBADF)?;
        match handle {
            multitask::KernelHandle::Console(console) => {
                write_console_fd(&console, user_ptr, user_len)
            }
            multitask::KernelHandle::InetSocket(_) => {
                // A compositor writing events to a client whose receive buffer is full
                // is the textbook Wayland head-of-line block - one slow client stops
                // every other client and the compositor with it, which is what
                // `V5-WAYLAND-HOL-013` names. The bound that prevents it is applied in
                // `syscall_linux_net6_with_timeout` for every socket data operation,
                // not just this one.
                kernel_vfs_result(super::vfs_meta::syscall_linux_net6(
                    SYSCALL_OFFLOAD_OP_LINUX_SENDTO,
                    fd,
                    user_ptr,
                    user_len,
                    0,
                    0,
                    0,
                ))
            }
            multitask::KernelHandle::Memfd(mut memfd) => {
                if !rights.allows_write() {
                    return Err(LINUX_EBADF);
                }
                kernel_vfs_result(super::local_memfd_io::write(&mut memfd, user_ptr, user_len))
            }
            multitask::KernelHandle::RemoteVfs(remote) => {
                if !rights.allows_write() {
                    return Err(LINUX_EBADF);
                }
                if remote.kind() == multitask::RemoteVfsHandleKind::Inotify {
                    return Err(LINUX_ESPIPE);
                }
                if remote.kind() == multitask::RemoteVfsHandleKind::Device
                    && let Some(result) =
                        try_write_virtual_device(remote.device_access(), user_ptr, user_len)
                {
                    return result;
                }
                write_remote_vfs(fd, &remote, user_ptr, user_len)
            }
            _ => Err(LINUX_EBADF),
        }
    })())
}

pub fn syscall_linux_vfs_pwrite64(fd: u64, user_ptr: u64, user_len: u64, offset: u64) -> u64 {
    linux_result((|| {
        if offset > i64::MAX as u64 {
            return Err(LINUX_EINVAL);
        }
        if let Some(result) = super::local_fd::not_seekable(fd) {
            return kernel_vfs_result(result);
        }
        let (handle, rights, _) = current_handle_and_rights(fd).ok_or(LINUX_EBADF)?;
        validate_handle_entry_for_io(&handle, rights, true, true)?;
        match handle {
            multitask::KernelHandle::Memfd(memfd) => kernel_vfs_result(
                super::local_memfd_io::write_at(&memfd, user_ptr, user_len, offset),
            ),
            multitask::KernelHandle::RemoteVfs(remote) => {
                if remote.kind() == multitask::RemoteVfsHandleKind::Device
                    && let Some(result) =
                        try_write_virtual_device(remote.device_access(), user_ptr, user_len)
                {
                    return result;
                }
                let len = usize::try_from(user_len).map_err(|_| LINUX_EINVAL)?;
                if len == 0 {
                    return Ok(0);
                }
                let remote_id = remote.remote_id();
                let _pin = RemoteVfsRefGuard::acquire(fd, remote_id)?;
                pwrite_remote_vfs_pinned(fd, remote_id, user_ptr, len, offset)
            }
            _ => Err(LINUX_EBADF),
        }
    })())
}

fn pwrite_remote_vfs_pinned(
    fd: u64,
    remote_id: u64,
    user_ptr: u64,
    len: usize,
    offset: u64,
) -> LinuxResult<u64> {
    let mut written = 0usize;
    while written < len {
        let chunk_len = (len - written).min(VFS_IPC_REQUEST_PAYLOAD_CAPACITY);
        let Some(chunk_offset) = offset.checked_add(written as u64) else {
            return partial_or_err(written, LINUX_EOVERFLOW);
        };
        if chunk_offset > i64::MAX as u64 {
            return partial_or_err(written, LINUX_EOVERFLOW);
        }
        let Some(chunk_user_ptr) = user_ptr.checked_add(written as u64) else {
            return partial_or_err(written, LINUX_EFAULT);
        };
        let mut request = new_vfs_request(VFS_IPC_OP_PWRITE64);
        request.fd = fd;
        request.remote_id = remote_id;
        request.arg0 = chunk_offset;
        request.arg1 = chunk_len as u64;
        request.payload_len = chunk_len as u32;
        if let Err(errno) = read_user_bytes_as(chunk_user_ptr, &mut request.payload[..chunk_len]) {
            return partial_or_err(written, errno);
        }
        let response = match call_vfs_ipc_request(&request).and_then(|response| {
            ensure_vfs_status(&response)?;
            Ok(response)
        }) {
            Ok(response) => response,
            Err(errno) => return partial_or_err(written, errno),
        };
        let chunk_written = response.value as usize;
        if chunk_written > chunk_len {
            return partial_or_err(written, LINUX_EINVAL);
        }
        written += chunk_written;
        if chunk_written == 0 || chunk_written < chunk_len {
            break;
        }
    }
    Ok(written as u64)
}

fn validate_handle_entry_for_io(
    handle: &multitask::KernelHandle,
    rights: kernel_object::api::HandleRights,
    write: bool,
    seek: bool,
) -> LinuxResult<()> {
    match handle {
        multitask::KernelHandle::Pipe(pipe) => {
            let expected = if write {
                kernel_ps::api::PipeRole::Write
            } else {
                kernel_ps::api::PipeRole::Read
            };
            if pipe.role() != expected {
                return Err(LINUX_EBADF);
            }
            if seek {
                return Err(LINUX_ESPIPE);
            }
            Ok(())
        }
        multitask::KernelHandle::EventFd(_)
        | multitask::KernelHandle::TimerFd(_)
        | multitask::KernelHandle::UnixSocket(_)
        | multitask::KernelHandle::InetSocket(_) => {
            if seek {
                Err(LINUX_ESPIPE)
            } else {
                Ok(())
            }
        }
        multitask::KernelHandle::Console(console) => {
            let readable = console.stream() == multitask::ConsoleStreamKind::Input;
            if write == readable {
                return Err(LINUX_EBADF);
            }
            if seek {
                return Err(LINUX_ESPIPE);
            }
            Ok(())
        }
        multitask::KernelHandle::Epoll(_) => Err(if seek { LINUX_ESPIPE } else { LINUX_EBADF }),
        multitask::KernelHandle::RemoteVfs(remote) => {
            let allowed = if write {
                rights.allows_write()
            } else {
                rights.allows_read()
            };
            if !allowed {
                return Err(LINUX_EBADF);
            }
            if (seek || write) && remote.kind() == multitask::RemoteVfsHandleKind::Inotify {
                return Err(LINUX_ESPIPE);
            }
            Ok(())
        }
        multitask::KernelHandle::Memfd(_) => {
            let allowed = if write {
                rights.allows_write()
            } else {
                rights.allows_read()
            };
            if allowed { Ok(()) } else { Err(LINUX_EBADF) }
        }
        multitask::KernelHandle::Device(_) => {
            let allowed = if write {
                rights.allows_write()
            } else {
                rights.allows_read()
            };
            if !allowed {
                return Err(LINUX_EBADF);
            }
            if seek {
                return Err(LINUX_ESPIPE);
            }
            Ok(())
        }
        multitask::KernelHandle::DeviceBuffer(_) => {
            if !write && rights.allows_read() {
                Ok(())
            } else {
                Err(LINUX_EBADF)
            }
        }
        _ => Err(LINUX_EBADF),
    }
}

fn validate_vectored_fd(fd: u64, write: bool, seek: bool) -> LinuxResult<()> {
    let (handle, rights, _) = current_handle_and_rights(fd).ok_or(LINUX_EBADF)?;
    validate_handle_entry_for_io(&handle, rights, write, seek)
}

fn vectored_positional_rw(
    fd: u64,
    iov_ptr: u64,
    iovcnt: u64,
    offset: u64,
    write: bool,
    step: impl FnMut(u64, u64, u64, Option<u64>) -> u64,
) -> LinuxResult<u64> {
    if offset > i64::MAX as u64 {
        return Err(LINUX_EINVAL);
    }
    validate_vectored_fd(fd, write, true)?;
    vectored_io_at(fd, iov_ptr, iovcnt, Some(offset), step)
}

pub fn syscall_linux_vfs_readv(fd: u64, iov_ptr: u64, iovcnt: u64) -> u64 {
    linux_result((|| {
        validate_vectored_fd(fd, false, false)?;
        vectored_io_at(fd, iov_ptr, iovcnt, None, |fd, ptr, len, _| {
            syscall_linux_vfs_read(fd, ptr, len)
        })
    })())
}

pub fn syscall_linux_vfs_preadv(fd: u64, iov_ptr: u64, iovcnt: u64, offset: u64) -> u64 {
    linux_result(vectored_positional_rw(
        fd,
        iov_ptr,
        iovcnt,
        offset,
        false,
        |fd, ptr, len, pos| syscall_linux_vfs_pread64(fd, ptr, len, pos.unwrap_or(offset)),
    ))
}

pub fn syscall_linux_vfs_pwritev(fd: u64, iov_ptr: u64, iovcnt: u64, offset: u64) -> u64 {
    linux_result(vectored_positional_rw(
        fd,
        iov_ptr,
        iovcnt,
        offset,
        true,
        |fd, ptr, len, pos| syscall_linux_vfs_pwrite64(fd, ptr, len, pos.unwrap_or(offset)),
    ))
}

fn preadv2_validate_offset_and_flags(pos_l: u64, flags: u64) -> Result<Option<u64>, i64> {
    const ACCEPTED_RWF_FLAGS: u64 = linux_abi::RWF_HIPRI
        | linux_abi::RWF_DSYNC
        | linux_abi::RWF_SYNC
        | linux_abi::RWF_NOWAIT
        | linux_abi::RWF_APPEND;
    if flags & !ACCEPTED_RWF_FLAGS != 0 {
        return Err(LINUX_EOPNOTSUPP);
    }
    if pos_l == u64::MAX {
        Ok(None)
    } else if pos_l > i64::MAX as u64 {
        Err(LINUX_EINVAL)
    } else {
        Ok(Some(pos_l))
    }
}

pub fn syscall_linux_vfs_preadv2(
    fd: u64,
    iov_ptr: u64,
    iovcnt: u64,
    pos_l: u64,
    _pos_h: u64,
    flags: u64,
) -> u64 {
    linux_result((|| {
        match preadv2_validate_offset_and_flags(pos_l, flags)? {
            Some(pos) => kernel_vfs_result(syscall_linux_vfs_preadv(fd, iov_ptr, iovcnt, pos)),
            None => kernel_vfs_result(syscall_linux_vfs_readv(fd, iov_ptr, iovcnt)),
        }
    })())
}

pub fn syscall_linux_vfs_pwritev2(
    fd: u64,
    iov_ptr: u64,
    iovcnt: u64,
    pos_l: u64,
    _pos_h: u64,
    flags: u64,
) -> u64 {
    linux_result((|| {
        match preadv2_validate_offset_and_flags(pos_l, flags)? {
            Some(pos) => kernel_vfs_result(syscall_linux_vfs_pwritev(fd, iov_ptr, iovcnt, pos)),
            None => kernel_vfs_result(syscall_linux_vfs_writev(fd, iov_ptr, iovcnt)),
        }
    })())
}

pub fn syscall_linux_vfs_fsync(fd: u64) -> u64 {
    syscall_linux_vfs_fsync_mode(fd, false)
}

pub fn syscall_linux_vfs_fsync_mode(fd: u64, datasync: bool) -> u64 {
    linux_unit_result((|| {
        let (handle, rights, _) = current_handle_and_rights(fd).ok_or(LINUX_EBADF)?;
        let remote = match handle {
            multitask::KernelHandle::Pipe(_)
            | multitask::KernelHandle::EventFd(_)
            | multitask::KernelHandle::TimerFd(_)
            | multitask::KernelHandle::Epoll(_)
            | multitask::KernelHandle::UnixSocket(_)
            | multitask::KernelHandle::InetSocket(_)
            | multitask::KernelHandle::Console(_) => return Err(LINUX_EINVAL),
            multitask::KernelHandle::RemoteVfs(remote) => {
                if !rights.allows_read() && !rights.allows_write() {
                    return Err(LINUX_EBADF);
                }
                if remote.kind() == multitask::RemoteVfsHandleKind::Device {
                    return Err(LINUX_EINVAL);
                }
                remote
            }
            _ => return Ok(()),
        };
        let remote_id = remote.remote_id();
        let _pin = RemoteVfsRefGuard::acquire(fd, remote_id)?;
        let _cursor = acquire_vfs_cursor_gate(remote_id)?;
        if remote.kind() == multitask::RemoteVfsHandleKind::File {
            let current_epoch = ipc_ops::service_endpoint_epoch(IPC_SERVICE_VFSD).unwrap_or(0);
            if remote.is_dirty(current_epoch) {
                let settled = settle_vfs_cursor_sync(fd, remote_id, remote.cursor())?;
                remote.mark_settled(settled, current_epoch);
            }
        }
        let mut request = new_vfs_request(VFS_IPC_OP_FSYNC);
        request.fd = fd;
        request.remote_id = remote_id;
        request.arg0 = u64::from(datasync);
        let response = call_vfs_ipc_request(&request)?;
        ensure_vfs_status(&response)
    })())
}

pub fn syscall_linux_vfs_sync() -> u64 {
    0
}

pub fn syscall_linux_vfs_syncfs(fd: u64) -> u64 {
    linux_unit_result(current_fd_exists(fd).then_some(()).ok_or(LINUX_EBADF))
}

pub fn syscall_linux_vfs_fadvise64(fd: u64, _offset: u64, len: u64, advice: u64) -> u64 {
    linux_unit_result((|| {
        let (handle, _, _) = current_handle_and_rights(fd).ok_or(LINUX_EBADF)?;
        if matches!(
            handle,
            multitask::KernelHandle::Pipe(_)
                | multitask::KernelHandle::EventFd(_)
                | multitask::KernelHandle::TimerFd(_)
                | multitask::KernelHandle::Epoll(_)
                | multitask::KernelHandle::UnixSocket(_)
                | multitask::KernelHandle::InetSocket(_)
        ) {
            return Err(LINUX_ESPIPE);
        }
        if (len as i64) < 0 || advice > linux_abi::POSIX_FADV_NOREUSE {
            return Err(LINUX_EINVAL);
        }
        Ok(())
    })())
}

fn validate_close_range(first: u64, last: u64, flags: u64) -> LinuxResult<bool> {
    if flags & !linux_abi::CLOSE_RANGE_CLOEXEC != 0 || first > last {
        return Err(LINUX_EINVAL);
    }
    Ok(flags & linux_abi::CLOSE_RANGE_CLOEXEC != 0)
}

pub fn syscall_linux_vfs_close_range(first: u64, last: u64, flags: u64) -> u64 {
    linux_unit_result((|| {
        let cloexec = validate_close_range(first, last, flags)?;
        if cloexec {
            let _ = multitask::with_current_user_process_state_mut(|_, _, process_state| {
                let matching: Vec<u64> = process_state
                    .handles()
                    .fd_numbers()
                    .into_iter()
                    .filter(|fd| (first..=last).contains(fd))
                    .collect();
                for fd in matching {
                    if let Some(entry) = process_state.handles_mut().get_entry_mut(fd) {
                        entry.set_fd_flags(entry.fd_flags() | multitask::FD_CLOEXEC);
                    }
                }
            });
            return Ok(());
        }
        let (closed_for_locks, release_now) =
            multitask::with_current_user_process_state_mut(|_, _, process_state| {
                let matching: Vec<u64> = process_state
                    .handles()
                    .fd_numbers()
                    .into_iter()
                    .filter(|fd| (first..=last).contains(fd))
                    .collect();
                let mut lock_handles = Vec::with_capacity(matching.len());
                let mut release_handles = Vec::with_capacity(matching.len());
                for fd in matching {
                    if let Some((handle, release_now)) =
                        process_state.close_handle_respecting_fork_pin(fd)
                    {
                        if release_now {
                            release_handles.push(handle.clone());
                        }
                        lock_handles.push(handle);
                    }
                }
                (lock_handles, release_handles)
            })
            .unwrap_or_default();
        for handle in &closed_for_locks {
            super::super::file_lock_ops::note_fd_closed(handle);
        }
        if !release_now.is_empty() {
            release_closed_handles(release_now);
        }
        Ok(())
    })())
}

pub fn syscall_linux_vfs_writev(fd: u64, iov_ptr: u64, iovcnt: u64) -> u64 {
    linux_result((|| {
        if let Some(snapshot) = super::vfs_meta::snapshot_socket(fd) {
            let bytes = read_socket_writev_payload(iov_ptr, iovcnt, snapshot.is_datagram())?;
            if snapshot.is_datagram() && iovcnt != 0 {
                let sent = call_netd_socket_payload(
                    &snapshot,
                    fd,
                    SYSCALL_OFFLOAD_OP_LINUX_SENDTO,
                    &bytes,
                )?;
                return if sent == bytes.len() as u64 {
                    Ok(sent)
                } else {
                    Err(LINUX_EIO)
                };
            }
            if bytes.is_empty() {
                return Ok(0);
            }
            let mut written = 0usize;
            while written < bytes.len() {
                let chunk = &bytes[written..];
                let count = match call_netd_socket_payload(
                    &snapshot,
                    fd,
                    SYSCALL_OFFLOAD_OP_LINUX_SENDTO,
                    chunk,
                ) {
                    Ok(count) => count as usize,
                    Err(errno) => return partial_or_err(written, errno),
                };
                if count == 0 {
                    break;
                }
                written = written.saturating_add(count);
            }
            return Ok(written as u64);
        }
        validate_vectored_fd(fd, true, false)?;
        if let Some(local) = super::local_fd::current_local_fd(fd) {
            match local {
                super::local_fd::LocalFd::UnixSocket(ref sock) => {
                    let iovecs = super::vfs_meta::read_current_iovecs_payload(iov_ptr, iovcnt)?;
                    let total = iovecs_total_len(&iovecs)?;
                    if sock.socket_type() != kernel_ps::api::UnixSocketType::Stream
                        && total > super::local_unix::MAX_UNIX_IO_BYTES
                    {
                        return Err(LINUX_EMSGSIZE);
                    }
                    if total == 0 {
                        return Ok(0);
                    }
                    if total <= 4096 {
                        let mut stack = [0_u8; 4096];
                        gather_iovecs_into(&iovecs, &mut stack[..total])?;
                        return super::local_fd::write_pinned_local(&local, &stack[..total])
                            .map(|n| n as u64);
                    }
                    if total <= super::local_unix::MAX_UNIX_IO_BYTES {
                        let payload = super::vfs_meta::read_iovec_prefix_with_mode(
                            &iovecs,
                            total,
                            super::vfs_meta::UserCopyMode::AddressSpace,
                        )?;
                        return super::local_fd::write_pinned_local(&local, &payload)
                            .map(|n| n as u64);
                    }
                    drop(local);
                    return vectored_io_with_iovecs(fd, iovecs, None, |f, p, l, _| {
                        syscall_linux_vfs_write(f, p, l)
                    });
                }
                super::local_fd::LocalFd::Pipe(_) => {
                    let iovecs = super::vfs_meta::read_current_iovecs_payload(iov_ptr, iovcnt)?;
                    let total = iovecs_total_len(&iovecs)?;
                    if total == 0 {
                        return Ok(0);
                    }
                    if total <= kernel_ps::api::PIPE_BUF {
                        let mut stack = [0_u8; kernel_ps::api::PIPE_BUF];
                        gather_iovecs_into(&iovecs, &mut stack[..total])?;
                        return super::local_fd::write_pinned_local(&local, &stack[..total])
                            .map(|n| n as u64);
                    }
                    drop(local);
                    return vectored_io_with_iovecs(fd, iovecs, None, |f, p, l, _| {
                        syscall_linux_vfs_write(f, p, l)
                    });
                }
                super::local_fd::LocalFd::EventFd(_) | super::local_fd::LocalFd::TimerFd(_) => {}
            }
        }
        vectored_io_at(fd, iov_ptr, iovcnt, None, |f, p, l, _| {
            syscall_linux_vfs_write(f, p, l)
        })
    })())
}

pub(super) fn current_fd_exists(fd: u64) -> bool {
    multitask::with_current_user_process_state(|_, _, process_state| {
        process_state.handles().get_entry(fd).is_some()
    })
    .unwrap_or(false)
}

/// The `readlink` text of `/proc/<own pid>/fd/<target_fd>`.
pub(super) fn proc_fd_link_text(target_fd: u64) -> Result<alloc::string::String, i64> {
    multitask::with_current_user_process_state(|_, _, process_state| {
        let Some(entry) = process_state.handles().get_entry(target_fd) else {
            return Err(LINUX_ENOENT);
        };
        Ok(match entry.handle() {
            multitask::KernelHandle::Memfd(memfd) => {
                let raw = memfd.path();
                if let Some(name) = raw
                    .strip_prefix("anon_inode:[memfd:")
                    .and_then(|s| s.strip_suffix(']'))
                {
                    alloc::format!("/memfd:{name} (deleted)")
                } else {
                    raw
                }
            }
            multitask::KernelHandle::Pipe(pipe) => alloc::format!("pipe:[{}]", pipe.inode()),
            multitask::KernelHandle::EventFd(_) => {
                alloc::string::String::from("anon_inode:[eventfd]")
            }
            multitask::KernelHandle::TimerFd(_) => {
                alloc::string::String::from("anon_inode:[timerfd]")
            }
            multitask::KernelHandle::Epoll(_) => {
                alloc::string::String::from("anon_inode:[eventpoll]")
            }
            multitask::KernelHandle::UnixSocket(socket) => {
                alloc::format!("socket:[{}]", socket.token_id())
            }
            multitask::KernelHandle::InetSocket(socket) => {
                alloc::format!("socket:[{}]", socket.token_id())
            }
            multitask::KernelHandle::RemoteVfs(remote) => remote.path(),
            multitask::KernelHandle::VfsDirectory(dir) => alloc::string::String::from(dir.path()),
            multitask::KernelHandle::Console(_) => alloc::string::String::from("/dev/console0"),
            multitask::KernelHandle::DeviceBuffer(_) => {
                alloc::string::String::from("/dev/dri/card0")
            }
            _ => alloc::string::String::from("anon_inode:[handle]"),
        })
    })
    .unwrap_or(Err(LINUX_ENOENT))
}

pub(super) fn validate_memfd_reopen_flags(
    flags: u64,
    source_rights: kernel_object::api::handle::HandleRights,
) -> Result<bool, i64> {
    const LINUX_O_EXCL: u64 = 0o200;
    if flags & linux_abi::O_DIRECTORY != 0 {
        return Err(LINUX_ENOTDIR);
    }
    if flags & linux_abi::O_CREAT != 0 && flags & LINUX_O_EXCL != 0 {
        return Err(LINUX_EEXIST);
    }
    if flags & linux_abi::O_PATH != 0 {
        return Ok(false);
    }
    let (wants_read, wants_write) = match flags & linux_abi::O_ACCMODE {
        linux_abi::O_RDONLY => (true, false),
        linux_abi::O_WRONLY => (false, true),
        linux_abi::O_RDWR => (true, true),
        _ => return Err(LINUX_EINVAL),
    };
    if (wants_read && !source_rights.allows_read())
        || (wants_write && !source_rights.allows_write())
    {
        return Err(LINUX_EACCES);
    }
    let truncate = flags & linux_abi::O_TRUNC != 0;
    if truncate && !wants_write {
        return Err(LINUX_EACCES);
    }
    Ok(truncate)
}


fn call_vfs_remote_close(remote_id: u64) -> Result<(), i64> {
    call_vfs_remote_close_bounded(remote_id)
}

pub(super) fn call_vfs_remote_close_bounded(remote_id: u64) -> Result<(), i64> {
    let mut request = new_vfs_request(VFS_IPC_OP_CLOSE);
    request.remote_id = remote_id;
    // Implicit/failure cleanup: the fd is retired and nobody consumes the
    // result, so keep the short rail; a timeout is replayed by housekeeping
    // under the state-mutation lease.
    call_vfs_ipc_request_with_timeout(&request,
        rustos_user_abi::performance::IPC_INTERACTIVE_CONTROL_HARD_LIMIT_MS)
        .and_then(|response| ensure_vfs_status(&response))
}

fn read_remote_vfs(
    fd: u64,
    remote: &multitask::RemoteVfsHandle,
    user_ptr: u64,
    user_len: u64,
    offset: Option<u64>,
) -> LinuxResult<u64> {
    let _pin = RemoteVfsRefGuard::acquire(fd, remote.remote_id())?;
    read_remote_vfs_pinned(fd, remote, user_ptr, user_len, offset)
}

const VFS_BULK_READ_COPY_CHUNK: usize = 64 * 1024;

trait RegularFileReadTransport {
    fn bulk_read(
        &mut self,
        offset: u64,
        want: usize,
        deliver: &mut dyn FnMut(&[u8]) -> Result<(), i64>,
    ) -> Result<usize, i64>;

    fn pread64(
        &mut self,
        offset: u64,
        want: usize,
        deliver: &mut dyn FnMut(&[u8]) -> Result<(), i64>,
    ) -> Result<usize, i64>;

    fn yield_between_chunks(&mut self);
}

struct VfsRemoteFileReadTransport {
    fd: u64,
    remote_id: u64,
}

impl RegularFileReadTransport for VfsRemoteFileReadTransport {
    fn bulk_read(
        &mut self,
        offset: u64,
        want: usize,
        deliver: &mut dyn FnMut(&[u8]) -> Result<(), i64>,
    ) -> Result<usize, i64> {
        let mut request = new_vfs_request(VFS_IPC_OP_BULK_READ);
        request.fd = self.fd;
        request.remote_id = self.remote_id;
        request.arg0 = offset;
        request.arg1 = want as u64;
        let (response_bytes, entries) = ipc_ops::call_service_endpoint_with_received_entries(
            IPC_SERVICE_VFSD,
            as_bytes(request.as_ref()),
            1,
        )?;
        let Some(response) = VfsBulkReadResponse::from_bytes(&response_bytes) else {
            ipc_ops::drop_transfer_entries(entries);
            return Err(LINUX_EINVAL);
        };
        if response.version != VFS_IPC_ABI_VERSION
            || response.op != VFS_IPC_OP_BULK_READ
            || response.reserved0 != 0
        {
            ipc_ops::drop_transfer_entries(entries);
            return Err(LINUX_EINVAL);
        }
        if let Err(errno) = check_ipc_status(response.status) {
            ipc_ops::drop_transfer_entries(entries);
            return Err(errno);
        }
        if response.bytes_read == 0 {
            let extra_handles = !entries.is_empty();
            ipc_ops::drop_transfer_entries(entries);
            return if extra_handles { Err(LINUX_EINVAL) } else { Ok(0) };
        }
        let Ok(bytes_read) = usize::try_from(response.bytes_read) else {
            ipc_ops::drop_transfer_entries(entries);
            return Err(LINUX_EINVAL);
        };
        if bytes_read > want || entries.len() != 1 {
            ipc_ops::drop_transfer_entries(entries);
            return Err(LINUX_EINVAL);
        }
        let memfd = match entries[0].entry().handle() {
            multitask::KernelHandle::Memfd(memfd) => memfd.clone(),
            _ => {
                ipc_ops::drop_transfer_entries(entries);
                return Err(LINUX_EINVAL);
            }
        };
        ipc_ops::drop_transfer_entries(entries);

        let mut bounce = alloc::vec![0_u8; bytes_read.min(VFS_BULK_READ_COPY_CHUNK)];
        let mut pos = 0usize;
        while pos < bytes_read {
            let step = (bytes_read - pos).min(bounce.len());
            let read = memfd.read_at(pos, &mut bounce[..step]);
            if read != step {
                return Err(LINUX_EIO);
            }
            deliver(&bounce[..read])?;
            pos += read;
        }
        Ok(bytes_read)
    }

    fn pread64(
        &mut self,
        offset: u64,
        want: usize,
        deliver: &mut dyn FnMut(&[u8]) -> Result<(), i64>,
    ) -> Result<usize, i64> {
        let mut request = new_vfs_request(VFS_IPC_OP_PREAD64);
        request.fd = self.fd;
        request.remote_id = self.remote_id;
        request.arg0 = offset;
        request.arg1 = want as u64;
        let response = call_vfs_ipc_request(&request)?;
        ensure_vfs_status(&response)?;
        let read = response.payload_len as usize;
        if read > want || read > response.payload.len() {
            return Err(LINUX_EINVAL);
        }
        if read > 0 {
            deliver(&response.payload[..read])?;
        }
        Ok(read)
    }

    fn yield_between_chunks(&mut self) {
        multitask::cond_resched();
    }
}

fn read_remote_vfs_file_loop_inner(
    cursor_desc: Option<&crate::user::handles::RemoteVfsOpenDescription>,
    start: u64,
    user_ptr: u64,
    user_len: usize,
    transport: &mut impl RegularFileReadTransport,
    mut copy_to_user: impl FnMut(u64, &[u8]) -> Result<(), i64>,
) -> LinuxResult<u64> {
    let mut copied = 0usize;
    let mut bulk_eligible = true;
    while copied < user_len {
        let remaining = user_len - copied;
        let Some(chunk_offset) = start.checked_add(copied as u64) else {
            return partial_or_err(copied, LINUX_EOVERFLOW);
        };
        if chunk_offset > i64::MAX as u64 {
            return partial_or_err(copied, LINUX_EOVERFLOW);
        }
        let bulk_want = remaining.min(VFS_BULK_READ_MAX_BYTES);
        let inline_want = remaining.min(VFS_IPC_PAYLOAD_CAPACITY);
        let outcome = {
            let mut deliver = |bytes: &[u8]| -> Result<(), i64> {
                let dest = user_ptr.checked_add(copied as u64).ok_or(LINUX_EINVAL)?;
                copy_to_user(dest, bytes)?;
                copied += bytes.len();
                if let Some(desc) = cursor_desc {
                    desc.advance_cursor(bytes.len());
                }
                Ok(())
            };
            if bulk_eligible && remaining >= VFS_BULK_READ_MIN_BYTES {
                match transport.bulk_read(chunk_offset, bulk_want, &mut deliver) {
                    Ok(read) => Ok((read, bulk_want)),
                    Err(LINUX_EOPNOTSUPP) => {
                        bulk_eligible = false;
                        transport
                            .pread64(chunk_offset, inline_want, &mut deliver)
                            .map(|read| (read, inline_want))
                    }
                    Err(LINUX_EAGAIN) => transport
                        .pread64(chunk_offset, inline_want, &mut deliver)
                        .map(|read| (read, inline_want)),
                    Err(errno) => Err(errno),
                }
            } else {
                transport
                    .pread64(chunk_offset, inline_want, &mut deliver)
                    .map(|read| (read, inline_want))
            }
        };
        match outcome {
            Ok((read, want)) => {
                if read == 0 || read < want {
                    break;
                }
                transport.yield_between_chunks();
            }
            Err(errno) => return partial_or_err(copied, errno),
        }
    }
    Ok(copied as u64)
}

#[cfg(test)]
fn read_remote_vfs_file_loop(
    cursor_desc: Option<&crate::user::handles::RemoteVfsOpenDescription>,
    start: u64,
    user_ptr: u64,
    user_len: usize,
    transport: &mut impl RegularFileReadTransport,
    copy_to_user: impl FnMut(u64, &[u8]) -> Result<(), i64>,
) -> u64 {
    linux_result(read_remote_vfs_file_loop_inner(
        cursor_desc,
        start,
        user_ptr,
        user_len,
        transport,
        copy_to_user,
    ))
}

fn read_remote_vfs_pinned(
    fd: u64,
    remote: &multitask::RemoteVfsHandle,
    user_ptr: u64,
    user_len: u64,
    offset: Option<u64>,
) -> LinuxResult<u64> {
    let user_len = usize::try_from(user_len).map_err(|_| LINUX_EINVAL)?;
    if user_len == 0 {
        return Ok(0);
    }
    validate_user_write_as(user_ptr, user_len)?;
    let remote_id = remote.remote_id();
    let _cursor = offset
        .is_none()
        .then(|| acquire_vfs_cursor_gate(remote_id))
        .transpose()?;
    if remote.kind() == multitask::RemoteVfsHandleKind::File {
        let (start, cursor_desc) = match offset {
            Some(explicit) => (explicit, None),
            None => (remote.cursor(), Some(remote.description().as_ref())),
        };
        let mut transport = VfsRemoteFileReadTransport { fd, remote_id };
        return read_remote_vfs_file_loop_inner(
            cursor_desc,
            start,
            user_ptr,
            user_len,
            &mut transport,
            write_user_bytes_as,
        );
    }
    // Non-regular remote descriptors (devices/directories) issue inline
    // `VFS_IPC_OP_READ` / `VFS_IPC_OP_PREAD64` without prepared cursor mutations.
    read_vfs_inline_chunks(fd, remote_id, user_len, offset, true, |copied, bytes| {
        let dest = user_ptr.checked_add(copied as u64).ok_or(LINUX_EINVAL)?;
        write_user_bytes_as(dest, bytes)
    })
    .map(|copied| copied as u64)
}

fn write_remote_vfs(
    fd: u64,
    remote: &multitask::RemoteVfsHandle,
    user_ptr: u64,
    user_len: u64,
) -> LinuxResult<u64> {
    let user_len = usize::try_from(user_len).map_err(|_| LINUX_EINVAL)?;
    if user_len == 0 {
        return Ok(0);
    }
    let remote_id = remote.remote_id();
    let _pin = RemoteVfsRefGuard::acquire(fd, remote_id)?;
    let _cursor = acquire_vfs_cursor_gate(remote_id)?;
    let is_file = remote.kind() == multitask::RemoteVfsHandleKind::File;
    let mut written = 0_usize;
    while written < user_len {
        let chunk_len = (user_len - written).min(VFS_IPC_REQUEST_PAYLOAD_CAPACITY);
        let Some(chunk_user_ptr) = user_ptr.checked_add(written as u64) else {
            return partial_or_err(written, LINUX_EFAULT);
        };
        let observed_epoch = if is_file {
            ipc_ops::service_endpoint_epoch(IPC_SERVICE_VFSD).unwrap_or(0)
        } else {
            0
        };
        let mut request = new_vfs_request(VFS_IPC_OP_WRITE);
        request.fd = fd;
        request.remote_id = remote_id;
        request.payload_len = chunk_len as u32;
        if is_file {
            request.flags |= VFS_WRITE_FLAG_CURSOR_SYNC;
            request.arg0 = remote.cursor();
        }
        if let Err(errno) = read_user_bytes_as(chunk_user_ptr, &mut request.payload[..chunk_len]) {
            return partial_or_err(written, errno);
        }
        let response = match call_vfs_ipc_request(&request).and_then(|response| {
            ensure_vfs_status(&response)?;
            Ok(response)
        }) {
            Ok(response) => response,
            Err(errno) => return partial_or_err(written, errno),
        };
        // A provider cannot have accepted more than it was offered.
        let chunk_written = response.value as usize;
        if chunk_written > chunk_len {
            return partial_or_err(written, LINUX_EINVAL);
        }
        if is_file {
            remote.set_synced_cursor(response.aux, observed_epoch);
        }
        written += chunk_written;
        if chunk_written == 0 || chunk_written < chunk_len {
            break;
        }
        if written < user_len {
            multitask::cond_resched();
        }
    }
    Ok(written as u64)
}

fn gather_iovecs_into(iovecs: &[linux_abi::LinuxIovec], dest: &mut [u8]) -> LinuxResult<()> {
    let mut offset = 0usize;
    for iov in iovecs {
        if offset >= dest.len() {
            break;
        }
        let len = usize::try_from(iov.iov_len)
            .map_err(|_| LINUX_EINVAL)?
            .min(dest.len() - offset);
        if len == 0 {
            continue;
        }
        read_user_bytes_as(iov.iov_base, &mut dest[offset..offset + len])?;
        offset += len;
    }
    Ok(())
}

fn iovecs_total_len(iovecs: &[linux_abi::LinuxIovec]) -> LinuxResult<usize> {
    let mut total = 0_usize;
    for iov in iovecs {
        total = total
            .checked_add(usize::try_from(iov.iov_len).map_err(|_| LINUX_EINVAL)?)
            .filter(|total| *total <= isize::MAX as usize)
            .ok_or(LINUX_EINVAL)?;
    }
    Ok(total)
}

fn vectored_io_at(
    fd: u64,
    iov_ptr: u64,
    iovcnt: u64,
    current_offset: Option<u64>,
    step: impl FnMut(u64, u64, u64, Option<u64>) -> u64,
) -> LinuxResult<u64> {
    let iovecs = super::vfs_meta::read_current_iovecs_payload(iov_ptr, iovcnt)?;
    vectored_io_with_iovecs(fd, iovecs, current_offset, step)
}

fn vectored_io_with_iovecs(
    fd: u64,
    iovecs: Vec<linux_abi::LinuxIovec>,
    mut current_offset: Option<u64>,
    mut step: impl FnMut(u64, u64, u64, Option<u64>) -> u64,
) -> LinuxResult<u64> {
    if iovecs_total_len(&iovecs)? == 0 {
        return Ok(0);
    }
    let mut total = 0_usize;
    for iov in iovecs {
        let mut done_for_iov = 0_u64;
        while done_for_iov < iov.iov_len {
            let Some(chunk_ptr) = iov.iov_base.checked_add(done_for_iov) else {
                return partial_or_err(total, LINUX_EFAULT);
            };
            let chunk_len = iov.iov_len - done_for_iov;
            if let Some(pos) = current_offset
                && pos > i64::MAX as u64
            {
                return partial_or_err(total, LINUX_EOVERFLOW);
            }
            let result = match kernel_vfs_result(step(fd, chunk_ptr, chunk_len, current_offset)) {
                Ok(count) => count,
                Err(errno) => return partial_or_err(total, errno),
            };
            if result == 0 {
                return Ok(total as u64);
            }
            let step_bytes = usize::try_from(result).map_err(|_| LINUX_EINVAL)?;
            total = total.checked_add(step_bytes).ok_or(LINUX_EINVAL)?;
            if let Some(pos) = &mut current_offset {
                let Some(next) = pos.checked_add(result) else {
                    return partial_or_err(total, LINUX_EOVERFLOW);
                };
                *pos = next;
            }
            done_for_iov = done_for_iov.checked_add(result).ok_or(LINUX_EINVAL)?;
            if result < chunk_len {
                return Ok(total as u64);
            }
        }
    }
    Ok(total as u64)
}

fn new_netd_socket_request(
    op: u16,
    socket_token: u64,
    deadline_ns: u64,
) -> alloc::boxed::Box<NetdIpcRequest> {
    let mut operation = [0_u8; 16];
    nucleus_core::util::random::Random::new().fill_bytes(&mut operation);
    let mut request = new_netd_request(op);
    request.socket_token = socket_token;
    request.deadline_ns = deadline_ns;
    request.operation_hi = u64::from_le_bytes(operation[..8].try_into().unwrap());
    request.operation_lo = u64::from_le_bytes(operation[8..].try_into().unwrap());
    if request.operation_hi == 0 && request.operation_lo == 0 {
        request.operation_lo = 1;
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
            match send_netd_ref_ack_with_timeout(
                &request,
                rustos_user_abi::performance::IPC_STATE_MUTATION_HARD_LIMIT_MS,
            ) {
                Ok(()) => true,
                Err(errno) if errno == LINUX_EBADF => true,
                Err(_) => false,
            }
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
                    return attempted + 1;
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
        attempted += 1;
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
    TimerFd(multitask::TimerFdHandle),
    UnixSocket(multitask::UnixSocketHandle),
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

pub fn release_closed_handles(handles: impl IntoIterator<Item = multitask::KernelHandle>) {
    let closed: Vec<multitask::KernelHandle> = handles.into_iter().collect();
    if closed.is_empty() {
        return;
    }
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

pub fn release_all_service_handle_refs(process_id: u64) {
    // vfsd drops this process's POSIX locks from the lifecycle exit event; only
    // the kernel-side "may hold locks" mark must not outlive the pid.
    super::super::file_lock_ops::forget_process(process_id);
    // Callers have already sealed the process as exiting, which revokes its
    // visibility; the teardown accessor is the one path that still reaches the
    // handle table, so every open-description reference is released exactly
    // once here instead of leaking with the process object.
    let closed = multitask::with_process_state_for_teardown_mut(process_id, |state| {
        let mut closed = state.handles_mut().close_all();
        closed.append(&mut state.take_deferred_fork_handle_drops());
        closed
    })
    .unwrap_or_default();
    // Derive provider releases from the exact handles removed under the
    // process-state mutation. A pre-close snapshot races dup/close/fd reuse
    // and can release the wrong service object or leak the one actually
    // removed during exit.
    release_closed_handles(closed);
}

pub fn purge_closed_console_handles(handles: Vec<multitask::ConsoleHandle>, _bounded: bool) {
    let mut descriptions = BTreeMap::new();
    for handle in handles {
        descriptions.insert(handle.token_id(), handle);
    }
    for (token, handle) in descriptions {
        if !handle.is_last_reference() {
            continue;
        }
        purge_kernel_epoll_object(WAITSET_PROVIDER_SESSIOND, token);
    }
}

fn release_service_handle_refs_inner(refs: &[ServiceHandleRef], bounded: bool) {
    let mut worklist: Vec<ServiceHandleRef> = refs.to_vec();
    while let Some(handle_ref) = worklist.pop() {
        match handle_ref {
            ServiceHandleRef::Socket(token) => {
                let closed = if bounded {
                    call_netd_socket_token_op_bounded(SYSCALL_OFFLOAD_OP_LINUX_CLOSE, token)
                } else {
                    call_netd_socket_token_op(SYSCALL_OFFLOAD_OP_LINUX_CLOSE, token)
                };
                if closed == Ok(0) {
                    purge_kernel_epoll_object(WAITSET_PROVIDER_NETD, token);
                }
            }
            ServiceHandleRef::RemoteVfs(remote_id) => {
                if release_remote_vfs_descriptor_ref(remote_id) == Ok(true) {
                    let _ = if bounded {
                        call_vfs_remote_close_bounded(remote_id)
                    } else {
                        call_vfs_remote_close(remote_id)
                    };
                    purge_kernel_epoll_object(WAITSET_PROVIDER_VFSD, remote_id);
                }
            }
            ServiceHandleRef::Epoll(epoll) => {
                if epoll.release_descriptor_reference() {
                    retire_kernel_epoll(epoll.token_id());
                }
            }
            ServiceHandleRef::Input(token) => {
                if super::super::broker_ops::waitset_broker_ops::release_input_open_description(
                    token,
                ) == Ok(true)
                {
                    purge_kernel_epoll_object(WAITSET_PROVIDER_INPUTD, token);
                }
            }
            ServiceHandleRef::Pipe(pipe) => super::local_fd::release_pipe(&pipe, bounded),
            ServiceHandleRef::EventFd(eventfd) => {
                super::local_fd::release_eventfd(&eventfd, bounded)
            }
            ServiceHandleRef::TimerFd(timerfd) => {
                super::local_fd::release_timerfd(&timerfd, bounded)
            }
            ServiceHandleRef::UnixSocket(sock) => {
                let (release, drained) = sock.release_descriptor_reference();
                super::local_fd::settle_release(
                    release,
                    sock.epoll_registered(),
                    sock.token_id(),
                    bounded,
                );
                for passed in drained {
                    if let Some(next_ref) = service_handle_ref_for_handle(passed.handle()) {
                        worklist.push(next_ref);
                    }
                }
            }
        }
    }
}

pub fn release_service_handle_refs(refs: &[ServiceHandleRef]) {
    release_service_handle_refs_inner(refs, false);
}

pub fn release_service_handle_refs_bounded(refs: &[ServiceHandleRef]) {
    release_service_handle_refs_inner(refs, true);
}

pub fn service_handle_ref_for_handle(handle: &multitask::KernelHandle) -> Option<ServiceHandleRef> {
    match handle {
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
        multitask::KernelHandle::TimerFd(timerfd) => {
            Some(ServiceHandleRef::TimerFd(timerfd.clone()))
        }
        multitask::KernelHandle::UnixSocket(sock) => {
            Some(ServiceHandleRef::UnixSocket(sock.clone()))
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
        ServiceHandleRef::TimerFd(timerfd) => timerfd
            .try_acquire_descriptor_reference()
            .then_some(())
            .ok_or(LINUX_EBADF),
        ServiceHandleRef::UnixSocket(sock) => sock
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
    let response = if status_flags & linux_abi::O_NONBLOCK != 0 {
        call_netd_ipc_request_with_timeout(&request, rustos_user_abi::performance::IPC_INTERACTIVE_CONTROL_HARD_LIMIT_MS)
            .map_err(|errno| socket_writev_errno(errno, true))?
    } else {
        call_netd_ipc_request(&request)?
    };
    if response.value > payload.len() as u64 { return Err(LINUX_EIO); }
    Ok(response.value)
}

fn socket_writev_errno(errno: i64, nonblocking: bool) -> i64 {
    if nonblocking && errno == LINUX_ETIMEDOUT { LINUX_EAGAIN } else { errno }
}

fn socket_read_with_length(len: u64, read: impl FnOnce() -> u64) -> u64 {
    if len == 0 { 0 } else { read() }
}

pub fn syscall_linux_inotify_init() -> u64 {
    syscall_linux_inotify_init1(0)
}

pub fn syscall_linux_inotify_init1(flags: u64) -> u64 {
    linux_result((|| {
        if flags & !(linux_abi::IN_NONBLOCK | linux_abi::IN_CLOEXEC) != 0 {
            return Err(LINUX_EINVAL);
        }
        let mut request = new_vfs_request(VFS_IPC_OP_INOTIFY_INIT);
        request.arg0 = flags;
        request.arg3 = mint_service_object_id();
        let response = call_remote_vfs_open_and_check(&request, request.arg3)?;
        let open_flags = linux_abi::O_RDONLY | (flags & (linux_abi::O_NONBLOCK | linux_abi::O_CLOEXEC));
        install_remote_vfs_response(response, request.arg3, open_flags, false)
    })())
}

fn current_inotify_handle(fd: u64) -> LinuxResult<multitask::RemoteVfsHandle> {
    let Some(remote) = current_remote_vfs_handle(fd) else {
        return Err(if current_fd_exists(fd) { LINUX_EINVAL } else { LINUX_EBADF });
    };
    if remote.kind() != multitask::RemoteVfsHandleKind::Inotify {
        return Err(LINUX_EINVAL);
    }
    Ok(remote)
}

pub fn syscall_linux_inotify_add_watch(fd: u64, pathname: u64, mask: u64) -> u64 {
    linux_result((|| {
        let remote = current_inotify_handle(fd)?;
        let mask_u32 = mask as u32;
        if (mask_u32 & (linux_abi::IN_MASK_ADD | linux_abi::IN_MASK_CREATE))
            == (linux_abi::IN_MASK_ADD | linux_abi::IN_MASK_CREATE)
        {
            return Err(LINUX_EINVAL);
        }
        let allowed = linux_abi::IN_ALL_EVENTS
            | linux_abi::IN_UNMOUNT
            | linux_abi::IN_Q_OVERFLOW
            | linux_abi::IN_IGNORED
            | linux_abi::IN_ONLYDIR
            | linux_abi::IN_DONT_FOLLOW
            | linux_abi::IN_EXCL_UNLINK
            | linux_abi::IN_MASK_CREATE
            | linux_abi::IN_MASK_ADD
            | linux_abi::IN_ISDIR
            | linux_abi::IN_ONESHOT;
        if mask_u32 & !allowed != 0 || mask_u32 & linux_abi::IN_ALL_EVENTS == 0 {
            return Err(LINUX_EINVAL);
        }
        let path = copy_current_user_path(pathname, VFS_IPC_PATH_CAPACITY)?;
        let mut request = new_vfs_request(VFS_IPC_OP_INOTIFY_ADD_WATCH);
        request.fd = fd;
        request.remote_id = remote.remote_id();
        request.arg0 = u64::from(mask_u32);
        populate_vfs_path_base(&mut request, linux_abi::AT_FDCWD as u64, &path)?;
        Ok(call_vfs_checked(&request)?.value)
    })())
}

pub fn syscall_linux_inotify_rm_watch(fd: u64, wd: u64) -> u64 {
    linux_unit_result((|| {
        let remote = current_inotify_handle(fd)?;
        let wd_i32 = wd as u32 as i32;
        if wd_i32 <= 0 {
            return Err(LINUX_EINVAL);
        }
        call_vfs_fd_status(VFS_IPC_OP_INOTIFY_RM_WATCH, fd, remote.remote_id(), |request| {
            request.arg0 = u64::from(wd_i32 as u32);
        })
    })())
}

pub(super) fn try_inotify_fionread(fd: u64, arg: u64) -> Option<u64> {
    let remote = current_remote_vfs_handle(fd)?;
    if remote.kind() != multitask::RemoteVfsHandleKind::Inotify {
        return None;
    }
    Some(linux_unit_result((|| {
        let mut request = new_vfs_request(VFS_IPC_OP_FCNTL);
        request.fd = fd;
        request.remote_id = remote.remote_id();
        request.arg0 = linux_abi::FIONREAD;
        let bytes = call_vfs_checked(&request)?.value.min(i32::MAX as u64) as i32;
        write_user_bytes_as(arg, &bytes.to_le_bytes())
    })()))
}

struct WaitsetWaitersGuard(u64);

impl Drop for WaitsetWaitersGuard {
    fn drop(&mut self) {
        super::super::broker_ops::waitset_broker_ops::remove_waitset_waiters(self.0);
    }
}

fn read_inotify_vfs(fd: u64, remote_id: u64, user_ptr: u64, user_len: u64) -> LinuxResult<u64> {
    let _pin = RemoteVfsRefGuard::acquire(fd, remote_id)?;
    read_inotify_vfs_pinned(fd, remote_id, user_ptr, user_len)
}

fn read_inotify_vfs_pinned(
    fd: u64,
    remote_id: u64,
    user_ptr: u64,
    user_len: u64,
) -> LinuxResult<u64> {
    let user_len = usize::try_from(user_len).map_err(|_| LINUX_EINVAL)?;
    if user_len < core::mem::size_of::<linux_abi::LinuxInotifyEventHeader>() {
        return Err(LINUX_EINVAL);
    }
    validate_user_write_as(user_ptr, user_len)?;
    let chunk_len = user_len.min(VFS_IPC_PAYLOAD_CAPACITY);
    let issue_read = || -> LinuxResult<alloc::boxed::Box<VfsIpcResponse>> {
        let mut request = new_vfs_request(VFS_IPC_OP_READ);
        request.fd = fd;
        request.remote_id = remote_id;
        request.arg1 = chunk_len as u64;
        call_vfs_ipc_request(&request)
    };
    let copy_events = |response: &VfsIpcResponse| -> LinuxResult<u64> {
        let count = response.payload_len as usize;
        if count == 0 || count > chunk_len || count > response.payload.len() {
            return Err(LINUX_EINVAL);
        }
        write_user_bytes_as(user_ptr, &response.payload[..count])?;
        Ok(count as u64)
    };
    loop {
        let response = issue_read()?;
        match check_ipc_status(response.status) {
            Ok(()) => return copy_events(&response),
            Err(LINUX_EAGAIN) => {}
            Err(errno) => return Err(errno),
        }
        let status_flags = current_fd_status_flags(fd).ok_or(LINUX_EBADF)?;
        if status_flags & linux_abi::O_NONBLOCK != 0 {
            return Err(LINUX_EAGAIN);
        }
        if current_wait_interrupted() {
            return Err(LINUX_EINTR);
        }
        let task_id = multitask::current_task_id().ok_or(LINUX_EINVAL)?;
        let process_id = multitask::current_user_process_id().ok_or(LINUX_EINVAL)?;
        let observation = [super::super::broker_ops::waitset_broker_ops::ProviderObservation {
            provider: rustos_user_abi::syscall::WAITSET_PROVIDER_VFSD,
            object_id: remote_id,
            generation: response.aux.max(1),
        }];
        super::super::broker_ops::waitset_broker_ops::register_waitset_waiters(
            task_id,
            process_id,
            &observation,
        )?;
        let waiters = WaitsetWaitersGuard(task_id);
        let second = issue_read()?;
        match check_ipc_status(second.status) {
            Ok(()) => return copy_events(&second),
            Err(LINUX_EAGAIN) => {}
            Err(errno) => return Err(errno),
        }
        if second.aux.max(1) != observation[0].generation {
            continue;
        }
        if !multitask::arm_block_current_task() {
            drop(waiters);
            if multitask::current_task_id() != Some(task_id) {
                return Err(LINUX_EINVAL);
            }
            multitask::cond_resched();
            continue;
        }
        if !super::super::broker_ops::waitset_broker_ops::waitset_waiters_match(
            task_id,
            process_id,
            &observation,
        ) {
            drop(waiters);
            let _ = multitask::cancel_block_current_task();
            continue;
        }
        if super::poll_epoll::has_unmasked_pending_signal() {
            drop(waiters);
            let _ = multitask::cancel_block_current_task();
            if current_wait_interrupted() {
                return Err(LINUX_EINTR);
            }
            continue;
        }
        match multitask::commit_block_current_task_and_yield() {
            Some(true) | Some(false) => {}
            None => return Err(LINUX_EINVAL),
        }
        drop(waiters);
        if current_wait_interrupted() {
            return Err(LINUX_EINTR);
        }
    }
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

    #[test]
    fn close_range_bounds_reject_inverted_and_unshare_without_u64_max_overflow() {
        assert_eq!(validate_close_range(0, u64::MAX, 0), Ok(false));
        assert_eq!(
            validate_close_range(3, u64::MAX, linux_abi::CLOSE_RANGE_CLOEXEC),
            Ok(true)
        );
        assert_eq!(validate_close_range(5, 4, 0), Err(LINUX_EINVAL));
        assert_eq!(
            validate_close_range(0, 10, linux_abi::CLOSE_RANGE_UNSHARE),
            Err(LINUX_EINVAL)
        );
        assert_eq!(validate_close_range(0, 10, 1 << 30), Err(LINUX_EINVAL));
        assert!((3..=u64::MAX).contains(&3));
        assert!((3..=u64::MAX).contains(&u64::MAX));
        assert!(!(3..=u64::MAX).contains(&2));
    }

    #[test]
    fn fcntl_dupfd_min_rejects_negative_and_out_of_range_bounds() {
        assert_eq!(validate_fcntl_dupfd_min(0), Ok(0));
        assert_eq!(
            validate_fcntl_dupfd_min(multitask::MAX_DYNAMIC_FD),
            Ok(multitask::MAX_DYNAMIC_FD)
        );
        assert_eq!(
            validate_fcntl_dupfd_min(multitask::MAX_DYNAMIC_FD + 1),
            Err(LINUX_EINVAL)
        );
        assert_eq!(
            validate_fcntl_dupfd_min((-1_i32 as u32) as u64),
            Err(LINUX_EINVAL)
        );
        assert_eq!(validate_fcntl_dupfd_min((-1_i64) as u64), Err(LINUX_EINVAL));
    }

    #[test]
    fn pinned_remote_vfs_fd_selects_dirfd_for_all_at_path_ops() {
        let mut req = VfsIpcRequest {
            dirfd: 11,
            fd: 22,
            remote_id: 99,
            ..VfsIpcRequest::default()
        };
        for op in [
            VFS_IPC_OP_OPENAT,
            VFS_IPC_OP_STATX,
            VFS_IPC_OP_NEWFSTATAT,
            VFS_IPC_OP_READLINKAT,
            VFS_IPC_OP_ACCESS,
            VFS_IPC_OP_CHDIR,
            VFS_IPC_OP_MKDIR,
            VFS_IPC_OP_UNLINKAT,
            VFS_IPC_OP_RENAMEAT2,
            VFS_IPC_OP_SYMLINKAT,
            VFS_IPC_OP_MKNOD,
            VFS_IPC_OP_CHMOD,
            VFS_IPC_OP_CHOWN,
        ] {
            req.op = op;
            assert_eq!(pinned_remote_vfs_fd(&req), 11, "op={op}");
        }
        req.op = VFS_IPC_OP_SET_ATTRS;
        req.path_len = 0;
        assert_eq!(pinned_remote_vfs_fd(&req), 22);
        req.path_len = 4;
        assert_eq!(pinned_remote_vfs_fd(&req), 11);

        req.op = VFS_IPC_OP_LINKAT;
        req.flags = 0;
        assert_eq!(pinned_remote_vfs_fd(&req), 11);
        req.flags = VFS_LINKAT_FLAG_SOURCE_DESCRIPTION;
        assert_eq!(pinned_remote_vfs_fd(&req), 22);

        req.op = VFS_IPC_OP_FCNTL;
        assert_eq!(pinned_remote_vfs_fd(&req), 22);
    }

    #[test]
    fn vectored_io_validation_enforces_rights_and_non_seekable_espipe() {
        let sock = multitask::KernelHandle::InetSocket(multitask::InetSocketHandle::from_token(
            5,
            linux_abi::AF_INET,
            linux_abi::SOCK_STREAM,
            6,
        ));
        let sock_rights = sock.default_rights(0);
        assert_eq!(validate_handle_entry_for_io(&sock, sock_rights, false, false), Ok(()));
        assert_eq!(validate_handle_entry_for_io(&sock, sock_rights, true, false), Ok(()));
        assert_eq!(
            validate_handle_entry_for_io(&sock, sock_rights, false, true),
            Err(LINUX_ESPIPE)
        );
        assert_eq!(
            validate_handle_entry_for_io(&sock, sock_rights, true, true),
            Err(LINUX_ESPIPE)
        );

        let epoll = multitask::KernelHandle::Epoll(multitask::EpollHandle::new());
        let epoll_rights = epoll.default_rights(0);
        assert_eq!(
            validate_handle_entry_for_io(&epoll, epoll_rights, false, false),
            Err(LINUX_EBADF)
        );
        assert_eq!(
            validate_handle_entry_for_io(&epoll, epoll_rights, false, true),
            Err(LINUX_ESPIPE)
        );

        let inotify = multitask::KernelHandle::RemoteVfs(
            multitask::RemoteVfsHandle::new(
                10,
                multitask::RemoteVfsHandleKind::Inotify,
                alloc::string::String::from("anon_inode:inotify"),
                0,
                0,
            )
            .unwrap(),
        );
        let inotify_rights = inotify.default_rights(0);
        assert_eq!(
            validate_handle_entry_for_io(&inotify, inotify_rights, false, false),
            Ok(())
        );
        assert_eq!(
            validate_handle_entry_for_io(&inotify, inotify_rights, false, true),
            Err(LINUX_ESPIPE)
        );
        assert_eq!(
            validate_handle_entry_for_io(&inotify, inotify_rights, true, false),
            Err(LINUX_EBADF)
        );

        let opath = multitask::KernelHandle::RemoteVfs(
            multitask::RemoteVfsHandle::new(
                11,
                multitask::RemoteVfsHandleKind::File,
                alloc::string::String::from("/data/f"),
                0,
                0,
            )
            .unwrap(),
        );
        let opath_rights = opath.default_rights(linux_abi::O_PATH);
        assert_eq!(
            validate_handle_entry_for_io(&opath, opath_rights, false, false),
            Err(LINUX_EBADF)
        );
        assert_eq!(
            validate_handle_entry_for_io(&opath, opath_rights, true, true),
            Err(LINUX_EBADF)
        );
    }

    #[test]
    fn iovecs_total_len_rejects_isize_overflow() {
        let ok = [
            linux_abi::LinuxIovec { iov_base: 0, iov_len: 1024 },
            linux_abi::LinuxIovec { iov_base: 0, iov_len: 0 },
        ];
        assert_eq!(iovecs_total_len(&ok), Ok(1024));
        let overflow = [
            linux_abi::LinuxIovec { iov_base: 0, iov_len: isize::MAX as u64 },
            linux_abi::LinuxIovec { iov_base: 0, iov_len: 1 },
        ];
        assert_eq!(iovecs_total_len(&overflow), Err(LINUX_EINVAL));
    }
}

fn read_socket_writev_payload(iov_ptr: u64, iovcnt: u64, datagram: bool) -> Result<Vec<u8>, i64> {
    if iovcnt == 0 { return Ok(Vec::new()); }
    let iovecs = super::vfs_meta::read_current_iovecs_payload(iov_ptr, iovcnt)?;
    let total = iovecs_total_len(&iovecs)?;
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
            read_user_bytes_as(iov.iov_base, &mut payload[start..])?;
        }
        if payload.len() == capacity { break; }
    }
    Ok(payload)
}
// RING3-MIGRATION-REFERENCE END: vfsd/netd fd-usercopy substrate exception.
