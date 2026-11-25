# VFS and storage contracts

[Contract index](../contracts-abi.md).

## VFS Surface (`vfsd`)

- Protocol: `VfsIpcRequest`/`VfsIpcResponse` (separate from `LinuxSyscallOffloadRequest`) for service-owned handles + chunked I/O.
- Vfsd remote and epoll object IDs are kernel-minted, boot-entropy-backed
  capabilities, not counters or caller-selected fd numbers. The exact token is
  preserved by dup/fork/IPC transfer, while rootd admits service lookup only on
  the declared caller-to-provider dependency edge. Vfsd receives the
  kernel-stamped sender identity and rejects a conflicting payload PID/TID.
  These checks are cumulative: sender identity never substitutes for object
  capability possession, and token obscurity never substitutes for the rootd
  dependency graph.
- Kernel fd tables mirror service-owned objects as `KernelHandle::RemoteVfs`.
- `/proc` is a vfsd-owned namespace (`proc_fs.rs`). `self` links to the
  kernel-stamped caller PID; for another PID only `oom_score_adj` and
  `task[/<tid>[/{stat,status,comm}]]` are exposed (all other foreign-PID nodes
  return `ENOENT`).
  Vfsd admits maps/smaps/smaps_rollup, status/stat/statm, cmdline/environ, auxv, limits,
  fd and task directories, then returns a typed delegation code. Ring0
  supplies only the caller's actual VMAs, credentials, threads, argv/env,
  auxiliary vector or fd snapshot; vfsd does not guess or retain them.
  Generated regular files have `S_IFREG`, stat size 0 and read-to-EOF
  semantics; snapshots over the 256-KiB bound fail, not truncate silently.
  Fd links use actual object inode/name facts (`socket:[ino]`, `pipe:[ino]`,
  memfd or remote pathname); proc directory handles support relative
  open/readlink/stat. `O_PATH` snapshots grant neither read nor getdents.
  Memfd magic-link reopen creates an independent cursor on the same sealed
  object and cannot escalate a readonly source. Remote reopen uses
  `VFS_FCNTL_REOPEN` (FCNTL arg0 `0x5255_0001`, source `remote_id`, requested
  flags arg1, fresh kernel-minted description arg2, arg3 zero; OPENAT reply
  shape). TmpFs pins the inode including an unlinked node; a backend without
  genuine object reopen support fails explicitly, never looks up a saved
  pathname and accidentally opens a replacement. Global `/proc/mounts`
  reflects the current MountTable rather than a fixed boot-time string.
- VFS device open responses use `VfsIpcResponse.aux` for service-owned route
  metadata such as `INPUTD_ACCESS_*` and `VFS_DEVICE_ACCESS_DRM_COMPAT`.
  Compat never classifies `/dev/*` path strings: vfsd resolves the namespace,
  then compat either transfers the devmgrd handle or retains the explicitly
  selected DRM compatibility description.
- Linux `openat` installs `KernelHandle::RemoteVfs` for regular files + directories after vfsd registration.
- Linux `dup`/`dup2`/`dup3` acquire the exact open-description reference before
  publishing the local duplicate. Socket and input descriptions cross their
  service boundary; remote VFS and epoll descriptor references are counted
  locally by the kernel fd substrate. Epoll sends only create and final-retire
  mutations to vfsd, so a routine `fcntl(F_DUPFD*)` cannot inherit vfsd
  scheduling latency. `close` removes the local descriptor first, then performs
  any token-addressed final provider release; a wedged vfsd/netd cannot retain
  a reusable numeric fd or block process retirement. Netd socket DUP, final
  CLOSE, and their exact replay ACK mutate the provider's open-description
  reference and therefore own one complete bounded 5 s state-mutation lease
  attempt (a short timeout leaves the provider outcome unknown); only a
  foreground final CLOSE keeps the 100 ms rail, because its fd is already
  retired and nobody consumes the result, and housekeeping replays it under
  the lease. They do not split the deadline or retry a real timeout; exact
  mutation identity transfers to housekeeping. Checkpointed epoll
  create/ADD/MOD/DEL/retire and matching purges follow the same control rail.
  `VFS_IPC_OP_CURSOR_SETTLE` (`COMMIT`/`CANCEL`/`SYNC`) and checkpoint visibility
  ACK mutate provider custody and use one full state-mutation lease attempt, not
  the 16 ms readiness rail or the 100 ms interactive rail. Explicit final
  remote-VFS `close` waits for the 5 s state-mutation lease and reports a
  provider error after retiring the numeric fd; a successful close must not
  leave a path-backed inode pinned against an immediate rename. Implicit cleanup
  keeps the short rail. If a prepared directory cursor rejects `close` with
  `EBUSY`, retain the exact close in deferred reconciliation (regular files
  never enter `prepared` state).
  Deferred replay preserves all four operation arguments,
  flags, dirfd, mutation identity, and bounded payload; in particular an epoll
  checkpoint ACK retains its original suboperation in arg3. Unconfirmed work
  retains its queue owner. One explicit in-flight owner plus per-description
  lane heads preserve order: after a failed turn only that head rotates behind
  other ready lanes; its followers cannot run until it confirms. Admission
  sees the in-flight owner too. The bounded allocation-free ring's selective
  removal preserves every other entry's relative order, including wraparound.
  A confirmed mutation awaiting ACK replaces itself by that ACK in its own
  reserved lane slot rather than replaying an older mutation or competing for
  fresh queue capacity.
  Regular-file (`RemoteVfsHandleKind::File`) live sequential cursors are owned
  by the kernel in `RemoteVfsOpenDescription` (shared via `Arc` across
  `dup`/`fork`/`SCM_RIGHTS`), so sequential `read()` translates to stateless
  positioned `VFS_IPC_OP_PREAD64` / `VFS_IPC_OP_BULK_READ` requests at
  `desc.cursor()`, advances `desc.cursor` only after `copy_to_user` succeeds,
  and emits zero `rootd` checkpoints per read. Regular-file cursors settle
  durably in `vfsd`/`rootd` at explicit settle points: `lseek` passes
  `arg2 = desc.cursor()` with `VFS_LSEEK_FLAG_SYNC_CURSOR`; `write` passes
  `arg0 = desc.cursor()` with `VFS_WRITE_FLAG_CURSOR_SYNC` and updates the
  kernel cursor from `response.aux`; and `fsync`/`fdatasync` flushes a dirty
  cursor via `VFS_IPC_OP_CURSOR_SETTLE` (`VFS_CURSOR_SETTLE_SYNC = 3`) before
  `VFS_IPC_OP_FSYNC`. Directory `getdents64` retains `vfsd`-owned opaque
  cookies and the `COMMIT`/`CANCEL` prepare/settle protocol.
  Cursor-using foreground syscalls (`read`, `write`, `lseek`, `fsync`,
  `sendfile`, `copy_file_range`, `getdents64`) first take a per-description
  cursor gate (64 slots, 5 s wait) held across stateless file read chunks or
  from the prepared directory request through user copy and `COMMIT`/`CANCEL`.
  The gate is granted only when no other foreground owner and no queued or
  in-flight deferred mutation exists for that description; while waiting, the
  caller replays that description's lane itself (one replay owner at a time)
  instead of waiting for housekeeping. `vfsd` therefore never sees a second
  cursor operation on a prepared directory description, so a timed-out
  `COMMIT` or two threads sharing a description no longer surface `EBUSY`. A
  gate that cannot be granted in 5 s returns `ETIMEDOUT`, never `EBUSY`. Residual
  gap: queue saturation (`ENOSPC` on admission) and a prepare that reaches a
  multi-receiver `vfsd` after its `CANCEL` are not fenced; `vfsd` is
  single-receiver.
  Only non-consuming provider readiness queries use the 16 ms deadline. vfsd is
  the only intended caller of `SYS_RUSTOS_FD_*_BROKER`; gated by `VFS_POLICY`.
  Generic apps must not call those brokers directly.
  Fork clones the address space and fd table in one process-state snapshot;
  provider refs are acquired from that frozen child table before publication.
  Resnapshotting the live parent afterward is forbidden because a sibling
  close/reuse could bind the child fd to one object and the provider ref to
  another.
  Exec cleanup is likewise derived only from the exact `KernelHandle` values
  removed by the atomic process-table CLOEXEC commit; a pre-commit CLOEXEC
  snapshot is not cleanup authority. Exit seals process visibility before it
  drains the exact removed handles; VFS requests use the scheduler's current
  task sender identity even after that seal, matching IPC receive attribution
  for final CLOSE and its checkpoint ACK. Session and credential facts still
  require a visible process snapshot.
- A devmgrd ioctl route is policy advice, not fd identity. Compat must resolve
  the live fd-table entry again before dispatch: sessiond TTY operations require
  an actual `KernelHandle::Console`, so a closed/reused ordinary file fd
  receives `ENOTTY` and cannot read or mutate the caller's console policy.
- `LinuxSyscallOffloadRequest.arg0..arg3` carry 64-bit fd-control args (target fd, cmd, arg, flags). **Do not pack pointer/flag values into the 32-bit `mask` field.**
- `mount`/`umount2` route to vfsd and currently return `EOPNOTSUPP` when
  no actual dynamic mount operation is available. Do not return false success
  by changing only cache epochs, or reintroduce generic-app ring0 mount policy.
- `poll`/`ppoll`/`epoll_*`: persistent `epoll` interest sets live in Ring0
  (`EpollHandle`, billed to the client Job; `vfsd` rejects legacy
  `VFS_POLL_QUERY_EPOLL_*` requests with `EOPNOTSUPP`). Remote-VFS readiness
  for `poll`/`ppoll`/`epoll_wait` still routes through `VFS_IPC_OP_POLL_QUERY`
  (`VFS_POLL_QUERY_POLL`); socket readiness is local (`UnixSocketHandle`) or
  `netd` policy (`AF_INET`/`AF_INET6`/`AF_NETLINK`), input readiness is `inputd`
  policy plus the narrow native-input wait/completion substrate, and non-system
  console-input readiness is `sessiond` policy. sessiond advances its generation only on an empty-to-ready
  line-discipline transition; the readiness query is non-consuming and bounded
  by the application's remaining deadline. Live console output/error are
  immediately writable, but they observe the same session generation so close
  becomes `POLLHUP` even when the caller requested only hangup events.
  Closing a session advances that generation and a subsequent query returns non-live, which compat exposes as `POLLHUP` without
  recreating the removed session. Stdin/stdout/stderr are real fd-table entries,
  not numeric exceptions. Each console open description carries an unforgeable
  monotonic token shared by dup/fork and retired only after final close,
  CLOEXEC, or process exit. Persistent epoll binds every non-system console
  stream to that token plus the
  exact sessiond endpoint epoch; final retirement purges the matching vfsd
  interest, and a stale token fails closed even if purge delivery failed.
  The shared `WaitSetInterestWire` admission function is the single envelope
  validator on both sides of the boundary: ABI v1, a known provider, zero
  flags/reserved word, a target fd representable by the bounded fd table, and
  nonzero object/endpoint-epoch authority are mandatory before either ring0 or
  vfsd resolves an interest. Proof-indexed Kani harnesses exhaust the symbolic
  envelope partition and require both accepted and rejected witnesses; they do
  not prove provider lookup, fd lifetime, or concurrent wake behavior.
  Vfsd admits at most 256 live epoll objects and 512 interests per object, for
  a machine-readable global ceiling of 131,072 persistent interests. Live
  create and checkpoint restore fail closed at the object boundary; batch
  restore is built in a candidate registry and becomes visible only after the
  complete batch validates, so a duplicate or cap overflow preserves the exact
  prior registry. A bounded reverse index maps each provider-object identity to
  its exact `(epoll, interest-key)` memberships. Final-object purge removes
  only that set and is proportional to affected registrations rather than a
  clone or scan of every live epoll object.
  EPOLL_CTL_ADD/MOD pins the exact provider or console open description across
  the vfsd mutation. Releasing that transaction guard performs normal
  last-close purge, so a concurrent final close cannot purge first and then
  leave a newly inserted interest for a retired object.
  Linux defines `epoll_ctl` as the synchronous interest-list control interface,
  distinct from the timeout-bearing `epoll_wait`; the compatibility boundary
  therefore keeps mutation completion separate from the 16 ms provider-query
  cap (<https://man7.org/linux/man-pages/man2/epoll_ctl.2.html>). Its ADD record
  pins the target open file description, matching Linux's documented reference
  semantics; acquiring that netd reference is part of the same interactive
  mutation, not a readiness probe (<https://docs.kernel.org/filesystems/files.html>).
  `O_NONBLOCK` is read from the same fd-table snapshot as the console
  handle; an empty sessiond read returns `EAGAIN` instead of re-entering
  the blocking retry loop after a readiness race.
  System-console bootstrap input remains a deliberate non-persistent exception
  because it has no sessiond-owned readiness generation. Ring0 keeps fd-table validation, epoll token
  handles, user-copy, a bounded provider-wait registry, and deadline wakeup.
  Both persistent epoll sets and syscall-scoped multi-fd poll sets use the same
  generation-based path: check, register the exact observed generations,
  recheck through ordinary service IPC while the task remains runnable, arm
  the scheduler, then verify that every registered waiter still exists before
  commit. A provider signal removes its waiter before wake, so an event in the
  recheck-to-arm window is detected without nesting an IPC block inside an
  already armed scheduler block. The bounded waiter registry derives its
  capacity from the scheduler task ceiling times the provider ceiling; every
  schedulable task can therefore arm one observation for every provider
  without an artificial mid-capacity `ENOSPC` failure. Every readiness IPC is
  capped at the shorter
  of the remaining syscall deadline and 16 ms; an infinite application wait
  therefore cannot turn a wedged provider into an unbounded kernel-service
  call. `ppoll`/`epoll_pwait` atomically
  apply their temporary signal mask, reject a non-native sigset size, and
  return `EINTR` for a pending unmasked signal; SIGKILL/SIGSTOP can never be
  added to the temporary mask.
  A scan asks each provider once, never once per descriptor. netd answers every
  socket of a `poll` array or epoll snapshot in one `NETD_IPC_OP_POLL_BATCH`
  exchange (up to `NETD_POLL_BATCH_MAX` `NetdPollQueryWire` entries in,
  `NetdPollResultWire` entries out in order). A socket netd cannot evaluate
  fails in its own entry (`status`), never the batch. The `generation` in each
  entry is that socket's own readiness generation, recorded only for live
  sockets and dropped at final close, so it advances only when that socket's
  readiness may have changed. vfsd-backed `poll` descriptors use
  `VFS_POLL_QUERY_POLL` with up to `VFS_POLL_BATCH_MAX` `pollfd` entries (the
  fixed request payload bounds both batches; a larger scan issues several).
  `VFS_POLL_QUERY_EPOLL_SNAPSHOT` returns `WaitSetSnapshotWire` entries: the
  persistent `WaitSetInterestWire` plus volatile report state
  (`edge_generation`, `WAITSET_SNAPSHOT_STATE_DISABLED`) that is never
  checkpointed, so a restored interest reports once more.
  `EPOLLET` and `EPOLLONESHOT` follow Linux's `epoll(7)`: an edge-triggered
  interest reports again only for a provider generation newer than the one it
  last reported, and a one-shot interest goes silent after one report until
  `EPOLL_CTL_MOD` re-arms it (MOD also clears edge history, so current
  readiness is reported again). `epoll_wait` turns each such report into
  ownership with `VFS_POLL_QUERY_EPOLL_CLAIM` (up to `VFS_EPOLL_CLAIM_MAX`
  `WaitSetClaimWire` per request, one byte per entry in the reply). vfsd
  decides atomically, so two threads waiting on one epoll object cannot both
  receive the same edge or one-shot event, and a candidate that lost the race
  is replaced by the next ready one instead of producing an empty result.
  Level-triggered interests are never claimed. Nested readiness (`poll` or
  `epoll` on an epoll fd) only peeks and never consumes. A claim is not
  replayed and rides the 100 ms interactive rail; a reply lost after vfsd
  consumed the report can drop one edge, an accepted limit because a retry
  could not tell "consumed by me" from "consumed by another waiter".
  `EPOLLRDHUP` is evaluated by netd (peer FIN or `SHUT_WR`; AF_INET also
  reports `POLLIN` at EOF, as reads return 0). `EPOLLEXCLUSIVE` is ADD-only,
  accepted only with the Linux `EPOLLEXCLUSIVE_OK_BITS`, and makes the
  interest immutable (MOD returns `EINVAL`); delivery stays level/edge as
  registered, which Linux permits because the flag only allows waking one or
  more waiters. `EPOLLWAKEUP` is dropped (no `CAP_BLOCK_SUSPEND`); the
  `EPOLLRDNORM`/`EPOLLWRNORM` aliases are reported only when registered.
  References: <https://man7.org/linux/man-pages/man7/epoll.7.html>,
  <https://man7.org/linux/man-pages/man2/epoll_ctl.2.html>.
- User-task retirement is a two-phase runtime boundary. The scheduler first
  revokes runnable, deadline, and IPC authority, stamps the exact task/process
  identity plus whether this was the process's last live thread, and keeps the
  slot unreapable. Kernel housekeeping is split into two independent tasks to
  prevent circular wait between thread materialization/reaping and Ring3
  service IPC: `nucleus_housekeeping_task` performs only pure non-blocking
  kernel work (`service_deferred_work`, pager/shared-region reclaim, profiles,
  heartbeat), while `nucleus_service_cleanup_task` owns Ring3 provider cleanup
  (`service_retired_task_runtime_cleanup` and `service_deferred_transfer_releases`)
  and wakes `nucleus_housekeeping_task` once cleanup completes. Similarly,
  `vfsd`'s `bulk_read_worker` reuses a lock-free cached worker pool with a 2 s
  idle retirement (0 resident threads at rest) instead of spawning a fresh
  thread via `SYS_CLONE` per bulk-read request, and published Ring3 service
  owners never run synchronous provider cleanup inside `FUTEX_WAKE`. The
  cleanup pass removes every task-scoped
  futex, wait-set, service-discovery, input, block, bootstrap-TTY, and proc
  broker record; terminal-process cleanup also closes provider references and
  revokes service publications. The directory publishes the immutable cleanup
  token only after all terminal metadata is complete. An independently locked
  Identity/lifecycle publication has an independent per-slot class-51 writer
  (IRQ-excluded leaf); metadata writers cannot supply lifecycle flags. Pending
  suspend/stop intent survives missing partial-initialization metadata; exact
  live commands reject a reused task ID. Four Scheduler gate arrays are
  test-only, not production authority. Cross-owner admission/exec/frame lifetime
  still belongs to the remaining directory/resource boundary. The
  per-slot `SchedulerRetirementCleanup` owner holds deferred producer custody
  and both cleanup/kernel Pending→Claimed→Complete transactions. Exec freezes
  process-detach metadata before either token becomes visible. Kernel claims
  are private, non-Copy and generation-bound; their final ACK follows all
  revocation/wake/detach work. Claim/ACK APIs do not acquire Scheduler and
  release the cell before external
  cleanup, user copy/faults or other subsystem locks. Only an exact claimed
  task and full-payload cleanup ACK plus kernel ACK permits reset/reuse;
  Pending/Claimed and deferred/in-progress producers remain unreapable. Reset
  precedes runtime/slot identity teardown; reaper notification is atomic
  consume-before-scan, not a lost-wakeup final false store. Producer and
  execution/process/TLB/stack quiescence still require their existing owners.
  A wait-lock unlock skips retired queue heads until it wakes one
  live task, so a stale identity cannot consume the only wakeup.
  A hardware-IRQ context validator may only quarantine the fixed task slot and
  record one allocation-free deferred reason. It must not scan the live
  current kernel frame as if it were a published runnable frame, nor enter
  IPC/process teardown from the IRQ. Housekeeping finalizes that quarantine
  through the same retirement boundary before reaping.
  Service-publication wait admission is task-scoped and its capacity derives
  from the scheduler task ceiling, not the smaller process-table ceiling. Its
  registry is a fixed allocation-free table: one task rearm replaces its exact
  slot, matching task IDs are extracted under the lock, and wake/hint calls run
  only after unlock. The native input wait substrate uses the same ceiling; the separate inputd
  ingestion waiter never competes with application slots.
- A vfsd epoll membership key is `target_fd + provider + open-description
  object_id`. The provider endpoint epoch is record state, never part of that
  identity: after restart, duplicate `ADD` remains `EEXIST`, `MOD`
  replaces the stale epoch, and `DEL` removes the exact registration.
  A stale epoch therefore fails closed but cannot create an undeletable or
  duplicate interest.
- Legacy `SYS_RUSTOS_{STATX,STAT,READLINK,ACCESS,GETCWD,CHDIR}_METADATA`: no generic-app VFS policy in ring0 after vfsd registers. Pre-vfsd bootstrap + registered policy-service callers retain direct kernel metadata access.
- `SYS_RUSTOS_BLOCK_BROKER` ABI v3 is DVM-only and gated by
  `STORAGE_POLICY`. `BOOT_INFO`, `BOOT_READ`, physical descriptors, and the
  `VFS_POLICY` lane do not exist. Operation-incompatible fields, the explicit
  timeout, and reserved words must be zero. Tickets bind generation, request
  ID, and transfer slot; a failed ticket copyout cancels the submitted request
  rather than publishing ownerless work.
- `SYS_RUSTOS_EARLY_SYSTEM_BROKER` ABI v1 is a separate vfsd-only immutable
  bootstrap-file lane. It accepts one inline exact path from the signed
  early-system table and provides only file length or at most 4 KiB of
  read-only bytes. It has no enumeration, directory, physical LBA, controller,
  mutation, or fallback operation. Vfsd owns path normalization, open
  descriptions, durable checkpointing, directory cursors, and the overlay
  decision (while kernel `RemoteVfsOpenDescription` owns live sequential
  regular-file cursors); digest and exact-entry admission remain in the bounded
  ring0 reader.

### Read-only large-file adapter

`storage-core::file_io` separates 64-bit positional ReadAt/WriteAt from optional
owned Data/Hole/Unwritten read maps. Extents are backend accelerators, never a
kernel-visible FAT layout or a requirement for compressed/inline future files.
FAT builds/coalesces its map once from the existing engine and checks geometry,
overlap/cycles, full coverage, metadata admission and mutation epoch. Identity
is the volume instance + short-directory-entry slot + a per-file registration
generation: repeated/case-alias opens agree; reused slots cannot revive a stale
read lease. A map records the byte spans it depends on (its data extents, the FAT
entries of its chain in every FAT copy, and its directory entry) in a registry
of at most 128 entries / 16 MiB; only a write that overlaps those spans revokes
it, so unrelated writes never abort a snapshot, and an evicted registration just
forces a replan. This is not a persistent inode/checkpoint identity across mutation.

Ordinary read maps are bounded to 128 entries / 8 MiB; per-snapshot planning is
bounded to 2 MiB. Detached readers hold only the checked map/device, not a FAT
namespace borrow or a second filesystem/cache. Runtime-supplied sleeping device
waits and per-device response storage preserve progress across blocking IPC and
old/new mounts. A write that overlaps a map's spans revokes it before media I/O.

FAT path identity: the volume matches names ASCII-case-insensitively (and the
engine resolves a long name and its 8.3 alias to one entry), so `/BIN/Ls` and
`/bin/ls` are one file. Vfsd's `path_inode`/checkpoint `content_identity` hash
the ASCII-lowercased path, and every path-addressed cache (file bytes, read
maps, sealed executable snapshots, metadata, directory listings) is keyed by
`mount_table::cache_key_for_mount` (the mount's `CasePolicy` decides whether the
path is lowercased) so case spellings share one entry. `path_inode` lives in
`vfsd::path` so its tests run. Residual: an 8.3 *alias spelling*
(`PROGRA~1`) hashes and caches as its own path, not as its long name; a handle
opened through it gets a different inode and cache entry than the long name. A
true alias identity needs the directory-entry slot, which is the persistent-
inode design deferred in the large-file plan; `content_identity` must stay a
pure function of the path text until then so checkpoint replay stays valid.

Executable publication uses the owner-bound asynchronous job and sealed memfd
contract in [loader scheduling](loader-scheduling.md). Shared buffered/mmap
coherence for FAT and ext4/NTFS adapters are later phases, not claims made by
the read-only accelerator; TmpFs has it (next section).

### Chrome-compatibility surfaces (TmpFs, procfs, locks, sockets)

- **TmpFs mounts.** vfsd mounts `TmpFs` (`FsVolume` with `posix_nodes()`) at
  `/tmp` (64 MiB), `/dev/shm` (64 MiB) and `/run` (16 MiB) before the boot disk;
  `/run/user/<desktop uid>` (`RuntimeDir(DESKTOP_SESSION_UID)`, 1000 today) is pre-created 0700 for that uid, `/tmp` and `/dev/shm` are 1777; runtimed ensures other launch uids' `/run/user/<uid>` and the profile directories (see `prebuilt-runtime.md` launch environment).
  `run_dirs.rs` and `mkdir_policy` are gone. Nodes are dir/file/symlink/socket
  with per-mount unique node ids, mode/uid/gid/times/nlink, 4 KiB sparse chunks,
  a byte quota (ENOSPC) and an inode limit. An open description pins its node
  (`retain_node`/`release_node`), so data survives unlink or rename-over until
  the last close. TmpFs paths bypass every path-keyed cache and are never
  admitted as executable snapshots. TmpFs `mkdir` requires parent search
  permission and reports `EEXIST` for an existing target before checking
  parent write permission. An absent target still requires write permission.
  Symlinks are followed component by component in `resolve_path_follow`
  (`state_storage.rs`, `SYMLINK_FOLLOW_MAX` = 40 hops, then ELOOP); `O_NOFOLLOW` gives ELOOP
  and a socket inode gives ENXIO on open. `/proc/mounts` is generated from the
  live `MountTable` (vfat rows `ro`/`rw`, tmpfs rows `rw,nosuid,nodev`).
- **New VFS ops** (`VFS_IPC_ABI_VERSION` stays 5): `MKNOD`=34 (dirfd+path,
  `arg0`=st_mode; regular, socket and FIFO kinds are admitted where the volume supports them; device nodes are refused; used by Linux `mknod`/`mknodat` and netd bind), `CHMOD`=35,
  `FCHMOD`=36, `FALLOCATE`=37 (mode 0 and KEEP_SIZE), `FILE_LOCK`=38,
  `SHARED_BACKING`=39. vfsd applies no umask: the kernel passes umask-adjusted
  modes. Linux timestamp requests carry nanoseconds and NOW/OMIT; the actual precision also depends on the backing filesystem.
- **Advisory locks (`FILE_LOCK`).** `VfsFileLockWire` (48 bytes LE:
  `version u16 | cmd u16 | owner u16 | lock_type u16 | whence u16 | flags u16 |
  reserved u32 | start i64 | len i64 | pid i64 | reserved u64`); cmd 1 GETLK,
  2 SETLK, 3 SETLKW, 4 CANCEL_WAIT, 5 RELEASE_FILE; owner 1 POSIX (kernel-stamped
  pid), 2 OFD, 3 flock (the description's `remote_id`). The table in
  `services/vfsd/src/locks.rs` is keyed by `(mount_id, node_id)`; same owner never
  conflicts, POSIX/OFD conflict with each other, flock is a separate namespace;
  512 locks per file and 8192 total (ENOLCK). vfsd never waits: SETLKW is a
  non-blocking attempt that records the waiter (wait-for cycle gives EDEADLK for
  POSIX owners) and the kernel polls with 1-32 ms sleeps, EINTR on a pending
  signal. A conflict is always EAGAIN. Release: final description close, any
  close of the file by a POSIX owner (kernel `note_fd_closed` -> RELEASE_FILE,
  only for pids that ever took a POSIX lock), `exec`-time `CLOEXEC` closes
  (`note_exec_cloexec_closed` via `vfsd::lock_release_proxy_shape`, admitted
  only from the live `loaderd` owner on its own thread for POSIX
  `RELEASE_FILE`), and process EXIT/FORK lifecycle events. The kernel queues the
  lifecycle EXIT **before** publishing the zombie to `wait4`, so a parent that
  returned from `wait4` never sees the dead child's locks. Against missed exit
  events or lifecycle queue overflow, `execute_verified` / `verify_all_owners`
  probes kernel process incarnations (`LIFECYCLE_BROKER_MODE_PROBE_PIDS`,
  `VfsPolicy`-only, cached for `LIVENESS_RECHECK_NS`) on conflict/overflow and
  drops dead or pid-reused owners. Checkpoint decision: the lock table is
  intentionally not checkpointed (locks die with a `vfsd` restart just as the
  kernel would drop them with the open descriptions, and clients re-take them
  after reopening).
- **procfs.** `/proc` serves the caller's own pid (`/proc/self`,
  `/proc/<own pid>`); for another pid only `oom_score_adj` (kernel-completed; write
  needs root or equal euid) and the `task` directory listing (Ring0 delegation with
  the target pid as number; ENOENT if the pid is not live) and
  `task/<tid>[/{stat,status,comm}]` exist. The per-tid node `number` is
  `pid << 28 | tid` (`pack_task_number`; pid 0 = the caller): Ring0 answers
  stat/access/`task/<tid>` listing with ENOENT unless the tid is a live thread of that
  pid, and vfsd formats the files read-only from `PROC_FACTS_OP_{PROCESS,THREAD}` facts
  of that pid; writing another process's `comm` is EACCES. `cmdline` is not a per-tid
  file (needs a cross-process memory read). `/proc/cpuinfo`
  and `/sys/devices/system/cpu/{online,possible,present}` report the kernel's
  admitted hardware CPUs (`PROC_FACTS_OP_ONLINE_CPUS`), not vfsd's affinity mask.
  Vfsd-generated virtual files (no kernel facts): `/proc/<pid>/mountinfo` (the
  mount table in mountinfo syntax), `/proc/<pid>/cgroup` (membership of the single
  cgroup namespace, no hierarchy mounted), and constant
  `/proc/sys/kernel/{ostype,osrelease,hostname}` mirroring the `uname(2)` fields.
  `/proc/<pid>/smaps_rollup` (`ProcKernelNode::SmapsRollup`, wire code 12) is
  formatted by vfsd (and by Ring0 for the caller's own pid, like `smaps`) from
  the same facts as `smaps`: the `[rollup]` header plus only counters Ring0 can
  attest process-wide (`Rss` when known, `Locked`); `Pss`, `Shared_*`, `Private_*`,
  `Anonymous`, `Referenced` and `Swap` are omitted, never reported as zero.
  Kernel-fact nodes (maps, smaps, smaps_rollup, status, stat, statm,
  cmdline, environ, auxv, limits, comm, oom_score_adj, `task/`, `fd/`, `ns/*`) are
  exposed through vfsd. For kernel-fact files handled by the Ring3 snapshot
  path, vfsd pulls attested binary facts through `SYS_RUSTOS_PROC_FACTS_BROKER`,
  formats them and captures the bytes when opening the file. Some special
  nodes still use the `PROC_DELEGATE` compat path. Generated read-only files
  are `S_IFREG` 0444 size 0, directories 0555. `fd/N` are magic
  links (`socket:[ino]`, `pipe:[ino]`, `anon_inode:[...]`, `/memfd:name (deleted)`,
  recorded path); `open("/proc/self/fd/N")` reopens memfd (seals apply),
  path-backed files (recorded path) and proc snapshots, `ENXIO` for sockets, pipes
  and anonymous inodes. Unavailable Linux fields are omitted rather than
  fabricated.
- **Pathname `AF_UNIX` socket nodes.** `bind(AF_UNIX, path)` (executed by Ring0
  `UnixSocketHandle`) creates an `S_IFSOCK` inode through vfsd `MKNOD`; `vfsd`
  also exposes `stat` (`AT_SYMLINK_NOFOLLOW`) and socket-only `UNLINKAT`. The inode is
  visible to `stat`/`access`/`unlink` and is not freed on socket close; a second
  bind gives EADDRINUSE until `unlink`, connect to a gone node gives ENOENT.
  Abstract names remain kernel-local without a vfsd inode.

### TmpFs writable `MAP_SHARED` (shared-frame backing)

- **Owner/model:** a TmpFs inode keeps 4 KiB chunks until a writable shared
  mapping needs coherence. `VFS_IPC_OP_SHARED_BACKING` (39; additive,
  `VFS_IPC_ABI_VERSION` unchanged) makes vfsd move the inode's bytes once into
  a vfsd-owned memfd (`SharedFrames`, `services/vfsd/src/shared_frames.rs`);
  the inode is then *shared-backed* for life: chunk map empty, every
  read/write/pread/pwrite/ftruncate/fallocate goes to the memfd with
  `pread64`/`pwrite64`/`ftruncate`, and memfd length == file length. Reply:
  `value` = vfsd descriptor, `aux` = memfd object id (the `st_ino` the kernel
  reports; never recycled).
- **Mapping:** syscalld plans `MmapPlan::FileShared` for `MAP_SHARED` +
  `PROT_WRITE` of a file and calls `MM_BROKER_OP_MAP_FILE_SHARED` (20). Ring0
  (`mm_broker_ops/file_shared.rs`) checks the client's rights (read, and write
  for `PROT_WRITE`), asks vfsd on the client's behalf (same proxy shape as
  `FILE_IDENTITY`), finds the memfd in vfsd's descriptor table by
  `(descriptor, object id)` and maps it like a shared memfd
  (`acquire_mapping_with_eof_guard` + `record_shared_memfd_mapping`), so fork, munmap,
  mprotect and exit reuse the memfd paths. Frames are therefore shared by
  every mapper and by vfsd I/O; `msync` has nothing to flush.
- **Path-backed volumes:** mutable FAT files use the shared path store, not the
  TmpFs inode upgrade. The store retains a memfd keyed by mount and normalized
  path; `msync` requests writeback to the volume. Immutable and early-system
  paths do not admit this shared backing. This does not establish full Linux
  `MAP_SHARED` coherence, admission, or durability for every FAT file.
- **Lifetime:** frames die with the TmpFs node (last name and open description)
  *and* the last mapping hold. `unlink`/`rename` while mapped is therefore
  ordinary. Quota is `ceil(len/4096)` pages of the mount (dense, sparse growth
  of a shared-backed file is charged); the pages are also charged to vfsd's
  Job like any memfd.
- **Truncate while mapped:** shrinking the backing memfd replaces released
  mapped pages with EOF guard leaves before frame release; a fault on a
  guard delivers `SIGBUS`. A shrink inside the last page zeroes the tail.
  Mapping ranges beyond EOF can include those guard leaves.
- **Known gaps:** `O_APPEND` files are not refused; `/proc/<pid>/maps` names the
  mapping `memfd:vfsd-tmpfs`; `mtime` is not advanced by stores through a
  mapping; `fallocate(KEEP_SIZE)` past EOF is `EOPNOTSUPP` on a shared-backed
  file.

- **Shared path-backed volume I/O:** zero-length pwrite does not grow the
  file or backing memfd. Snapshot creation retries short positive frame writes
  and rejects premature source EOF or zero-progress copies before publication.
  Flush reports premature frame EOF as an I/O error, not a successful partial
  writeback. Rename moves shared backings for the renamed path and its
  component-boundary descendants to their new mount-relative names, retaining
  frame identity and replacing any backing at the destination. Sequential
  zero-byte writes, including O_APPEND, leave the cursor unchanged.

### Filesystem-neutral volumes, `MountTable`, and writable FAT

- `storage-core::fs` defines `FsVolume: Send + Sync` as the filesystem-neutral
  `&self` volume trait, parameterized by `FsCapabilities` (`read_only`,
  `case_policy: CasePolicy`, feature flags, `max_name_len`), `FsMetadata`,
  `FsDirEntry`, `FsStatFs`, `FsOpenOptions`, and `FsError`. Optional operations
  (`set_times`, `set_dos_attrs`, `symlink`, `readlink`) default to
  `FsError::Unsupported`.
- `VfsIpcRequest`/`VfsIpcResponse` adds `VFS_IPC_OP_PWRITE64` (28),
  `VFS_IPC_OP_FSYNC` (29), `VFS_IPC_OP_RENAMEAT2` (30, two-path payload via
  `encode_two_path_payload`), `VFS_IPC_OP_STATFS` (31, 120-byte little-endian
  `VfsStatFsWire`), `VFS_IPC_OP_SYMLINKAT` (32, two-path payload), and
  `VFS_IPC_OP_SET_ATTRS` (33).
- `VfsFileIdentityWire` (`VFS_IPC_OP_FILE_IDENTITY`) distinguishes immutable
  (`VFS_FILE_IDENTITY_FLAG_IMMUTABLE_VOLUME`, bit 0) from mutable
  (`VFS_FILE_IDENTITY_FLAG_MUTABLE_VOLUME`, bit 1) backing volumes; setting both
  flags is non-canonical. Only `IMMUTABLE_VOLUME` files are cacheable in the
  shared library cache; `MUTABLE_VOLUME` (or non-immutable) files report
  `(mount_id, node_id, mount_generation, storage_epoch, mount_generation, len)`
  and are never cached.
- `vfsd::mount_table::MountTable` resolves absolute normalized paths by longest
  mount-point prefix match and injects immediate submount names into directory
  listings. `MountTable::mount_fat_disk_into` wraps the DVM block device in
  `SharedBlockDevice` and inspects partitions via `detect_partitions`:
  0 or 1 partition mounts `/` with `device_read_only`; 2+ partitions force
  partition 0 read-only at `/` and mount partition 1 at `/data` with
  `device_read_only` (so `/data` is writable when both the mount and DVM device
  admit writes). If the second slice or FAT mount fails, `/` stays available,
  but vfsd logs the distinct `/data` failure, omits that mount from listings,
  and returns the saved error for `/data` and its descendants instead of
  treating them as boot-volume paths. A disk without a second partition is
  distinct from a second partition that failed admission.
- `vfsd` keys path caches (`metadata_cache`, `dir_entries_cache`,
  `file_bytes_cache`, `read_maps`, `executable_snapshot_cache`) by
  `CacheKey = (mount_id, case_normalized_rel_path)` (`CasePolicy` lowercases
  ASCII/Unicode-insensitive mounts and preserves case for `Exact`). Every
  filesystem mutation invalidates the target and parent `CacheKey` entries
  (and matching subtree keys on directory/rename mutations); remounts clear all
  caches via `invalidate_caches_if_remounted`.
- **Directory iteration:** vfsd merges immediate mount children with the
  backing entries for node-, path- and virtual-backed directories. Namespace
  entries shadow equivalent backing names. `getdents64` returns opaque `d_off`
  cookies: fixed namespace slots/mount IDs, monotonic TmpFs name-entry IDs
  (separate for hard links), or FAT directory-stream offsets. Removing earlier
  entries does not renumber the survivors. Rewind with `lseek(fd, 0, SEEK_SET)`;
  seek to a returned cookie to continue. This is not a directory snapshot:
  inserts and renames may reorder or reuse slots on disk.
- **Replay:** open-description checkpoint version 3 records the start/end
  cookies and a fingerprint of the admitted buffer size and encoded bytes.
  A changed page is refused and the exact transaction cancelled, not accepted
  just because its record count matches. Old nonzero directory positions are
  stale on restore; v1/v2 file-read checkpoints keep their original semantics.
- **Path limits:** pathname admission and expansion past RustOS's 512-byte
  VFS wire capacity report `ENAMETOOLONG`, as do filesystem name-length
  failures. Invalid syntax and user-copy errors remain distinct. The wire
  capacity is unchanged; Linux's larger pathname limit is not claimed.

## Storage Surface (`storaged`)

- Kernel storage inventory, `SYS_RUSTOS_STORAGE_LIST_BROKER`, boot extent
  leases, raw disk offsets, and AHCI/NVMe policy replies are retired. Storaged
  derives one DVM descriptor only from authenticated live DVM geometry.
- Boot protocol version 18 retains the former physical extent pointer/length
  only as two mandatory-zero reserved words. Multiboot accepts exactly one
  `rustos-early-system` module and has no `rustos-root-extents` selector.
- GRUB loads one signed, immutable, uncompressed early-system module containing
  the exact bounded bootstrap allowlist needed to start rootd, the core policy
  services and storaged. Hostd and the storage-DVM kernel/rootfs/control
  artifacts are separately signed host inputs. The early-system fixed table uses checked
  offsets, lengths, and per-file SHA-256 digests; pages stay reserved until no
  bootstrap executable or registry references them. Ring0 does not gain an
  archive decompressor, filesystem namespace, general path traversal, or
  mutable package policy.
- The exact allowlist includes the minimal dynamic ELF runtime closure needed
  before storage-DVM readiness (`/etc/ld.so.cache`,
  `/lib64/ld-linux-x86-64.so.2`, `libc.so.6`, and `libgcc_s.so.1`) in addition
  to bootstrap services and registries. The loader cache is part of this
  closure so glibc's cache lookup cannot turn the first UI process into a
  DVM-volume probe. Missing runtime members fail image staging; boot never
  falls through to a physical controller or an unverified host filesystem.
- The early-system module is a bootstrap capability, not a native-storage
  fallback. Once its manifest is admitted, rootd and loaderd may open only its
  exact entries until storaged publishes the DVM-backed root volume. Missing,
  duplicate, overlapping, out-of-range, digest-mismatched, or undeclared
  entries stop bootstrap. They must never trigger NVMe/AHCI probing.
- Vfsd resolves exact early-system files through the dedicated immutable
  broker before consulting the DVM FAT volume. This lets loaderd start initd
  and the fixed bootstrap closure without racing storage-DVM readiness; all
  non-entry paths continue to the service-owned DVM volume and an invalid
  early-system image fails closed instead of becoming a false `ENOENT`.
- On the read-only DVM root volume, vfsd may materialize a complete bounded
  file once per mount generation and serve later `pread`/mapping windows from
  that service-owned snapshot. Per-entry and aggregate byte ceilings are
  mandatory; oversized files fall back to bounded range reads, remount clears
  every cached byte, and enabling mutation requires explicit path/generation
  invalidation before this optimization remains legal.
- Per-file digest admission is bound to the immutable module identity, exact
  payload range, length, and expected digest. A successful proof may be reused
  for later bounded reads and process mappings during that boot; the cache is
  cleared if boot identity is initialized again and is strictly bounded.
  Reuse must never skip table/range/path validation, admit mutable DVM bytes,
  or turn a failed digest into a cached result.
- Early-system ownership is resolved before applying the broker's 4-KiB
  transfer bound. A non-owned path returns to the DVM volume even when the
  caller supplied a larger VFS buffer; an owned immutable entry is read in
  bounded 4-KiB chunks. An entry that disappears between INFO and READ is
  corruption and fails closed rather than falling through to mutable storage.
- The boot disk is a storage-DVM backing artifact, never a ring0 mount. Xtask
  reopens every staged FAT file after image construction and compares its
  exact bytes; early-system creation separately requires every allowlisted
  file, rejects duplicates/empty payloads, and signs the completed image.
- RustOS contains no AHCI/NVMe probe, queue, DMA allocator, partition registry,
  FAT opener, or physical-block fallback. Missing or malformed early-system
  state stops bootstrap; it cannot reactivate physical-storage code.
- Physical NVMe/AHCI ownership transfers exactly once on the host:
  `exclusive whole-device open -> reject holders/mounts/swaps -> bounded
  fsync+BLKFLSBUF -> validate exact controller and IOMMU group -> unbind the
  signed original driver -> reset -> bind vfio-pci -> launch one storage DVM
  with a newer generation -> authenticated check-arm-recheck readiness`.
  Host-native and DVM authority must never overlap. Failure after detach
  revokes the aperture before controller restoration; failed revocation
  quarantines the controller and retains recovery records.
- An active storage lease cannot use the direct `release --activate` path.
  `recover` must load the durable runtime record, bind the exact PID plus
  process start time, request bounded QMP/ACPI shutdown, observe that process
  exit, and only then revoke the generation-bound aperture. Controller reset
  and original-driver restoration follow aperture revocation; any failed step
  retains the lease and quarantine evidence instead of fabricating release.
- `driver-domain-protocol` owns the versioned, address-free DVM block wire
  contract. Version 2 fixes one 8-MiB power-of-two PCI BAR, two 64-entry rings,
  and 64 host-owned 64-KiB transfer slots. The unused tail after the fixed slot
  array is reserved and grants no addressable request/data authority. Requests
  name only a slot, launch generation, request ID, monotonic
  mutation operation ID, sector range, and Virtio-compatible
  READ/WRITE/FLUSH/DISCARD/WRITE_ZEROES operation. Unknown flags, reserved
  bytes, stale generations, unaligned/overflowing ranges, read-only mutations,
  and unsupported features fail closed. Its immutable geometry and generation
  carry an Ed25519 L0 signature. The verifying key is embedded in the
  GRUB-authenticated early-system header; the caller-owned signing key remains
  on L0 and is never mapped into the storage DVM. Signed schema-4 policy binds
  the SHA-256 of that verifying key, so hostd rejects an operator-supplied
  signer for a different RustOS image before freezing the whole device.
- Every shared block-header scalar is naturally aligned. The four 64-bit ring
  cursors are single-copy atomic little-endian values: each ring has exactly
  one producer and one consumer, the producer publishes its record/data before
  a Release cursor store, and the consumer performs an Acquire cursor load
  before reading that record/data. Header identity and geometry are immutable
  for one generation; only readiness and ring cursors may change. RustOS
  validates immutable fields separately from live cursors so normal peer
  progress cannot be mistaken for corruption, while any identity, geometry,
  feature, read-only-mode, or generation mutation revokes the aperture.
- L0 initializes signed immutable geometry with both readiness bits
  clear. RustOS alone sets `RUSTOS_READY`, only after the exact aperture and
  its MSI-X receiver are installed, then rings peer 1. Publication is one
  compare-exchange from the exact verified flags snapshot; if the peer changes
  any readiness state between verification and publication, the operation
  fails without setting `RUSTOS_READY` and the epoch remains revoked. The
  storage DVM may
  bind the initial aperture but must not publish `DVM_READY` until it observes
  that RustOS bit, validates the Linux block geometry, and owns the physical
  controller. Pre-setting either peer's bit, retaining readiness across an
  epoch, or publishing DVM readiness first fails admission.
- Storage-DVM relay admission reports the exact failing stage (`lock`,
  `transport`, `header`, `block-device`, `rustos-ready`, `publish-ready`, or
  `publish-evidence`) with the preserved errno. A generic readiness timeout is
  not acceptable evidence for this boundary. Authenticated block evidence must
  match the live signed aperture's capacity, logical/physical block sizes,
  features, and read-only mode exactly. Read-only is a supported least-
  authority transport mode, not an error or permission to report writable
  media.
- Both directions use ivshmem MSI-X vector 0 only after the corresponding
  Release cursor publication. RustOS writes BAR0 doorbell peer 1 for requests;
  the storage DVM writes peer 0 for completions and readiness withdrawal.
  Doorbells are coalescible wake hints, never queue state: each consumer checks
  its cursor before arming, arms the one event source, rechecks, and drains
  until producer equals consumer. Startup also drains pre-existing work, so a
  pre-boot or coalesced edge cannot require polling.
- A physical storage domain uses signed driver-domain policy schema 4 only:
  every non-block transport is disabled, `BLOCK_TRANSPORT=block-ring-msix`,
  the driver is exactly `nvme` or `ahci`, PCI vendor/device, class/prog-if,
  original host driver, and the one-function IOMMU group match exactly, FLUSH
  is mandatory, queue depth and slot size equal the compiled ABI, and
  handoff/reset deadlines are bounded.
  Schema 2/3 cannot opt into the block transport, so a generic or display
  domain cannot acquire storage authority by changing one transport string.
- The durable VFIO lease uses schema 5 and binds the SHA-256 of the exact
  immutable epoch signing bytes plus its signature before controller
  assignment. Generation and aperture path alone are not sufficient. Hostd
  rechecks that identity immediately before DVM launch and again when
  admitting readiness, so coordinated mutation of shared geometry and DVM
  evidence cannot manufacture supervisor success. Retired lease schemas fail
  closed instead of silently weakening this binding.
- AHCI admission includes the complete minimal Linux driver closure: `ahci`
  owns the PCI controller and signed `sd_mod` must publish its one whole-disk
  block namespace before the relay starts. NVMe uses its native namespace
  driver directly. `ahci`, `libahci`, `libata`, `sd_mod`, `nvme`, and
  `nvme-core` are signed modules, never built-in controller authority; the
  `S12` storage bootstrap loads only the exact assigned PCI modalias closure.
  Missing class-driver, signature, or namespace publication fails closed.
- **Admitted storage controller classes.** The relay and `S12` admit exactly
  one controller from a closed class set, identified from the guest kernel's
  own PCI/virtio identity and never from the harness that created it:
  `ahci` (PCI class `0x0106`, bound to `ahci`), `nvme` (class `0x0108`, bound
  to `nvme`), and `virtio_blk` (vendor `1af4`, PCI class `0x0100`, bound to
  `virtio-pci`, with exactly one `virtioN` child of virtio device type 2 bound
  to `virtio_blk`). Any other combination (another virtio device type, a
  foreign driver, two virtio children, legacy IDE) does not classify, and a
  second admitted controller is `EEXIST`. The evidence `DRIVER` token is the
  admitted class's Linux driver name; the physical-assignment policy (schema 4)
  still names only `nvme` or `ahci`, so `virtio_blk` evidence parses but can
  never satisfy a physical VFIO assignment. `virtio-pci` and `virtio_blk` are
  built in (no module closure); optical/ATAPI media is not an admitted
  namespace (`CONFIG_BLK_DEV_SR` is forbidden), so a CD drive on an assigned
  controller can never appear as a second namespace.
- **Block-size source of truth.** The relay reads logical and physical block
  sizes from the kernel (`BLKSSZGET`/`BLKPBSZGET`), capacity from
  `BLKGETSIZE64`, and read-only state from `BLKROGET`, and refuses a device that
  disagrees with the L0-signed header on any of them (logging both sides). The
  relay publishes the device-reported geometry in its evidence, and the kernel,
  storaged and vfsd use the header/info values, never a constant: any logical
  size in `{512, 1024, 2048, 4096}` is admitted and capacity stays in 512-byte
  protocol sectors. Only the KVM harness (L0) *declares* a geometry, because
  the header is signed before the guest boots: `DVM_STORAGE_LOGICAL_BLOCK_BYTES`
  and `DVM_STORAGE_PHYSICAL_BLOCK_BYTES` (512/512, matching the staged FAT
  images) feed both the virtio-blk device properties and the signed header.
- **Read-only enforcement is end to end.** QEMU opens the harness boot volume
  `readonly=on` and virtio-blk advertises `VIRTIO_BLK_F_RO`; Linux marks the
  disk read-only (`BLKROGET`); the signed header carries `READ_ONLY`; the relay
  rejects a device/header disagreement in either direction, opens a read-only
  transport's device `O_RDONLY` only (it never holds a writable descriptor),
  and refuses WRITE/DISCARD/WRITE_ZEROES while admitting only reads and the
  FLUSH media barrier; vfsd's `BootBlockDevice` rejects writes on RO media.
- **KVM harness frontend.** Both the read-only boot volume and
  `--writable-storage` attach one `virtio-blk-pci` (queued, unlike the former
  emulated ATAPI frontend that executed one command at a time) on a q35 DVM
  machine with `sata=off`, so the explicit disk is the DVM's only storage
  controller. The relay's `BLKRASET` window and read-ahead prefetch predate
  this frontend and have not been re-measured on it; treat them as advisory
  until a run compares `rustos-dvm-block: read-stats` against a run with them
  disabled.
- Completion authority binds the exact generation, request ID, operation ID,
  and slot. Successful reads report no durability. Successful FUA writes and
  FLUSH report the exact durable-through operation ID; ordinary writeback
  completions may report only an earlier accepted mutation. DVM restart clears
  both rings and readiness, revokes the old generation, and requires a new
  authenticated readiness publication before requests resume. RustOS accepts
  a successor aperture on the existing mapping only after the prior epoch has
  revoked, all four cursors are zero, the generation strictly increases, and
  the successor signature verifies against the immutable early-system key.
  A stale, unsigned, or DVM-forged header remains revoked.
- Configured `block.read`, `block.write`, and `block.flush` fault points are
  admitted only after the current signed ready generation has been rechecked.
  The exact request is then published and must receive a generation-bound DVM
  completion before RustOS substitutes `DeviceFault` for the caller-visible
  result. This exercises the real transport and models the conservative
  outcome of an operation whose device result is uncertain; it never
  fabricates success. `block.write` covers WRITE, DISCARD, and WRITE_ZEROES
  mutations; `block.flush` remains separate so durability failure can be
  exercised without claiming a successful stable-write boundary.
- Aperture revocation clears only live readiness and ring cursors. It preserves
  the signed immutable `READ_ONLY` bit, so an interrupted read-only handoff can
  be revoked repeatedly by explicit recovery without invalidating its own L0
  epoch signature. Retry observes the same generation and static geometry; it
  never mints a successor epoch or restores controller authority.
- Ring0's DVM block endpoint is transport substrate only. It validates one
  exact aperture, arms exactly one MSI-X leaf that only wakes, copies bounded
  records/data, and exposes nonblocking submit/collect/cancel plus a
  check-arm-recheck wait. The wait accepts only a 1--30,000 ms deadline and
  shares the RTC wake path; it never spins or applies storage policy.
  Cancellation does not reuse a slot until the exact late completion is
  consumed or the generation is revoked.
- Aperture installation and provider readiness are separate composition
  points. Before RustOS has ever observed `DVM_READY`, its absence is
  `EAGAIN` and the wait predicate remains sleepable; it is not a fault event.
  A newly observed ready bit is itself one wait event, closing the race where
  the DVM publishes between storaged's failed INFO check and scheduler arm.
  Storaged applies one shared four-second product-boot deadline to the INFO
  wait and first generation-bound FLUSH proof, using one-second wait slices.
  Withdrawal after a successful observation is revoke, and timeout never
  selects early-system or a physical-controller fallback.
- Vfsd treats the storaged block-info response and on-disk FAT image as
  separate untrusted admissions. `admit_dvm_block_geometry_with_allowed_flags`
  binds response generation and capacity to the exact envelope, admits only
  `flags == 0` (RW) or `BLOCK_BROKER_INFO_FLAG_READ_ONLY` (RO) while rejecting
  unknown flags and nonzero `reserved0`, requires logical sectors in
  `{512, 1024, 2048, 4096}` (`<= max_block_size`), and checks physical
  alignment and total byte-length multiplication before allocation.
  `BootBlockDevice::write_blocks` rejects writes on a read-only device with
  `StorageError::Unsupported`, and `flush` is a no-op on read-only devices
  while issuing `COMMERCIAL_MAX_STORAGED_OP_DVM_BLOCK_FLUSH` on writable ones.
  `storage-fat` snapshots that geometry and rejects a zero, unsupported, or
  overflowing device before parsing its boot sector. A malformed image never
  publishes a mounted volume.
- Common PCI resource discovery disables command decoding, probes and restores
  each standard BAR dword independently, and restores the low half of a 64-bit
  BAR before touching its high partner. Resource size is the least significant
  implemented address-mask bit, not a full-width two's-complement inversion,
  so zero upper mask bits cannot turn an 8-MiB ivshmem BAR into an enormous
  resource. This follows Linux `__pci_size_stdbars`/`__pci_read_base`; QEMU's
  ivshmem contract assigns the shared-memory object to BAR2.
- `storaged` binds every I/O to the current DVM generation, owns a 15-second
  operation deadline, and cancels timed-out tickets. Control operations and
  writes retain the generic 4-KiB commercial payload. Reads additionally use
  operation 12 and one dedicated exact 64-KiB response: an 80-byte fixed
  header plus at most 65,456 payload bytes. `vfsd` chooses only a
  block-aligned prefix of that payload (60 KiB for a 4-KiB logical block) and
  validates the echoed complete request header, generation, LBA, block count,
  reserved word, and exact payload length before advancing its cursor. The
  bulk operation reuses the ordinary read capability bit; it does not mint
  wider authority. `vfsd` still obtains geometry and performs FAT
  reads/writes/flushes only through storaged and never calls a physical
  boot-block broker.
- Storaged startup performs one generation-bound, non-mutating FLUSH and emits
  its storage E2E proof only after the request crosses the block broker and
  shared ring, the Linux DVM completes the read-only media barrier, and
  storaged consumes the exact completion. This proves transport liveness, not
  backing-image write durability; the host separately syncs the private image
  and its containing directory before signing the aperture. Storage-DVM KVM
  acceptance requires this proof in addition to both peer readiness flags and
  exact geometry.
  `cargo xtask kvm-smoke --timeout 30 --storage-dvm-only` is the independent
  acceptance gate; it cannot fail or pass because of a UI/GPU marker.
- Storage transport availability is a separate state from the `storaged`
  endpoint namespace publication. A registered endpoint proves only the exact
  service owner and capability lease; it must not make a caller wait behind a
  startup media barrier or a different caller's `DVM_READY` wait. The startup
  proof remains generation-bound and mandatory for its acceptance gate, while
  requests before that proof receive an explicit bounded not-ready result and
  retry through their owning policy path. The early-system runtime/compositor
  closure must not depend on that result: it is admitted from signed immutable
  entries, and mutable applications or DVM-volume assets wait on their own
  storage readiness. One response turn uses exactly the `INFO` generation that
  matched the proof: metadata is built from that snapshot, and a stale read,
  write, flush, or bulk-read argument returns `EAGAIN` before it can submit a
  DVM ticket. If the sole readiness worker cannot start, storaged exits for
  rootd revocation/restart rather than advertising an endpoint that can remain
  not-ready indefinitely.
- The Linux-DVM block relay starts immediately after the guest's module
  admission turn (`S12` after Buildroot `S11modules`), before unrelated guest
  network, display, and control-agent scripts. It still discovers exactly one
  assigned AHCI/NVMe/virtio-blk controller (the admitted class set above) and
  admits the complete AHCI `sd_mod` closure when that class is AHCI before
  opening the relay. The DVM build rejects a stale late-start script,
  so a rootfs overlay change cannot silently re-serialize storage behind DHCP
  or GPU startup.
- A DVM image carrying GPU vendor modules, GPU firmware, Mesa/LLVM, and a
  storage relay cannot satisfy the five-second mutable-volume readiness
  requirement merely by reordering init scripts: kernel initramfs unpack is
  itself before every service. The required bootstrap topology is therefore a
  separate signed **storage-bootstrap DVM** with only the fixed block relay,
  its exact AHCI/NVMe closure, the bounded block aperture, and the minimum
  init/runtime dependencies. The full display/network DVM remains a separate
  least-authority domain and cannot gate storage readiness. Each DVM has its
  own image digest, launch secret/epoch, readiness generation, restart budget,
  and revoke path; neither may borrow the other's device or controller
  authority. A storage-bootstrap `READY` proves only its exact block DVM
  identity, and RustOS still requires the generation-bound FLUSH completion
  before mounting the mutable volume. A staging/validation implementation must
  measure kernel-start-to-peer-ready independently, reject a five-second
  breach, and prove that a display/network DVM restart neither revokes nor
  stalls an already-live storage generation.
- Boot diagnosis records kernel-stamped `dvm-block-transport-installed`,
  `dvm-block-peer-ready`, `dvm-block-first-completion`, and the Required
  `dvm-block-transport-revoked` milestone. The revoke frame binds a closed
  nonzero reason in `arg0` and the installed generation in `arg1`, while the
  fixed diagnostic line records the pre-clear flags and ring cursors. Together
  with `ipc-service-capability-request`, `ipc-service-capability-reply`, and
  `ipc-service-register`, these delimit transport admission, the first real
  DVM completion, terminal transport evidence, rootd capability latency, and
  endpoint publication without treating debug print ordering as timing
  evidence. A UI-critical path above five seconds is a failed performance
  acceptance result; the three-second target is the optimization objective,
  not permission to bypass storage or rootd authorization.
- Physical display and storage device classes are signed loadable modules:
  `i915`, `xe`, `amdgpu`, `nvidia-drm`, AHCI, and NVMe may be selected only by
  a kernel-produced modalias for an assigned PCI function after the DVM is
  live. The fixed QEMU bootstrap keeps only virtio display, block, input, and
  network transports built in. Modularity is a lifecycle and authority
  boundary, not permission to omit a supported vendor profile or to probe
  unrelated devices; every packaged module remains signature-pinned and
  artifact-verified, and NVIDIA UVM/compute stays outside the display DVM.
- The schema-9 DVM boot artifact is one hash-bound `cpio.zst` initramfs. The
  build uses deterministic zstd level 3 with one compression worker and the
  kernel must advertise `CONFIG_RD_ZSTD`; an XZ artifact, schema-8 manifest,
  stale alternate rootfs, or decompressor fallback is not launchable. This is
  a boot-critical latency choice only: it does not weaken artifact hashing,
  module signatures, device ownership, or the Linux/Windows application ABI.
- The x86 arch default is only a configuration seed. Final DVM admission
  rejects unrelated built-in driver families including wireless, PCMCIA,
  software RAID/device-mapper, generic Ethernet vendors, sound, media, HID,
  desktop input classes, disk/network filesystems, SELinux policy, kexec,
  suspend, and hibernation. The fixed virtio bootstrap remains built in while
  physical GPU and storage classes remain signed modules. A source change to
  one external leaf module has its own Buildroot invalidation stamp and may
  rebuild that module plus the rootfs, but never the host toolchain, Mesa,
  LLVM, or unrelated services.
- Runtime diagnostics are bounded observation, not an event transport. Uiserver
  keeps its one-second aggregate heartbeat and bounded fault/cursor evidence,
  but does not print a per-input-pulse line while a pointer moves. Input state
  remains in the service-owned queue and its readiness generation; eliminating
  that debug-console work cannot alter delivery, wake, or compositor policy.
- Storaged serves reads through one read engine (`services/storaged/src/read_engine.rs`).
  The device is cached and fetched in aligned 64-KiB windows (the size of one
  fixed DVM slot; the last window is short at the device end). A window is
  `Ready`, `InFlight` (one DVM ticket) or `Failed`, and is requested from the
  transport at most once, so concurrent readers of one range share a single
  fetch. At most 192 windows (12 MiB) are cached or in flight and at most 48
  tickets are outstanding, leaving transport slots for writes and flushes; a
  window a stream has fully consumed is evicted first. Every read still
  revalidates live DVM geometry and the exact caller generation, and a cached
  window never outlives a quarantine, revoke, restart or generation change.
  - Streams: a read that starts at most two windows past, or four windows
    before, the previous read's last window continues that stream (the
    backward slack absorbs parallel readers whose adjacent chunks arrive out of
    order). From the second sequential read the stream reads ahead; its depth
    starts at four windows and doubles per sequential read up to forty (2.5 MiB, so
    read-ahead keeps running while vfsd hashes and writes), as in Linux
    readahead ramp-up. Up to eight streams are tracked independently.
    Random reads create a stream but never read ahead. Read-ahead is refilled
    after the reply has been sent, so its submit syscalls overlap the
    requester's own work.
  - Completion: one pump thread (`storaged-read-pump`) collects completed
    tickets outside the engine lock and wakes the waiting readers; readers never
    wait on the transport themselves. If the pump cannot start, storaged exits
    rather than let every read time out. A ticket older than the 15-second
    deadline quarantines the generation, is cancelled and its window dropped.
  - Concurrency: storaged serves its endpoint from four receive threads, each
    with its own bulk reply slot, so several vfsd lanes are in flight at once.
  - Invalidation: every write first drops overlapping cached windows and flags
    overlapping in-flight windows stale; a stale window is collected and
    discarded, never cached, so bytes read before a write cannot repopulate the
    cache after it. A transport error fails only the waiting reader and is not
    cached. Cached bytes therefore cannot survive revoke, restart, or mutation,
    and the optimization adds no controller authority or hidden fallback to
    vfsd.
- Vfsd reads a large range (at least 2 MiB; the executable snapshot pipeline reads
  4-MiB windows, while loaderd and the early-system broker keep the shared
  256-KiB window) through `services/vfsd/src/bulk_lanes.rs`: the calling thread
  and up to three helper threads claim chunk indexes from one cursor and each
  issue one bulk storaged call per chunk, so adjacent chunks are read
  concurrently. A lane is never the VFS receive owner and uses only storaged IPC
  and kernel-direct futexes, so it cannot wait on syscalld. Four job slots allow
  several large reads at once; a read falls back to its own thread only when
  every slot is busy, the read is smaller than the threshold, or the pool is
  unavailable. Helper threads live on static stacks for the process lifetime;
  nothing is leaked or cached between calls. The caller does not return before
  every claimed chunk completed (including after an error), the first error
  aborts unclaimed chunks, and each chunk's echoed header, generation, LBA,
  block count and length are validated exactly as for a single bulk call.
- The executable snapshot is built by `services/vfsd/src/snapshot_pipeline.rs`,
  a bounded three-stage pipeline over a ring of 2-3 windows of up to 4 MiB
  (`SnapshotBudget` bills the whole ring, at most 12 MiB per snapshot). The
  caller thread reads window N+1 straight into its ring slot through the lanes
  while a writer helper writes window N into the sealed memfd and a hasher
  helper hashes window N-1; with no helper available the caller drives the
  other stages itself, serially. Window counters (`read_done`, `written`,
  `hashed`) bound the reader to the slowest consumer, the first error aborts
  every stage and joins them, and a partial memfd is never sealed. The SHA-256
  digest is computed only for a verbose plan (product evidence for a DVM volume);
  a plan that skipped it cannot publish product evidence. A write to the volume
  during the transfer cannot publish stale bytes: each window read revalidates
  its read-map source and `commit_executable_snapshot` re-checks the map.
- Bulk reply and request wire shape: a `READ_BULK` reply is the 80-byte response
  header plus exactly `payload_len` bytes, a `WRITE_BULK` request is the header
  plus exactly `payload_len` bytes, and the receiver validates the exact length
  (`StoragedBulkReadResponse::payload_of_reply`, `StoragedBulkWriteRequest::
  payload_of_request`); no tail is zero-filled and no byte past the received
  length is read. Vfsd caches the storaged endpoint and pid, drops the cache on
  any IPC error or envelope failure and retries once only when the endpoint id
  changed, so a restarted storaged is found without a per-call lookup.
- Large IPC replies cost one validated copy into a reserved (unzeroed) buffer that
  becomes the frame spill by move (`complete_fast_reply_moving`), and one copy
  to the caller; the request direction uses the same direct copy. Storaged
  validates its requester with the stable-publication ownership check
  (`process_owns_stable_live_service_endpoint`), which fails closed while that
  service is being republished, instead of the global registry mutation lock.
- Memfd writes copy user bytes straight into private, unzeroed frames for the
  pages a write adds (only uncovered head/tail/hole bytes are zeroed) and install
  them under the lock after the copy succeeded, so a failed copy exposes nothing;
  writes to existing pages stage in one per-syscall scratch buffer and copy under
  the lock after the seals are re-checked. The write cursor is reserved first and
  restored on a short write unless another writer moved it. Write chunks are
  256 KiB.
- The storage-DVM relay (`rustos-dvm-block.c`) executes ring requests strictly
  one at a time, so a sequential stream is bounded by the guest block layer's
  per-command latency unless the page cache can overlap commands (measured:
  ~10.7 ms per 64-KiB window, ~6 MB/s, for a 306 MB Chrome snapshot; a lone
  cold window costs ~12.8 ms). The relay therefore (1) raises the whole-disk
  kernel readahead window to 2 MiB with `BLKRASET` and (2) before executing a
  read, offers every read already published in the request ring to the kernel
  with `readahead(2)`, merging adjacent requests into one range. Both are
  advisory page-cache hints issued after the watchdog `begin` event: the `pread`
  that follows still produces the bytes in ring order, the scan stops at the
  first record that is not a well-formed read of the current generation, and
  request admission, ordering, durability, FUA/flush and completion publication
  are unchanged. Failure of either hint is reported once and ignored.
- The relay's worker and supervisor keep the strict watchdog protocol: only the
  supervisor publishes cursors and completions, each request has its own
  deadline that starts at its `begin` event, and a stopped worker cannot publish
  after quarantine. Inside one batch the worker does not wait for the
  supervisor between requests: a completion with more requests queued
  (`begin == 2`) is sent and the next request starts at once, the worker counts
  the completion cursor itself because the supervisor applies events strictly in
  order, and only the last completion of the batch (`begin == 0`) is
  acknowledged, after the supervisor has published it and rung the doorbell, so
  the worker never sleeps on the doorbell with an unpublished completion. The
  supervisor also rings the doorbell every eight unacknowledged completions so
  RustOS can consume a long batch while it is still running. A cursor mismatch,
  a missed deadline or an event out of order still quarantines the generation.
- Sequential-read diagnostics (bounded, aggregate): the relay logs
  `rustos-dvm-block: read-stats` every 256 reads (`pread_us`, `pread_max_us`,
  `readahead_*`); storaged logs `storaged: read engine` every 1024 reads
  (`waited` = reads that blocked on a window, `stall_us` = their total wait,
  `demand_windows`, `ahead_windows`, `in_flight_peak`, `streams`); vfsd's
  `snapshot transfer` line carries `total_us` (wall), per-stage busy times
  `read_us`/`write_us`/`hash_us` that overlap, `windows`, `helper_stages`,
  `*_stall_us` per stage, and `storaged_calls`, `storaged_us` (sampled 1 in 32
  calls, scaled) and `storaged_max_us`. Together they split a slow snapshot into DVM device time,
  broker/IPC time and vfsd time without per-request output.
- TmpFs mounts `/tmp`, `/dev/shm` and `/run` use `CasePolicy::Exact` and
  independent allocation-byte quotas, sparse holes, inode-pinned descriptions,
  symlinks/socket nodes, rename modes, metadata updates and orphan reclamation.
  the desktop session's `/run/user/<uid>` is created with mode 0700 and that uid/gid; `/tmp` and
  `/dev/shm` are 01777. Traversal/open/mutation checks use attested credentials,
  including sticky-directory ownership and chmod ownership. Policy remains in
  vfsd. Supplementary-group/capability delegation is not fabricated by the
  current primary-credential envelope.
- A volatile description restored after provider restart is stale, never
  converted into a pathname handle. Pinned directory enumeration uses the
  node; renamed-directory bases are recovered from the live inode, not the
  old path. Symlinks are expanded before later `..` components.
- Netd's live service owner alone may use `VFS_REQUEST_FLAG_NETD_SOCKET_NODE`
  for socket-node create/query/guarded rollback. `arg3` attests the original
  client whose vfsd cwd resolves a relative path; pid/tid still identify the
  actual netd sender. Create/query return stable `(mount_id,node_id)` in
  `value/aux`; rollback arg1/arg2 must match that exact socket inode before
  unlink, so a replacement node cannot be removed. Netd never treats path text
  alone as filesystem binding identity.
- Generic mount/umount operations that cannot update the actual mount owner
  return unsupported; incrementing a cache generation is not mount success.
  Automatic root/data/TmpFs publication remains service-owned MountTable state.
- Vfsd's separate immutable file-byte cache is an admission-controlled
  convenience cache, not a DVM-read substitute. It may materialize at most
  one 16-MiB file within a 32-MiB total budget only for a zero-offset request
  that covers that complete file. ELF headers, program-header probes, partial
  sequential reads, and nonzero-offset reads must use the bounded FAT
  range-read path instead. Thus a loader validation request for tens or
  hundreds of bytes cannot silently turn into a complete executable transfer
  across the block-DVM boundary; remount invalidation still clears every
  materialized byte before a new generation is served. The already-open
  read-only description's validated length bounds each range read and cache
  decision, so vfsd must not re-open FAT metadata merely to serve a small
  `pread`; reopen/metadata policy remains explicit at `openat` and remount.
- The FAT adapter retains one clean logical-block scratch line across adjacent
  small reads. Repeated directory-entry and FAT-chain reads within that block
  therefore cause one DVM block RPC, not one RPC per parser fragment. A write
  overlapping the line invalidates it before submission; a successful partial
  read-modify-write republishes the updated line, while an error leaves it
  invalid. This cache owns no dirty data and grants no authority beyond the
  mounted block device.
- A storaged write's declared logical-block count must checked-multiply to
  exactly the payload length and fit the admitted capacity before submission
  or read-cache invalidation. Payload length cannot silently redefine the
  caller's requested write extent. On a read-only device
  (`BLOCK_BROKER_INFO_FLAG_READ_ONLY`), `storaged` marks its descriptor
  `STORAGE_FLAG_READONLY` and rejects `COMMERCIAL_MAX_STORAGED_OP_DVM_BLOCK_WRITE`
  (and `write_admitted_with`) with `EROFS` before invalidating the read engine or
  submitting a DVM ticket; on a writable device it invalidates the overlapping
  read-engine windows before submission and supports `COMMERCIAL_MAX_STORAGED_OP_DVM_BLOCK_FLUSH`
  (`BLOCK_BROKER_OP_DVM_SUBMIT_FLUSH`).
- Storaged invokes RustOS-private syscalls only through the raw
  `rustos-svc-runtime` entrypoints. A libc wrapper must not collapse raw
  negative results into `-1` plus TLS `errno`; transient `EAGAIN`, absence,
  revocation, timeout, and protocol failure remain distinct across the full
  kernel-to-vfsd path.
- Storaged and vfsd each emit one diagnostic for a contiguous startup
  `EAGAIN`/`ENODEV`/`ENOSYS` interval and reset that diagnostic state only after
  one validated DVM or storaged reply. They never suppress envelope, binding,
  I/O, or authorization errors. Runtimed keeps a DVM-not-ready launch entry
  retryable and backs it off for 250 ms, so loader probes cannot flood the same
  bounded transport or turn expected readiness into debug-console work while
  the readiness owner is completing its proof.
- Rootd's least-authority service graph contains the explicit
  `vfsd -> storaged` edge. Before storaged publication, lookup preserves the
  transient registry errno; denial preserves `EACCES`. Kernel lookup validates
  canonical error responses before applying success-only value invariants, so
  neither state can be fabricated as `EINVAL` and cached as permanent.
- The signed early-system closure includes the `uiserver` executable and its
  already-required dynamic runtime, so the compositor can launch without a
  cold DVM-volume read. This grants neither storage-controller nor display
  authority to ring0: the DVM still owns the root volume and GPU/KMS provider,
  and uiserver waits for its authenticated display contract. General apps,
  WayClick, desktop assets, and mutable policy remain DVM-volume owned. A
  transient loader or DVM display-provider error schedules a bounded 500-ms
  supervisor retry; only the existing permanent-launch errno set disables the
  entry. Successful spawn ownership prevents duplicates while endpoint
  readiness is pending.
- Design sources: Linux VFIO defines the IOMMU group as the minimum viable
  ownership unit (`https://docs.kernel.org/driver-api/vfio.html`);
  RFC 8032 defines the Ed25519 signature encoding and verification algorithm
  used to authenticate immutable storage transport epochs
  (`https://www.rfc-editor.org/rfc/rfc8032.html`);
  Linux writeback-cache control defines PREFLUSH/FUA completion ordering and
  the rule that a successful flush makes earlier writes durable
  (`https://docs.kernel.org/block/writeback_cache_control.html`);
  Linux stable block queue ABI defines logical/physical sizes, FUA, discard,
  write cache, and write-zeroes
  (`https://docs.kernel.org/admin-guide/abi-stable-files.html`);
  QEMU ivshmem-doorbell defines the shared-memory server and interrupt-vector
  topology (`https://www.qemu.org/docs/master/system/devices/ivshmem.html`,
  `https://www.qemu.org/docs/master/specs/ivshmem-spec.html`); Linux sizes
  standard BAR dwords with decoding disabled before composing a 64-bit
  resource
  (`https://github.com/torvalds/linux/blob/master/drivers/pci/probe.c`).


`SET_ATTRS` time bits 0/1 select atime/mtime, bits 4/5 request
server-resolved NOW for the respective selected time. `arg1`/`arg2` carry
signed seconds, `arg3` packs atime nanoseconds in its low 32 bits and mtime
nanoseconds in its high 32 bits. DOS attributes (bit 3) cannot be combined
with these time fields. Both NOW may be set by an owner, privileged caller, or
a caller with write permission; mixed or explicit times require ownership or
privilege. Both OMIT is handled as Linux's target-independent no-op.

TmpFs quota tracks allocated chunks and symlink bytes, not Job provider-byte
charges. A same-size truncate releases KEEP_SIZE allocations beyond EOF,
including for an unlinked inode that remains open. Connecting Job byte charges
needs a receipt-bearing reservation transaction outside the storage lock,
actual allocation delta and rollback/reconciliation
for concurrent writes, truncate, unlink and pinned orphans, plus durable
ownership across cross-Job access and provider restart. Existing vfsd provider
billing only charges open descriptions, and cannot safely stand in for that
byte lifecycle.

## VFS errno and flag rules added by audits

- `renameat2`: unknown flag bits, or `RENAME_NOREPLACE|RENAME_EXCHANGE`
  together, are `EINVAL`; `RENAME_EXCHANGE` alone is `EOPNOTSUPP` (backend has
  no atomic swap). The rule is `permissions::renameat2_flags_errno`.
- `lseek` `SEEK_DATA`/`SEEK_HOLE` (whence 3/4) use the dense-file model: the
  offset is absolute, `SEEK_DATA` returns it and `SEEK_HOLE` returns EOF; an
  offset at or past EOF is `ENXIO`, a negative one `EINVAL`.
- `utimensat`: `(UTIME_NOW, UTIME_OMIT)` combinations count as a touch
  (`permissions::utimes_touch_only`): the owner or any writer may do it
  (`EACCES` otherwise); an explicit time needs the owner (`EPERM`). Both times
  `UTIME_OMIT` succeeds before the target is looked at and emits no `IN_ATTRIB`.
- `inotify_add_watch` requires read permission on the target (`EACCES`). The
  queue holds at most one `IN_Q_OVERFLOW` event until that event is read.
- FAT `write_at`/`truncate` that extend the file are all-or-nothing when no
  byte lands: the zero-filled gap is cut back and the length is unchanged.
