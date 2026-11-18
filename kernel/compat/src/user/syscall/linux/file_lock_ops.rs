//! Linux advisory file locks: `fcntl(F_GETLK/F_SETLK/F_SETLKW)`, the OFD
//! variants (`F_OFD_*`) and `flock(2)`.
//!
//! - **Owner:** vfsd owns every lock rule (`services/vfsd/src/locks.rs`):
//!   range arithmetic, owners, conflicts, deadlock detection, bounds, release
//!   on description close and on process exit. This file is mechanism only: it
//!   copies `struct flock` in and out (`EFAULT`), checks fd access modes, and
//!   marshals one `VfsFileLockWire` (`VFS_IPC_OP_FILE_LOCK`).
//! - **Boundary:** only the live descriptor capability and kernel-stamped
//!   process/thread identity authorize requests; a user flock PID is not authority.
//! - **Lifecycle:** the provider retires a lock on process exit or final
//!   description close according to its owner type.
//! - **Concurrency:** pin the description for each syscall, including waits.
//! - **Forbidden:** running inode conflict or deadlock policy in Ring0.
//! - **Evidence:** lock-wire, range and owner tests, plus the Chrome guest probe.
//! - **Blocking:** vfsd never waits (single receiver). A blocking request is a
//!   bounded poll here: each attempt is a non-blocking `SETLKW` that makes
//!   vfsd remember the waiter; `EAGAIN` means "still held", `EDEADLK` ends the
//!   wait, and between attempts the thread sleeps `1..=MAX_RETRY_SLEEP_MS`
//!   ms, cut short by a deliverable signal (`EINTR`; the waiter is cancelled).
//! - **POSIX close semantics:** closing *any* fd of a file drops all of the
//!   process's POSIX locks on it. The kernel therefore tells vfsd about a
//!   close, but only for processes that ever took a POSIX lock
//!   ([`PosixLockPids`]), so ordinary closes cost no IPC. The notice precedes
//!   the description reference drop, so vfsd still knows the description.
//! - **`CLOEXEC` at `exec`:** the process-table commit closes the descriptors
//!   while the loader (not the exec'ing process) runs the broker, so
//!   [`note_exec_cloexec_closed`] sends the same `RELEASE_FILE` with the
//!   exec'ing pid in `VfsIpcRequest::pid`. vfsd accepts a pid that is not the
//!   IPC sender only from the live loaderd owner, only for a POSIX
//!   `RELEASE_FILE` and only on the sender's own thread
//!   (`vfsd::lock_release_proxy_shape`); an ordinary client cannot name
//!   another pid. The notice is best effort like [`note_fd_closed`]: if vfsd is
//!   unreachable the lock lasts until the process exits.
//! - **Not covered:** descriptions that are not remote-VFS files (pipes,
//!   sockets, memfd, eventfd, epoll, devices) are `EINVAL`, not silently
//!   unlocked.
//! - **Failure:** a conflict is `EAGAIN` for `F_SETLK` (Linux may also use
//!   `EACCES`; RustOS always reports `EAGAIN`). Table overflow is `ENOLCK`.

use core::sync::atomic::{AtomicU64, Ordering};

use rustos_user_abi::linux::LinuxFlock;
use rustos_user_abi::syscall::{
    VfsFileLockWire, VFS_FILE_LOCK_CMD_CANCEL_WAIT, VFS_FILE_LOCK_CMD_GETLK,
    VFS_FILE_LOCK_CMD_RELEASE_FILE, VFS_FILE_LOCK_CMD_SETLK, VFS_FILE_LOCK_CMD_SETLKW,
    VFS_FILE_LOCK_OWNER_FLOCK, VFS_FILE_LOCK_OWNER_OFD, VFS_FILE_LOCK_OWNER_POSIX,
    VFS_FILE_LOCK_TYPE_RDLCK, VFS_FILE_LOCK_TYPE_UNLCK, VFS_FILE_LOCK_TYPE_WRLCK,
    VFS_FILE_LOCK_WHENCE_END, VFS_FILE_LOCK_WIRE_VERSION, VFS_IPC_OP_FILE_LOCK,
};

use super::*;

const LINUX_EDEADLK: i64 = 35;
const LINUX_ENOLCK: i64 = 37;

/// A lock attempt mutates provider state and needs the full mutation lease.
const LOCK_IPC_TIMEOUT_MS: u64 = rustos_user_abi::performance::IPC_STATE_MUTATION_HARD_LIMIT_MS;
/// Poll interval ceiling for a blocked lock request (first retry is 1 ms).
const MAX_RETRY_SLEEP_MS: u64 = 32;

// ---- processes that may hold POSIX locks ---------------------------------

const POSIX_LOCK_PID_SLOTS: usize = 256;

/// Lock-free set of pids that took a POSIX record lock. A false positive only
/// costs one vfsd round trip on a close; a false negative would leak a lock,
/// so a pid is inserted *before* the request that may create the lock.
pub(super) struct PosixLockPids {
    slots: [AtomicU64; POSIX_LOCK_PID_SLOTS],
}

impl PosixLockPids {
    pub(super) const fn new() -> Self {
        Self {
            slots: [const { AtomicU64::new(0) }; POSIX_LOCK_PID_SLOTS],
        }
    }

    /// `false` when the set is full (the caller reports `ENOLCK`).
    pub(super) fn insert(&self, pid: u64) -> bool {
        debug_assert!(pid != 0);
        for slot in &self.slots {
            // ORDERING: publish the PID before issuing a provider lock mutation.
            match slot.compare_exchange(0, pid, Ordering::AcqRel, Ordering::Acquire) {
                Ok(_) => return true,
                Err(current) if current == pid => return true,
                Err(_) => {}
            }
        }
        false
    }

    pub(super) fn contains(&self, pid: u64) -> bool {
        // ORDERING: observe the PID published before the potential mutation.
        self.slots.iter().any(|slot| slot.load(Ordering::Acquire) == pid)
    }

    /// Clears every slot naming `pid` (a hole-reuse race can duplicate one).
    pub(super) fn remove(&self, pid: u64) {
        for slot in &self.slots {
            // ORDERING: remove only the retiring subject's published PID.
            let _ = slot.compare_exchange(pid, 0, Ordering::AcqRel, Ordering::Acquire);
        }
    }
}

static POSIX_LOCK_PIDS: PosixLockPids = PosixLockPids::new();

fn current_pid() -> Option<u64> {
    multitask::current_user_snapshot().map(|snapshot| snapshot.process_id())
}

/// Process exit: forget the pid. vfsd drops the locks from the lifecycle exit
/// event; the pid may be reused, so a stale entry must not outlive the process.
pub(super) fn forget_process(process_id: u64) {
    POSIX_LOCK_PIDS.remove(process_id);
}

/// Called with a handle this process just removed from its fd table, before
/// its description reference is dropped. Best effort: if vfsd is unreachable
/// the pid stays registered, so the next close of any file retries.
pub(super) fn note_fd_closed(handle: &multitask::KernelHandle) {
    let multitask::KernelHandle::RemoteVfs(remote) = handle else {
        return;
    };
    let Some(pid) = current_pid() else { return };
    if !POSIX_LOCK_PIDS.contains(pid) {
        return;
    }
    let wire = control_wire(VFS_FILE_LOCK_CMD_RELEASE_FILE, VFS_FILE_LOCK_OWNER_POSIX);
    let _ = send(0, remote.remote_id(), &wire);
}

/// Remote descriptions among `closed` (deduplicated, order kept).
fn remote_ids_of(closed: &[multitask::KernelHandle]) -> alloc::vec::Vec<u64> {
    let mut ids = alloc::vec::Vec::new();
    for handle in closed {
        if let multitask::KernelHandle::RemoteVfs(remote) = handle {
            let id = remote.remote_id();
            if !ids.contains(&id) {
                ids.push(id);
            }
        }
    }
    ids
}

/// `exec` of `target_pid` just closed its `CLOEXEC` descriptors `closed`. The
/// caller is the loader, so this names the exec'ing pid explicitly (see the
/// module docs); the descriptions are still referenced, so vfsd still knows
/// them. The pid stays registered: the process lives on and may still hold
/// POSIX locks through descriptors that survived the exec.
pub(super) fn note_exec_cloexec_closed(target_pid: u64, closed: &[multitask::KernelHandle]) {
    if target_pid == 0 || !POSIX_LOCK_PIDS.contains(target_pid) {
        return;
    }
    let wire = control_wire(VFS_FILE_LOCK_CMD_RELEASE_FILE, VFS_FILE_LOCK_OWNER_POSIX);
    for remote_id in remote_ids_of(closed) {
        let _ = send_for(Some(target_pid), 0, remote_id, &wire);
    }
}

// ---- wire construction (pure) --------------------------------------------

fn control_wire(cmd: u16, owner: u16) -> VfsFileLockWire {
    VfsFileLockWire {
        version: VFS_FILE_LOCK_WIRE_VERSION,
        cmd,
        owner,
        lock_type: VFS_FILE_LOCK_TYPE_UNLCK,
        ..VfsFileLockWire::default()
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum FcntlLockCmd {
    Get,
    Set,
    SetWait,
}

/// `(command, owner)` for an `fcntl` command number; `None` if not a lock one.
fn classify_fcntl(cmd: u64) -> Option<(FcntlLockCmd, u16)> {
    Some(match cmd {
        linux_abi::F_GETLK | linux_abi::F_GETLK64 => (FcntlLockCmd::Get, VFS_FILE_LOCK_OWNER_POSIX),
        linux_abi::F_SETLK | linux_abi::F_SETLK64 => (FcntlLockCmd::Set, VFS_FILE_LOCK_OWNER_POSIX),
        linux_abi::F_SETLKW | linux_abi::F_SETLKW64 => {
            (FcntlLockCmd::SetWait, VFS_FILE_LOCK_OWNER_POSIX)
        }
        linux_abi::F_OFD_GETLK => (FcntlLockCmd::Get, VFS_FILE_LOCK_OWNER_OFD),
        linux_abi::F_OFD_SETLK => (FcntlLockCmd::Set, VFS_FILE_LOCK_OWNER_OFD),
        linux_abi::F_OFD_SETLKW => (FcntlLockCmd::SetWait, VFS_FILE_LOCK_OWNER_OFD),
        _ => return None,
    })
}

/// Validates a user `struct flock` for `cmd`/`owner` and builds the wire.
fn fcntl_wire(cmd: FcntlLockCmd, owner: u16, flock: &LinuxFlock) -> Result<VfsFileLockWire, i64> {
    let lock_type = match flock.l_type {
        linux_abi::F_RDLCK => VFS_FILE_LOCK_TYPE_RDLCK,
        linux_abi::F_WRLCK => VFS_FILE_LOCK_TYPE_WRLCK,
        linux_abi::F_UNLCK => VFS_FILE_LOCK_TYPE_UNLCK,
        _ => return Err(LINUX_EINVAL),
    };
    if !(0..=VFS_FILE_LOCK_WHENCE_END as i16).contains(&flock.l_whence) {
        return Err(LINUX_EINVAL);
    }
    // An OFD lock has no owning pid; Linux demands l_pid == 0.
    if owner == VFS_FILE_LOCK_OWNER_OFD && flock.l_pid != 0 {
        return Err(LINUX_EINVAL);
    }
    // F_GETLK asks "what would block this": it names a lock type to test.
    if cmd == FcntlLockCmd::Get && lock_type == VFS_FILE_LOCK_TYPE_UNLCK {
        return Err(LINUX_EINVAL);
    }
    Ok(VfsFileLockWire {
        version: VFS_FILE_LOCK_WIRE_VERSION,
        cmd: match cmd {
            FcntlLockCmd::Get => VFS_FILE_LOCK_CMD_GETLK,
            FcntlLockCmd::Set => VFS_FILE_LOCK_CMD_SETLK,
            FcntlLockCmd::SetWait => VFS_FILE_LOCK_CMD_SETLKW,
        },
        owner,
        lock_type,
        whence: flock.l_whence as u16,
        flags: 0,
        start: flock.l_start,
        len: flock.l_len,
        pid: 0,
    })
}

/// Result of `F_GETLK`: the user's struct with the answer written into it.
/// Without a conflict only `l_type` becomes `F_UNLCK`, as on Linux.
fn flock_from_reply(original: &LinuxFlock, reply: &VfsFileLockWire) -> LinuxFlock {
    if reply.lock_type == VFS_FILE_LOCK_TYPE_UNLCK {
        return LinuxFlock { l_type: linux_abi::F_UNLCK, ..*original };
    }
    LinuxFlock {
        l_type: if reply.lock_type == VFS_FILE_LOCK_TYPE_WRLCK {
            linux_abi::F_WRLCK
        } else {
            linux_abi::F_RDLCK
        },
        l_whence: linux_abi::SEEK_SET as i16,
        l_start: reply.start,
        l_len: reply.len,
        l_pid: i32::try_from(reply.pid).unwrap_or(-1),
        ..LinuxFlock::default()
    }
}

/// `(lock_type, blocking)` for `flock(2)`'s `operation`.
fn flock_operation(operation: u64) -> Result<(u16, bool), i64> {
    let blocking = operation & linux_abi::LOCK_NB == 0;
    let lock_type = match operation & !linux_abi::LOCK_NB {
        linux_abi::LOCK_SH => VFS_FILE_LOCK_TYPE_RDLCK,
        linux_abi::LOCK_EX => VFS_FILE_LOCK_TYPE_WRLCK,
        linux_abi::LOCK_UN => VFS_FILE_LOCK_TYPE_UNLCK,
        _ => return Err(LINUX_EINVAL),
    };
    Ok((lock_type, blocking && lock_type != VFS_FILE_LOCK_TYPE_UNLCK))
}

// ---- IPC ---------------------------------------------------------------------

fn send(fd: u64, remote_id: u64, wire: &VfsFileLockWire) -> Result<VfsFileLockWire, i64> {
    send_for(None, fd, remote_id, wire)
}

/// Names the process a request acts for when it is not the IPC sender. The
/// thread stays the sender's: vfsd requires it to match.
fn apply_proxy_pid(request: &mut VfsIpcRequest, on_behalf_pid: Option<u64>) {
    if let Some(pid) = on_behalf_pid {
        request.pid = pid;
    }
}

fn send_for(
    on_behalf_pid: Option<u64>,
    fd: u64,
    remote_id: u64,
    wire: &VfsFileLockWire,
) -> Result<VfsFileLockWire, i64> {
    // A fresh request (fresh operation id) per attempt: polling must never
    // look like a replay of an earlier one.
    let mut request = new_vfs_request(VFS_IPC_OP_FILE_LOCK);
    apply_proxy_pid(&mut request, on_behalf_pid);
    request.fd = fd;
    request.remote_id = remote_id;
    let bytes = wire.to_bytes();
    request.payload[..bytes.len()].copy_from_slice(&bytes);
    request.payload_len = bytes.len() as u32;
    let response = call_vfs_ipc_request_with_timeout(&request, LOCK_IPC_TIMEOUT_MS)?;
    ensure_vfs_status(&response)?;
    let payload = response
        .payload
        .get(..response.payload_len as usize)
        .ok_or(LINUX_EINVAL)?;
    if payload.is_empty() {
        return Ok(*wire);
    }
    VfsFileLockWire::from_bytes(payload).ok_or(LINUX_EINVAL)
}

/// Runs `wire` against vfsd; when `blocking`, polls until it is granted.
fn run(fd: u64, remote_id: u64, wire: &VfsFileLockWire, blocking: bool) -> Result<VfsFileLockWire, i64> {
    let mut sleep_ms = 1_u64;
    loop {
        match send(fd, remote_id, wire) {
            Err(LINUX_EAGAIN) if blocking => {
                if current_wait_interrupted() {
                    cancel_wait(fd, remote_id, wire.owner);
                    return Err(LINUX_EINTR);
                }
                crate::arch::rtc::sleep_unless(sleep_ms, current_wait_interrupted);
                sleep_ms = (sleep_ms * 2).min(MAX_RETRY_SLEEP_MS);
            }
            Err(errno) => {
                if blocking && errno != LINUX_EDEADLK {
                    cancel_wait(fd, remote_id, wire.owner);
                }
                return Err(errno);
            }
            Ok(reply) => return Ok(reply),
        }
    }
}

fn cancel_wait(fd: u64, remote_id: u64, owner: u16) {
    let _ = send(fd, remote_id, &control_wire(VFS_FILE_LOCK_CMD_CANCEL_WAIT, owner));
}

/// Pins the exact description for the whole (possibly sleeping) operation, so
/// a concurrent close cannot retire it between two polls.
fn with_pinned_description<T>(
    fd: u64,
    remote_id: u64,
    body: impl FnOnce() -> Result<T, i64>,
) -> Result<T, i64> {
    acquire_current_remote_vfs_ref(fd, remote_id)?;
    let result = body();
    release_service_handle_refs_bounded(&[ServiceHandleRef::RemoteVfs(remote_id)]);
    result
}

/// `EBADF` for a closed fd, `EINVAL` for a live fd that cannot carry locks.
fn not_lockable_errno(fd: u64) -> i64 {
    let exists = multitask::with_current_user_process_state(|_, _, state| {
        state.handles().get_entry(fd).is_some()
    })
    .unwrap_or(false);
    if exists { LINUX_EINVAL } else { LINUX_EBADF }
}

// ---- syscalls --------------------------------------------------------------

/// `fcntl` lock commands. `None`: `cmd` is not a lock command.
pub(super) fn fcntl(fd: u64, cmd: u64, arg: u64) -> Option<u64> {
    let (kind, owner) = classify_fcntl(cmd)?;
    Some(match fcntl_lock(fd, kind, owner, arg) {
        Ok(value) => value,
        Err(errno) => {
            nucleus_core::debug::println_serialized(format_args!(
                "fcntl_lock failed pid={:?} fd={} cmd={} owner={} errno={}",
                current_pid(),
                fd,
                cmd,
                owner,
                errno
            ));
            linux_errno(errno)
        }
    })
}

fn fcntl_lock(fd: u64, kind: FcntlLockCmd, owner: u16, flock_ptr: u64) -> Result<u64, i64> {
    let Some((remote, rights)) = current_remote_vfs_handle_with_rights(fd) else {
        return Err(not_lockable_errno(fd));
    };
    // An O_PATH description grants neither read nor write: no locks either.
    if !rights.allows_read() && !rights.allows_write() {
        return Err(LINUX_EBADF);
    }
    let flock = usermem::read_current_user_struct::<LinuxFlock>(flock_ptr)
        .map_err(user_pointer_error_to_linux_errno)?;
    let wire = fcntl_wire(kind, owner, &flock)?;
    if kind != FcntlLockCmd::Get {
        // Linux: a read lock needs a readable fd, a write lock a writable one.
        match wire.lock_type {
            VFS_FILE_LOCK_TYPE_RDLCK if !rights.allows_read() => return Err(LINUX_EBADF),
            VFS_FILE_LOCK_TYPE_WRLCK if !rights.allows_write() => return Err(LINUX_EBADF),
            _ => {}
        }
        if owner == VFS_FILE_LOCK_OWNER_POSIX && wire.lock_type != VFS_FILE_LOCK_TYPE_UNLCK {
            let pid = current_pid().ok_or(LINUX_EBADF)?;
            if !POSIX_LOCK_PIDS.insert(pid) {
                return Err(LINUX_ENOLCK);
            }
        }
    }
    let remote_id = remote.remote_id();
    let reply = with_pinned_description(fd, remote_id, || {
        run(fd, remote_id, &wire, kind == FcntlLockCmd::SetWait)
    })?;
    if kind == FcntlLockCmd::Get {
        let answer = flock_from_reply(&flock, &reply);
        usermem::write_current_user_bytes(flock_ptr, &answer.to_bytes())
            .map_err(user_pointer_error_to_linux_errno)?;
    }
    Ok(0)
}

/// `flock(fd, operation)`.
pub(super) fn syscall_linux_flock(fd: u64, operation: u64) -> u64 {
    match flock_lock(fd, operation) {
        Ok(()) => 0,
        Err(errno) => linux_errno(errno),
    }
}

fn flock_lock(fd: u64, operation: u64) -> Result<(), i64> {
    let Some((remote, rights)) = current_remote_vfs_handle_with_rights(fd) else {
        return Err(not_lockable_errno(fd));
    };
    if !rights.allows_read() && !rights.allows_write() {
        return Err(LINUX_EBADF);
    }
    let (lock_type, blocking) = flock_operation(operation)?;
    let wire = VfsFileLockWire {
        version: VFS_FILE_LOCK_WIRE_VERSION,
        cmd: if blocking { VFS_FILE_LOCK_CMD_SETLKW } else { VFS_FILE_LOCK_CMD_SETLK },
        owner: VFS_FILE_LOCK_OWNER_FLOCK,
        lock_type,
        ..VfsFileLockWire::default()
    };
    let remote_id = remote.remote_id();
    with_pinned_description(fd, remote_id, || run(fd, remote_id, &wire, blocking)).map(|_| ())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn flock(l_type: i16, whence: i16, start: i64, len: i64, pid: i32) -> LinuxFlock {
        LinuxFlock { l_type, l_whence: whence, l_start: start, l_len: len, l_pid: pid, ..LinuxFlock::default() }
    }

    #[test]
    fn every_lock_command_is_classified_and_others_are_not() {
        use FcntlLockCmd::*;
        let posix = VFS_FILE_LOCK_OWNER_POSIX;
        let ofd = VFS_FILE_LOCK_OWNER_OFD;
        assert_eq!(classify_fcntl(5), Some((Get, posix)));
        assert_eq!(classify_fcntl(6), Some((Set, posix)));
        assert_eq!(classify_fcntl(7), Some((SetWait, posix)));
        assert_eq!(classify_fcntl(12), Some((Get, posix)));
        assert_eq!(classify_fcntl(13), Some((Set, posix)));
        assert_eq!(classify_fcntl(14), Some((SetWait, posix)));
        assert_eq!(classify_fcntl(36), Some((Get, ofd)));
        assert_eq!(classify_fcntl(37), Some((Set, ofd)));
        assert_eq!(classify_fcntl(38), Some((SetWait, ofd)));
        for other in [0_u64, 1, 2, 3, 4, 8, 9, 10, 11, 15, 35, 39, 1024, 1030, 1033] {
            assert_eq!(classify_fcntl(other), None, "{other}");
        }
    }

    #[test]
    fn flock_struct_becomes_a_wire_with_unresolved_range() {
        let wire = fcntl_wire(FcntlLockCmd::SetWait, VFS_FILE_LOCK_OWNER_POSIX, &flock(1, 2, -8, -4, 0)).unwrap();
        assert_eq!(wire.cmd, VFS_FILE_LOCK_CMD_SETLKW);
        assert_eq!(wire.owner, VFS_FILE_LOCK_OWNER_POSIX);
        assert_eq!(wire.lock_type, VFS_FILE_LOCK_TYPE_WRLCK);
        assert_eq!((wire.whence, wire.start, wire.len, wire.pid), (2, -8, -4, 0));
        assert!(wire.is_well_formed());
        // POSIX locks ignore a stray l_pid on input (Linux does too).
        assert!(fcntl_wire(FcntlLockCmd::Set, VFS_FILE_LOCK_OWNER_POSIX, &flock(0, 0, 0, 1, 99)).is_ok());
    }

    #[test]
    fn invalid_flock_structs_are_einval() {
        let posix = VFS_FILE_LOCK_OWNER_POSIX;
        let ofd = VFS_FILE_LOCK_OWNER_OFD;
        assert_eq!(fcntl_wire(FcntlLockCmd::Set, posix, &flock(3, 0, 0, 0, 0)), Err(LINUX_EINVAL));
        assert_eq!(fcntl_wire(FcntlLockCmd::Set, posix, &flock(-1, 0, 0, 0, 0)), Err(LINUX_EINVAL));
        assert_eq!(fcntl_wire(FcntlLockCmd::Set, posix, &flock(0, 3, 0, 0, 0)), Err(LINUX_EINVAL));
        assert_eq!(fcntl_wire(FcntlLockCmd::Set, posix, &flock(0, -1, 0, 0, 0)), Err(LINUX_EINVAL));
        assert_eq!(fcntl_wire(FcntlLockCmd::Set, ofd, &flock(0, 0, 0, 0, 5)), Err(LINUX_EINVAL));
        assert!(fcntl_wire(FcntlLockCmd::Set, ofd, &flock(0, 0, 0, 0, 0)).is_ok());
        assert_eq!(fcntl_wire(FcntlLockCmd::Get, posix, &flock(2, 0, 0, 0, 0)), Err(LINUX_EINVAL));
        assert!(fcntl_wire(FcntlLockCmd::Set, posix, &flock(2, 0, 0, 0, 0)).is_ok(), "F_UNLCK is a valid SETLK");
    }

    #[test]
    fn getlk_reply_without_conflict_only_changes_l_type() {
        let original = flock(1, 1, 7, 9, 0);
        let reply = VfsFileLockWire { lock_type: VFS_FILE_LOCK_TYPE_UNLCK, ..VfsFileLockWire::default() };
        let answer = flock_from_reply(&original, &reply);
        assert_eq!(answer, LinuxFlock { l_type: linux_abi::F_UNLCK, ..original });
    }

    #[test]
    fn getlk_reply_with_conflict_fills_the_blocker() {
        let original = flock(0, 2, -4, 4, 0);
        let reply = VfsFileLockWire {
            lock_type: VFS_FILE_LOCK_TYPE_WRLCK,
            start: 4096,
            len: 0,
            pid: 321,
            ..VfsFileLockWire::default()
        };
        assert_eq!(flock_from_reply(&original, &reply), flock(1, 0, 4096, 0, 321));
        let ofd = VfsFileLockWire { pid: -1, lock_type: VFS_FILE_LOCK_TYPE_RDLCK, ..reply };
        assert_eq!(flock_from_reply(&original, &ofd).l_pid, -1);
        assert_eq!(flock_from_reply(&original, &ofd).l_type, linux_abi::F_RDLCK);
    }

    #[test]
    fn flock_operations_decode_and_block_only_when_asked() {
        assert_eq!(flock_operation(1), Ok((VFS_FILE_LOCK_TYPE_RDLCK, true)));
        assert_eq!(flock_operation(2), Ok((VFS_FILE_LOCK_TYPE_WRLCK, true)));
        assert_eq!(flock_operation(1 | 4), Ok((VFS_FILE_LOCK_TYPE_RDLCK, false)));
        assert_eq!(flock_operation(2 | 4), Ok((VFS_FILE_LOCK_TYPE_WRLCK, false)));
        assert_eq!(flock_operation(8), Ok((VFS_FILE_LOCK_TYPE_UNLCK, false)));
        assert_eq!(flock_operation(8 | 4), Ok((VFS_FILE_LOCK_TYPE_UNLCK, false)));
        for bad in [0_u64, 3, 4, 16, 1 | 2, 1 | 8, 32 | 1, 1 << 40 | 1] {
            assert_eq!(flock_operation(bad), Err(LINUX_EINVAL), "{bad:#x}");
        }
    }

    #[test]
    fn control_wires_are_well_formed_and_carry_no_range() {
        for cmd in [VFS_FILE_LOCK_CMD_RELEASE_FILE, VFS_FILE_LOCK_CMD_CANCEL_WAIT] {
            let wire = control_wire(cmd, VFS_FILE_LOCK_OWNER_POSIX);
            assert!(wire.is_well_formed());
            assert_eq!((wire.start, wire.len, wire.pid), (0, 0, 0));
        }
    }

    #[test]
    fn posix_lock_pid_set_inserts_finds_removes_and_is_bounded() {
        let set = PosixLockPids::new();
        assert!(!set.contains(5));
        assert!(set.insert(5));
        assert!(set.insert(5), "idempotent");
        assert!(set.insert(6));
        assert!(set.contains(5) && set.contains(6) && !set.contains(7));
        set.remove(5);
        assert!(!set.contains(5) && set.contains(6));
        set.remove(5);
        set.remove(6);
        assert!(!set.contains(6));
        for pid in 1..=POSIX_LOCK_PID_SLOTS as u64 {
            assert!(set.insert(pid));
        }
        assert!(!set.insert(10_000), "full set refuses (ENOLCK)");
        assert!(set.insert(7), "existing pids still succeed");
        set.remove(1);
        assert!(set.insert(10_000), "a freed slot is reused");
    }

    #[test]
    fn published_pid_is_visible_without_separate_counter_publication() {
        let set = PosixLockPids::new();
        // A close can run immediately after the inserting CAS publishes a pid.
        // ORDERING: model the release publication independently of insertion.
        set.slots[0].store(42, Ordering::Release);
        assert!(set.contains(42));
        set.remove(42);
        assert!(!set.contains(42));
    }

    #[test]
    fn proxy_pid_replaces_only_the_pid_and_never_the_thread() {
        let mut request = VfsIpcRequest { pid: 5, tid: 9, ..VfsIpcRequest::default() };
        apply_proxy_pid(&mut request, None);
        assert_eq!((request.pid, request.tid), (5, 9));
        apply_proxy_pid(&mut request, Some(77));
        assert_eq!((request.pid, request.tid), (77, 9));
    }

    #[test]
    fn exec_close_of_a_process_without_posix_locks_sends_nothing() {
        // The pid is not registered, so this must return before any IPC (the
        // test would otherwise try to reach vfsd).
        note_exec_cloexec_closed(0, &[]);
        note_exec_cloexec_closed(0xdead_0001, &[]);
    }

    #[test]
    fn lock_attempt_uses_state_mutation_lease() {
        assert_eq!(LOCK_IPC_TIMEOUT_MS, 5_000);
    }

    #[test]
    fn lock_errno_numbers_are_the_linux_ones() {
        assert_eq!((LINUX_EDEADLK, LINUX_ENOLCK), (35, 37));
    }
}
