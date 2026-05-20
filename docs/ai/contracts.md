# AI Contracts

Package manifest:

- File name: `RUSTOS.package.toml`.
- Parser: `tools/xtask/src/package_manifest.rs`.
- Package ids are stable dependency keys.
- `runtime_deps` references package `id`, not path or desktop id.
- Valid `kind`: `boot`, `kernel`, `bridge-driver`, `user-driver`, `service`, `app`, `compat`.
- Valid `execution_domain`: `kernel`, `user`.
- Valid `startup`: `none`, `init`, `session`, `desktop`.
- Valid `install.layout`: `file`, `directory`.
- Valid `desktop.entries.launch`: `none`, `new-session`, `all-sessions`.
- Driver `[autoload]` supports `deps` for required module preloads and
  `softdeps` for best-effort preloads, matching Linux `modules.dep` and
  `modprobe.d softdep pre:` roles.
- Driver autoload list fields are staged as comma-separated registry values;
  individual list items must not contain tab, newline, carriage return, or
  comma.
- Driver `[autoload].linux_driver_names` lists Linux in-module driver names
  allowed to register from that package. If omitted, staging defaults it to the
  autoload `name`. Linux driver compat registration must use this registry
  field rather than hardcoded driver names.
- Driver `[autoload].provider_group` is a mutually exclusive provider contract.
  Once a loadable or native provider marks the group active, later candidates in
  the same group are skipped. `fallback_only` candidates are ordered after normal
  candidates and should be used only as lower-priority substitutes.
- In the `display-primary` provider group, real hardware/virtio display
  providers must be ordered ahead of firmware framebuffer fallbacks. `bootfb`
  is a last-resort fallback, not the default primary for QEMU or hardware GPUs.
- `driverd` owns driver autoload policy. Kernel driver brokers may expose
  narrow hardware-presence primitives for staged aliases such as
  `platform:bootfb`, `pci:*`, and `virtio:*`, but they must not pick provider
  order or bypass registry `provider_group` policy.

Stage outputs:

- Boot image root: `build/image`.
- Artifact root: `build/artifacts`.
- Static overlay: `assets/image`.
- UEFI entry: GRUB-generated `build/image/EFI/BOOT/BOOTX64.EFI`.
- Kernel payload signature: `build/image/nucleus.elf.sig`.
- Registries:
  - `system/registry/kernel/loadable-drivers.tsv`
  - `system/registry/system/desktop-programs.tsv`
  - `system/registry/system/runtime-launch-programs.tsv`
  - `system/registry/system/startup-programs.tsv`
  - `system/registry/system/linux-runtime-access.tsv`
  - `system/registry/system/runtime-env.tsv`
  - `system/registry/compat/windows-system-dlls.txt`
- Linux runtime filesystem access is staged into
  `system/registry/system/linux-runtime-access.tsv` as `kind=dir|file` and
  `path=/absolute/path` fields. Kernel VFS should consume that registry instead
  of carrying hardcoded default runtime path allowlists.
- Userspace default environment policy is staged into
  `system/registry/system/runtime-env.tsv` as `scope=init|runtime`, `key=NAME`,
  and `value=VALUE` fields. `initd` and `runtimed` should consume this registry
  instead of hardcoding default `PATH`, `HOME`, `XDG_*`, or display env values.

Runtime control:

- Client crate: `libs/runtime-control`.
- Default socket: `/run/runtimed.sock`.
- Main methods: `snapshot_running_programs`, `request_launch_program_new_session`, `request_terminate_session`, `request_terminate_pid`, `notify_ui_ready`.
- Request text max: `MAX_REQUEST_PATH_BYTES`.

Kernel/userspace ABI:

- Shared ABI crate: `libs/rustos-user-abi`.
- Kernel re-export surfaces: `kernel/ps/src/user/abi.rs` and
  `kernel/compat/src/user/abi.rs`.
- The kernel launches `services/rootd/rootd.elf` as the first user process.
  `rootd` is the bootstrap initial task: it must avoid Linux libc/std dynamic
  runtime dependencies, start `syscalld`, `vfsd`, `loaderd`, and `procd`, then
  hand off normal service orchestration to `services/initd/initd.elf`. Kernel boot code
  should not grow generic POSIX compatibility exceptions for `initd`; any
  unavoidable early service bootstrap surface must stay narrow, explicit, and
  tied to `rootd` bringing up foundational policy services.
- Current refactor phase is service-first ring0 evacuation. The old
  line-commented Linux compat reference files have been consumed and removed
  from the kernel tree. Linux MM ABI policy for `brk`, `mmap`, `mprotect`, and
  `munmap` belongs to `syscalld`; kernel MM keeps only the gated
  `SYS_RUSTOS_MM_BROKER` primitive for target address-space PTE mutation, fd
  backing revalidation, shared-frame lifetime holds, and display-surface
  mapping. Linux thread/fork/exec/signal policy and Windows PE/Win32 policy now
  have live service-first implementations through `loaderd`, `procd`,
  `syscalld`, and narrow kernel brokers; process and signal policy belongs to
  `procd`, executable image policy belongs to `loaderd`, and hot time syscalls
  such as `clock_gettime`, `nanosleep`, and `clock_nanosleep` stay kernel-direct
  unless a non-recursive broker is introduced. Do not evacuate a syscall by
  routing it to a policy service that immediately reissues the same Linux
  syscall. io-manager VFS/network/USB/input/provider policy belongs to
  the user services. Do not restore deleted or commented ring0 policy modules
  for quick compatibility fixes. Extend service implementations and keep
  kernel code to deliberate privileged primitives such as syscall entry,
  address-space mutation, scheduler state, user-memory copy, interrupt/MMIO
  access, DMA, or module-load substrate. Compile and QEMU verification can be
  deferred for tasks whose explicit goal is structural evacuation rather than
  bootability.
- The target architecture is a hybrid kernel. Services own routing and policy,
  but ring0 intentionally retains compatibility-critical mechanisms:
  syscall/trap entry, gated syscall brokers, address-space mutation,
  scheduler/task mutation, user-copy, IRQ/MMIO/DMA/IOMMU access, and
  kernel-address-space `.ko` module execution. Linux `.ko` modules must remain
  ring0 for commercial driver compatibility: the kernel module ABI assumes
  direct access to kernel symbols, IRQ state, DMA/MMIO mappings, Linux-style
  callback context, and shared in-kernel driver data structures. Do not
  describe the system as a pure microkernel, and do not move `.ko` execution or
  syscall broker primitives out of the kernel without a replacement that
  preserves native Linux ELF, Windows PE, and commercial driver behavior.
  Move the policy around those mechanisms to services instead; see
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
  `SYS_RUSTOS_DRIVER_PROBE_ALIAS_BROKER`, and
  `SYS_RUSTOS_DRIVER_PROVIDER_ACTIVE_BROKER`; the kernel side only loads an
  explicit module image, probes hardware aliases, or reports active provider
  groups for final safety checks. Early boot may still use the legacy kernel
  registry path until driver service bootstrap owns display/input/network
  bring-up.
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
- User task weights are scheduler budgets in microseconds, not priorities.
  The default runtime budget is 100 us; do not stage services/apps with 50 us
  defaults unless a benchmark proves the interrupt/context-switch cost is
  acceptable. Input/display responsiveness should come from bounded lower-half
  work and explicit service readiness, not from 20 kHz user-task quanta.
- CPU-bound display owners may have explicit larger budgets: `uiserver` gets a
  longer render/present slice so large framebuffer copies are not preempted
  every 100 us, and `runtimed` must pass manifest `weight_micros` through
  `loaderd` instead of replacing it with the default. Do not raise early
  broker/driver service budgets without a boot-time probe, because startup
  concurrency can regress perceived boot time.
- Scheduler changes must follow Linux scheduler invariants rather than ad hoc
  interactivity heuristics. Timer interrupts that hit a user task while it is
  executing a kernel frame should set a deferred reschedule request, and long
  lock-free kernel paths must call the RustOS `cond_resched` equivalent at
  safe points. Do not preempt arbitrary kernel frames from the interrupt
  handler unless the path has explicit preemption-count/lock-state safety.
- Ring0 Linux `.ko` module init runs inside the calling user service's kernel
  frame. Linux compat exports such as `schedule`, `schedule_timeout`,
  `cond_resched`/`_cond_resched`/`__cond_resched`, and long callback bridges
  (`driver_register`/HID bind/probe/open/report parse, USB register/URB/control
  callbacks, virtio probe loops, and similar lock-free callback chains) must
  provide explicit RustOS
  `cond_resched` safe points so `driverd` module init cannot starve `uiserver`,
  `wayclick`, or other ready user tasks until module init returns.
- The default RustOS fair scheduler should stay Linux CFS-like: fixed periodic
  scheduler tick, per-task nanosecond vruntime accounting, weighted CPU share,
  smallest-vruntime pick, bounded sleeper credit, and an idle/root class that
  is excluded from fair competition once bootstrap finalize has ended.
  Per-task weights must not reprogram the hardware timer; weights affect
  vruntime only.
- Borrow seL4/Windows scheduler rules only for the roles they prove well:
  seL4-style IPC handoff may donate a bounded next-pick vruntime credit to the
  receiver/replier, and Windows-style
  time-critical priority must be brief and mostly blocked. Do not turn these
  into broad permanent real-time boosts for normal services or apps.
- Kernel code must not hold `KernelSpinLock` across disk, filesystem, IPC, or
  framebuffer-sized copy loops. Use `KernelWaitLock` or split the critical
  section, then add safe `cond_resched` points in long loops. This mirrors the
  Linux rule that spinlocked/atomic regions are not sleeping preemption
  points.
- Init-time user services are ordered for perceived boot readiness: driver and
  input policy services should start before runtime UI launchers. `runtimed`
  depends on `netd` for its runtime control socket and waits for the `devmgrd`
  endpoint before UI bootstrap because display ioctl policy routes there.
  After `runtimed` is launched, defer secondary storage policy briefly so it
  does not contend with display bring-up.

Kernel API:

- Prefer `kernel/*/src/api.rs` public wrappers over private subsystem modules.
- Main API surfaces:
  - `kernel/hal/src/api.rs`
  - `kernel/mm/src/api.rs`
  - `kernel/object/src/api.rs`
  - `kernel/ipc-runtime/src/api.rs`
  - `kernel/ps/src/api.rs`
  - `kernel/io-manager/src/api.rs`
  - `kernel/compat/src/api.rs`
- Kernel entry boot ordering lives in `kernel/src/main.rs`.
- Human reference: `docs/api/kernel.md`.
- Boot order: disable interrupts -> boot trace init -> GDT -> IDT -> paging -> higher-half jump -> stack switch -> executive bootstrap.
- Cross-crate rule: import `kernel_x::api as x_api`; do not reach into another crate's private modules when `api.rs` exposes a wrapper.
- User-memory IO APIs belong in syscall/process-context-aware paths only.
- Scheduler-aware wait users should use `kernel_ps::api::{current_task_id,
  block_current_task, wake_task}`. The `current_user_id`,
  `block_current_user_task`, and `wake_user_task` wrappers remain userspace-task
  helpers, not general kernel wait primitives.

Kernel build:

- Kernel-target Cargo invocations route through
  `tools/xtask/src/build/cargo.rs::kernel_rustflags_env`.
- RustOS operational config lives in `config/rustos.toml`.
- Kernel build-shape defaults live under `[kernel.build]`; set
  `KERNEL_BUILD_CONFIG` to test an alternate TOML file.
- Default kernel `RUSTFLAGS`: `--cfg rustos_boot_image`, `-C no-redzone`,
  `-C codegen-units=1`, `-C opt-level=2`, `-C overflow-checks=true`,
  `-C debug-assertions=false`, `-C debuginfo=0`, `-C panic=abort`.
- `RUSTOS_KERNEL_CODEGEN_UNITS` overrides the kernel codegen unit count for
  experiments without changing userspace builds. `KERNEL_CODEGEN_UNITS` remains
  a deprecated alias. The supported sweep range is `1..=256`.
- Kernel build-shape knobs also include `lto`, `force_frame_pointers`,
  `incremental`, `debuginfo`, `embed_bitcode`, `panic`, `relocation_model`,
  `strip`, and `extra_rustflags`. `incremental` is applied as
  `CARGO_INCREMENTAL` on kernel Cargo invocations.
- Kernel Cargo invocations disable a configured `sccache` rustc wrapper because
  kernel build-std/LTO flag probes are not accepted by sccache.
- `embed_bitcode=true` is required whenever `lto` is `thin` or `fat`.
- `cargo xtask config check` validates effective config; `cargo xtask config
  show` prints the effective kernel build config.
- Driver module loading must be stable across the supported
  `codegen_units=1..=256` and `opt_level=0..=3` sweep. Loader policy may ignore
  relocation sections that target non-loaded or non-ALLOC debug sections, but
  loaded text/data relocations must still resolve through explicit ABI surfaces.
- Linux compat load failures should write the first disallowed or unresolved
  external symbol to debugcon directly. Do not rely only on category-filtered
  logs for module ABI diagnostics.

Fault injection:

- Human guide: `docs/fault-injection.md`.
- Shared parser crate: `libs/rustos-fault-injection`.
- Host config parser: `tools/xtask/src/config/project.rs` `[fault_injection]`.
- QEMU handoff: `tools/xtask/src/qemu/mod.rs` passes rules through fw_cfg
  `opt/rustos/fault-injection`.
- Kernel runtime: `kernel/nucleus-core/src/util/fault_injection.rs`.
- Kernel init: `kernel/executive/src/boot.rs` calls
  `fault_injection::init_from_qemu_fw_cfg()` after heap init.
- Rule format: `location=action`.
- Valid actions: `off`, `fail`, `drop-every:N`, `fail-after:N`, `rate:N`,
  `delay-ms:N`. `delay-ms` is parsed but not wired to sleep/delay yet.
- Current fault points: `alloc.frame`, `block.read`, `block.write`,
  `display.present`, `display.provider.register`, `driver.module.load`,
  `input.event.enqueue`, `pci.config.read`, `process.spawn`, `socket.recv`,
  `socket.send`, `virtio-gpu.control.submit`.
- Add new points only at realistic failure boundaries such as allocation, block
  IO, device registration, queue submit, process spawn, IPC/socket send/recv,
  and driver probe/load. Do not scatter fault checks through arbitrary helper
  functions.
- `config/rustos.toml` may use normal TOML formatting for fault rules,
  including multiline arrays; logging extraction must ignore non-logging
  sections.

Linux network and driver compat:

- During the service-first evacuation, network namespace and socket policy live
  in `netd`, module/provider policy lives in `driverd`, and the kernel
  `kernel/io-manager/src/driver/mod.rs` surface is limited to privileged DMA,
  IRQ, IOMMU, and MMIO primitives plus explicit module-load substrate hooks.
- `driverd` selects autoload/provider policy from staged registries and calls
  the gated driver broker. The kernel broker owns only the privileged module
  loader and driver ABI substrate: validating `.ko` images, relocating them,
  exposing `DriverKernelApiV1`, mapping MMIO, and executing module init in the
  kernel address space when the module ABI requires it. Do not move Linux `.ko`
  execution to ring3 as a generic goal; ring3 work should remove surrounding
  policy, namespace, queue, and provider decisions from the kernel while
  leaving commercial-driver ABI execution in ring0.
- Deleted io-manager VFS/network/USB/input/provider policy files are not source
  of truth. Do not restore them to make `.ko` loading or Linux socket behavior
  appear to work; add service-owned policy and narrow privileged brokers.
- Linux compat symbols must be explicitly implemented. Do not add broad
  prefix-based `0`, `NULL`, or no-op fallbacks to make a module load; unresolved
  required externals should fail module load.
- Optional Linux net features such as XDP, BPF, AF_XDP, failover, ethtool
  offloads, DIM, and trace/static-call hooks may be represented by explicit
  per-symbol disabled/no-op shims only when RustOS does not advertise that
  capability. These shims must fail closed or return disabled state; they must
  not fabricate packets, carrier, queue progress, or successful offload.

Logging:

- Policy file: `config/rustos.toml` `[logging]`.
- Parser/cfg emitter: `tools/build_log_cfg.rs`.
- Canonical categories: `libs/rustos-observability/src/lib.rs`.
- Config is mostly build-time cfg; rebuild after changes.
- Kernel macros: `crate::debug::{trace,debug,info,warn,error}`.
- Userspace macros: `observability_client::{trace,debug,info,warn,error}`.

Docs:

- Human docs are bilingual; English first.
- Human docs must have language jump links.
- AI docs are English-only and compact.
- mdBook nav source: `docs/SUMMARY.md`.
- mdBook config: `book.toml`; output under `build/mdbook`.
- Mandatory token policy: `docs/ai/token-policy.md`.

Token-saving context rules:

- Mandatory policy first; see `docs/ai/token-policy.md`.
- Task classification second; see `docs/ai/task-router.md`.
- Prefer source files named in contracts over human docs.
- For broad docs updates, inspect `docs/SUMMARY.md` and the specific target doc only.
- For code changes, inspect docs only if behavior touches a documented contract.
- If behavior changes a stable contract, update the relevant AI doc in the same change.
