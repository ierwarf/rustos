//! Filesystem-visible AF_UNIX names: `bind(path)` creates an `S_IFSOCK` node.
//!
//! - **Owner:** vfsd owns the namespace and the inode; netd owns only the
//!   binding (name -> listening socket token) and asks the namespace to create,
//!   stat and (on rollback) remove the node. Policy stays in Ring3.
//! - **Boundary:** paths are untrusted and already validated as UTF-8 without
//!   NUL by `sockaddr_bind_address_from_payload`; modes/ids come from the
//!   request credentials, never from the payload.
//! - **Lifecycle:** the node outlives the socket (Linux semantics): closing a
//!   bound socket frees only netd's binding, so a later `connect` to the still
//!   present node is `ECONNREFUSED`. `unlink` frees the *name*: a later `bind`
//!   succeeds and replaces any stale binding, a later `connect` is `ENOENT`.
//! - **Concurrency:** no `NetState` lock is held across a namespace call (a real
//!   backend is synchronous vfsd IPC). `bind` therefore validates, releases the
//!   lock, calls the namespace, then revalidates the socket before publishing
//!   the binding and rolls the node back if the socket changed in between.
//! - **Failure:** the namespace's `EEXIST` is reported as `EADDRINUSE`; every
//!   other errno (`ENOENT` parent, `EACCES`, `ENOTDIR`, ...) passes through.
//! - **Forbidden:** inventing a node that vfsd does not have, or keeping a name
//!   bound after vfsd reports it absent.
//!
//! # Production backend
//!
//! [`VfsNamespace`] is the vfsd-backed [`PathNamespace`]; [`IpcVfsTransport`]
//! is its synchronous, deadline-bounded (`VFS_CALL_TIMEOUT_MS`) IPC client
//! (netd -> vfsd lookup edge: `SERVICE_ROUTING_MANIFEST` in rootd).
//!
//! - `create` = `VFS_IPC_OP_MKNOD` (`S_IFSOCK | mode`), ownership from the
//!   binding caller's request `euid`/`egid`; vfsd `EEXIST` -> `EADDRINUSE`.
//! - `stat` = `VFS_IPC_OP_NEWFSTATAT`: `S_IFSOCK` -> `Socket`, any other kind
//!   -> `NotSocket` (bind `EADDRINUSE`, connect `ECONNREFUSED`, as Linux),
//!   `ENOENT` -> `Absent`, other errno passes through.
//! - Creation and lookup return mount/node identity in `value`/`aux`.
//! - `remove` = identity-checked `VFS_IPC_OP_UNLINKAT` of the node this bind made.
//! - Every request carries `VFS_REQUEST_FLAG_NETD_SOCKET_NODE`: vfsd accepts
//!   delegated ownership only from the live netd owner and only for these
//!   shapes (`vfsd::netd_namespace_admission`), and unlinks only socket nodes.
//! - The original caller PID is delegated in `arg3`; vfsd resolves relative
//!   names against that caller's cwd after authenticating netd.
//! - vfsd unreachable or timing out is an error to the caller (`EIO`/errno of
//!   the call), never a silent fallback to a private binding.

use super::*;
use rustos_user_abi::syscall::{
    IPC_SERVICE_VFSD, VFS_IPC_OP_MKNOD, VFS_IPC_OP_NEWFSTATAT, VFS_IPC_OP_UNLINKAT, VFS_MKNOD_S_IFMT,
    VFS_MKNOD_S_IFSOCK, VFS_REQUEST_FLAG_NETD_SOCKET_NODE, VfsIpcRequest, VfsIpcResponse,
};

/// Stable mount and inode identity returned atomically by vfsd.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct NodeIdentity { pub mount: u64, pub node: u64 }

/// What the namespace knows about one path.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum NodeState {
    /// Nothing at the path (never created, or unlinked). Produced only by a
    /// vfsd-backed namespace.
    Absent,
    /// An `S_IFSOCK` node exists. Produced only by a vfsd-backed namespace.
    Socket(NodeIdentity),
    /// Something that is not a socket exists at the path (regular file,
    /// directory, symlink). Produced only by a vfsd-backed namespace.
    NotSocket,
    /// The namespace does not model this path; netd's binding table is the
    /// only authority (the pre-vfsd behaviour).
    Unmanaged,
}

pub(super) trait PathNamespace {
    fn stat_socket_node(&mut self, client_pid: u64, path: &str, uid: u32, gid: u32, check_write: bool) -> Result<NodeState, i32>;
    /// Creates the node atomically. Must fail with `EEXIST` (or `EADDRINUSE`)
    /// when any node exists at `path`.
    fn create_socket_node(&mut self, client_pid: u64, path: &str, uid: u32, gid: u32, mode: u32) -> Result<Option<NodeIdentity>, i32>;
    /// Rollback of a node this bind just created; never user-initiated.
    fn remove_socket_node(&mut self, client_pid: u64, path: &str, uid: u32, gid: u32, identity: NodeIdentity) -> Result<(), i32>;
}

/// No inode: netd's table is the namespace. Test-only baseline.
#[cfg(test)]
pub(super) struct PrivateNamespace;

#[cfg(test)]
impl PathNamespace for PrivateNamespace {
    fn stat_socket_node(&mut self, _: u64, _path: &str, _: u32, _: u32, _: bool) -> Result<NodeState, i32> {
        Ok(NodeState::Unmanaged)
    }
    fn create_socket_node(&mut self, _: u64, _: &str, _: u32, _: u32, _: u32) -> Result<Option<NodeIdentity>, i32> {
        Ok(None)
    }
    fn remove_socket_node(&mut self, _: u64, _path: &str, _: u32, _: u32, _: NodeIdentity) -> Result<(), i32> {
        Ok(())
    }
}

/// Upper bound of one vfsd call. A bind/connect to a stuck vfsd fails with
/// `ETIMEDOUT` instead of wedging netd's loop indefinitely.
const VFS_CALL_TIMEOUT_MS: u64 = 2_000;

/// One synchronous request/response exchange with vfsd.
pub(super) trait VfsTransport {
    /// `(pid, tid)` that vfsd must see as the request's own sender identity.
    fn identity(&mut self) -> Result<(u64, u64), i32>;
    fn call(&mut self, request: &VfsIpcRequest, response: &mut VfsIpcResponse) -> Result<(), i32>;
}

/// The vfsd-backed namespace. Buffers are boxed: a request is ~1 KiB and the
/// reply ~4 KiB, too large to build on every bind's stack frame twice.
pub(super) struct VfsNamespace<T: VfsTransport> {
    transport: T,
    request: Box<VfsIpcRequest>,
    response: Box<VfsIpcResponse>,
    serial: u64,
}

impl<T: VfsTransport> VfsNamespace<T> {
    pub(super) fn new(transport: T) -> Self {
        Self {
            transport,
            request: Box::<VfsIpcRequest>::default(),
            response: Box::<VfsIpcResponse>::default(),
            serial: 0,
        }
    }

    /// Runs one request; `Ok(())` is vfsd status 0, `Err` the errno.
    fn exchange(
        &mut self,
        op: u16,
        flags: u32,
        ids: (u32, u32),
        arg0: u64,
        identity: Option<NodeIdentity>,
        client_pid: u64,
        path: &str,
    ) -> Result<(), i32> {
        if path.len() > rustos_user_abi::syscall::VFS_IPC_PATH_CAPACITY {
            return Err(libc::ENAMETOOLONG);
        }
        let (pid, tid) = self.transport.identity()?;
        self.serial = self.serial.wrapping_add(1).max(1);
        *self.request = VfsIpcRequest {
            op,
            flags: flags | VFS_REQUEST_FLAG_NETD_SOCKET_NODE,
            pid,
            tid,
            uid: ids.0,
            gid: ids.1,
            euid: ids.0,
            egid: ids.1,
            dirfd: linux_abi::AT_FDCWD as u64,
            arg0,
            arg1: identity.map_or(0, |id| id.mount),
            arg2: identity.map_or(0, |id| id.node),
            arg3: client_pid,
            operation_hi: pid,
            operation_lo: self.serial,
            path_len: path.len() as u32,
            ..VfsIpcRequest::default()
        };
        self.request.path[..path.len()].copy_from_slice(path.as_bytes());
        *self.response = VfsIpcResponse::default();
        self.transport.call(&self.request, &mut self.response)?;
        match self.response.status {
            0 => Ok(()),
            status => Err(status.unsigned_abs() as i32),
        }
    }
    fn identity(&self) -> Result<NodeIdentity, i32> {
        if self.response.aux == 0 { return Err(libc::EPROTO); }
        Ok(NodeIdentity { mount: self.response.value, node: self.response.aux })
    }

}

impl<T: VfsTransport> PathNamespace for VfsNamespace<T> {
    fn stat_socket_node(&mut self, client_pid: u64, path: &str, uid: u32, gid: u32, check_write: bool) -> Result<NodeState, i32> {
        match self.exchange(VFS_IPC_OP_NEWFSTATAT, 0, (uid, gid), 0, None, client_pid, path) {
            Ok(()) => {}
            Err(libc::ENOENT) => return Ok(NodeState::Absent),
            Err(errno) => return Err(errno),
        }
        let stat_len = size_of::<linux_abi::LinuxStat>();
        let payload = &self.response.payload;
        if (self.response.payload_len as usize) < stat_len || payload.len() < stat_len {
            return Err(libc::EPROTO);
        }
        let at = core::mem::offset_of!(linux_abi::LinuxStat, st_mode);
        let mode = u32::from_ne_bytes([payload[at], payload[at + 1], payload[at + 2], payload[at + 3]]);
        Ok(if u64::from(mode) & VFS_MKNOD_S_IFMT == VFS_MKNOD_S_IFSOCK {
            let read_u32 = |offset: usize| u32::from_ne_bytes(payload[offset..offset + 4].try_into().unwrap());
            let owner = read_u32(core::mem::offset_of!(linux_abi::LinuxStat, st_uid));
            let group = read_u32(core::mem::offset_of!(linux_abi::LinuxStat, st_gid));
            let bits = if uid == owner { mode >> 6 } else if gid == group { mode >> 3 } else { mode };
            if check_write && uid != 0 && bits & 2 == 0 { return Err(libc::EACCES); }
            NodeState::Socket(self.identity()?)
        } else {
            NodeState::NotSocket
        })
    }

    fn create_socket_node(&mut self, client_pid: u64, path: &str, uid: u32, gid: u32, mode: u32) -> Result<Option<NodeIdentity>, i32> {
        let arg0 = VFS_MKNOD_S_IFSOCK | u64::from(mode & 0o7777);
        self.exchange(VFS_IPC_OP_MKNOD, 0, (uid, gid), arg0, None, client_pid, path)?;
        Ok(Some(self.identity()?))
    }

    fn remove_socket_node(&mut self, client_pid: u64, path: &str, uid: u32, gid: u32, identity: NodeIdentity) -> Result<(), i32> {
        self.exchange(VFS_IPC_OP_UNLINKAT, 0, (uid, gid), 0, Some(identity), client_pid, path)
    }
}

/// The production transport: a cached vfsd endpoint, `call_bounded`.
pub(super) struct IpcVfsTransport {
    endpoint: i64,
}

impl IpcVfsTransport {
    const fn new() -> Self {
        Self { endpoint: 0 }
    }
}

impl VfsTransport for IpcVfsTransport {
    fn identity(&mut self) -> Result<(u64, u64), i32> {
        const SYS_GETPID: u64 = 39;
        const SYS_GETTID: u64 = 186;
        // SAFETY: getpid/gettid take no arguments and touch no memory.
        let pid = unsafe { rustos_svc_runtime::syscall::syscall0(SYS_GETPID) };
        let tid = unsafe { rustos_svc_runtime::syscall::syscall0(SYS_GETTID) };
        if pid <= 0 || tid <= 0 {
            return Err(libc::EIO);
        }
        Ok((pid as u64, tid as u64))
    }

    fn call(&mut self, request: &VfsIpcRequest, response: &mut VfsIpcResponse) -> Result<(), i32> {
        if self.endpoint <= 0 {
            let endpoint = rustos_svc_runtime::ipc::lookup_service_endpoint(IPC_SERVICE_VFSD);
            if endpoint < 0 {
                return Err(endpoint.unsigned_abs().min(i32::MAX as u64) as i32);
            }
            if endpoint == 0 {
                return Err(libc::EPROTO);
            }
            self.endpoint = endpoint;
        }
        // SAFETY: both buffers are exclusive, fixed-size and outlive the call.
        let got = unsafe {
            rustos_svc_runtime::ipc::call_bounded(
                self.endpoint as u64,
                (request as *const VfsIpcRequest).cast::<u8>(),
                size_of::<VfsIpcRequest>(),
                (response as *mut VfsIpcResponse).cast::<u8>(),
                size_of::<VfsIpcResponse>(),
                VFS_CALL_TIMEOUT_MS,
            )
        };
        if got < 0 {
            // A restarted vfsd has a new endpoint; look it up on the next call.
            // The failed call is not retried: MKNOD is not replay safe.
            self.endpoint = 0;
            return Err(got.unsigned_abs().min(i32::MAX as u64) as i32);
        }
        if got as usize != size_of::<VfsIpcResponse>() {
            return Err(libc::EPROTO);
        }
        Ok(())
    }
}

fn production_namespace() -> &'static Mutex<VfsNamespace<IpcVfsTransport>> {
    static NAMESPACE: OnceLock<Mutex<VfsNamespace<IpcVfsTransport>>> = OnceLock::new();
    NAMESPACE.get_or_init(|| Mutex::new(VfsNamespace::new(IpcVfsTransport::new())))
}

pub(super) fn with_production_namespace<R>(f: impl FnOnce(&mut dyn PathNamespace) -> R) -> R {
    let mut guard = production_namespace()
        .lock()
        .unwrap_or_else(PoisonError::into_inner);
    f(&mut *guard)
}
