# Chrome gap audit: filesystem / procfs / sockets / fd kinds

Static read-only audit, 2026-11-14 (no guest run; Chrome behavior below is from
Chromium source knowledge, not from an strace of this bundle). Item 0 of each
section is the fix to verify with a real `strace -f` of Chrome on a Linux host
first (`chrome --ozone-platform=wayland --no-sandbox --disable-gpu`).

Launch shape today: `services/runtimed/src/catalog.rs:524` runs
`/apps/chrome/chrome --ozone-platform=wayland`, `env: Vec::new()`
(no HOME / XDG_RUNTIME_DIR / XDG_CONFIG_HOME from the catalog entry).

Already fine (do not re-audit): `/proc/self/exe` readlink, `prctl`
(NAME/DUMPABLE/NNP), `eventfd2`, `pipe2`, `epoll_create1/ctl/pwait` incl.
`EPOLLET/ONESHOT/EXCLUSIVE` (`poll_epoll.rs:878,1518`), `poll/ppoll`,
`dup/dup2/dup3`, `F_DUPFD(_CLOEXEC)/F_GETFD/F_SETFD/F_GETFL/F_SETFL`,
`memfd_create` + `F_ADD_SEALS/F_GET_SEALS` (SEAL/SHRINK/GROW/WRITE,
`kernel/ps/src/user/memfd.rs:262-283`), SCM_RIGHTS on AF_UNIX stream
(`netd/src/unix_stream.rs`), `SO_PEERCRED/SO_PASSCRED`, `MSG_PEEK`,
`getdents64`, `statx`, `newfstatat`, `readlinkat`, `faccessat`, `mkdir` under
`/run/user/<euid>` only, `getrandom`, `/dev/null` `/dev/urandom` `/dev/random`.

Deliberately out of scope (docs/ai/abi/core.md:389, apps/chrome/README.md:31):
seccomp filter engine (`PR_SET_SECCOMP` -> EINVAL), setuid/userns sandbox,
`PR_SET_VMA`, `PR_SET/GET_TIMERSLACK`, `PR_CAPBSET_READ`, GPU/DRM/gbm
(software wl_shm). Not listed as gaps below.

## MUST-FIX (ordered by startup order)

### M1. No writable filesystem at all (profile dir, /tmp, /dev/shm, XDG)
- Chrome use: very first thing is `mkdir -p $XDG_CONFIG_HOME|~/.config/google-chrome`
  (mkdir 0700, chmod), `open(O_CREAT|O_EXCL|O_RDWR)` for `Local State`, `lockfile`,
  `First Run`, `Default/Preferences`, leveldb / sqlite / disk cache; `mkdtemp`
  in `/tmp` (`.org.chromium.Chromium.XXXXXX`, ProcessSingleton socket dir);
  `base::GetShmemTempDir()` = `/dev/shm` (else `/tmp`) for anonymous shared
  memory when memfd is not chosen; `base::CreateTemporaryFile`.
- RustOS: every write path is EROFS. `services/vfsd/src/lib.rs:866-883`
  (`persistent_mutation_status`, `mkdir_policy` allows only `/run`,
  `/run/user`, `/run/user/<euid>`), `services/vfsd/src/state_storage.rs:185-187`
  (`O_CREAT|O_TRUNC` on non-device => EROFS), `state_requests.rs:162-181`
  (WRITE only accepted for 3 device nodes, FTRUNCATE EROFS), root listing
  (`state_storage.rs:747-751`) has only `dev proc run` + FAT. No `/tmp`,
  `/dev/shm`, `/home`, `/var`. `docs/ai/performance/core-paths-gaps.md` A2 records
  this as unimplemented; the "journal" excuse only applies to persistence.
- Linux-correct: `/tmp`, `/dev/shm`, `/run/user/<uid>`, `$HOME` are writable;
  data survives while the process lives; `O_EXCL`, `O_TRUNC`, `O_APPEND`,
  `O_TMPFILE` (or EOPNOTSUPP), sparse `ftruncate` growth, `pread/pwrite`,
  unlink-while-open keeps data, `st_nlink`, mtime updates, `ENOSPC` bound.
- Sketch (owner `services/vfsd`): add a memory-backed `TmpFs` node tree
  (dir/file/symlink/socket-inode kinds, inode numbers, mode/uid/gid/times,
  per-mount byte quota charged via `provider_accounting.rs`) mounted at `/tmp`,
  `/dev/shm`, `/run` (replace `run_dirs.rs`) and an overlay for `$HOME`
  (default `/home/<user>` or `/run/user/<euid>/home`; point `HOME`,
  `XDG_RUNTIME_DIR=/run/user/1000`, `XDG_CONFIG_HOME` in
  `runtimed/src/catalog.rs`/`spawn.rs` env, they are empty today). Route
  `WRITE/FTRUNCATE/MKDIR/UNLINKAT/RENAME/SYMLINK/FSYNC/FALLOCATE/CHMOD` ops in
  `state_requests.rs` to it before the FAT read-only fallthrough; extend
  `metadata()`/`dir_entries()` (`state_storage.rs:670,747`). Persistent FAT
  writes stay EROFS (separate journal work). New VFS ops need
  `libs/rustos-user-abi/src/syscall.rs:715-736` ids + `formal` witnesses.
  Chrome's shared memory should go through `memfd` (exists), so `/dev/shm` is a
  fallback + `/proc/self/fd` reopen target (see M4), not the hot path.

### M2. Missing Linux syscalls reached during startup (all return ENOSYS)
Dispatch is `kernel/compat/src/user/syscall/linux.rs:508` ->
`dispatch_ext.rs` -> `error_ops.rs:39 unimplemented_linux_syscall` (ENOSYS,
milestone `linux-syscall-unimplemented`). The ABI defines only ~95 numbers
(`libs/rustos-user-abi/src/linux.rs:30-130`). Absent and Chrome/glibc-relevant:
- `rename`/`renameat`/`renameat2` (profile writes use write-temp + rename;
  `base::ImportantFileWriter`, `ReplaceFile`), `symlink`/`symlinkat`,
  `link`, `rmdir` (verify `unlinkat(AT_REMOVEDIR)` in vfsd `linux_unlinkat`),
  `fsync`/`fdatasync` (every `ImportantFileWriter`, sqlite, leveldb),
  `pwrite64`, `readv`, `preadv/pwritev`, `fallocate`, `chmod/fchmod/fchmodat`,
  `fchown`, `utimensat`, `flock`, `truncate`, `statfs/fstatfs`
  (`base::SysInfo::AmountOfFreeDiskSpace`, shm-dir noexec check), `sendfile`
  and `copy_file_range` (disk cache copy), `fadvise64` (EINVAL is fine; ENOSYS
  also tolerated by libc callers).
- `sysinfo` (glibc `sysconf(_SC_PHYS_PAGES)` -> `base::SysInfo::
  AmountOfPhysicalMemory`; 0 poisons V8 heap sizing and renderer memory limits),
  `gettimeofday`, `time`, `clock_getres` (no vDSO exists: `rg vdso` empty),
  `getrusage`, `times`, `getrlimit/setrlimit` (legacy callers), `mremap`
  (glibc realloc falls back; PartitionAlloc/V8 may not), `mincore`, `mlock`,
  `msync`, `membarrier`, `sched_setscheduler/getparam/getscheduler`,
  `setpriority/getpriority`, `getgroups`, `getresuid/getresgid`, `capget`,
  `getcpu`, `rt_sigtimedwait/pending/suspend`, `pselect6/select`, `sendmmsg/
  recvmmsg`, `close_range`, `epoll_pwait2`, `pidfd_*`, `waitid`.
- Linux-correct: each exists with real or well-defined errno (`ENOSYS` is only
  valid for calls the kernel truly lacks). `sysinfo`, `gettimeofday`, `time`,
  `clock_getres`, `getrusage`, `getrlimit`, `readv`, `pwrite64` have no
  dependency on writable fs and are cheap: do these first.
- Sketch: add `SYS_*` to `libs/rustos-user-abi/src/linux.rs`, arms in
  `dispatch_ext.rs` (keep `linux.rs` match small); time/rlimit/sysinfo in
  `syscalld_ops.rs`/`process_time.rs`/`proc_identity.rs`; fs ops via new VFS
  ops (M1); `readv` per the `local-fd.md:67` design (first iovec blocks).

### M3. /proc, /sys content Chrome and glibc read (all ENOENT today)
- `services/vfsd/src/proc_fs.rs:55-75`: only `/proc/self`, `/proc/<pid>`,
  `/proc/<pid>/exe`. Everything else is `Some(Err(ENOENT))` (test asserts it:
  `proc_fs.rs:212-215`); `/proc` lists empty (`state_storage.rs:753`); no `/sys`
  node exists in the root listing at all (`state_storage.rs:747`).
- Chrome / glibc use and Linux-correct content:
  - `/proc/self/maps` (+`smaps`): `pthread_getattr_np` on the main thread parses
    it (V8/base stack-bounds, `base::debug::StackTrace`, PartitionAlloc
    sanity, `ReadProcMaps` in `base::debug::...`). Needs real VMA list:
    `start-end perms offset dev:inode path`, `[stack]`, `[heap]`.
  - `/proc/self/status` (VmRSS, Threads, Uid/Gid; `ProcessMetrics`, sandbox
    checks), `/proc/self/stat` (utime/stime/starttime/num_threads; base
    `GetProcessCreationTime`, `GetCumulativeCPUUsage`), `/proc/self/statm`,
    `/proc/self/cmdline` and `environ` (NUL-separated), `/proc/self/auxv`
    (`getauxval` fallback), `/proc/self/task/` (+`<tid>/stat`, `comm` — thread
    names via `prctl(PR_SET_NAME)` already stored), `/proc/self/limits`,
    `/proc/self/oom_score_adj`, `/proc/self/fd/` (dir listing, used for
    leaked-fd checks and closing inherited fds after fork in `base::
    LaunchProcess`, which iterates `/proc/self/fd` and falls back to
    `getrlimit(RLIMIT_NOFILE)` loop).
  - `/proc/meminfo` (`SysInfo::AmountOfAvailablePhysicalMemory`, memory
    pressure monitor), `/proc/cpuinfo`, `/proc/stat` (`SystemMetrics`),
    `/proc/sys/kernel/{osrelease,pid_max,random/boot_id}`,
    `/proc/sys/vm/{overcommit_memory,max_map_count}`, `/proc/loadavg`.
  - `/sys/devices/system/cpu/{online,possible,present,cpu*/topology,cpufreq}`
    (`sysconf(_SC_NPROCESSORS_ONLN)`, `base::SysInfo::NumberOfProcessors`,
    `NumberOfEfficientProcessors`), `/sys/fs/cgroup` (cgroup v2 memory/cpu
    quota detection; ENOENT = "no cgroup", acceptable).
- Sketch: extend `proc_fs.rs` `ProcNode` with generated-content nodes.
  vfsd cannot see kernel VMAs/threads: add a kernel query (existing
  `proc_identity.rs` is the precedent for `exe`) that returns a bounded,
  versioned text snapshot per node for the *caller's own pid* (keep the
  caller-only rule at `proc_fs.rs:63`). Order: `maps`, `status`, `stat`,
  `statm`, `cmdline`, `environ`, `auxv`, `fd/` + `fd/N` readlink (target
  `socket:[ino]`, `pipe:[ino]`, `/memfd:name (deleted)`, `/path`), `task/`,
  then `meminfo`, `cpuinfo`, `stat`, `/sys/devices/system/cpu/*`. Each needs
  `statx` S_IFREG size 0 semantics (procfs reports size 0, readers use read-to-EOF),
  `getdents64` for the directories, `O_DIRECTORY` open of `/proc/self/fd`.

### M4. `/proc/self/fd/N` open/readlink (shared-memory read-only conversion)
- Chrome use: `PlatformSharedMemoryRegion::ConvertToReadOnly`
  (base/memory/platform_shared_memory_region_posix.cc) reopens the region via
  `open("/proc/self/fd/<N>", O_RDONLY)` to get a fd that can never be mapped
  writable; also used by `base::File::GetPath`-style debugging and glibc
  `ttyname`/`realpath` fd checks. Without it every Mojo shared buffer and
  Wayland shm buffer creation fails or cannot be handed read-only to another
  process (verify the exact branch with strace; recent Chromium prefers
  `memfd_create` and seals with `F_SEAL_FUTURE_WRITE`, see S2).
- RustOS: `proc_fs.rs:70-72` -> `/proc/self/fd` is ENOENT; `readlinkat` for
  non-exe nodes impossible.
- Linux-correct: open creates a NEW open description of the same object
  with the requested access (read-only reopen of a memfd keeps seals/`size`,
  its mappings are `MAP_SHARED` read-only and counted against
  `writable_mapping_count`); a socket/pipe reopen is `ENXIO`.
- Sketch: syscalld/kernel side (fd table lives in `kernel/ps`), not vfsd: in
  `syscall_linux_vfs_openat` intercept `/proc/self/fd/<n>` and `/proc/<pid>/fd/<n>`
  (own pid only) when the resolved fd is a memfd: call
  `Memfd::open_description(readonly)` in `kernel/ps/src/user/memfd.rs`; for
  vfsd-remote files re-issue `VFS_IPC_OP_OPENAT` with the recorded path.
  `readlinkat` for fd links: compose from the fd table kind.

### M5. AF_UNIX supports SOCK_STREAM only; SEQPACKET/DGRAM -> EAFNOSUPPORT
- Chrome use: Mojo `PlatformChannel` / `NamedPlatformChannel`
  (mojo/public/cpp/platform/platform_channel.cc) creates
  `socketpair(AF_UNIX, SOCK_SEQPACKET)` on Linux/ChromeOS (message boundaries
  with FD attachments, `MSG_CMSG_CLOEXEC`, `SO_SNDBUF`, `MSG_NOSIGNAL`); zygote
  and crash reporter also use SEQPACKET (`base::UnixDomainSocket`), plus
  `SOCK_DGRAM` AF_UNIX for syslog-style `/dev/log` (logging, tolerated).
  Browser <-> renderer/utility/GPU/network service channels are Mojo => without
  SEQPACKET no child process ever connects, and `--single-process` is unsupported.
- RustOS: `libs/rustos-user-abi/src/syscall.rs:2761-2765` accepts only
  `SOCK_STREAM`, protocol 0; both `handle_socket` (`netd/src/socket_lifecycle.rs:
  101-107`) and `handle_socketpair` (`:168-174`) return `EAFNOSUPPORT`
  (Linux gives `ESOCKTNOSUPPORT`/works). Chrome's fallback to `SOCK_STREAM` is
  not guaranteed on every path.
- Linux-correct: SEQPACKET = reliable, connection-oriented, message
  boundaries preserved, `MSG_TRUNC` on short buffer, `SCM_RIGHTS` attached to
  the message that carries it, 0-byte message allowed, EOF as 0 return after
  `shutdown/close`. Receive queue limit per `SO_RCVBUF`.
- Sketch (`services/netd`): add a `kind: Stream | SeqPacket | Dgram` to
  `UnixSocket` (`main.rs:348`) and make `unix_stream.rs` enqueue whole
  messages (record boundary + ancillary custody per record, already modeled as
  ordered stream + tickets). `connect/accept/listen/bind` unchanged. Extend
  `net_broker_unix_socket_shape_valid` and its formal shape witness. Error
  code: `ESOCKTNOSUPPORT` for truly unsupported types.

### M6. Unix-socket namespace is private to netd (no filesystem inode, no abstract)
- Chrome use: `ProcessSingleton` (chrome/browser/process_singleton_posix.cc):
  `mkdtemp /tmp/.org.chromium.Chromium.XXXXXX`, `bind(AF_UNIX, tmp/SingletonSocket)`,
  `symlink(tmp/SingletonSocket -> <profile>/SingletonSocket)`,
  `symlink("hostname-pid" -> <profile>/SingletonLock)`, `symlink(cookie ->
  SingletonCookie)`, then `lstat/readlink/unlink` on them; a failing `symlink`
  other than `EEXIST` aborts browser start (profile error). Abstract sockets
  (`\0name`) used by `dbus` fallback and `base::...Sandbox IPC`.
- RustOS: `socket_info.rs:205-207` rejects leading NUL (`EINVAL`); binds live in
  `state.bindings` (`socket_lifecycle.rs:410-421`) keyed by path string, no vfsd
  inode, so `stat(path)` is ENOENT, `unlink(path)` of a stale socket fails,
  `bind` twice on a reused path is `EADDRINUSE` forever (no unlink),
  `symlink` is ENOSYS (M2).
- Linux-correct: bind creates an `S_IFSOCK` inode visible to `stat`,
  `access`, `getdents64`; `unlink` removes it (bound name freed at unlink);
  abstract names live in a per-netns table, length = addrlen.
- Sketch: vfsd tmpfs gets a `Socket` node kind; netd asks vfsd to create it on
  bind (`VFS_IPC_OP_MKNOD`-like, owner uid/mode, `EADDRINUSE` if exists), vfsd
  notifies netd on unlink (or netd checks vfsd on connect). Abstract: separate
  `abstract_bindings: BTreeMap<Vec<u8>, token>` in netd, `sockaddr_path_from_
  payload` returning an enum. Wayland path stays special-cased
  (`is_wayland_path`); make it ordinary once `/run/user/1000` is tmpfs.

### M7. Advisory locks: `F_SETLK/F_SETLKW/F_GETLK/F_OFD_*` and `flock` -> EINVAL/ENOSYS
- Chrome use: `base::File::Lock` (`fcntl(F_SETLK, F_WRLCK)`) on `<profile>/lockfile`
  (profile directory lock, fatal if it fails: "Chrome cannot open its profile"),
  leveldb `LOCK` files (Local Storage, IndexedDB, Sync, Extension state),
  sqlite (`F_SETLK` ranges; with unix-excl VFS only on some builds), disk-cache.
- RustOS: `vfs_socket.rs:483` `_ => EINVAL`; remote-vfs fcntl
  `state_files.rs:350-` only knows GETFL/SETFL (+seals on memfd). `flock`
  has no syscall number.
- Linux-correct: per-process POSIX record locks (released on any close of the
  file by the process, not inherited by fork), OFD locks per open
  description, `F_GETLK` conflict report, `EAGAIN/EACCES` on conflict,
  `F_SETLKW` blocking + `EDEADLK`; locks are released when the process dies.
- Sketch (`services/vfsd` owns the inode, M1): lock table keyed by inode id with
  owner = (pid) for POSIX / (open-description id) for OFD/flock; kernel
  fcntl routes cmds 5/6/7 and 36/37/38 (`F_OFD_*`) to
  `VFS_IPC_OP_FCNTL` with a `struct flock` payload; process exit cleanup via
  the existing vfsd pid-cleanup (`provider_accounting.rs`). Minimum viable:
  single-writer lock that succeeds uncontended and reports conflict
  correctly across processes.

### M8. Environment / launch contract for a sandbox-less run
- `/apps/chrome/chrome --ozone-platform=wayland` without `--no-sandbox` makes
  Chrome try `clone(CLONE_NEWUSER)`/setuid helper/seccomp then abort with
  "No usable sandbox". README and `core.md:389` say no `--no-sandbox`
  fallback will be added: this is a product decision for the user, but
  Chrome cannot start otherwise. Also needed in the catalog entry env
  (empty today, `catalog.rs:525`): `HOME`, `XDG_RUNTIME_DIR=/run/user/1000`,
  `XDG_CONFIG_HOME`, `XDG_CACHE_HOME`, `WAYLAND_DISPLAY=wayland-0`,
  `TMPDIR=/tmp`, `LANG`, `PATH`; flags: `--disable-gpu` (or
  `--use-gl=disabled --disable-gpu-compositing`), `--no-zygote`,
  `--disable-dev-shm-usage` (until M1 `/dev/shm` exists),
  `--user-data-dir=<writable>`, `--disable-crash-reporter` (crashpad
  forks a handler and uses `PR_SET_PTRACER`/`ptrace`), `--no-first-run`.
  Verify `runtimed/src/spawn.rs` merges the env of spawn_tests.rs:255-289.

## SHOULD-FIX (Chrome starts degraded or wrong, does not abort)

- S1. `/dev` nodes: `services/devmgrd/src/main.rs:1131-1139,1157-1158` exposes
  `console0 display0 input0 input/event0 dri/card0 null urandom random` only.
  Missing `/dev/zero` (used by `mmap` fallbacks in some libs), `/dev/full`,
  `/dev/tty`, `/dev/shm`. `/dev/dri/card0` *exists* for Linux apps: Chrome
  with GL enabled will `open` it, `drmGetVersion`/ioctl and then fail late.
  Return `ENOENT` for `/dev/dri/*` to non-ui callers (`state_storage.rs:684`
  lookup) or ensure the failure is a clean `ENODEV` at open.
- S2. `F_SEAL_FUTURE_WRITE (0x10)` rejected by `memfd.rs:267-271`
  (`allowed` lacks it): Chromium memfd regions seal with it after read-only
  conversion; EINVAL there makes `ConvertToReadOnly` fail. Implement as
  "new writable mappings refused, existing ones stay" and add the witness.
  Also `MFD_HUGETLB`/`MFD_NOEXEC_SEAL` flag acceptance (kernel 6.3+: glibc
  callers pass `MFD_NOEXEC_SEAL` and retry; EINVAL is accepted).
- S3. `AF_NETLINK`: `socket(AF_NETLINK, SOCK_RAW, NETLINK_ROUTE)` falls into
  `net_broker_unix_socket_shape_valid` => `EAFNOSUPPORT`
  (`socket_lifecycle.rs:101`). Chrome's `AddressTrackerLinux` /
  `NetworkChangeNotifierLinux` then log an error and assume
  `CONNECTION_UNKNOWN`; `net::GetNetworkList` (`getifaddrs`) also fails
  (glibc falls back to ioctl `SIOCGIFCONF`, absent too). Browser starts;
  `navigator.onLine` and DNS-over-network heuristics are off; some pages
  report offline. Cheapest fix: a netd `AF_NETLINK/NETLINK_ROUTE` stub that
  answers `RTM_GETLINK/GETADDR` with `lo` + the inet stack address and sends
  `NLMSG_DONE`.
- S4. `SO_PEEK_OFF`, `SO_SNDBUF/RCVBUF` for unix, `SO_TYPE` for dgram, `MSG_PEEK`
  with ancillary: `socket_info.rs:72-73` returns constants for unix; `SO_PEEK_OFF`
  is unhandled (Mojo does not use it; sandbox IPC does). `MSG_CMSG_CLOEXEC` for
  AF_UNIX stream recvmsg: verify the received fd gets `FD_CLOEXEC` in
  `net_broker_ops.rs:422` path (Wayland and Mojo both pass it).
- S5. `inotify_init1/add_watch/rm_watch`: ENOSYS. Used by
  `base::FilePathWatcher` (Preferences, policy, bookmarks, themes, fontconfig
  cache). Chrome tolerates failure; add once tmpfs (M1) can produce events.
- S6. `timerfd_create/settime`, `signalfd`: ENOSYS. Chromium message pump
  uses `epoll` timeouts and `eventfd`, not timerfd; Wayland event repeat uses
  base timers. Needed only by some third-party libs (`libinput` is not used).
- S7. `getdents64` on `/proc`, `/sys`, `/proc/self/fd` (empty today) and long
  FAT names: Chrome enumerates `/proc/self/fd` after fork and `/sys/class/*`
  (battery, input) in `ui::DeviceDataManager`; empty dir is acceptable only if
  open does not return ENOENT. Coordinate with M3.
- S8. `open` flags: no source in vfsd/kernel mentions `O_PATH`, `O_NOFOLLOW`,
  `O_TMPFILE`, `O_DIRECT` (only pipe handling at `local_fd.rs:264`). Chrome
  uses `O_NOFOLLOW` in `base::DeleteFile`/`OpenFile` and `O_PATH` via glibc
  `fdopendir`/`realpath`; they are silently treated as plain flags (works for
  read-only FAT, wrong for symlinks and once tmpfs exists). Validate unknown
  flags and honor `O_NOFOLLOW` (ELOOP), `O_PATH` (no read/write, usable with
  `AT_EMPTY_PATH`/`fstat`), `O_TMPFILE` (EOPNOTSUPP until tmpfs unnamed files).
- S9. `fstat` on memfd/socket/pipe/eventfd/epoll must give `st_mode`
  (`S_IFSOCK/S_IFIFO/ ...`), `st_ino` unique per open object (Chrome keys
  `base::FileDescriptorWatcher` and Mojo handle dedupe on `st_ino`); verify
  `vfs_meta.rs` fstat for local fds. `fstat(dirfd)` on `/proc/self/fd` links.
- S10. `/etc/passwd`, `/etc/group`, `/etc/nsswitch.conf`, `/etc/resolv.conf`,
  `/etc/hosts`, `/etc/os-release`, `/etc/fonts/fonts.conf` + `/usr/share/fonts`,
  `/etc/ssl/certs`, `/usr/share/zoneinfo`, `/etc/localtime`, `/usr/share/
  icons`, `/usr/share/X11/xkb` (Wayland keymap arrives from compositor,
  not needed): source of truth is the boot FAT volume (not audited whether
  the image installs them). `getpwuid_r(1000)` failing sets `HOME` fallback
  `/tmp`; fontconfig without config prints warnings but Skia then has *no
  system fonts* (all text invisible) unless Chrome's bundled fonts are used.
  Verify the image ships a minimal fontconfig + one TTF and `resolv.conf`
  (netd DNS path). FAT is case-insensitive and has no symlinks: any
  package symlink (`libfoo.so -> libfoo.so.1`) must be materialized at import
  (`tools/agent/import-chrome.py`).
- S11. `sched_getaffinity` exists, but `/sys/devices/system/cpu/online` is the
  primary source in glibc `get_nprocs`; if ENOENT glibc falls back to
  `/proc/stat` (also ENOENT) then `sched_getaffinity`: OK but slow
  (several failed opens per call, each a vfsd IPC).

## Verification still needed before trusting this document
1. A single `strace -f -tt` of the bundled Chrome on a Linux host to confirm
   the shm path (memfd vs `/dev/shm` + `/proc/self/fd` reopen), Mojo
   `socketpair` type and the exact `/proc` files read before first paint.
2. `vfsd linux_unlinkat` `AT_REMOVEDIR`, `fstat` of local fds (S9), `MSG_CMSG_
   CLOEXEC` for stream unix (S4) were not read line by line.
3. No guest probe issues Chrome's real call shapes for any item above
   (see `docs/ai/session-handoff.md` "Chrome startup blockers 1-5").

## Suggested implementation order
M2 (cheap time/sysinfo/rlimit/readv/pwrite) -> M3 `maps/status/stat/cmdline/fd`
-> M5 SEQPACKET -> M1 tmpfs (+rename/symlink/fsync/chmod/fallocate in M2)
-> M4 -> M6 -> M7 -> M8 catalog env/flags -> S2, S8, S9 -> S3, S5, S1.
