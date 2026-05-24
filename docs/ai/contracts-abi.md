# AI Contracts — Kernel/Service ABI

IPC service IDs, broker syscalls, handle transfer, and service routing
contracts. For package/stage/build/logging: `contracts-infra.md`.

Kernel/userspace ABI:

- Shared ABI crate: `libs/rustos-user-abi`.
- Kernel re-export surface: `kernel/ps/src/user/{abi,handles,sysops}.rs`;
  `kernel/compat` re-exports through `kernel_ps::api` instead of carrying
  shadow ABI, handle, or user-memory sysop files.
- The kernel launches `services/rootd/rootd.elf` as the first user process.
  `rootd` is the bootstrap initial task: it must avoid Linux libc/std dynamic
  runtime dependencies, start `syscalld`, `vfsd`, `loaderd`, and `procd`, then
  hand off normal service orchestration to `services/initd/initd.elf`. Kernel boot code
  should not grow generic POSIX compatibility exceptions for `initd`; any
  unavoidable early service bootstrap surface must stay narrow, explicit, and
  tied to `rootd` bringing up foundational policy services.
- Evacuation policy, ring0/ring3 boundary, and service ownership: see
  `docs/ai/ring3-evacuation.md`.
- Device, console, and UI `repr(C)` structs and ioctl numbers must be defined
  in `rustos-user-abi`; services such as `uiserver` and `runtimed` should use
  that crate rather than duplicating request structs or ioctl encoding logic.
- RustOS IPC and Linux syscall-offload ABI lives in `rustos-user-abi::syscall`.
  Service endpoints are registered through
  `SYS_RUSTOS_IPC_REGISTER_SERVICE_ENDPOINT` and looked up by stable
  `IPC_SERVICE_*` ids. Registering endpoint `0` revokes the service endpoint
  and later lookups fail closed. `syscalld` is service id 1, `vfsd` is service
  id 2, `netd` is service id 3, `devmgrd` is service id 4, `driverd` is
  service id 5, `loaderd` is service id 6, `storaged` is service id 7,
  `inputd` is service id 8, `procd` is service id 9, `rootd` supervisor is
  service id 10, `sessiond` is reserved as service id 11, `pagerd` as service
  id 12, and non-`.ko` service-driver coordination as service id 13. File/path
  Linux syscall policy should route to `vfsd`; AF_UNIX and socket control policy should route to
  `netd`; device open/ioctl policy should route to `devmgrd`; module
  autoload/provider policy belongs in `driverd`; executable format and launch
  policy belongs in `loaderd`; storage inventory policy belongs in `storaged`;
  input observability/control policy belongs in `inputd`; process/fork/wait and
  signal policy belongs in `procd`. Kernel
  FD/socket/module/process/storage/input data paths remain as gated narrow
  broker primitives for privileged DMA/MMIO/IRQ/module-load/socket/user-copy/
  address-space mutation compatibility; moving drivers to isolated ring-3 domains
  is intentionally excluded for Linux/Windows commercial compatibility.
- Registered service endpoints also carry kernel-tracked broker capability
  bits derived from their `IPC_SERVICE_*` id. Broker authorization must check
  the current process' registered service capability, not its executable path.
  Current capability constants are `IPC_SERVICE_CAP_LINUX_SYSCALL_POLICY`,
  `IPC_SERVICE_CAP_VFS_POLICY`, `IPC_SERVICE_CAP_NET_POLICY`,
  `IPC_SERVICE_CAP_DEVICE_POLICY`, `IPC_SERVICE_CAP_DRIVER_POLICY`,
  `IPC_SERVICE_CAP_PROCESS_LOADER`, `IPC_SERVICE_CAP_STORAGE_POLICY`,
  `IPC_SERVICE_CAP_INPUT_POLICY`, `IPC_SERVICE_CAP_PROCESS_POLICY`,
  `IPC_SERVICE_CAP_ROOT_SUPERVISOR`, `IPC_SERVICE_CAP_SESSION_POLICY`,
  `IPC_SERVICE_CAP_PAGER_POLICY`, and
  `IPC_SERVICE_CAP_SERVICE_DRIVER_POLICY`.
  `procd` owns process/thread namespace policy for Linux `execve`,
  process-copy `fork`/`clone`, `wait4`, `rt_sigaction`, `rt_sigprocmask`,
  `sigaltstack`, `tgkill`, and signal selection. Kernel code must keep only
  user-copy, address-space replacement, scheduler mutation, pending-signal
  wakeup, and Linux x86_64 `rt_sigframe`/`rt_sigreturn` primitives.
- Kernel IPC endpoint calls support bounded cap-transfer slots through
  `kernel_ipc_runtime::api::KernelTransferredHandle` and the
  `*_with_handles` endpoint APIs. Byte-only recv/take wrappers must keep failing
  with `BufferTooSmall` when a queued message or reply contains transferred
  handles, so older paths do not silently drop capabilities. Transferred handles
  require an explicit nonzero transfer ticket and rights whose
  `HandleRights::allows_transfer()` is true. Supervisor-style services that
  must keep polling independent brokers must use `SYS_RUSTOS_IPC_TRY_RECV`
  rather than blocking `SYS_RUSTOS_IPC_RECV` in their main ownership loop.
- Process FD tables expose transfer only through
  `kernel_ps::api::TransferredHandleEntry` and `HandleTable::{duplicate_for_transfer,
  install_transferred}`. Export requires the source handle class and rights to
  permit descriptor transfer; directory FDs are file capabilities and are
  transferable for VFS service migration.
- Userspace handle-aware IPC uses
  `SYS_RUSTOS_IPC_{CALL,RECV,REPLY}_WITH_HANDLES` with the shared
  `Ipc*WithHandlesArgs` structs. Send handles are Linux fd arrays; received
  handles are installed into the receiver fd table and returned as `i32` fd
  arrays plus a `u16` count. `recv_fd_count_ptr` is mandatory for the
  handle-aware ABI, even when no handles are returned. Counts are bounded by
  `IPC_MAX_TRANSFER_HANDLES`, and byte-only IPC syscalls must not silently carry
  or discard transferred handles.
- VFS ownership uses `VfsIpcRequest` / `VfsIpcResponse`, separate from
  `LinuxSyscallOffloadRequest`, for service-owned file handles and chunked I/O.
  `vfsd` owns the Linux-visible namespace, cwd, directory cursors, regular file
  cursors, remote file ids, and root FAT parsing. Kernel fd tables mirror these
  service-owned objects as `KernelHandle::RemoteVfs`; kernel code may still own
  bootstrapping, process/MM, user-memory copying at syscall entry, and explicit
  broker primitives.
- `SYS_RUSTOS_BLOCK_BROKER` is the narrow boot-volume read broker for `vfsd`.
  It is gated by `IPC_SERVICE_CAP_VFS_POLICY`, accepts `RustosBlockBrokerArgs`,
  and exposes only boot-volume info plus bounded read-only block reads. This
  migration does not depend on `storaged`.
- `storaged` owns block-device inventory policy after it registers
  `IPC_SERVICE_STORAGED`. It calls the gated `SYS_RUSTOS_STORAGE_LIST_BROKER`
  primitive, which is authorized only by `IPC_SERVICE_CAP_STORAGE_POLICY`, to
  enumerate kernel-discovered storage descriptors without exposing direct
  generic-app storage probing. `StoragedRequest`/`StoragedResponse` also expose
  `STORAGED_OP_ROOT_STATUS` and `STORAGED_OP_BOOT_EXTENT_LOOKUP`; the matching
  `SYS_RUSTOS_BOOT_EXTENT_BROKER` stays a gated early/bootstrap read lease
  primitive. When a path is present in the staged root-file extent registry,
  the broker must return `BootExtentLeaseWire.extents[]`, `extent_count`, and a
  nonzero `hash_or_generation`; metadata-only fallback is only for paths whose
  extents have not yet been staged.
- `inputd` owns input ingest draining and input-read payload policy after it
  registers `IPC_SERVICE_INPUTD`. Kernel Linux input reads call
  `InputdIpcRequest` with `INPUTD_IPC_OP_READ`; `inputd` drains raw reports via
  the gated `SYS_RUSTOS_INPUT_INGEST_BROKER`, chooses native or evdev bytes,
  and returns a bounded `InputdReadResponse` payload capped at 32 KiB. Ring0
  performs only current-process user-copy of the service-returned bytes.
  `INPUTD_IPC_OP_AUTHORIZE_READ` and `SYS_RUSTOS_INPUT_STATS_BROKER` remain
  compatibility/observability surfaces while the remaining event queue is being
  evacuated.
- Linux `openat` installs `KernelHandle::RemoteVfs` for regular files and
  directories after `vfsd` registration. Explicit device paths route to
  `devmgrd` `DEVMGRD_IPC_OP_OPEN`; `devmgrd` calls
  `SYS_RUSTOS_DEVICE_OPEN_BROKER` and replies with the transferred device fd.
  Policy services may still use the bootstrap VFS path to avoid recursive
  self-IPC during service startup. Before `vfsd` registers its
  endpoint, the gated bootstrap VFS broker remains available for service
  dynamic-loader bootstrap; after registration, generic Linux app VFS syscalls
  must route to `vfsd` or fail closed if `vfsd` is unavailable.
- Linux `close`, `dup`/`dup2`/`dup3`, and `fcntl` route through `vfsd` before
  mutating the app fd table. `vfsd` is the only intended caller of the
  gated narrow `SYS_RUSTOS_FD_*_BROKER` broker primitives; generic apps must
  not use them directly. `LinuxSyscallOffloadRequest.arg0..arg3` are the 64-bit extension
  slots for fd control arguments such as target fd, command, argument, and
  flags. Do not pack pointer or flag values into the 32-bit `mask` field.
- Linux `getdents64` also routes through `vfsd`; the gated narrow
  `SYS_RUSTOS_FD_GETDENTS64_BROKER` broker primitive writes directory records into the target
  process address space selected by pid. FD broker syscalls must remain gated to
  the registered `IPC_SERVICE_CAP_VFS_POLICY` owner, not broad policy-service
  callers.
- Linux `mount` and `umount2` must preserve Linux ELF compatibility while
  keeping namespace policy in `vfsd`: generic app syscalls route to `vfsd`,
  then `vfsd` calls the gated narrow `SYS_RUSTOS_VFS_*_BROKER` broker primitives for
  the narrow kernel mount-table mutation. Do not reintroduce direct generic-app
  `linux_ops::mount` or `linux_ops::umount2` paths.
- Legacy RustOS metadata syscall numbers
  `SYS_RUSTOS_{STATX,STAT,READLINK,ACCESS,GETCWD,CHDIR}_METADATA` must not
  perform direct generic-app VFS policy in ring0 after `vfsd` registers. They
  route through `vfsd` for generic callers and retain direct kernel metadata
  access only for pre-`vfsd` bootstrap and registered policy-service callers.
- Runtime launches must route through `loaderd`, not direct generic
  `SYS_RUSTOS_SPAWN_EXEC` calls. `loaderd` registers `IPC_SERVICE_LOADERD`,
  validates executable format policy, and uses the gated narrow
  `SYS_RUSTOS_PROC_*_BROKER` broker primitives for kernel-owned process commit work.
  `SYS_RUSTOS_PROC_*_BROKER` calls must fail with `EACCES` unless the caller
  owns `IPC_SERVICE_CAP_PROCESS_LOADER`. `SYS_RUSTOS_SPAWN_EXEC` is restricted
  to `rootd` spawning the fixed bootstrap service allowlist
  (`syscalld`, `vfsd`, `loaderd`, `procd`, `initd`) and must fail closed for
  `initd`, generic apps, and broad service restarts. `rootd` may use this
  direct spawn primitive only during fixed bootstrap and for `loaderd` recovery;
  post-bootstrap restarts of other leases must call `loaderd`. `rootd` stays
  resident as `IPC_SERVICE_ROOTD`, serves `ROOTD_IPC_OP_STATUS` and
  `ROOTD_IPC_OP_LEASE_LIST`, and tracks `CoreServiceLeaseWire` state from the
  gated `SYS_RUSTOS_LIFECYCLE_DRAIN_BROKER`.
- Process broker sessions start with `SYS_RUSTOS_PROC_PREPARE_BROKER` using
  `PROC_BROKER_ABI_VERSION` and an explicit executable format
  (`PROC_BROKER_FORMAT_ELF64` or `PROC_BROKER_FORMAT_PE64`). The returned
  `prepare_handle` is owned by the loader service process and must be supplied
  to `SYS_RUSTOS_PROC_COMMIT_BROKER` or `SYS_RUSTOS_PROC_ABORT_BROKER`.
  `SYS_RUSTOS_PROC_MAP_ZEROED_BROKER`, `SYS_RUSTOS_PROC_MAP_DATA_BROKER`, and
  `SYS_RUSTOS_PROC_MAP_FILE_BROKER` use
  `PROC_BROKER_MAP_{READ,WRITE,EXEC,PRIVATE}` flags and record non-overlapping
  page-aligned mappings in the prepare session.
  `SYS_RUSTOS_PROC_MAP_FILE_BROKER` and its batch variant
  `SYS_RUSTOS_PROC_MAP_FILE_BATCH_BROKER` are **fd/cap-backed only**: the
  kernel resolves the caller's fd to a pinned `KernelHandle` at registration
  time; no path re-open occurs at commit. Backing must be a file-kind handle
  (`VfsFile`, `RemoteVfs(File)`, or `Memfd`); directory, device, and socket
  descriptors are rejected with `EINVAL`/`EACCES`. `loaderd` must emit ELF
  `PT_LOAD` mappings for both the main image and its `PT_INTERP` interpreter
  via `SYS_RUSTOS_PROC_MAP_FILE_BROKER`, using the static-PIE load biases
  (`PROC_BROKER_USER_SPACE_BASE + 0x0040_0000` for the main image and
  `PROC_BROKER_USER_SPACE_BASE + 0x0200_0000` for the interpreter). The kernel
  prepare session stores loader-materialized data pages for mappings that need
  service-side fixups. Linux ELF sessions must use
  `SYS_RUSTOS_PROC_SET_LINUX_RUNTIME_BROKER` to supply minimal launch metadata
  (entry, phdr, phnum, phent, brk_start, interpreter_base) instead of
  streaming the raw binary with `SYS_RUSTOS_PROC_SET_IMAGE_BLOB_BROKER`; the
  kernel derives process launch state from this metadata and the pre-built
  address space. Commit builds the child address space from the recorded
  mappings. Windows
  PE64 policy belongs to `loaderd`: PE validation, section materialization,
  base relocation, import/export resolution, staged system-DLL registry lookup,
  and PEB/TEB/runtime blob construction happen before commit. PE64 commit must
  include `SYS_RUSTOS_PROC_SET_WINDOWS_RUNTIME_BROKER` metadata after all
  `SYS_RUSTOS_PROC_MAP_DATA_BROKER` mappings and before
  `SYS_RUSTOS_PROC_COMMIT_BROKER`; the kernel validates the metadata and
  spawns the already-materialized address space, but must not reintroduce PE
  import/export/system-DLL policy. Linux app `execve` goes through `procd` for
  target authorization and then `loaderd` for executable image materialization;
  if loader materialization fails, `procd` must cancel the exec ticket with
  `SYS_RUSTOS_PROC_CANCEL_EXEC_BROKER` before replying. Do not move
  executable-format, import/export, or DLL namespace policy back into the
  kernel.
- Policy-sensitive Linux `ioctl` requests route through `devmgrd` after
  bootstrap. `devmgrd` owns request authorization and calls
  `SYS_RUSTOS_DEVICE_IOCTL_BROKER`, which is gated by
  `IPC_SERVICE_CAP_DEVICE_POLICY` and performs the kernel-owned user
  memory/device operation against the target process id and fd. The kernel may
  use a direct ioctl fallback before `IPC_SERVICE_DEVMGRD` is registered, and
  hot data-path ioctls such as display present may remain direct broker calls
  to avoid per-frame policy IPC.
- Windows syscall policy routes through `syscalld` using
  `Win32SyscallOffloadRequest`/`Win32SyscallOffloadResponse` and the
  `SYSCALL_OFFLOAD_OP_WIN32_*` operation range. The kernel Windows dispatcher
  calls this service policy first, then performs only the narrow privileged
  action that still requires current-process user memory, handle, scheduler, or
  address-space access.
- Linux `wait4` routes process ownership validation through `procd`; the kernel
  still performs the narrow process-table wait and status/rusage copyout.
  Linux `memfd_create` route policy validation remains in `syscalld`, while the
  kernel performs handle installation and memfd read/write/truncate/seal
  actions because those operate on current process handles and user memory.
- Linux socket namespace and socket I/O operations route through `netd` after
  bootstrap. `socket`, `socketpair`, `bind`, `listen`, `accept/accept4`,
  `connect`, `sendto`, `recvfrom`, `sendmsg`, `recvmsg`, `getsockname`,
  `getpeername`, `setsockopt`, `getsockopt`, and `shutdown` call `netd`, and
  `netd` invokes the gated `SYS_RUSTOS_NET_BROKER` primitive with the target
  process id. Kernel code still performs handle installation, target
  user-memory validation/copy, and the current in-kernel socket/inet substrate;
  policy routing and namespace sequencing belong to `netd`. The net broker
  argument struct carries six 64-bit syscall argument slots for this migration
  surface.
- `driverd` owns registry parsing and provider/autoload ordering after it
  registers `IPC_SERVICE_DRIVERD`. The gated driver broker surface is
  `SYS_RUSTOS_DRIVER_LOAD_MODULE_BROKER`,
  `SYS_RUSTOS_DRIVER_PROBE_ALIAS_BROKER`; the kernel side only loads an
  explicit module image or probes hardware aliases. Provider-group active state,
  fallback ordering, and virtio display preferred scanout policy are `driverd`
  state, not a ring0 provider-policy broker. Early boot may still use the
  legacy kernel registry path until driver service bootstrap owns
  display/input/network bring-up.
- `devmgrd` owns the visible `/dev` registry protocol exposed to `vfsd` through
  `DevmgrdIpcRequest`/`DevmgrdIpcResponse` with `DEVMGRD_IPC_OP_LOOKUP` and
  `DEVMGRD_IPC_OP_READDIR`. `vfsd` may mirror explicit nodes (`console0`,
  `display0`, `input0`, `input/event0`, `dri/card0`) only as a pre-devmgrd
  bootstrap fallback; do not reintroduce a wildcard `/dev/*` success path.
  Device-open policy uses `DevmgrdDeviceOpenRequest` /
  `DevmgrdDeviceOpenResponse`; the kernel broker accepts only typed
  `DeviceId + access + rights` and must not infer policy from paths. The
  broker must install the returned device fd with the exact reduced
  `DeviceHandleRights` chosen by `devmgrd`, not the default native-device
  rights.
- Commercial-max protocol prework lives in
  `rustos-user-abi::syscall::CommercialMaxProtocol*`. It reserves versioned
  protocol ids and op ids for rootd supervisor, procd, loaderd, syscalld, vfsd,
  devmgrd, inputd, storaged, netd, driverd, sessiond, pagerd, service-driver,
  and capability-lease work. These structs are shared ABI scaffolding only;
  ring0 still exposes new privileged actions only when a narrow broker is
  implemented and capability-gated.
- `syscalld` keeps the service-side Linux policy DB for per-process credentials
  and `RLIMIT_STACK`. Linux-visible `get*id`, `set*id`, and `prlimit64` policy
  must be sourced from `syscalld`; kernel process credentials are a gated
  bootstrap/security primitive and must not be mutated by Linux `set*id`.
- `device::DisplayInfo.flags` distinguishes boot firmware framebuffers from a
  real primary display provider. Userspace display surfaces must require
  `DISPLAY_INFO_FLAG_PRIMARY_PROVIDER`; GRUB/firmware boot framebuffers are for
  early console and panic output by default. The `bootfb` driver is the only
  last-resort exception; if it exposes a firmware framebuffer as a primary
  provider, it must stay behind hardware/virtio providers and preserve
  `DISPLAY_INFO_FLAG_BOOT_FRAMEBUFFER`.
- Driver framebuffer registration also carries explicit source flags in
  `drivers/libs/driver-abi::DisplayFramebufferRegistration`; do not infer
  primary display ownership from framebuffer geometry or from `display_info()`
  being present.
- Display surface present is a kernel fast path: it copies validated shared
  surface contents into the active framebuffer and queues virtio-gpu flush work
  for the bounded housekeeping lower half. Do not reintroduce synchronous
  virtio-gpu command waits into app syscall context for normal uiserver
  presents.
- Native virtio-gpu scanout backing is DMA memory and must be mapped
  write-combining on the CPU side. Present latency regressions often show up as
  slow `present_ms` in `uiserver`, so check the cache mode before changing UI
  drawing code. The direct-map cache flag is level-specific: 2 MiB PDEs use PAT
  bit 12, but split 4 KiB PTEs must use the PTE PAT selector bit instead of
  carrying bit 12 into the physical address field.
- `uiserver` partial dirty rects should stay split unless the merged union is
  nearly as small as the separate areas. Over-coalescing disjoint topbar,
  taskbar, and window updates turns small changes into large framebuffer copies
  and delays input feedback.
- Task weights are microsecond vruntime budgets (default 100 µs). `uiserver`
  gets a longer render/present slice. `runtimed` must pass manifest
  `weight_micros` through `loaderd`; never replace with the default.
- Scheduler: Linux CFS-like — fixed tick, nanosecond vruntime, weighted share,
  bounded sleeper credit. Weights affect vruntime only; never reprogram the
  hardware timer. Timer IRQ hitting a user-task kernel frame: set deferred
  reschedule, do not preempt arbitrary kernel frames.
- `.ko` module init runs as a user-service kernel frame. Long lock-free compat
  callbacks (`driver_register`, HID/USB/virtio probes) must call `cond_resched`
  at safe points so module init cannot starve ready user tasks.
- `KernelSpinLock` must not be held across disk/filesystem/IPC/framebuffer-copy
  loops. Use `KernelWaitLock` or split the section; add `cond_resched` in long
  loops.
- Boot service order: driver/input policy services before UI launchers.
  `runtimed` waits on `devmgrd` endpoint before UI bootstrap.
