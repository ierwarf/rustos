//! Linux `AF_UNIX` socket syscalls over kernel-local [`UnixSocketHandle`].
//!
//! - **Owner:** `kernel-compat` owns Linux socket syscall ABI, user copies,
//!   blocking/timeouts, wakeups, `SCM_RIGHTS` fd installation/acquisition, and
//!   `vfsd` filesystem-node creation/stat/unlink; `kernel-ps` owns socket
//!   state, channels, bind namespace, and readiness (`user/unix_socket.rs`).
//! - **Boundary:** All user pointers, lengths, `sockaddr_un`, `msghdr`,
//!   `cmsghdr`, fds, and socket options are untrusted. Every user copy and
//!   `vfsd` IPC completes with no `LocalFdObject` or `UnixSocketNamespace`
//!   lock held.
//! - **Lifecycle:** Create/install descriptor(s) in process `HandleTable`;
//!   dup/fork/exec/exit settle via `ServiceHandleRef::UnixSocket`; final close
//!   drains unread `PassedHandle`s iteratively without recursion.
//! - **Concurrency:** Blocking operations wait on `local_wait` generation via
//!   `WAITSET_PROVIDER_KERNEL` with optional `SO_RCVTIMEO`/`SO_SNDTIMEO` RTC
//!   deadlines; wakes are published after every lock is released.
//! - **Failure:** `EPIPE` queues `SIGPIPE` unless `MSG_NOSIGNAL`; `EINTR` only
//!   when no progress was made; `EMFILE`/copy-out errors roll back installed fds
//!   and release all acquired `PassedHandle`s.
//! - **Forbidden:** Holding any socket/namespace lock across user copy, sleep,
//!   wake, `vfsd` IPC, or `PassedHandle` drop; routing `AF_UNIX` to `netd`.
//! - **Evidence:** `userspace-wait-set`, `vfs-open-description`.

use alloc::vec;
use alloc::vec::Vec;
use core::mem::size_of;

use kernel_ps::api::{
    LocalWake, PassedHandle, SocketCredentials, UnixAddress, UnixNamespaceKey, UnixSocketError,
    UnixSocketHandle, UnixSocketType, UNIX_MAX_PATH_LEN, UNIX_MAX_SCM_RIGHTS,
};
use rustos_user_abi::syscall::{
    VFS_IPC_OP_ACCESS, VFS_IPC_OP_MKNOD, VFS_IPC_OP_NEWFSTATAT, VFS_IPC_OP_UNLINKAT, VFS_MKNOD_S_IFMT,
    VFS_MKNOD_S_IFSOCK, WAITSET_PROVIDER_KERNEL,
};

use super::super::broker_ops::waitset_broker_ops::{
    register_waitset_waiters, remove_waitset_waiters, waitset_waiters_match, wake_local_fd_object,
    ProviderObservation,
};
use super::*;

const LINUX_ENFILE: i64 = 23;
const LINUX_ENAMETOOLONG: i64 = 36;
const LINUX_EDESTADDRREQ: i64 = 89;
const LINUX_ENOPROTOOPT: i64 = 92;
const LINUX_EPROTONOSUPPORT: i64 = 93;
const LINUX_ESOCKTNOSUPPORT: i64 = 94;
const LINUX_EAFNOSUPPORT: i64 = 97;
const LINUX_EADDRINUSE: i64 = 98;
const LINUX_EADDRNOTAVAIL: i64 = 99;
const LINUX_ECONNRESET: i64 = 104;
const LINUX_EISCONN: i64 = 106;
const LINUX_ECONNREFUSED: i64 = 111;

const AF_UNSPEC: u16 = 0;
const SO_RCVTIMEO: u64 = 20;
const SO_SNDTIMEO: u64 = 21;
const MSG_WAITALL: u64 = 0x100;
const MAX_UNIX_IO_BYTES: usize = 1024 * 1024;
const MAX_IOV_COUNT: usize = 1024;
const MAX_CONTROL_BYTES: usize = 64 * 1024;

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct LinuxIovec {
    iov_base: u64,
    iov_len: u64,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct LinuxTimeval {
    tv_sec: i64,
    tv_usec: i64,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct LinuxUcred {
    pid: i32,
    uid: u32,
    gid: u32,
}

struct PinnedUnixSocket(UnixSocketHandle);

impl Drop for PinnedUnixSocket {
    fn drop(&mut self) {
        release_service_handle_refs_bounded(&[ServiceHandleRef::UnixSocket(self.0.clone())]);
    }
}

fn pin_unix_socket(fd: u64) -> Option<PinnedUnixSocket> {
    multitask::with_current_user_process_state(|_, _, process_state| {
        match process_state.handles().get(fd) {
            Some(multitask::KernelHandle::UnixSocket(sock))
                if sock.try_acquire_descriptor_reference() =>
            {
                Some(PinnedUnixSocket(sock.clone()))
            }
            _ => None,
        }
    })
    .flatten()
}

fn current_credentials() -> SocketCredentials {
    let pid = multitask::current_user_process_id().unwrap_or(0) as i32;
    let (uid, gid) = multitask::with_current_process_credentials(|sec| (sec.euid(), sec.egid()))
        .unwrap_or((0, 0));
    SocketCredentials::new(pid, uid, gid)
}

fn publish_wake(wake: Option<LocalWake>) {
    if let Some(wake) = wake {
        wake_local_fd_object(wake.token, wake.generation);
    }
}

fn unix_error_errno(err: UnixSocketError) -> i64 {
    match err {
        UnixSocketError::WouldBlock { .. } => LINUX_EAGAIN,
        UnixSocketError::BrokenPipe => LINUX_EPIPE,
        UnixSocketError::ConnectionRefused => LINUX_ECONNREFUSED,
        UnixSocketError::ConnectionReset => LINUX_ECONNRESET,
        UnixSocketError::AddressInUse => LINUX_EADDRINUSE,
        UnixSocketError::AddressNotAvailable => LINUX_EADDRNOTAVAIL,
        UnixSocketError::AlreadyConnected => LINUX_EISCONN,
        UnixSocketError::NotConnected => LINUX_ENOTCONN,
        UnixSocketError::InvalidArgument => LINUX_EINVAL,
        UnixSocketError::OperationNotSupported => LINUX_EOPNOTSUPP,
        UnixSocketError::MessageTooLong => LINUX_EMSGSIZE,
        UnixSocketError::TooManyObjects => LINUX_ENFILE,
        UnixSocketError::NoMemory => LINUX_ENOMEM,
        UnixSocketError::DestinationRequired => LINUX_EDESTADDRREQ,
    }
}

fn raise_sigpipe() {
    let (Some(process_id), Some(thread_id)) = (
        multitask::current_user_process_id(),
        multitask::current_user_thread_id(),
    ) else {
        return;
    };
    for _ in 0..8 {
        if multitask::queue_linux_signal(process_id, thread_id, linux_abi::SIGPIPE).is_ok() {
            return;
        }
        multitask::cond_resched();
    }
}

fn deadline_tick_from_us(timeout_us: Option<u64>) -> Option<u64> {
    let us = timeout_us?;
    if us == 0 {
        return None;
    }
    let ticks_per_sec = crate::arch::rtc::ticks_per_second().max(1) as u128;
    let ticks = ((us as u128).saturating_mul(ticks_per_sec).saturating_add(999_999)) / 1_000_000;
    let delta = u64::try_from(ticks).unwrap_or(u64::MAX).max(1);
    Some(crate::arch::rtc::ticks().saturating_add(delta))
}

fn wait_for_generation(
    token: u64,
    generation: u64,
    deadline_tick: Option<u64>,
) -> Result<(), i64> {
    if deadline_tick.is_some_and(|d| crate::arch::rtc::ticks() >= d) {
        return Err(LINUX_EAGAIN);
    }
    let Some(task_id) = multitask::current_task_id() else {
        return Err(LINUX_EINVAL);
    };
    let Some(process_id) = multitask::current_user_process_id() else {
        return Err(LINUX_EINVAL);
    };
    let observation = [ProviderObservation {
        provider: WAITSET_PROVIDER_KERNEL,
        object_id: token,
        generation,
    }];
    register_waitset_waiters(task_id, process_id, &observation)?;
    match kernel_ps::api::local_fd_readiness_of(token) {
        Some(current) if current.generation == generation => {}
        _ => {
            remove_waitset_waiters(task_id);
            return Ok(());
        }
    }
    if !multitask::arm_block_current_task() {
        remove_waitset_waiters(task_id);
        if multitask::current_task_id() != Some(task_id) {
            return Err(LINUX_EINVAL);
        }
        multitask::cond_resched();
        return Ok(());
    }
    if !waitset_waiters_match(task_id, process_id, &observation) {
        remove_waitset_waiters(task_id);
        let _ = multitask::cancel_block_current_task();
        return Ok(());
    }
    if multitask::current_linux_thread_state()
        .is_some_and(|state| state.pending_signals & !state.signal_mask != 0)
    {
        remove_waitset_waiters(task_id);
        let _ = multitask::cancel_block_current_task();
        multitask::cond_resched();
        return Ok(());
    }
    if let Some(deadline) = deadline_tick
        && !crate::arch::rtc::arm_sleep_waiter_until_tick(task_id, deadline)
    {
        remove_waitset_waiters(task_id);
        let _ = multitask::cancel_block_current_task();
        return Err(LINUX_EAGAIN);
    }
    let committed = multitask::commit_block_current_task_and_yield();
    remove_waitset_waiters(task_id);
    if deadline_tick.is_some() {
        crate::arch::rtc::disarm_sleep_waiter(task_id);
    }
    if committed.is_none() {
        return Err(LINUX_EINVAL);
    }
    if current_wait_interrupted() {
        return Err(LINUX_EINTR);
    }
    if deadline_tick.is_some_and(|d| crate::arch::rtc::ticks() >= d) {
        return Err(LINUX_EAGAIN);
    }
    Ok(())
}

// ---- Address parsing / encoding & vfsd helpers ----------------------------

enum ParsedUnixAddr {
    Unspec,
    Abstract(Vec<u8>),
    Pathname(alloc::string::String, Vec<u8>),
}

fn read_user_unix_addr(addr_ptr: u64, addrlen: u64) -> Result<ParsedUnixAddr, i64> {
    let Ok(len) = usize::try_from(addrlen as u32) else {
        return Err(LINUX_EINVAL);
    };
    if len < 2 || len > 2 + UNIX_MAX_PATH_LEN {
        return Err(LINUX_EINVAL);
    }
    let mut buf = [0_u8; 2 + UNIX_MAX_PATH_LEN];
    usermem::copy_from_current_user_exact(addr_ptr, &mut buf[..len])
        .map_err(user_pointer_error_to_linux_errno)?;
    let family = u16::from_ne_bytes([buf[0], buf[1]]);
    if family == AF_UNSPEC {
        return Ok(ParsedUnixAddr::Unspec);
    }
    if u64::from(family) != linux_abi::AF_UNIX {
        return Err(LINUX_EAFNOSUPPORT);
    }
    if len == 2 {
        return Err(LINUX_EINVAL);
    }
    let sun_path = &buf[2..len];
    if sun_path[0] == 0 {
        return Ok(ParsedUnixAddr::Abstract(sun_path[1..].to_vec()));
    }
    let nul_pos = sun_path.iter().position(|&b| b == 0).unwrap_or(sun_path.len());
    if nul_pos == 0 {
        return Err(LINUX_EINVAL);
    }
    let raw = &sun_path[..nul_pos];
    if raw.len() >= UNIX_MAX_PATH_LEN {
        return Err(LINUX_ENAMETOOLONG);
    }
    let path_str = core::str::from_utf8(raw).map_err(|_| LINUX_EINVAL)?;
    Ok(ParsedUnixAddr::Pathname(
        alloc::string::String::from(path_str),
        raw.to_vec(),
    ))
}

fn encode_unix_addr(addr: &UnixAddress) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(&(linux_abi::AF_UNIX as u16).to_ne_bytes());
    match addr {
        UnixAddress::Unnamed => {}
        UnixAddress::Abstract(name) => {
            out.push(0);
            out.extend_from_slice(name);
        }
        UnixAddress::Pathname(path) => {
            out.extend_from_slice(path);
            out.push(0);
        }
    }
    out
}

fn vfs_stat_socket_inode(path: &str, follow_symlinks: bool) -> Result<(u64, u64), i64> {
    let mut request = new_vfs_request(VFS_IPC_OP_NEWFSTATAT);
    request.flags = if follow_symlinks {
        0
    } else {
        linux_abi::AT_SYMLINK_NOFOLLOW as u32
    };
    populate_vfs_path_base(&mut request, linux_abi::AT_FDCWD as u64, path)?;
    let response = call_pinned_remote_vfs_request(&request)?;
    ensure_vfs_status(&response)?;
    if response.payload_len as usize != LINUX_STAT_SIZE {
        return Err(LINUX_EINVAL);
    }
    let st_dev = u64::from_ne_bytes(response.payload[0..8].try_into().map_err(|_| LINUX_EINVAL)?);
    let st_ino = u64::from_ne_bytes(response.payload[8..16].try_into().map_err(|_| LINUX_EINVAL)?);
    let st_mode =
        u32::from_ne_bytes(response.payload[24..28].try_into().map_err(|_| LINUX_EINVAL)?);
    if u64::from(st_mode) & VFS_MKNOD_S_IFMT != VFS_MKNOD_S_IFSOCK {
        return Err(LINUX_ECONNREFUSED);
    }
    Ok((st_dev, st_ino))
}

/// `connect(2)` and datagram `sendto(2)` to a pathname socket need write
/// permission on the socket file for the caller's effective identity; vfsd
/// owns the permission policy, so ask it instead of deciding here.
fn vfs_check_socket_write_access(path: &str) -> Result<(), i64> {
    let mut request = new_vfs_request(VFS_IPC_OP_ACCESS);
    request.flags = linux_abi::AT_EACCESS as u32;
    request.arg0 = 0o2;
    populate_vfs_path_base(&mut request, linux_abi::AT_FDCWD as u64, path)?;
    let response = call_pinned_remote_vfs_request(&request)?;
    ensure_vfs_status(&response)
}

fn vfs_create_socket_node(path: &str) -> Result<(u64, u64), i64> {
    // The socket file's permission bits are `0777 & ~umask`, as for any other
    // node `bind(2)` creates; the umask is syscalld policy.
    let mode = 0o777 & !u64::from(super::super::syscalld_ops::query_umask()?);
    let mut request = new_vfs_request(VFS_IPC_OP_MKNOD);
    request.arg0 = VFS_MKNOD_S_IFSOCK | mode;
    request.arg1 = 0;
    populate_vfs_path_base(&mut request, linux_abi::AT_FDCWD as u64, path)?;
    let response = call_pinned_remote_vfs_request(&request)?;
    if let Err(errno) = ensure_vfs_status(&response) {
        return Err(if errno == LINUX_EEXIST {
            LINUX_EADDRINUSE
        } else {
            errno
        });
    }
    match vfs_stat_socket_inode(path, false) {
        Ok(inode) => Ok(inode),
        Err(errno) => {
            vfs_unlink_socket_node(path);
            Err(errno)
        }
    }
}

fn vfs_unlink_socket_node(path: &str) {
    let mut request = new_vfs_request(VFS_IPC_OP_UNLINKAT);
    request.arg0 = 0;
    if populate_vfs_path_base(&mut request, linux_abi::AT_FDCWD as u64, path).is_ok() {
        let _ = call_pinned_remote_vfs_request(&request);
    }
}

fn resolve_target_key(addr_ptr: u64, addrlen: u64) -> Result<UnixNamespaceKey, i64> {
    match read_user_unix_addr(addr_ptr, addrlen)? {
        ParsedUnixAddr::Unspec => Err(LINUX_EINVAL),
        ParsedUnixAddr::Abstract(name) => Ok(UnixNamespaceKey::Abstract(name)),
        ParsedUnixAddr::Pathname(path, _) => {
            let (mount_id, node_id) = vfs_stat_socket_inode(&path, true)?;
            vfs_check_socket_write_access(&path)?;
            Ok(UnixNamespaceKey::Inode { mount_id, node_id })
        }
    }
}

// ---- socket / socketpair --------------------------------------------------

fn parse_socket_type_and_flags(
    type_and_flags: u64,
    protocol: u64,
) -> Result<(UnixSocketType, bool, bool), i64> {
    let raw = u64::from(type_and_flags as u32);
    let flags = raw & (linux_abi::SOCK_NONBLOCK | linux_abi::SOCK_CLOEXEC);
    let base_type = raw & !(linux_abi::SOCK_NONBLOCK | linux_abi::SOCK_CLOEXEC);
    let Some(sock_type) = UnixSocketType::from_linux_type(base_type) else {
        return Err(LINUX_ESOCKTNOSUPPORT);
    };
    if protocol != 0 {
        return Err(LINUX_EPROTONOSUPPORT);
    }
    Ok((
        sock_type,
        flags & linux_abi::SOCK_NONBLOCK != 0,
        flags & linux_abi::SOCK_CLOEXEC != 0,
    ))
}

fn install_single_unix_socket(sock: UnixSocketHandle, cloexec: bool) -> Result<u64, i64> {
    let fd_flags = if cloexec { multitask::FD_CLOEXEC } else { 0 };
    let entry = multitask::HandleEntry::new(
        multitask::KernelHandle::UnixSocket(sock.clone()),
        fd_flags,
        sock.status_flags(),
    );
    let installed = multitask::with_current_user_process_state_mut(|_, _, process_state| {
        process_state.handles_mut().install_entry(entry)
    });
    match installed {
        Some(Some(fd)) => Ok(fd),
        Some(None) => {
            release_service_handle_refs_bounded(&[ServiceHandleRef::UnixSocket(sock)]);
            Err(LINUX_EMFILE)
        }
        None => {
            release_service_handle_refs_bounded(&[ServiceHandleRef::UnixSocket(sock)]);
            Err(LINUX_ENOSYS)
        }
    }
}

pub(in crate::user::syscall::linux) fn syscall_linux_unix_socket(
    type_and_flags: u64,
    protocol: u64,
) -> u64 {
    let (sock_type, nonblocking, cloexec) = match parse_socket_type_and_flags(type_and_flags, protocol)
    {
        Ok(parsed) => parsed,
        Err(errno) => return linux_errno(errno),
    };
    let sock = match UnixSocketHandle::new(sock_type, nonblocking, current_credentials()) {
        Ok(sock) => sock,
        Err(err) => return linux_errno(unix_error_errno(err)),
    };
    match install_single_unix_socket(sock, cloexec) {
        Ok(fd) => fd,
        Err(errno) => linux_errno(errno),
    }
}

pub(in crate::user::syscall::linux) fn syscall_linux_unix_socketpair(
    type_and_flags: u64,
    protocol: u64,
    sv_ptr: u64,
) -> u64 {
    let (sock_type, nonblocking, cloexec) = match parse_socket_type_and_flags(type_and_flags, protocol)
    {
        Ok(parsed) => parsed,
        Err(errno) => return linux_errno(errno),
    };
    if let Err(err) = usermem::validate_current_user_write_buffer(sv_ptr, 2 * size_of::<i32>()) {
        return linux_errno(user_pointer_error_to_linux_errno(err));
    }
    let (first, second) =
        match UnixSocketHandle::new_pair(sock_type, nonblocking, current_credentials()) {
            Ok(pair) => pair,
            Err(err) => return linux_errno(unix_error_errno(err)),
        };
    let fd_flags = if cloexec { multitask::FD_CLOEXEC } else { 0 };
    let e0 = multitask::HandleEntry::new(
        multitask::KernelHandle::UnixSocket(first.clone()),
        fd_flags,
        first.status_flags(),
    );
    let e1 = multitask::HandleEntry::new(
        multitask::KernelHandle::UnixSocket(second.clone()),
        fd_flags,
        second.status_flags(),
    );
    let installed = multitask::with_current_user_process_state_mut(|_, _, process_state| {
        let handles = process_state.handles_mut();
        let fd0 = handles.install_entry(e0)?;
        let Some(fd1) = handles.install_entry(e1) else {
            let _ = handles.close(fd0);
            return None;
        };
        Some((fd0, fd1))
    });
    let (fd0, fd1) = match installed {
        Some(Some(pair)) => pair,
        Some(None) => {
            release_service_handle_refs_bounded(&[
                ServiceHandleRef::UnixSocket(first),
                ServiceHandleRef::UnixSocket(second),
            ]);
            return linux_errno(LINUX_EMFILE);
        }
        None => {
            release_service_handle_refs_bounded(&[
                ServiceHandleRef::UnixSocket(first),
                ServiceHandleRef::UnixSocket(second),
            ]);
            return linux_errno(LINUX_ENOSYS);
        }
    };
    let mut out = [0_u8; 2 * size_of::<i32>()];
    out[..4].copy_from_slice(&(fd0 as i32).to_ne_bytes());
    out[4..].copy_from_slice(&(fd1 as i32).to_ne_bytes());
    if let Err(err) = usermem::write_current_user_bytes(sv_ptr, &out) {
        let closed = multitask::with_current_user_process_state_mut(|_, _, process_state| {
            let handles = process_state.handles_mut();
            (handles.close(fd0), handles.close(fd1))
        });
        if let Some((h0, h1)) = closed {
            release_closed_handles(h0.into_iter().chain(h1));
        }
        return linux_errno(user_pointer_error_to_linux_errno(err));
    }
    0
}

// ---- bind / listen / accept / connect -------------------------------------

pub(in crate::user::syscall::linux) fn try_bind(
    fd: u64,
    addr_ptr: u64,
    addrlen: u64,
) -> Option<u64> {
    let sock = pin_unix_socket(fd)?;
    let parsed = match read_user_unix_addr(addr_ptr, addrlen) {
        Ok(p) => p,
        Err(errno) => return Some(linux_errno(errno)),
    };
    let result = match parsed {
        ParsedUnixAddr::Unspec => Err(LINUX_EINVAL),
        ParsedUnixAddr::Abstract(name) => {
            let addr = UnixAddress::Abstract(name.clone());
            let key = UnixNamespaceKey::Abstract(name);
            sock.0.bind(addr, key).map_err(unix_error_errno)
        }
        ParsedUnixAddr::Pathname(path, raw) => match vfs_create_socket_node(&path) {
            Ok((mount_id, node_id)) => {
                let addr = UnixAddress::Pathname(raw);
                let key = UnixNamespaceKey::Inode { mount_id, node_id };
                if let Err(err) = sock.0.bind(addr, key) {
                    vfs_unlink_socket_node(&path);
                    Err(unix_error_errno(err))
                } else {
                    Ok(())
                }
            }
            Err(errno) => Err(errno),
        },
    };
    Some(match result {
        Ok(()) => 0,
        Err(errno) => linux_errno(errno),
    })
}

pub(in crate::user::syscall::linux) fn try_listen(fd: u64, backlog: u64) -> Option<u64> {
    let sock = pin_unix_socket(fd)?;
    Some(match sock.0.listen(backlog as i32) {
        Ok(()) => 0,
        Err(err) => linux_errno(unix_error_errno(err)),
    })
}

pub(in crate::user::syscall::linux) fn try_accept4(
    fd: u64,
    addr_ptr: u64,
    addrlen_ptr: u64,
    flags: u64,
) -> Option<u64> {
    let sock = pin_unix_socket(fd)?;
    let flags = u64::from(flags as u32);
    if flags & !(linux_abi::SOCK_NONBLOCK | linux_abi::SOCK_CLOEXEC) != 0 {
        return Some(linux_errno(LINUX_EINVAL));
    }
    let child_nonblock = flags & linux_abi::SOCK_NONBLOCK != 0;
    let child_cloexec = flags & linux_abi::SOCK_CLOEXEC != 0;
    let deadline = deadline_tick_from_us(sock.0.rcvtimeo_us());
    loop {
        match sock.0.try_accept(child_nonblock) {
            Ok((accepted, peer_addr)) => {
                if addr_ptr != 0 {
                    let encoded = encode_unix_addr(&peer_addr);
                    if let Err(errno) =
                        super::vfs_meta::write_current_sockaddr_payload(addr_ptr, addrlen_ptr, &encoded)
                    {
                        release_service_handle_refs_bounded(&[ServiceHandleRef::UnixSocket(
                            accepted,
                        )]);
                        return Some(linux_errno(errno));
                    }
                }
                return Some(match install_single_unix_socket(accepted, child_cloexec) {
                    Ok(new_fd) => new_fd,
                    Err(errno) => linux_errno(errno),
                });
            }
            Err(UnixSocketError::WouldBlock { generation }) => {
                if sock.0.nonblocking() {
                    return Some(linux_errno(LINUX_EAGAIN));
                }
                if current_wait_interrupted() {
                    return Some(linux_errno(LINUX_EINTR));
                }
                if let Err(errno) = wait_for_generation(sock.0.token_id(), generation, deadline) {
                    return Some(linux_errno(errno));
                }
            }
            Err(err) => return Some(linux_errno(unix_error_errno(err))),
        }
    }
}

pub(in crate::user::syscall::linux) fn try_connect(
    fd: u64,
    addr_ptr: u64,
    addrlen: u64,
) -> Option<u64> {
    let sock = pin_unix_socket(fd)?;
    let parsed = match read_user_unix_addr(addr_ptr, addrlen) {
        Ok(p) => p,
        Err(errno) => return Some(linux_errno(errno)),
    };
    if matches!(parsed, ParsedUnixAddr::Unspec) {
        return Some(match sock.0.disconnect_dgram() {
            Ok(()) => 0,
            Err(err) => linux_errno(unix_error_errno(err)),
        });
    }
    let key = match parsed {
        ParsedUnixAddr::Unspec => unreachable!(),
        ParsedUnixAddr::Abstract(name) => UnixNamespaceKey::Abstract(name),
        ParsedUnixAddr::Pathname(path, _) => match vfs_stat_socket_inode(&path, true) {
            Ok((mount_id, node_id)) => UnixNamespaceKey::Inode { mount_id, node_id },
            Err(errno) => return Some(linux_errno(errno)),
        },
    };
    let creds = current_credentials();
    Some(match sock.0.connect(&key, creds) {
        Ok(wake) => {
            publish_wake(wake);
            0
        }
        Err(err) => linux_errno(unix_error_errno(err)),
    })
}

// ---- getsockname / getpeername / shutdown / sockopts ----------------------

pub(in crate::user::syscall::linux) fn try_getsockname(
    fd: u64,
    addr_ptr: u64,
    addrlen_ptr: u64,
) -> Option<u64> {
    let sock = pin_unix_socket(fd)?;
    let encoded = encode_unix_addr(&sock.0.sock_name());
    Some(
        match super::vfs_meta::write_current_sockaddr_payload(addr_ptr, addrlen_ptr, &encoded) {
            Ok(()) => 0,
            Err(errno) => linux_errno(errno),
        },
    )
}

pub(in crate::user::syscall::linux) fn try_getpeername(
    fd: u64,
    addr_ptr: u64,
    addrlen_ptr: u64,
) -> Option<u64> {
    let sock = pin_unix_socket(fd)?;
    let peer_addr = match sock.0.peer_name() {
        Ok(addr) => addr,
        Err(err) => return Some(linux_errno(unix_error_errno(err))),
    };
    let encoded = encode_unix_addr(&peer_addr);
    Some(
        match super::vfs_meta::write_current_sockaddr_payload(addr_ptr, addrlen_ptr, &encoded) {
            Ok(()) => 0,
            Err(errno) => linux_errno(errno),
        },
    )
}

pub(in crate::user::syscall::linux) fn try_shutdown(fd: u64, how: u64) -> Option<u64> {
    let sock = pin_unix_socket(fd)?;
    Some(match sock.0.shutdown(u64::from(how as u32)) {
        Ok(wakes) => {
            for wake in wakes {
                publish_wake(wake);
            }
            0
        }
        Err(err) => linux_errno(unix_error_errno(err)),
    })
}

fn read_opt_i32(optval_ptr: u64, optlen: u64) -> Result<i32, i64> {
    if (optlen as u32 as usize) < size_of::<i32>() {
        return Err(LINUX_EINVAL);
    }
    let mut bytes = [0_u8; 4];
    read_user_bytes(optval_ptr, &mut bytes)?;
    Ok(i32::from_ne_bytes(bytes))
}

fn read_opt_timeval_us(optval_ptr: u64, optlen: u64) -> Result<Option<u64>, i64> {
    if (optlen as u32 as usize) < size_of::<LinuxTimeval>() {
        return Err(LINUX_EINVAL);
    }
    let mut bytes = [0_u8; size_of::<LinuxTimeval>()];
    read_user_bytes(optval_ptr, &mut bytes)?;
    let tv_sec = i64::from_ne_bytes(bytes[0..8].try_into().unwrap());
    let tv_usec = i64::from_ne_bytes(bytes[8..16].try_into().unwrap());
    if tv_sec < 0 || !(0..1_000_000).contains(&tv_usec) {
        return Err(LINUX_EINVAL);
    }
    let total = (tv_sec as u64)
        .saturating_mul(1_000_000)
        .saturating_add(tv_usec as u64);
    Ok((total > 0).then_some(total))
}

fn write_opt_bytes(optval_ptr: u64, optlen_ptr: u64, payload: &[u8]) -> Result<(), i64> {
    if optval_ptr == 0 || optlen_ptr == 0 {
        return Err(LINUX_EFAULT);
    }
    let cap = read_user_struct::<u32>(optlen_ptr)? as usize;
    let copy_len = cap.min(payload.len());
    write_user_bytes(optval_ptr, &payload[..copy_len])?;
    let written = copy_len as u32;
    write_user_struct(optlen_ptr, &written)
}

pub(in crate::user::syscall::linux) fn try_setsockopt(
    fd: u64,
    level: u64,
    optname: u64,
    optval_ptr: u64,
    optlen: u64,
) -> Option<u64> {
    let sock = pin_unix_socket(fd)?;
    let level = u64::from(level as u32);
    let optname = u64::from(optname as u32);
    if level != linux_abi::SOL_SOCKET {
        return Some(linux_errno(LINUX_ENOPROTOOPT));
    }
    let res = match optname {
        linux_abi::SO_PASSCRED => read_opt_i32(optval_ptr, optlen).map(|v| {
            sock.0.set_passcred(v != 0);
        }),
        linux_abi::SO_SNDBUF => read_opt_i32(optval_ptr, optlen).map(|v| {
            sock.0.set_sndbuf(v.max(0) as usize);
        }),
        linux_abi::SO_RCVBUF => read_opt_i32(optval_ptr, optlen).map(|v| {
            sock.0.set_rcvbuf(v.max(0) as usize);
        }),
        SO_RCVTIMEO => read_opt_timeval_us(optval_ptr, optlen).map(|us| {
            sock.0.set_rcvtimeo_us(us);
        }),
        SO_SNDTIMEO => read_opt_timeval_us(optval_ptr, optlen).map(|us| {
            sock.0.set_sndtimeo_us(us);
        }),
        linux_abi::SO_KEEPALIVE => read_opt_i32(optval_ptr, optlen).map(|v| {
            sock.0.set_keepalive(v != 0);
        }),
        linux_abi::SO_REUSEADDR => read_opt_i32(optval_ptr, optlen).map(|v| {
            sock.0.set_reuseaddr(v != 0);
        }),
        linux_abi::SO_REUSEPORT => read_opt_i32(optval_ptr, optlen).map(|v| {
            sock.0.set_reuseport(v != 0);
        }),
        _ => Err(LINUX_ENOPROTOOPT),
    };
    Some(match res {
        Ok(()) => 0,
        Err(errno) => linux_errno(errno),
    })
}

pub(in crate::user::syscall::linux) fn try_getsockopt(
    fd: u64,
    level: u64,
    optname: u64,
    optval_ptr: u64,
    optlen_ptr: u64,
) -> Option<u64> {
    let sock = pin_unix_socket(fd)?;
    let level = u64::from(level as u32);
    let optname = u64::from(optname as u32);
    if level != linux_abi::SOL_SOCKET {
        return Some(linux_errno(LINUX_ENOPROTOOPT));
    }
    let res = match optname {
        linux_abi::SO_TYPE => write_opt_bytes(
            optval_ptr,
            optlen_ptr,
            &(sock.0.socket_type().to_linux_type() as i32).to_ne_bytes(),
        ),
        linux_abi::SO_DOMAIN => write_opt_bytes(
            optval_ptr,
            optlen_ptr,
            &(linux_abi::AF_UNIX as i32).to_ne_bytes(),
        ),
        linux_abi::SO_PROTOCOL => write_opt_bytes(optval_ptr, optlen_ptr, &0_i32.to_ne_bytes()),
        linux_abi::SO_ERROR => {
            write_opt_bytes(optval_ptr, optlen_ptr, &sock.0.take_error().to_ne_bytes())
        }
        linux_abi::SO_ACCEPTCONN => write_opt_bytes(
            optval_ptr,
            optlen_ptr,
            &(i32::from(sock.0.is_listening())).to_ne_bytes(),
        ),
        linux_abi::SO_PEERCRED => {
            let c = sock.0.peer_credentials();
            let mut buf = [0_u8; size_of::<LinuxUcred>()];
            buf[0..4].copy_from_slice(&c.pid().to_ne_bytes());
            buf[4..8].copy_from_slice(&c.uid().to_ne_bytes());
            buf[8..12].copy_from_slice(&c.gid().to_ne_bytes());
            write_opt_bytes(optval_ptr, optlen_ptr, &buf)
        }
        linux_abi::SO_PASSCRED => write_opt_bytes(
            optval_ptr,
            optlen_ptr,
            &(i32::from(sock.0.passcred())).to_ne_bytes(),
        ),
        linux_abi::SO_SNDBUF => write_opt_bytes(
            optval_ptr,
            optlen_ptr,
            &(sock.0.sndbuf() as i32).to_ne_bytes(),
        ),
        linux_abi::SO_RCVBUF => write_opt_bytes(
            optval_ptr,
            optlen_ptr,
            &(sock.0.rcvbuf() as i32).to_ne_bytes(),
        ),
        SO_RCVTIMEO => {
            let us = sock.0.rcvtimeo_us().unwrap_or(0);
            let mut buf = [0_u8; 16];
            buf[0..8].copy_from_slice(&((us / 1_000_000) as i64).to_ne_bytes());
            buf[8..16].copy_from_slice(&((us % 1_000_000) as i64).to_ne_bytes());
            write_opt_bytes(optval_ptr, optlen_ptr, &buf)
        }
        SO_SNDTIMEO => {
            let us = sock.0.sndtimeo_us().unwrap_or(0);
            let mut buf = [0_u8; 16];
            buf[0..8].copy_from_slice(&((us / 1_000_000) as i64).to_ne_bytes());
            buf[8..16].copy_from_slice(&((us % 1_000_000) as i64).to_ne_bytes());
            write_opt_bytes(optval_ptr, optlen_ptr, &buf)
        }
        linux_abi::SO_KEEPALIVE => write_opt_bytes(
            optval_ptr,
            optlen_ptr,
            &(i32::from(sock.0.keepalive())).to_ne_bytes(),
        ),
        linux_abi::SO_REUSEADDR => write_opt_bytes(
            optval_ptr,
            optlen_ptr,
            &(i32::from(sock.0.reuseaddr())).to_ne_bytes(),
        ),
        linux_abi::SO_REUSEPORT => write_opt_bytes(
            optval_ptr,
            optlen_ptr,
            &(i32::from(sock.0.reuseport())).to_ne_bytes(),
        ),
        _ => Err(LINUX_ENOPROTOOPT),
    };
    Some(match res {
        Ok(()) => 0,
        Err(errno) => linux_errno(errno),
    })
}

// ---- SCM_RIGHTS / SCM_CREDENTIALS control message helpers -----------------

const CMSG_HDR_LEN: usize = size_of::<linux_abi::LinuxCmsghdr>();

const fn cmsg_align(len: usize) -> usize {
    let align = size_of::<usize>();
    (len + align - 1) & !(align - 1)
}

fn release_passed_handles(handles: Vec<PassedHandle>) {
    if handles.is_empty() {
        return;
    }
    release_closed_handles(handles.into_iter().map(|p| p.handle().clone()));
}

fn acquire_passed_fds(fds: &[i32]) -> Result<Vec<PassedHandle>, i64> {
    let mut snapshots = Vec::with_capacity(fds.len());
    let snap_res = multitask::with_current_user_process_state(|_, _, process_state| {
        for &raw_fd in fds {
            if raw_fd < 0 {
                return Err(LINUX_EBADF);
            }
            let Some(entry) = process_state.handles().get_entry(raw_fd as u64) else {
                return Err(LINUX_EBADF);
            };
            if !entry.handle().supports_descriptor_transfer(entry.rights()) {
                return Err(LINUX_EBADF);
            }
            snapshots.push(PassedHandle::new_with_rights(
                entry.handle().clone(),
                entry.status_flags(),
                entry.rights(),
            ));
        }
        Ok(())
    });
    match snap_res {
        Some(Ok(())) => {}
        Some(Err(errno)) => return Err(errno),
        None => return Err(LINUX_EBADF),
    }

    let mut acquired = Vec::with_capacity(snapshots.len());
    for passed in snapshots {
        if let Some(handle_ref) = service_handle_ref_for_handle(passed.handle()) {
            if let Err(errno) = acquire_service_handle_ref(&handle_ref) {
                release_passed_handles(acquired);
                return Err(errno);
            }
        }
        acquired.push(passed);
    }
    Ok(acquired)
}

fn parse_send_control(
    control_ptr: u64,
    control_len: usize,
    caller_creds: SocketCredentials,
) -> Result<(Vec<PassedHandle>, SocketCredentials), i64> {
    if control_len == 0 {
        return Ok((Vec::new(), caller_creds));
    }
    if control_ptr == 0 || control_len > MAX_CONTROL_BYTES {
        return Err(LINUX_EINVAL);
    }
    let mut buf = vec![0_u8; control_len];
    usermem::copy_from_current_user_exact(control_ptr, &mut buf)
        .map_err(user_pointer_error_to_linux_errno)?;

    let mut raw_fds = Vec::new();
    let mut creds = caller_creds;
    let mut offset = 0_usize;
    while offset + CMSG_HDR_LEN <= buf.len() {
        let cmsg_len = u64::from_ne_bytes(buf[offset..offset + 8].try_into().unwrap()) as usize;
        let cmsg_level = u32::from_ne_bytes(buf[offset + 8..offset + 12].try_into().unwrap());
        let cmsg_type = u32::from_ne_bytes(buf[offset + 12..offset + 16].try_into().unwrap());
        if cmsg_len < CMSG_HDR_LEN || offset.saturating_add(cmsg_len) > buf.len() {
            return Err(LINUX_EINVAL);
        }
        let data = &buf[offset + CMSG_HDR_LEN..offset + cmsg_len];
        if u64::from(cmsg_level) == linux_abi::SOL_SOCKET {
            match u64::from(cmsg_type) {
                linux_abi::SCM_RIGHTS => {
                    if !data.len().is_multiple_of(size_of::<i32>()) {
                        return Err(LINUX_EINVAL);
                    }
                    for chunk in data.chunks_exact(4) {
                        raw_fds.push(i32::from_ne_bytes(chunk.try_into().unwrap()));
                        if raw_fds.len() > UNIX_MAX_SCM_RIGHTS {
                            return Err(LINUX_EINVAL);
                        }
                    }
                }
                linux_abi::SCM_CREDENTIALS => {
                    if data.len() != size_of::<LinuxUcred>() {
                        return Err(LINUX_EINVAL);
                    }
                    let pid = i32::from_ne_bytes(data[0..4].try_into().unwrap());
                    let uid = u32::from_ne_bytes(data[4..8].try_into().unwrap());
                    let gid = u32::from_ne_bytes(data[8..12].try_into().unwrap());
                    if caller_creds.uid() != 0
                        && (pid != caller_creds.pid()
                            || uid != caller_creds.uid()
                            || gid != caller_creds.gid())
                    {
                        return Err(LINUX_EPERM);
                    }
                    creds = SocketCredentials::new(pid, uid, gid);
                }
                _ => return Err(LINUX_EINVAL),
            }
        } else {
            return Err(LINUX_EINVAL);
        }
        let step = cmsg_align(cmsg_len);
        if offset.saturating_add(step) <= buf.len() {
            offset += step;
        } else {
            offset += cmsg_len;
        }
    }
    let rights = if raw_fds.is_empty() {
        Vec::new()
    } else {
        acquire_passed_fds(&raw_fds)?
    };
    Ok((rights, creds))
}

fn write_recv_control(
    control_ptr: u64,
    control_cap: usize,
    rights: Vec<PassedHandle>,
    creds: Option<SocketCredentials>,
    cloexec: bool,
) -> Result<(usize, u32), i64> {
    if control_ptr == 0 || control_cap == 0 {
        let truncated = !rights.is_empty() || creds.is_some();
        release_passed_handles(rights);
        return Ok((0, if truncated { linux_abi::MSG_CTRUNC as u32 } else { 0 }));
    }

    let mut out = Vec::new();
    let mut msg_flags = 0_u32;

    if let Some(c) = creds {
        let len = CMSG_HDR_LEN + size_of::<LinuxUcred>();
        let aligned = cmsg_align(len);
        if out.len() + len <= control_cap {
            let start = out.len();
            let end_space = (start + aligned).min(control_cap);
            out.resize(end_space, 0);
            out[start..start + 8].copy_from_slice(&(len as u64).to_ne_bytes());
            out[start + 8..start + 12]
                .copy_from_slice(&(linux_abi::SOL_SOCKET as u32).to_ne_bytes());
            out[start + 12..start + 16]
                .copy_from_slice(&(linux_abi::SCM_CREDENTIALS as u32).to_ne_bytes());
            out[start + 16..start + 20].copy_from_slice(&c.pid().to_ne_bytes());
            out[start + 20..start + 24].copy_from_slice(&c.uid().to_ne_bytes());
            out[start + 24..start + 28].copy_from_slice(&c.gid().to_ne_bytes());
        } else {
            msg_flags |= linux_abi::MSG_CTRUNC as u32;
        }
    }

    let mut installed_fds = Vec::new();
    if !rights.is_empty() {
        let remaining = control_cap.saturating_sub(out.len());
        let max_fit = remaining.saturating_sub(CMSG_HDR_LEN) / size_of::<i32>();
        if max_fit == 0 {
            msg_flags |= linux_abi::MSG_CTRUNC as u32;
            release_passed_handles(rights);
        } else {
            let fd_flags = if cloexec { multitask::FD_CLOEXEC } else { 0 };
            let mut leftover = Vec::new();
            let mut iter = rights.into_iter();
            let _ = multitask::with_current_user_process_state_mut(|_, _, process_state| {
                for passed in iter.by_ref() {
                    if installed_fds.len() >= max_fit {
                        leftover.push(passed);
                        continue;
                    }
                    let entry = multitask::HandleEntry::new_with_rights(
                        passed.handle().clone(),
                        passed.rights(),
                        fd_flags,
                        passed.status_flags(),
                    );
                    if let Some(new_fd) = process_state.handles_mut().install_entry(entry) {
                        installed_fds.push(new_fd as i32);
                    } else {
                        leftover.push(passed);
                    }
                }
            });
            if !leftover.is_empty() {
                msg_flags |= linux_abi::MSG_CTRUNC as u32;
                release_passed_handles(leftover);
            }
            if !installed_fds.is_empty() {
                let len = CMSG_HDR_LEN + installed_fds.len() * size_of::<i32>();
                let aligned = cmsg_align(len);
                let start = out.len();
                let end_space = (start + aligned).min(control_cap);
                out.resize(end_space, 0);
                out[start..start + 8].copy_from_slice(&(len as u64).to_ne_bytes());
                out[start + 8..start + 12]
                    .copy_from_slice(&(linux_abi::SOL_SOCKET as u32).to_ne_bytes());
                out[start + 12..start + 16]
                    .copy_from_slice(&(linux_abi::SCM_RIGHTS as u32).to_ne_bytes());
                for (i, &fd) in installed_fds.iter().enumerate() {
                    let pos = start + CMSG_HDR_LEN + i * 4;
                    out[pos..pos + 4].copy_from_slice(&fd.to_ne_bytes());
                }
            }
        }
    }

    if !out.is_empty() {
        if let Err(err) = usermem::write_current_user_bytes(control_ptr, &out) {
            if !installed_fds.is_empty() {
                let mut closed = Vec::new();
                let _ = multitask::with_current_user_process_state_mut(|_, _, process_state| {
                    for fd in installed_fds {
                        if let Some(h) = process_state.handles_mut().close(fd as u64) {
                            closed.push(h);
                        }
                    }
                });
                release_closed_handles(closed);
            }
            return Err(user_pointer_error_to_linux_errno(err));
        }
    }

    Ok((out.len(), msg_flags))
}

// ---- send / recv / read / write -------------------------------------------

fn send_core(
    sock: &UnixSocketHandle,
    data: &[u8],
    mut rights: Vec<PassedHandle>,
    creds: SocketCredentials,
    flags: u64,
    target_key: Option<&UnixNamespaceKey>,
) -> u64 {
    let nonblocking = sock.nonblocking() || flags & linux_abi::MSG_DONTWAIT != 0;
    let nosignal = flags & linux_abi::MSG_NOSIGNAL != 0;
    let deadline = deadline_tick_from_us(sock.sndtimeo_us());
    let mut sent = 0_usize;

    loop {
        let slice = &data[sent..];
        let to_send_rights = core::mem::take(&mut rights);
        match sock.try_send(slice, to_send_rights, creds, target_key) {
            Ok(outcome) => {
                publish_wake(outcome.wake);
                sent += outcome.bytes;
                if sock.socket_type() != UnixSocketType::Stream || sent >= data.len() {
                    return sent as u64;
                }
            }
            Err((UnixSocketError::WouldBlock { generation }, returned_rights)) => {
                rights = returned_rights;
                if sent > 0 {
                    release_passed_handles(rights);
                    return sent as u64;
                }
                if nonblocking {
                    release_passed_handles(rights);
                    return linux_errno(LINUX_EAGAIN);
                }
                if current_wait_interrupted() {
                    release_passed_handles(rights);
                    return linux_errno(LINUX_EINTR);
                }
                if let Err(errno) = wait_for_generation(sock.token_id(), generation, deadline) {
                    release_passed_handles(rights);
                    return linux_errno(errno);
                }
            }
            Err((UnixSocketError::BrokenPipe, returned_rights)) => {
                release_passed_handles(returned_rights);
                if sent > 0 {
                    return sent as u64;
                }
                if !nosignal {
                    raise_sigpipe();
                }
                return linux_errno(LINUX_EPIPE);
            }
            Err((err, returned_rights)) => {
                release_passed_handles(returned_rights);
                if sent > 0 {
                    return sent as u64;
                }
                return linux_errno(unix_error_errno(err));
            }
        }
    }
}

fn recv_core(
    sock: &UnixSocketHandle,
    max_len: usize,
    flags: u64,
) -> Result<kernel_ps::api::UnixReceivedMessage, i64> {
    let nonblocking = sock.nonblocking() || flags & linux_abi::MSG_DONTWAIT != 0;
    let peek = flags & linux_abi::MSG_PEEK != 0;
    let deadline = deadline_tick_from_us(sock.rcvtimeo_us());
    loop {
        match sock.try_recv(max_len, peek) {
            Ok(msg) => {
                publish_wake(msg.wake);
                return Ok(msg);
            }
            Err(UnixSocketError::WouldBlock { generation }) => {
                if nonblocking {
                    return Err(LINUX_EAGAIN);
                }
                if current_wait_interrupted() {
                    return Err(LINUX_EINTR);
                }
                wait_for_generation(sock.token_id(), generation, deadline)?;
            }
            Err(err) => return Err(unix_error_errno(err)),
        }
    }
}

pub(super) fn read_unix_socket(sock: &UnixSocketHandle, user_ptr: u64, user_len: u64) -> u64 {
    let Ok(len) = usize::try_from(user_len) else {
        return linux_errno(LINUX_EINVAL);
    };
    if len == 0 {
        return 0;
    }
    let capped = len.min(MAX_UNIX_IO_BYTES);
    if let Err(err) = usermem::validate_current_user_write_buffer(user_ptr, capped.min(4096)) {
        return linux_errno(user_pointer_error_to_linux_errno(err));
    }
    let msg = match recv_core(sock, capped, 0) {
        Ok(msg) => msg,
        Err(errno) => return linux_errno(errno),
    };
    release_passed_handles(msg.rights);
    if !msg.data.is_empty()
        && let Err(err) = usermem::write_current_user_bytes(user_ptr, &msg.data)
    {
        return linux_errno(user_pointer_error_to_linux_errno(err));
    }
    msg.data.len() as u64
}

pub(super) fn write_unix_socket(sock: &UnixSocketHandle, user_ptr: u64, user_len: u64) -> u64 {
    let Ok(len) = usize::try_from(user_len) else {
        return linux_errno(LINUX_EINVAL);
    };
    let capped = len.min(MAX_UNIX_IO_BYTES);
    let mut buf = Vec::new();
    if capped > 0 {
        if buf.try_reserve_exact(capped).is_err() {
            return linux_errno(LINUX_ENOMEM);
        }
        buf.resize(capped, 0);
        if let Err(err) = usermem::copy_from_current_user_exact(user_ptr, &mut buf) {
            return linux_errno(user_pointer_error_to_linux_errno(err));
        }
    }
    send_core(sock, &buf, Vec::new(), current_credentials(), 0, None)
}

pub(in crate::user::syscall::linux) fn try_sendto(
    fd: u64,
    buf_ptr: u64,
    len: u64,
    flags: u64,
    dest_addr: u64,
    addrlen: u64,
) -> Option<u64> {
    let sock = pin_unix_socket(fd)?;
    Some(linux_result((|| {
        let len = usize::try_from(len).map_err(|_| LINUX_EINVAL)?;
        let capped = len.min(MAX_UNIX_IO_BYTES);
        let mut buf = Vec::new();
        if capped > 0 {
            buf.try_reserve_exact(capped).map_err(|_| LINUX_ENOMEM)?;
            buf.resize(capped, 0);
            read_user_bytes(buf_ptr, &mut buf)?;
        }
        let target_key = if dest_addr != 0 && addrlen != 0 {
            Some(resolve_target_key(dest_addr, addrlen)?)
        } else {
            None
        };
        let raw = send_core(
            &sock.0,
            &buf,
            Vec::new(),
            current_credentials(),
            flags,
            target_key.as_ref(),
        );
        if is_linux_error(raw) { Err(-(raw as i64)) } else { Ok(raw) }
    })()))
}

pub(in crate::user::syscall::linux) fn try_recvfrom(
    fd: u64,
    buf_ptr: u64,
    len: u64,
    flags: u64,
    src_addr: u64,
    addrlen_ptr: u64,
) -> Option<u64> {
    let sock = pin_unix_socket(fd)?;
    Some(linux_result((|| {
        let len = usize::try_from(len).map_err(|_| LINUX_EINVAL)?;
        let capped = len.min(MAX_UNIX_IO_BYTES);
        if capped > 0 {
            validate_user_write(buf_ptr, capped.min(4096))?;
        }
        let msg = recv_core(&sock.0, capped, flags)?;
        release_passed_handles(msg.rights);
        if !msg.data.is_empty() {
            write_user_bytes(buf_ptr, &msg.data)?;
        }
        if src_addr != 0 && addrlen_ptr != 0 {
            let encoded = if msg.sender_addr == UnixAddress::Unnamed {
                Vec::new()
            } else {
                encode_unix_addr(&msg.sender_addr)
            };
            super::vfs_meta::write_current_sockaddr_payload(src_addr, addrlen_ptr, &encoded)?;
        }
        let ret = if flags & linux_abi::MSG_TRUNC != 0 {
            msg.original_len
        } else {
            msg.data.len()
        };
        Ok(ret as u64)
    })()))
}

fn read_iovecs(iov_ptr: u64, iovlen: u64) -> Result<Vec<LinuxIovec>, i64> {
    let Ok(count) = usize::try_from(iovlen) else {
        return Err(LINUX_EMSGSIZE);
    };
    if count > MAX_IOV_COUNT {
        return Err(LINUX_EMSGSIZE);
    }
    if count == 0 {
        return Ok(Vec::new());
    }
    let mut raw = vec![0_u8; count * size_of::<LinuxIovec>()];
    usermem::copy_from_current_user_exact(iov_ptr, &mut raw)
        .map_err(user_pointer_error_to_linux_errno)?;
    let mut iovs = Vec::with_capacity(count);
    for chunk in raw.chunks_exact(size_of::<LinuxIovec>()) {
        iovs.push(LinuxIovec {
            iov_base: u64::from_ne_bytes(chunk[0..8].try_into().unwrap()),
            iov_len: u64::from_ne_bytes(chunk[8..16].try_into().unwrap()),
        });
    }
    Ok(iovs)
}

pub(in crate::user::syscall::linux) fn try_sendmsg(
    fd: u64,
    msg_ptr: u64,
    flags: u64,
) -> Option<u64> {
    let sock = pin_unix_socket(fd)?;
    let hdr = match usermem::read_current_user_struct::<linux_abi::LinuxMsghdr>(msg_ptr) {
        Ok(hdr) => hdr,
        Err(err) => return Some(linux_errno(user_pointer_error_to_linux_errno(err))),
    };
    let iovs = match read_iovecs(hdr.msg_iov, hdr.msg_iovlen) {
        Ok(iovs) => iovs,
        Err(errno) => return Some(linux_errno(errno)),
    };
    let mut total_len = 0_usize;
    for iov in &iovs {
        let Ok(n) = usize::try_from(iov.iov_len) else {
            return Some(linux_errno(LINUX_EINVAL));
        };
        total_len = match total_len.checked_add(n) {
            Some(v) if v <= MAX_UNIX_IO_BYTES => v,
            _ => return Some(linux_errno(LINUX_EMSGSIZE)),
        };
    }
    let mut payload = Vec::new();
    if total_len > 0 {
        if payload.try_reserve_exact(total_len).is_err() {
            return Some(linux_errno(LINUX_ENOMEM));
        }
        payload.resize(total_len, 0);
        let mut off = 0_usize;
        for iov in &iovs {
            let n = iov.iov_len as usize;
            if n == 0 {
                continue;
            }
            if let Err(err) =
                usermem::copy_from_current_user_exact(iov.iov_base, &mut payload[off..off + n])
            {
                return Some(linux_errno(user_pointer_error_to_linux_errno(err)));
            }
            off += n;
        }
    }

    let caller_creds = current_credentials();
    let (rights, creds) = match parse_send_control(
        hdr.msg_control,
        hdr.msg_controllen as usize,
        caller_creds,
    ) {
        Ok(parsed) => parsed,
        Err(errno) => return Some(linux_errno(errno)),
    };
    let target_key = if hdr.msg_name != 0 && hdr.msg_namelen != 0 {
        match resolve_target_key(hdr.msg_name, u64::from(hdr.msg_namelen)) {
            Ok(key) => Some(key),
            Err(errno) => {
                release_passed_handles(rights);
                return Some(linux_errno(errno));
            }
        }
    } else {
        None
    };

    Some(send_core(
        &sock.0,
        &payload,
        rights,
        creds,
        flags,
        target_key.as_ref(),
    ))
}

pub(in crate::user::syscall::linux) fn try_recvmsg(
    fd: u64,
    msg_ptr: u64,
    flags: u64,
) -> Option<u64> {
    let sock = pin_unix_socket(fd)?;
    Some(linux_result((|| {
        let mut hdr: linux_abi::LinuxMsghdr = read_user_struct(msg_ptr)?;
        let iovs = read_iovecs(hdr.msg_iov, hdr.msg_iovlen)?;
        let mut total_cap = 0_usize;
        for iov in &iovs {
            let n = usize::try_from(iov.iov_len).map_err(|_| LINUX_EINVAL)?;
            total_cap = total_cap.saturating_add(n).min(MAX_UNIX_IO_BYTES);
        }

        let msg = recv_core(&sock.0, total_cap, flags & !MSG_WAITALL)?;

        let mut copied = 0_usize;
        for iov in &iovs {
            let n = (iov.iov_len as usize).min(msg.data.len().saturating_sub(copied));
            if n == 0 {
                continue;
            }
            if let Err(errno) = write_user_bytes(iov.iov_base, &msg.data[copied..copied + n]) {
                release_passed_handles(msg.rights);
                return Err(errno);
            }
            copied += n;
        }

        let mut out_flags = 0_u32;
        if msg.original_len > msg.data.len() {
            out_flags |= linux_abi::MSG_TRUNC as u32;
        }

        if hdr.msg_name != 0 && hdr.msg_namelen != 0 {
            let encoded = if msg.sender_addr == UnixAddress::Unnamed {
                Vec::new()
            } else {
                encode_unix_addr(&msg.sender_addr)
            };
            let copy_len = encoded.len().min(hdr.msg_namelen as usize);
            if copy_len > 0
                && let Err(errno) = write_user_bytes(hdr.msg_name, &encoded[..copy_len])
            {
                release_passed_handles(msg.rights);
                return Err(errno);
            }
            hdr.msg_namelen = encoded.len() as u32;
        }

        let cloexec = flags & linux_abi::MSG_CMSG_CLOEXEC != 0;
        let (ctrl_written, ctrl_flags) = write_recv_control(
            hdr.msg_control,
            hdr.msg_controllen as usize,
            msg.rights,
            msg.creds,
            cloexec,
        )?;
        hdr.msg_controllen = ctrl_written as u64;
        hdr.msg_flags = out_flags | ctrl_flags;

        write_user_struct(msg_ptr, &hdr)?;

        let ret = if flags & linux_abi::MSG_TRUNC != 0 {
            msg.original_len
        } else {
            copied
        };
        Ok(ret as u64)
    })()))
}
