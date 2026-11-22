//! Filesystem-visible and abstract AF_UNIX names through `PathNamespace`.

use super::*;
use std::collections::BTreeSet;
use path_namespace::NodeIdentity;

/// Recording fake of the vfsd namespace: a set of socket nodes.
///
/// Inode bindings live in the process-global net state, so concurrently running
/// tests must not hand out the same `(mount, node)`: every instance numbers its
/// nodes from a disjoint range.
struct FakeNamespace {
    nodes: BTreeSet<String>,
    ids: BTreeMap<String, u64>,
    next_id: u64,
    created: Vec<(String, u32, u32, u32)>,
    stats: Vec<String>,
    fail_create: Option<i32>,
}

impl Default for FakeNamespace {
    fn default() -> Self {
        static NEXT_RANGE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
        Self {
            nodes: BTreeSet::new(),
            ids: BTreeMap::new(),
            next_id: NEXT_RANGE.fetch_add(1, std::sync::atomic::Ordering::Relaxed) << 20,
            created: Vec::new(),
            stats: Vec::new(),
            fail_create: None,
        }
    }
}

impl FakeNamespace {
    /// What `unlink(2)` does in vfsd: the name goes away, netd is not told.
    fn unlink(&mut self, path: &str) -> bool {
        self.ids.remove(path);
        self.nodes.remove(path)
    }
}

impl PathNamespace for FakeNamespace {
    fn stat_socket_node(&mut self, _: u64, path: &str, _: u32, _: u32, _: bool) -> Result<NodeState, i32> {
        self.stats.push(path.to_string());
        Ok(if self.nodes.contains(path) {
            NodeState::Socket(NodeIdentity { mount: 1, node: self.ids[path] })
        } else {
            NodeState::Absent
        })
    }
    fn create_socket_node(&mut self, _: u64, path: &str, uid: u32, gid: u32, mode: u32) -> Result<Option<NodeIdentity>, i32> {
        if let Some(errno) = self.fail_create {
            return Err(errno);
        }
        if !self.nodes.insert(path.to_string()) {
            return Err(libc::EEXIST);
        }
        self.created.push((path.to_string(), uid, gid, mode));
        self.next_id += 1;
        self.ids.insert(path.to_string(), self.next_id);
        Ok(Some(NodeIdentity { mount: 1, node: self.next_id }))
    }
    fn remove_socket_node(&mut self, _: u64, path: &str, _: u32, _: u32, identity: NodeIdentity) -> Result<(), i32> {
        if self.ids.get(path) != Some(&identity.node) { return Err(libc::ESTALE); }
        self.ids.remove(path);
        self.nodes.remove(path);
        Ok(())
    }
}

fn idle(kind: UnixSocketKind) -> UnixSocket {
    UnixSocket {
        kind,
        owner: Credentials::default(),
        refs: 1,
        options: SocketOptions::default(),
        bound_path: None,
        bound_inode: None,
        local_path: None,
        peer_path: None,
        state: UnixSocketState::Idle,
    }
}

fn sockaddr(token: u64, name: &[u8]) -> NetdIpcRequest {
    let mut request = NetdIpcRequest {
        socket_token: token,
        pid: 123,
        arg3: 0o022,
        euid: 1000,
        egid: 1001,
        ..NetdIpcRequest::default()
    };
    request.payload[..2].copy_from_slice(&(linux_abi::AF_UNIX as u16).to_ne_bytes());
    request.payload[2..2 + name.len()].copy_from_slice(name);
    request.payload_len = (2 + name.len()) as u32;
    request
}

fn fresh(tokens: &[u64]) {
    let mut state = lock_net_state();
    for &token in tokens {
        state.sockets.insert(token, idle(UnixSocketKind::Stream));
    }
}

fn drop_sockets(tokens: &[u64]) {
    let mut state = lock_net_state();
    for &token in tokens {
        // Final close path: frees the binding exactly as production does.
        let _ = retire_unix_socket(&mut state, token);
    }
}

fn bound_to(path: &UnixAddress) -> Option<u64> {
    lock_net_state().bindings.get(path).copied()
}

#[test]
fn bind_creates_a_socket_node_owned_by_the_caller_and_second_bind_is_eaddrinuse() {
    let (a, b) = (0x9a0_0001, 0x9a0_0002);
    fresh(&[a, b]);
    let mut ns = FakeNamespace::default();
    assert_eq!(
        handle_bind_with(&sockaddr(a, b"/tmp/ns-bind-a\0"), &mut ns),
        0
    );
    assert_eq!(
        ns.created,
        vec![(
            "/tmp/ns-bind-a".to_string(),
            1000,
            1001,
            0o755
        )]
    );
    assert_eq!(bound_to(&UnixAddress::from("/tmp/ns-bind-a")), Some(a));
    assert_eq!(
        handle_bind_with(&sockaddr(b, b"/tmp/ns-bind-a\0"), &mut ns),
        libc::EADDRINUSE
    );
    // The node outlives the socket, so the name stays taken after close...
    drop_sockets(&[a]);
    assert_eq!(bound_to(&UnixAddress::from("/tmp/ns-bind-a")), None);
    assert_eq!(
        handle_bind_with(&sockaddr(b, b"/tmp/ns-bind-a\0"), &mut ns),
        libc::EADDRINUSE
    );
    // ...until unlink frees it.
    assert!(ns.unlink("/tmp/ns-bind-a"));
    assert_eq!(
        handle_bind_with(&sockaddr(b, b"/tmp/ns-bind-a\0"), &mut ns),
        0
    );
    assert_eq!(bound_to(&UnixAddress::from("/tmp/ns-bind-a")), Some(b));
    drop_sockets(&[b]);
}

#[test]
fn bind_after_unlink_replaces_the_stale_binding_of_a_live_socket() {
    let (old, new) = (0x9a0_0011, 0x9a0_0012);
    fresh(&[old, new]);
    let mut ns = FakeNamespace::default();
    let name = sockaddr(old, b"/tmp/ns-stale\0");
    assert_eq!(handle_bind_with(&name, &mut ns), 0);
    assert!(ns.unlink("/tmp/ns-stale"));
    // The old socket is still open and still thinks it owns the name.
    assert_eq!(bound_to(&UnixAddress::from("/tmp/ns-stale")), Some(old));
    assert_eq!(
        handle_bind_with(&sockaddr(new, b"/tmp/ns-stale\0"), &mut ns),
        0
    );
    assert_eq!(bound_to(&UnixAddress::from("/tmp/ns-stale")), Some(new));
    // Closing the old socket must not free the new owner's binding.
    drop_sockets(&[old]);
    assert_eq!(bound_to(&UnixAddress::from("/tmp/ns-stale")), Some(new));
    drop_sockets(&[new]);
}

#[test]
fn connect_to_an_unlinked_or_unbound_name_fails_with_the_linux_errno() {
    let (server, client) = (0x9a0_0021, 0x9a0_0022);
    fresh(&[server, client]);
    let mut ns = FakeNamespace::default();
    assert_eq!(
        handle_bind_with(&sockaddr(server, b"/tmp/ns-conn\0"), &mut ns),
        0
    );
    // Node unlinked: ENOENT even though the old listener's binding is live.
    assert!(ns.unlink("/tmp/ns-conn"));
    assert_eq!(
        handle_connect_with(&sockaddr(client, b"/tmp/ns-conn\0"), &mut ns),
        libc::ENOENT
    );
    // Node present but nobody bound to it (owner closed): ECONNREFUSED.
    ns.create_socket_node(123, "/tmp/ns-orphan", 0, 0, 0).unwrap();
    assert_eq!(
        handle_connect_with(&sockaddr(client, b"/tmp/ns-orphan\0"), &mut ns),
        libc::ECONNREFUSED
    );
    // Never created at all: ENOENT.
    assert_eq!(
        handle_connect_with(&sockaddr(client, b"/tmp/ns-never\0"), &mut ns),
        libc::ENOENT
    );
    drop_sockets(&[server, client]);
}

#[test]
fn namespace_errors_pass_through_and_leave_no_binding() {
    let sock = 0x9a0_0031;
    fresh(&[sock]);
    let mut ns = FakeNamespace {
        fail_create: Some(libc::EACCES),
        ..FakeNamespace::default()
    };
    assert_eq!(
        handle_bind_with(&sockaddr(sock, b"/proc/denied\0"), &mut ns),
        libc::EACCES
    );
    assert_eq!(bound_to(&UnixAddress::from("/proc/denied")), None);
    // A refused bind leaves the socket bindable.
    ns.fail_create = None;
    assert_eq!(
        handle_bind_with(&sockaddr(sock, b"/tmp/ns-retry\0"), &mut ns),
        0
    );
    drop_sockets(&[sock]);
}

#[test]
fn wayland_path_is_managed_in_the_namespace() {
    let sock = 0x9a0_0041;
    fresh(&[sock]);
    let mut ns = FakeNamespace::default();
    assert_eq!(
        handle_bind_with(&sockaddr(sock, b"/run/user/1000/wayland-0\0"), &mut ns),
        0
    );
    assert_eq!(ns.created.len(), 1);
    assert_eq!(ns.stats.len(), 1);
    assert_eq!(
        bound_to(&UnixAddress::from("/run/user/1000/wayland-0")),
        Some(sock)
    );
    drop_sockets(&[sock]);
}

#[test]
fn abstract_names_use_addrlen_not_nul_termination_and_free_on_close() {
    let (a, b, c) = (0x9a0_0051, 0x9a0_0052, 0x9a0_0053);
    fresh(&[a, b, c]);
    let mut ns = FakeNamespace::default();
    // "\0ab\0cd": an embedded NUL is part of the name.
    let name = b"\0ab\0cd";
    assert_eq!(handle_bind_with(&sockaddr(a, name), &mut ns), 0);
    assert!(
        ns.created.is_empty() && ns.stats.is_empty(),
        "abstract names have no inode"
    );
    assert_eq!(bound_to(&UnixAddress::Abstract(name.to_vec())), Some(a));
    // A different length is a different name.
    assert_eq!(handle_bind_with(&sockaddr(c, b"\0ab"), &mut ns), 0);
    assert_eq!(
        handle_bind_with(&sockaddr(b, name), &mut ns),
        libc::EADDRINUSE
    );
    // getsockname returns exactly addrlen bytes.
    let mut response = NetdIpcResponse::default();
    let query = NetdIpcRequest {
        socket_token: a,
        ..NetdIpcRequest::default()
    };
    assert_eq!(handle_getsockname(&query, &mut response), 0);
    assert_eq!(response.payload_len as usize, 2 + name.len());
    assert_eq!(&response.payload[2..2 + name.len()], name);
    // Closing frees the name for reuse.
    drop_sockets(&[a]);
    assert_eq!(handle_bind_with(&sockaddr(b, name), &mut ns), 0);
    drop_sockets(&[b, c]);
}

#[test]
fn connect_to_an_unbound_abstract_name_is_econnrefused() {
    let client = 0x9a0_0061;
    fresh(&[client]);
    let mut ns = FakeNamespace::default();
    assert_eq!(
        handle_connect_with(&sockaddr(client, b"\0nobody-listens"), &mut ns),
        libc::ECONNREFUSED
    );
    drop_sockets(&[client]);
}

#[test]
fn autobind_assigns_a_unique_five_digit_abstract_name() {
    let (a, b) = (0x9a0_0071, 0x9a0_0072);
    fresh(&[a, b]);
    let mut ns = FakeNamespace::default();
    for token in [a, b] {
        let mut request = sockaddr(token, b"");
        request.payload_len = 2;
        assert_eq!(handle_bind_with(&request, &mut ns), 0);
    }
    let names: Vec<_> = [a, b]
        .iter()
        .map(|token| lock_net_state().sockets[token].local_path.clone().unwrap())
        .collect();
    assert_ne!(names[0], names[1]);
    for name in &names {
        let UnixAddress::Abstract(bytes) = name else {
            panic!("autobind is abstract")
        };
        assert_eq!(bytes.len(), 6);
        assert_eq!(bytes[0], 0);
    }
    drop_sockets(&[a, b]);
}

// ---- the vfsd-backed namespace against a fake vfsd -------------------------

mod vfsd_backed {
    use super::*;
    use path_namespace::{VfsNamespace, VfsTransport};
    use rustos_user_abi::syscall::{
        VFS_IPC_ABI_VERSION, VFS_IPC_OP_MKNOD, VFS_IPC_OP_NEWFSTATAT, VFS_IPC_OP_UNLINKAT,
        VFS_MKNOD_S_IFMT, VFS_MKNOD_S_IFREG, VFS_MKNOD_S_IFSOCK, VFS_REQUEST_FLAG_NETD_SOCKET_NODE,
        VfsIpcRequest, VfsIpcResponse,
    };
    use std::sync::Arc;

    const NETD_PID: u64 = 77;

    /// What vfsd would hold: directories and typed nodes, plus the requests it
    /// saw. It applies the same admission rules as `vfsd::netd_namespace_admission`
    /// (flag => netd-only shapes) and `S_IFSOCK`-only unlink.
    #[derive(Default)]
    struct FakeVfsd {
        dirs: Vec<String>,
        nodes: BTreeMap<String, u64>,
        ids: BTreeMap<String, u64>,
        next_id: u64,
        owners: BTreeMap<String, (u32, u32, u64)>,
        requests: Vec<(u16, u32, u32, u32, String)>,
        calls_fail: Option<i32>,
        protected_parent: Option<(String, u32)>,
    }

    #[derive(Clone, Default)]
    struct Shared(Arc<Mutex<FakeVfsd>>);

    impl Shared {
        fn with_dirs(dirs: &[&str]) -> Self {
            let shared = Self::default();
            shared.0.lock().unwrap().dirs = dirs.iter().map(|d| d.to_string()).collect();
            shared
        }
        /// `unlink(2)` / `mknod(S_IFREG)` by some other client.
        fn unlink(&self, path: &str) -> bool {
            let mut vfs = self.0.lock().unwrap();
            vfs.ids.remove(path);
            vfs.nodes.remove(path).is_some()
        }
        fn make_regular(&self, path: &str) {
            self.0.lock().unwrap().nodes.insert(path.to_string(), VFS_MKNOD_S_IFREG);
        }
        fn has_socket(&self, path: &str) -> bool {
            self.0.lock().unwrap().nodes.get(path) == Some(&VFS_MKNOD_S_IFSOCK)
        }
    }

    impl VfsTransport for Shared {
        fn identity(&mut self) -> Result<(u64, u64), i32> {
            Ok((NETD_PID, NETD_PID))
        }
        fn call(&mut self, request: &VfsIpcRequest, response: &mut VfsIpcResponse) -> Result<(), i32> {
            let mut vfsd = self.0.lock().unwrap();
            if let Some(errno) = vfsd.calls_fail {
                return Err(errno);
            }
            assert_eq!(request.version, VFS_IPC_ABI_VERSION);
            assert!(request.operation_lo != 0);
            assert_eq!((request.pid, request.tid), (NETD_PID, NETD_PID));
            assert!(request.flags & VFS_REQUEST_FLAG_NETD_SOCKET_NODE != 0);
            let original = core::str::from_utf8(&request.path[..request.path_len as usize]).unwrap();
            let path = if original.starts_with('/') { original.to_string() } else { format!("/cwd/{original}") };
            assert_ne!(request.arg3, 0, "caller pid must be delegated");
            vfsd.requests.push((request.op, request.flags, request.euid, request.egid, path.clone()));
            let parent = path.rsplit_once('/').map(|(p, _)| if p.is_empty() { "/" } else { p }).unwrap();
            response.status = 0;
            if vfsd.protected_parent.as_ref().is_some_and(|(dir, owner)| path.starts_with(&format!("{dir}/")) && request.euid != *owner && request.euid != 0) {
                response.status = libc::EACCES;
                return Ok(());
            }
            match request.op {
                VFS_IPC_OP_MKNOD => {
                    assert_eq!(request.arg0 & VFS_MKNOD_S_IFMT, VFS_MKNOD_S_IFSOCK);
                    if !vfsd.dirs.iter().any(|d| d == parent) {
                        response.status = libc::ENOENT;
                    } else if vfsd.nodes.contains_key(&path) {
                        response.status = libc::EEXIST;
                    } else {
                        vfsd.nodes.insert(path.clone(), VFS_MKNOD_S_IFSOCK);
                        vfsd.next_id += 1;
                        let id = vfsd.next_id;
                        vfsd.ids.insert(path.clone(), id);
                        response.value = 1; response.aux = id;
                        vfsd.owners.insert(path, (request.euid, request.egid, request.arg0 & 0o7777));
                    }
                }
                VFS_IPC_OP_NEWFSTATAT => match vfsd.nodes.get(&path) {
                    None if vfsd.dirs.iter().any(|d| d == parent) => response.status = libc::ENOENT,
                    None => response.status = libc::ENOENT,
                    Some(kind) => {
                        let mut stat = linux_abi::LinuxStat::default();
                        let (uid, gid, mode) = vfsd.owners.get(&path).copied().unwrap_or((0, 0, 0o644));
                        stat.st_mode = (*kind | mode) as u32;
                        stat.st_uid = uid; stat.st_gid = gid;
                        let bytes = unsafe {
                            core::slice::from_raw_parts(
                                (&stat as *const linux_abi::LinuxStat).cast::<u8>(),
                                size_of::<linux_abi::LinuxStat>(),
                            )
                        };
                        response.payload[..bytes.len()].copy_from_slice(bytes);
                        response.payload_len = bytes.len() as u32;
                        if let Some(id) = vfsd.ids.get(&path) { response.value = 1; response.aux = *id; }
                    }
                },
                VFS_IPC_OP_UNLINKAT => match vfsd.nodes.get(&path) {
                    None => response.status = libc::ENOENT,
                    Some(kind) if *kind != VFS_MKNOD_S_IFSOCK => response.status = libc::EPERM,
                    Some(_) if request.arg1 != 1 || vfsd.ids.get(&path) != Some(&request.arg2) => response.status = libc::ESTALE,
                    Some(_) => {
                        vfsd.ids.remove(&path);
                        vfsd.nodes.remove(&path);
                    }
                },
                other => panic!("netd sent unexpected vfsd op {other}"),
            }
            Ok(())
        }
    }

    fn namespace(shared: &Shared) -> VfsNamespace<Shared> {
        VfsNamespace::new(shared.clone())
    }

    #[test]
    fn bind_makes_a_socket_inode_second_bind_fails_unlink_then_rebind_works() {
        let (a, b) = (0x9b0_0001, 0x9b0_0002);
        fresh(&[a, b]);
        let vfsd = Shared::with_dirs(&["/", "/tmp"]);
        let mut ns = namespace(&vfsd);
        assert_eq!(handle_bind_with(&sockaddr(a, b"/tmp/vb-a\0"), &mut ns), 0);
        assert!(vfsd.has_socket("/tmp/vb-a"));
        // Ownership and mode come from the binding caller's credentials.
        assert_eq!(
            vfsd.0.lock().unwrap().owners["/tmp/vb-a"],
            (1000, 1001, 0o755_u64)
        );
        assert_eq!(handle_bind_with(&sockaddr(b, b"/tmp/vb-a\0"), &mut ns), libc::EADDRINUSE);
        // Closing the socket leaves the node: the name stays taken.
        drop_sockets(&[a]);
        assert_eq!(handle_bind_with(&sockaddr(b, b"/tmp/vb-a\0"), &mut ns), libc::EADDRINUSE);
        assert!(vfsd.unlink("/tmp/vb-a"));
        assert_eq!(handle_bind_with(&sockaddr(b, b"/tmp/vb-a\0"), &mut ns), 0);
        assert!(vfsd.has_socket("/tmp/vb-a"));
        drop_sockets(&[b]);
    }

    #[test]
    fn connect_follows_the_inode_not_the_private_binding() {
        let (server, client) = (0x9b0_0011, 0x9b0_0012);
        fresh(&[server, client]);
        let vfsd = Shared::with_dirs(&["/", "/run"]);
        let mut ns = namespace(&vfsd);
        assert_eq!(handle_bind_with(&sockaddr(server, b"/run/vb-conn\0"), &mut ns), 0);
        assert!(vfsd.unlink("/run/vb-conn"));
        assert_eq!(
            handle_connect_with(&sockaddr(client, b"/run/vb-conn\0"), &mut ns),
            libc::ENOENT
        );
        // A regular file at the path is a name in use but never a listener.
        vfsd.make_regular("/run/vb-file");
        assert_eq!(
            handle_connect_with(&sockaddr(client, b"/run/vb-file\0"), &mut ns),
            libc::ECONNREFUSED
        );
        assert_eq!(
            handle_bind_with(&sockaddr(client, b"/run/vb-file\0"), &mut ns),
            libc::EADDRINUSE
        );
        drop_sockets(&[server, client]);
    }

    #[test]
    fn missing_parent_and_vfsd_failures_surface_without_binding() {
        let sock = 0x9b0_0121;
        fresh(&[sock]);
        let vfsd = Shared::with_dirs(&["/"]);
        let mut ns = namespace(&vfsd);
        assert_eq!(handle_bind_with(&sockaddr(sock, b"/nodir/x\0"), &mut ns), libc::ENOENT);
        assert_eq!(bound_to(&UnixAddress::from("/nodir/x")), None);
        vfsd.0.lock().unwrap().calls_fail = Some(libc::ETIMEDOUT);
        assert_eq!(handle_bind_with(&sockaddr(sock, b"/x\0"), &mut ns), libc::ETIMEDOUT);
        assert_eq!(bound_to(&UnixAddress::from("/x")), None);
        vfsd.0.lock().unwrap().calls_fail = None;
        assert_eq!(handle_bind_with(&sockaddr(sock, b"/x\0"), &mut ns), 0);
        drop_sockets(&[sock]);
    }

    #[test]
    fn delegated_credentials_control_traversal_and_socket_write_access() {
        let (server, client) = (0x9b0_0091, 0x9b0_0092);
        fresh(&[server, client]);
        let vfsd = Shared::with_dirs(&["/", "/private", "/public"]);
        vfsd.0.lock().unwrap().protected_parent = Some(("/private".into(), 1000));
        let mut ns = namespace(&vfsd);
        let mut stranger = sockaddr(client, b"/private/missing\0");
        stranger.euid = 2000; stranger.egid = 2000;
        assert_eq!(handle_connect_with(&stranger, &mut ns), libc::EACCES);
        assert_eq!(handle_bind_with(&stranger, &mut ns), libc::EACCES);
        assert_eq!(handle_bind_with(&sockaddr(server, b"/public/socket\0"), &mut ns), 0);
        let mut connect = sockaddr(client, b"/public/socket\0");
        connect.euid = 2000; connect.egid = 2000;
        assert_eq!(handle_connect_with(&connect, &mut ns), libc::EACCES);
        connect.egid = 1001;
        // Bind mode 0755 has no group write; ownership does not override it.
        assert_eq!(handle_connect_with(&connect, &mut ns), libc::EACCES);
        vfsd.0.lock().unwrap().owners.get_mut("/public/socket").unwrap().2 = 0o720;
        assert!(matches!(ns.stat_socket_node(123, "/public/socket", 2000, 1001, true), Ok(NodeState::Socket(_))));
        assert_eq!(ns.stat_socket_node(123, "/public/socket", 2000, 2000, true), Err(libc::EACCES));
        assert!(matches!(ns.stat_socket_node(123, "/public/socket", 0, 2000, true), Ok(NodeState::Socket(_))));
        let requests = &vfsd.0.lock().unwrap().requests;
        assert!(requests.iter().any(|(op, _, uid, gid, path)| *op == VFS_IPC_OP_NEWFSTATAT && *uid == 2000 && *gid == 2000 && path == "/private/missing"));
        drop_sockets(&[server, client]);
    }

    #[test]
    fn rollback_unlinks_only_the_node_this_bind_created() {
        let vfsd = Shared::with_dirs(&["/"]);
        let mut ns = namespace(&vfsd);
        let id = ns.create_socket_node(123, "/rb", 5, 6, 0o700).unwrap().unwrap();
        assert_eq!(ns.remove_socket_node(123, "/rb", 0, 0, id), Ok(()));
        assert!(!vfsd.has_socket("/rb"));
        // A non-socket node is never removed by netd's rollback.
        vfsd.make_regular("/data");
        assert_eq!(ns.remove_socket_node(123, "/data", 0, 0, id), Err(libc::EPERM));
        assert_eq!(ns.stat_socket_node(123, "/data", 0, 0, false), Ok(NodeState::NotSocket));
        assert_eq!(ns.stat_socket_node(123, "/gone", 0, 0, false), Ok(NodeState::Absent));
        let ops: Vec<u16> = vfsd.0.lock().unwrap().requests.iter().map(|r| r.0).collect();
        assert_eq!(
            ops,
            [VFS_IPC_OP_MKNOD, VFS_IPC_OP_UNLINKAT, VFS_IPC_OP_UNLINKAT, VFS_IPC_OP_NEWFSTATAT, VFS_IPC_OP_NEWFSTATAT]
        );
    }

    #[test]
    fn relative_and_wayland_names_use_vfsd_but_abstract_does_not() {
        let (a, b, c) = (0x9b0_0031, 0x9b0_0032, 0x9b0_0033);
        fresh(&[a, b, c]);
        let vfsd = Shared::with_dirs(&["/", "/cwd/rel", "/run/user/1000"]);
        let mut ns = namespace(&vfsd);
        assert_eq!(handle_bind_with(&sockaddr(a, b"rel/path.sock\0"), &mut ns), 0);
        assert_eq!(handle_bind_with(&sockaddr(b, b"\0abstract"), &mut ns), 0);
        assert_eq!(handle_bind_with(&sockaddr(c, b"/run/user/1000/wayland-7\0"), &mut ns), 0);
        assert!(vfsd.has_socket("/cwd/rel/path.sock"));
        assert!(vfsd.has_socket("/run/user/1000/wayland-7"));
        assert_eq!(vfsd.0.lock().unwrap().requests.len(), 4);
        drop_sockets(&[a, b, c]);
    }
}

#[test]
fn socket_inode_binding_survives_aliases_and_rejects_replacement() {
    let token = 0x9a0_9001;
    fresh(&[token]);
    let mut ns = FakeNamespace::default();
    assert_eq!(handle_bind_with(&sockaddr(token, b"/tmp/inode\0"), &mut ns), 0);
    let id = match ns.stat_socket_node(123, "/tmp/inode", 0, 0, false).unwrap() { NodeState::Socket(id) => id, _ => panic!("missing inode") };
    ns.nodes.insert("/tmp/./inode".to_string());
    ns.ids.insert("/tmp/./inode".to_string(), id.node);
    let alias = match ns.stat_socket_node(123, "/tmp/./inode", 0, 0, false).unwrap() { NodeState::Socket(id) => id, _ => panic!("missing alias") };
    assert_eq!(lock_net_state().inode_bindings.get(&(alias.mount, alias.node)), Some(&token));
    assert!(ns.unlink("/tmp/inode"));
    let replacement = ns.create_socket_node(123, "/tmp/inode", 0, 0, 0).unwrap().unwrap();
    assert_ne!(replacement, id);
    assert_eq!(lock_net_state().inode_bindings.get(&(replacement.mount, replacement.node)), None);
    assert_eq!(ns.remove_socket_node(123, "/tmp/inode", 0, 0, id), Err(libc::ESTALE));
    assert_eq!(ns.stat_socket_node(123, "/tmp/inode", 0, 0, false), Ok(NodeState::Socket(replacement)));
    drop_sockets(&[token]);
}

#[test]
fn socket_node_mode_uses_the_callers_queried_umask() {
    let token = 0x9a0_9002;
    fresh(&[token]);
    let mut ns = FakeNamespace::default();
    let mut request = sockaddr(token, b"/tmp/mode\0");
    request.arg3 = 0o077;
    assert_eq!(handle_bind_with(&request, &mut ns), 0);
    assert_eq!(ns.created[0].3, 0o700);
    drop_sockets(&[token]);
}
