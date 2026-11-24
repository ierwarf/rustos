# AF_UNIX sockets (kernel-local data path, Ring3-owned name policy)

Modules: `kernel/ps/src/user/unix_socket.rs` (objects, namespace table, buffers),
`kernel/compat/src/user/syscall/linux/service_ops/local_unix.rs` (syscalls, user copy,
blocking). netd owns no AF_UNIX socket: it serves only INET and AF_NETLINK sockets and
rejects `socket(AF_UNIX)` with `EAFNOSUPPORT` and `socketpair` with `EOPNOTSUPP`/`EAFNOSUPPORT`.

## Owner decision

- A connected pair (and anonymous `socketpair`) is a kernel-local object like pipe/eventfd
  (`docs/ai/abi/local-fd.md`): send/recv/poll never pay an IPC. Chrome's Mojo traffic is
  `socketpair` + `sendmsg`; a Ring3 data path would cost >= 2 extra IPCs per message (>= 3.5 us at the
  measured ~7k-cycle IPC) and serialize every process through one service.
- Ring0 holds only the mechanism: the `(mount_id, node_id)` / abstract-name -> listener table
  (`UNIX_NAMESPACE`), buffers, backlog and queued SCM_RIGHTS bounds.
- Policy stays in Ring3 services and is asked on every use, not cached in Ring0:
  - **Path identity and creation:** `bind` creates the socket node through vfsd `MKNOD`
    (`EEXIST` -> `EADDRINUSE`); the node is unlinked again if the namespace insert fails.
    Mode is `0777 & ~umask`; the umask is read from syscalld (`SYSCALL_OFFLOAD_UMASK_QUERY`).
  - **Connect permission:** `connect`, datagram `sendto` and `sendmsg` with a destination
    resolve the path (`NEWFSTATAT`, must be `S_IFSOCK`, else `ECONNREFUSED`) and then ask vfsd
    `ACCESS` with `W_OK` and `AT_EACCESS`; a refusal is returned as vfsd's errno (`EACCES`).
- Abstract names have no filesystem object and therefore no permission check; they are
  visible to every process on the system (Linux scopes them to a network namespace; RustOS has one).

## Open

- The Ring0 table does not scope abstract names per sandbox/session.
- The ACCESS check and the node lookup are two vfsd calls, so a rename between them can pair
  the permission of one node with the identity of another. The namespace key is the stat result, so a
  connection never reaches a different listener than the one stat'ed.
- The connect check is verified only for root (euid 0) in a Chrome KVM boot (Wayland connect still succeeds); a non-root denial case has no test yet.
