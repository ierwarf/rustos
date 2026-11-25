# Chrome Linux filesystem/network compatibility

Owner/status record for the current integration batch (2026-11-18). The former
2026-11-14 static audit described an older tree; its blanket statements that
writes, proc snapshots and SEQPACKET are absent no longer describe this tree.
This document is not evidence that Chrome runs on RustOS. See the
[`615b115c` batch record](../validation/ring3-reliability-batch.md) for later
host, formal and KVM smoke evidence; that batch did not rerun Chrome.

## Privilege and launch boundary

- Ring3 `vfsd` owns filesystem names, mounts, access policy, inode/open-description
  lifetime, quotas and advisory-lock policy. `netd` owns `AF_INET`/`AF_INET6`/`AF_NETLINK`
  sockets and route/uevent policy, while `AF_UNIX` (stream/seqpacket/dgram + `SCM_RIGHTS`)
  and persistent `epoll` live in Ring0 (`UnixSocketHandle` / `EpollHandle`, billed to the
  client Job).
- Ring0 owns the fd table, attested caller identity, bounded user copy, page
  tables, process/MM generations, capability/reference lifetime and transport.
  It does not invent another process's proc data or a private filesystem.
- `vfsd` authorizes proc nodes before the kernel supplies facts: `/proc/self`
  and `/proc/<own pid>` expose full caller nodes (including `smaps_rollup`),
  while foreign PIDs expose only `oom_score_adj` and `task[/<tid>[/{stat,status,comm}]]`
  (all other foreign-PID nodes remain `ENOENT`). Service proxy/replay authority
  must be separately authenticated, never inferred from a payload PID.
- Chrome remains an opt-in desktop launch of
  `/apps/chrome/chrome --ozone-platform=wayland`. As of 2026-11-21 the manifest
  also carries the user-approved interim `--no-sandbox` (see
  `apps/chrome/README.md`); no setuid privilege import, automatic Chrome launch or claim of sandbox support is
  introduced. Namespace/setuid/seccomp sandbox completion is outside this batch.

## Implemented surface and integration seams

### Writable namespaces and environment (M1/M8)

`FsVolume`/`MountTable` select the longest matching mount. TmpFs is mounted at
`/tmp`, `/dev/shm`, `/run`, with exact-case names, independent byte quotas,
symlinks, socket inodes, sparse growth, unlink-open node retention, rename
variants and mode/time attributes. In the two-partition layout, `/` mounts
read-only and `/data` is the second FAT partition when its mount succeeds.
`/proc/mounts` is generated from the live mount table.
The old special-purpose `run_dirs.rs` namespace is removed.

Runtime defaults are staged in `runtime-env.tsv` as templates, merged beneath
explicit catalog environment entries and resolved by runtimed per launch:

```text
HOME={profile_root}/home                 -> /data/home, or /tmp/rustos-profile-<uid>/home
XDG_CONFIG_HOME={profile_root}/config
XDG_CACHE_HOME={profile_root}/cache
XDG_RUNTIME_DIR={runtime_dir}            -> /run/user/<launch uid>
TMPDIR={temp_dir}
WAYLAND_DISPLAY={wayland_display}
```

Launch directories are ensured on every launch (a successful previous mkdir is
not cached across deletion or remount); without a writable `/data` the profile
moves to a verified per-uid TmpFs directory, and with no usable root the launch
fails with the real errno instead of starting Chrome with a dead `HOME`. Ordinary build/boot does not run Chrome.
`kvm-run --writable-storage --data-disk <path>` and the corresponding smoke
options preserve partition 2 while refreshing the boot partition. Existing
images are layout/CRC validated; publication uses an exclusive same-directory
temporary file, never a predictable truncating `.partial` sibling.

### Proc and fd identity (M3/M4/S9)

`/proc/self` and `/proc/<own pid>` expose maps/smaps, status/stat/statm
(including `FDSize:`), `oom_score_adj` (read/write `-1000..=1000`), `comm`,
NUL-separated cmdline/environ, auxv, limits, task directories and fd directories.
`vfsd` pulls raw kernel-attested facts via `SYS_RUSTOS_PROC_FACTS_BROKER`
(`0x5255_0066`, `VfsPolicy`-only) and formats `/proc` ASCII nodes in Ring3;
writable `/proc` nodes (`comm`, `task/<tid>/comm`, `oom_score_adj`) route
through `proc_identity::write_snapshot`. Generated regular files report `S_IFREG`, size 0 and are read to EOF. Fd links
are generated from live fd objects, including socket/pipe inode text, memfd
names and remote paths. Directories enumerate actual fd/TID snapshots.
Oversized snapshots fail explicitly rather than silently returning a partial
maps/fd/task listing. Proc directory descriptors support relative path resolution.

A memfd magic-link reopen creates a new description of the same object, with
attenuated access and shared seals; a readonly source cannot manufacture write
authority. Remote TmpFs reopen is inode-bound (`VFS_FCNTL_REOPEN`), not a new
lookup of a stale pathname. A volume without genuine object reopen support
fails explicitly rather than retargeting an old fd after rename/unlink.
`O_PATH` removes I/O and directory-enumeration rights; final-symlink nofollow
must be applied before following a proc magic link.

Local object stat derives mode and inode from the object identity, not a numeric
fd or one universal anonymous-file inode. Dup/fork retain object identity.

### Advisory locks (M7)

The ABI carries Linux POSIX/OFD record locks and flock separately. Vfsd keys
locks by `(mount_id, node_id)`; POSIX owner is the process, OFD/flock owner the
open description. Blocking calls must not block the single vfsd receiver.
Conflicts return `EAGAIN`, GETLK describes the actual blocker, and POSIX wait
cycles are checked. Any close (including `exec`-time `CLOEXEC` closes via
`note_exec_cloexec_closed` + `vfsd::lock_release_proxy_shape`) releases the
process's record locks on that file; final description retirement releases
OFD/flock locks. Against missed exit events or lifecycle queue overflow,
`execute_verified` / `verify_all_owners` probes kernel process incarnations via
`LIFECYCLE_BROKER_MODE_PROBE_PIDS` and drops dead or pid-reused owners. The
lock table is intentionally not checkpointed (locks die on `vfsd` restart).

### Unix messages and route dumps (M5/M6/S3)

The implementation adds AF_UNIX SEQPACKET record queues, abstract byte-addressed
names, filesystem socket nodes and NETLINK_ROUTE GETLINK/GETADDR dumps. Message
boundaries, short receive/MSG_TRUNC, empty records, ancillary custody and
MSG_CMSG_CLOEXEC must be validated through the complete kernel/netd path, not
only a queue helper. Filesystem bindings must compare actual socket inode
identity after unlink/recreate and resolve through vfsd, including cwd-relative
names; private path-string fallback is not a replacement namespace.

Route dumps must include loopback plus a currently live provider interface,
correct sequence/multipart termination and a kernel-port sockaddr_nl source.
`SOL_NETLINK` membership options (`NETLINK_ADD_MEMBERSHIP`,
`NETLINK_DROP_MEMBERSHIP`, `NETLINK_LIST_MEMBERSHIPS`) and lazy
interface-change notifications are wired in `netd` (`poll.rs` / `netlink.rs`),
`umask` is applied to filesystem socket nodes, and Wayland (`uiserver`) binds in
the normal namespace (`$XDG_RUNTIME_DIR/wayland-0`). Full dynamic route
notification and AF_UNIX datagrams are not implied.

### Linux calls and open semantics (M2/S8)

Dispatch and 32-bit argument canonicalization are extended together. Added
families cover memory queries/remap, file attributes/reservation/copy,
lock calls, timed signals/select, message batches, epoll timespec waits,
rlimits and scheduler/priority operations. Having a dispatch arm or an argument
helper test is not proof of Linux behavior.

Copied-page mremap was unsafe with sibling writes, so movement is now done by the
MM owner (`MM_BROKER_OP_MOVE`: leaves, frame-ledger claims, memlock ledger and
pager VMA generations move together, no copy; see `abi/core.md`), and `fork`
clones the parent's `syscalld` `MM_POLICY` VMAs (including `PROT_NONE`
reservations) via `LifecycleConsumer::LinuxSyscallPolicy`. `mlock`/`mlock2`
pre-fault resident pages (unless `MLOCK_ONFAULT`), apply partial mapped
prefixes before returning `ENOMEM` on holes, and account `RLIMIT_MEMLOCK`.
`madvise` implements `MADV_POPULATE_READ`/`WRITE` and `MADV_DONTNEED`/`FREE`
on anonymous private pages. Writable `MAP_SHARED` supports FAT (`msync`
`VFS_SHARED_BACKING_FLAG_SYNC`) as well as TmpFs, and `truncate`/`ftruncate`
shrink revokes past-EOF leaves with an EOF guard for `SIGBUS`. `chown`/`mknod`/
`link`/`utimensat` (ns) and `sendfile` to pipes/sockets are wired. KEEP_SIZE
allocation must not report guarantees it does not establish. Copy pins must be released only
when acquired; overlap checks use inode identity and effective gated cursors.
Open honors `O_NOFOLLOW`/`ELOOP`, attenuated `O_PATH`, and an explicit unsupported
answer where anonymous `O_TMPFILE` is not implemented.

## Boot runtime assets (S10)

Staging supplies hosts/resolver/NSS configuration, Fontconfig configuration,
vendor-licensed TTF fonts and a materialized TLS CA bundle. Missing mandatory
fonts/trust inputs or a malformed existing PEM bundle fail staging; no synthetic
certificates or silent TLS fallback is allowed. The existing target-loader
closure check validates imported Chrome ELF dependencies and materializes
symlink aliases before FAT serialization. Missing dependencies and unresolved
aliases fail rather than use the host loader's accidental search path.

## Actual host trace evidence

Collected with the explicitly imported bundle, no sandbox bypass:

```sh
timeout --signal=TERM --kill-after=3 20 strace -f -tt -s 180 \
  -o logs/chrome-integration/chrome-host.strace \
  apps/chrome/bundle/chrome --ozone-platform=wayland \
  --user-data-dir=<new disposable host directory>
```

Exit 124 records the bounded timeout, **not** a Chrome startup acceptance result.
The host Wayland environment was inherited. `strace` was extracted locally from
an Arch package whose detached signature verified with the host keyring; it was
not installed system-wide. Full logs are ignored under `logs/chrome-integration/`.
The signature summary counted 131 distinct syscall names (entry records only).

Observed shapes include:

- `memfd_create(..., MFD_CLOEXEC|MFD_ALLOW_SEALING|MFD_NOEXEC_SEAL)`, seals and
  `openat(..., "/proc/self/fd/N", O_RDONLY|O_CLOEXEC)`;
- `socketpair(AF_UNIX, SOCK_SEQPACKET[|SOCK_CLOEXEC], ...)`, fd-directory opens;
- Singleton symlinks and filesystem bind under a temporary directory;
- POSIX whole-file and SQLite byte-range `F_SETLK` calls;
- NETLINK_ROUTE GETADDR/GETLINK, with a multicast group mask of `0x113`;
- mremap (25 entry records), sendfile (3), fallocate (20), sendmmsg (14),
  utimensat (272), sched_setscheduler (9), setpriority (367).

Namespace/seccomp calls were also observed and remain an independent sandbox
blocker. Absence of mincore/mlock/msync/membarrier/recvmmsg/epoll_pwait2 in this
short trace does not waive their requested compatibility tests.

## Acceptance (not yet complete)

1. Historical host evidence includes the isolated integrated run in item 4.
   The later `615b115c` host suite is recorded separately; a host pass does
   not validate the Chrome guest workload.
2. Host trace/signature comparison above collected; not RustOS execution.
3. `--chrome-compat-probe` is opt-in and requires every named case plus PASS.
   One earlier guest profile passed (2026-11-18, default vCPUs, one disposable disk, explicit
   phases): `chromeprobe: persist first-boot ok` then `persist second-boot ok`,
   `chromeprobe: PASS (386 checks, 0 failures)` on both boots, `kvm-smoke` exit 0.
   Reproduce with:

   ```sh
   cargo xtask kvm-smoke --chrome-compat-probe --chrome-probe-persist first --data-disk <path>
   cargo xtask kvm-smoke --chrome-compat-probe --chrome-probe-persist second --data-disk <same path>
   ```

   A missing/corrupt second-boot marker is failure, not another first-boot pass.
   As of 2026-11-25, `--chrome-compat-probe` passes all **505 checks / 0 failures**
   (including `memfd.keymap-scm-mmap`), and desktop Chrome holds pass across
   2048/4096/6144 MiB KVM runs (see [Session Handoff](session-handoff.md)).
   Bugs found and fixed by the final runs: vfsd `memfd_create` offloaded to
   syscalld while syscalld waited for vfsd inside the `MAP_FILE_SHARED` broker op
   (self-deadlock, ETIMEDOUT; vfsd now applies the stateless memfd policy
   locally); lifecycle EXIT was queued after the zombie became visible to `wait4`
   (flaky `lock.posix-data` EAGAIN). Two readiness failures
   (`uiserver: gpu-compositor active`, uiserver watchdog) occurred only while
   other cargo jobs loaded the host; clean-host reruns passed, but the
   historical few-percent KVM flake baseline still applies.
4. The final serial package suite and `cargo xtask check` passed using an
   isolated Cargo target. `bash formal/verify-all.sh --profile pr` likewise
   passed and sealed 45 artifacts with that target. The shared-target run
   had failed with a foreign storage-core `FsError::Provider` definition.
   Formal routing selects the owner models; a sealed gate does not override
   the reproducible failures in the Linux guest probe.
