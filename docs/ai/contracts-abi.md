# AI Contracts — Kernel/Service ABI

IPC service IDs, broker syscalls, handle transfer, and service routing. For package/stage/build/logging: `contracts-infra.md`.

## Kernel/Userspace ABI Surface

- Shared ABI crate: `libs/rustos-user-abi`.
- Kernel re-export: `kernel/ps/src/user/{abi,handles,sysops}.rs`. `kernel/compat` re-exports through `kernel_ps::api`; no shadow ABI/handle/user-memory sysop files.
- Device/console/UI `repr(C)` structs and ioctl numbers must live in `rustos-user-abi`. Services (`uiserver`, `runtimed`) consume that crate — never duplicate request structs or ioctl encoding.
- Evacuation policy, ring0/ring3 boundary, service ownership: live source
  `RING3-MIGRATION-REFERENCE` markers plus `cargo xtask ring3-inventory`.
- `RING3-MIGRATION-REFERENCE` / `RING3-MIGRATION-COMMENTED-OUT` blocks are references for migration, not dormant code to revive. Do not fix breakage by uncommenting them unless the exact lines are the remaining ring0 substrate.
- For each slice, move policy/state/lifecycle behavior into the owning service, leave only narrow ring0 fd-table/user-copy/page-table/privileged-device substrate, then delete or bypass the reference block.

## Boot Initial Task

`rootd` (`services/rootd/rootd.elf`) is the first user process:

- Must avoid Linux libc/std dynamic runtime deps.
- Spawns `syscalld`, `vfsd`, `loaderd`, `procd`, then hands off to `services/initd/initd.elf`.
- Kernel boot code must not grow generic POSIX compat exceptions for `initd`; early bootstrap surface stays narrow, explicit, tied to `rootd` bringing up foundational policy services.
- Stays resident as `IPC_SERVICE_ROOTD`; serves `ROOTD_IPC_OP_STATUS`, `ROOTD_IPC_OP_LEASE_LIST`; tracks `CoreServiceLeaseWire` via `SYS_RUSTOS_LIFECYCLE_DRAIN_BROKER`.

## IPC Service Registry

Endpoints registered via `SYS_RUSTOS_IPC_REGISTER_SERVICE_ENDPOINT`, looked up by stable `IPC_SERVICE_*` id. Registering endpoint `0` revokes; later lookups fail closed.

| ID | Service | Capability (`IPC_SERVICE_CAP_*`) | Owns |
|----|---------|----------------------------------|------|
| 1 | `syscalld` | `LINUX_SYSCALL_POLICY` | Linux cold validation, credentials, rlimits, clock/random/MM policy, Win32 syscalls |
| 2 | `vfsd` | `VFS_POLICY` | File namespace, cwd, dir/file cursors, mount/umount, metadata |
| 3 | `netd` | `NET_POLICY` | Socket namespace + all socket syscalls (AF_UNIX too) |
| 4 | `devmgrd` | `DEVICE_POLICY` | `/dev` registry, device open, ioctl auth |
| 5 | `driverd` | `DRIVER_POLICY` | Registry parsing, provider order, autoload |
| 6 | `loaderd` | `PROCESS_LOADER` | ELF/PE image policy, mapping, launch |
| 7 | `storaged` | `STORAGE_POLICY` | Block inventory after registration |
| 8 | `inputd` | `INPUT_POLICY` | Input ingest, read payload |
| 9 | `procd` | `PROCESS_POLICY` | exec/fork/wait/signal |
| 10 | `rootd` | `ROOT_SUPERVISOR` | Core-service leases, restart budgets |
| 11 | `sessiond` (reserved) | `SESSION_POLICY` | Console/TTY/session |
| 12 | `pagerd` (reserved) | `PAGER_POLICY` | Backing/page-cache |
| 13 | service-driver (reserved) | `SERVICE_DRIVER_POLICY` | Non-`.ko` driver coord |

Broker authorization checks the caller's registered service capability — **not** its executable path.

Kernel data paths (FD/socket/module/process/storage/input) remain narrow gated broker primitives for privileged DMA/MMIO/IRQ/module-load/user-copy/address-space mutation. Moving drivers to isolated ring-3 domains is **intentionally excluded** for Linux/Windows commercial compat.

## Handle Transfer

- Bounded cap-transfer via `kernel_ipc_runtime::api::KernelTransferredHandle` + `*_with_handles` endpoint APIs.
- Byte-only recv/take wrappers must fail with `BufferTooSmall` when a queued message contains transferred handles. Older paths must never silently drop capabilities.
- Transferred handles require nonzero transfer ticket + `HandleRights::allows_transfer()` true.
- Supervisor services polling independent brokers must use `SYS_RUSTOS_IPC_TRY_RECV`, not blocking `SYS_RUSTOS_IPC_RECV`.
- FD-table transfer goes through `kernel_ps::api::TransferredHandleEntry` + `HandleTable::{duplicate_for_transfer, install_transferred}`. Source class + rights must permit descriptor transfer; directory FDs are file capabilities and transferable for VFS migration.
- Userspace handle-aware IPC: `SYS_RUSTOS_IPC_{CALL,RECV,REPLY}_WITH_HANDLES` with `Ipc*WithHandlesArgs`. Send handles = Linux fd arrays; received handles install into receiver fd table and return as `i32` fd arrays + `u16` count. `recv_fd_count_ptr` mandatory even when no handles returned. Counts bounded by `IPC_MAX_TRANSFER_HANDLES`.

## VFS Surface (`vfsd`)

- Protocol: `VfsIpcRequest`/`VfsIpcResponse` (separate from `LinuxSyscallOffloadRequest`) for service-owned handles + chunked I/O.
- Kernel fd tables mirror service-owned objects as `KernelHandle::RemoteVfs`.
- Linux `openat` installs `KernelHandle::RemoteVfs` for regular files + directories after vfsd registration.
- Linux `close`/`dup`/`dup2`/`dup3`/`fcntl`/`getdents64` route through vfsd before app fd-table mutation. vfsd is the only intended caller of `SYS_RUSTOS_FD_*_BROKER`; gated by `VFS_POLICY`. Generic apps must not call directly.
- `LinuxSyscallOffloadRequest.arg0..arg3` carry 64-bit fd-control args (target fd, cmd, arg, flags). **Do not pack pointer/flag values into the 32-bit `mask` field.**
- `mount`/`umount2` route to vfsd → gated `SYS_RUSTOS_VFS_*_BROKER` for kernel mount-table mutation. Do not reintroduce direct generic-app `linux_ops::mount`/`umount2` paths.
- `poll`/`ppoll`/`epoll_*` route readiness policy and epoll interest state through `VFS_IPC_OP_POLL_QUERY`. Ring0 keeps fd-table validation, epoll token handles, user-copy, and bounded timeout sleeping.
- Legacy `SYS_RUSTOS_{STATX,STAT,READLINK,ACCESS,GETCWD,CHDIR}_METADATA`: no generic-app VFS policy in ring0 after vfsd registers. Pre-vfsd bootstrap + registered policy-service callers retain direct kernel metadata access.
- `SYS_RUSTOS_BLOCK_BROKER`: narrow boot-volume read broker, gated by `VFS_POLICY`, accepts `RustosBlockBrokerArgs`. Does not depend on `storaged`.

## Storage Surface (`storaged`)

- Gated `SYS_RUSTOS_STORAGE_LIST_BROKER` (gated by `STORAGE_POLICY`) enumerates kernel-discovered descriptors; no direct generic-app storage probing.
- `StoragedRequest`/`Response` exposes `STORAGED_OP_ROOT_STATUS`, `STORAGED_OP_BOOT_EXTENT_LOOKUP`.
- Boot extent leases are storaged policy, sourced from `system/registry/kernel/root-file-extents.tsv` and returned over `STORAGED_OP_BOOT_EXTENT_LOOKUP`. Do not reintroduce generic ring0 boot-extent policy; ring0 storage brokers remain descriptor/block substrate only.

## Input Surface (`inputd`)

- Linux input reads call `InputdIpcRequest` with `INPUTD_IPC_OP_READ`.
- inputd drains raw reports via gated `SYS_RUSTOS_INPUT_INGEST_BROKER`, chooses native/evdev bytes, returns bounded `InputdReadResponse` capped at 32 KiB.
- Ring0 performs only current-process user-copy of service-returned bytes.
- `INPUTD_IPC_OP_AUTHORIZE_READ` + `SYS_RUSTOS_INPUT_STATS_BROKER` remain compat/observability surfaces while remaining event queue is evacuated.

## Device Surface (`devmgrd`)

- `/dev` registry exposed to vfsd via `DevmgrdIpcRequest`/`Response`: `DEVMGRD_IPC_OP_LOOKUP`, `DEVMGRD_IPC_OP_READDIR`.
- vfsd may mirror only the explicit pre-devmgrd bootstrap nodes: `console0`, `display0`, `input0`, `input/event0`, `dri/card0`. **Do not reintroduce wildcard `/dev/*` success path.**
- Explicit device paths route through `devmgrd` `DEVMGRD_IPC_OP_OPEN` → `SYS_RUSTOS_DEVICE_OPEN_BROKER` → transferred device fd in reply.
- Device-open uses `DevmgrdDeviceOpenRequest`/`Response` with typed `DeviceId + access + rights`. Broker must install fd with the **exact reduced `DeviceHandleRights` chosen by devmgrd**, not default native-device rights. Broker must not infer policy from paths.
- Policy-sensitive `ioctl` routes through devmgrd → `SYS_RUSTOS_DEVICE_IOCTL_BROKER` (gated by `DEVICE_POLICY`). Direct ioctl fallback allowed only pre-devmgrd. Hot data-path ioctls (display present) may stay direct broker calls to avoid per-frame policy IPC.

## Driver Surface (`driverd`)

- Gated brokers: `SYS_RUSTOS_DRIVER_LOAD_MODULE_BROKER`, `SYS_RUSTOS_DRIVER_PROBE_ALIAS_BROKER`. Kernel side only loads an explicit module image or probes hardware aliases.
- Provider-group active state, fallback ordering, virtio display preferred scanout = **driverd state, not a ring0 broker**.
- Early boot may use legacy kernel registry path until driver-service bootstrap owns display/input/network bring-up.

## Process Loader Surface (`loaderd` + `procd`)

- Runtime launches route through `loaderd` (`IPC_SERVICE_LOADERD`), not direct `SYS_RUSTOS_SPAWN_EXEC`.
- `SYS_RUSTOS_PROC_*_BROKER` calls fail with `EACCES` unless caller owns `PROCESS_LOADER`.
- `SYS_RUSTOS_SPAWN_EXEC` restricted to `rootd` spawning the fixed bootstrap allowlist (`syscalld`, `vfsd`, `loaderd`, `procd`, `initd`). Fails closed for `initd`, generic apps, broad service restarts. rootd may use direct spawn only during fixed bootstrap + `loaderd` recovery; post-bootstrap restarts of other leases must call loaderd.
- Linux `execve` → `procd` (target auth) → `loaderd` (image materialization). If loader materialization fails, procd must cancel the exec ticket via `SYS_RUSTOS_PROC_CANCEL_EXEC_BROKER` before replying.
- **Do not move** executable-format, import/export, or DLL namespace policy back into the kernel.

### Process Broker Session

Start: `SYS_RUSTOS_PROC_PREPARE_BROKER` with `PROC_BROKER_ABI_VERSION` + explicit format (`PROC_BROKER_FORMAT_ELF64` or `PROC_BROKER_FORMAT_PE64`). Returned `prepare_handle` is owned by the loader process; supply to `SYS_RUSTOS_PROC_COMMIT_BROKER` or `_ABORT_BROKER`.

Mapping ops use `PROC_BROKER_MAP_{READ,WRITE,EXEC,PRIVATE}` flags and record non-overlapping page-aligned mappings:

- `SYS_RUSTOS_PROC_MAP_ZEROED_BROKER`
- `SYS_RUSTOS_PROC_MAP_DATA_BROKER`
- `SYS_RUSTOS_PROC_MAP_FILE_BROKER` + batch variant `_BATCH_BROKER` — **fd/cap-backed only**. Kernel resolves fd to pinned `KernelHandle` at registration; no path re-open at commit. Backing must be file-kind (`VfsFile`, `RemoteVfs(File)`, `Memfd`); directory/device/socket fd rejected with `EINVAL`/`EACCES`.

**ELF:** loaderd emits `PT_LOAD` mappings for main image + `PT_INTERP` via `MAP_FILE_BROKER`. Static-PIE biases: main = `PROC_BROKER_USER_SPACE_BASE + 0x0040_0000`, interpreter = `+ 0x0200_0000`. Must use `SYS_RUSTOS_PROC_SET_LINUX_RUNTIME_BROKER` for minimal launch metadata (entry, phdr, phnum, phent, brk_start, interpreter_base) — **not** raw blob streaming via `SET_IMAGE_BLOB_BROKER`. Kernel derives launch state from this metadata and the pre-built address space.

**PE64:** PE validation, section materialization, base relocation, import/export resolution, staged system-DLL registry lookup, PEB/TEB/runtime blob construction all happen in loaderd before commit. PE64 commit includes `SYS_RUSTOS_PROC_SET_WINDOWS_RUNTIME_BROKER` after all `MAP_DATA_BROKER` ops and before `COMMIT_BROKER`. Kernel validates metadata + spawns the materialized address space but **must not** reintroduce PE import/export/system-DLL policy.

Commit (`COMMIT_BROKER`) builds the child address space from recorded mappings.

## Network Surface (`netd`)

Routed Linux ops after bootstrap: `socket`, `socketpair`, socket `dup`/`dup2`/`dup3`, socket `close`, socket `read`/`write`/`writev`, `bind`, `listen`, `accept`/`accept4`, `connect`, `sendto`, `recvfrom`, `sendmsg`, `recvmsg`, `getsockname`, `getpeername`, `setsockopt`, `getsockopt`, `shutdown`.

netd invokes gated `SYS_RUSTOS_NET_BROKER` with target pid. Net broker arg struct carries six 64-bit syscall arg slots. Kernel performs handle install and target user-memory validation/copy; AF_UNIX socket lifecycle, binding/listen queues, byte queues, and socket option policy belong to netd. `NetdIpcRequest`/`Response` carry `socket_token`, fd `status_flags`, and a bounded inline payload for this service-owned socket path.

## Process Policy Surface (`procd`)

procd owns Linux `execve`, `fork`/`clone`, `wait4`, `rt_sigaction`, `rt_sigprocmask`, `sigaltstack`, `tgkill`, signal selection.

`wait4` routes through procd for ownership validation; kernel still performs narrow process-table wait + status/rusage copyout.

Kernel keeps only: user-copy, address-space replacement, scheduler mutation, pending-signal wakeup, Linux x86_64 `rt_sigframe`/`rt_sigreturn`.

## Syscalld Residual Surface

- Per-process credentials + `RLIMIT_STACK` policy DB: source of truth for Linux-visible `get*id`, `set*id`, `prlimit64`. Kernel process credentials = gated bootstrap/security primitive; **must not** be mutated by Linux `set*id`.
- Linux `memfd_create`: policy validation in syscalld; kernel performs handle install + read/write/truncate/seal (current handles, user memory).
- Windows syscall policy: `Win32SyscallOffloadRequest`/`Response` + `SYSCALL_OFFLOAD_OP_WIN32_*` range. Kernel dispatcher calls service policy first, then performs only the narrow privileged action.

## Commercial-Max Protocol

`rustos-user-abi::syscall::CommercialMaxProtocol*` reserves versioned protocol/op ids for rootd, procd, loaderd, syscalld, vfsd, devmgrd, inputd, storaged, netd, driverd, sessiond, pagerd, service-driver, and capability-lease work.

**Shared ABI scaffolding only** — ring0 exposes new privileged actions only when a narrow broker is implemented and capability-gated.

## Display Surface

- `device::DisplayInfo.flags`: `DISPLAY_INFO_FLAG_PRIMARY_PROVIDER` distinguishes a real primary provider from GRUB/firmware framebuffers (default = early console + panic output only).
- `bootfb` is the only last-resort exception: if exposed as primary, must stay behind hardware/virtio providers and preserve `DISPLAY_INFO_FLAG_BOOT_FRAMEBUFFER`.
- Driver framebuffer registration carries explicit source flags in `drivers/libs/driver-abi::DisplayFramebufferRegistration`. **Do not infer primary ownership from framebuffer geometry or `display_info()` presence.**
- Surface present = kernel fast path: copies validated shared-surface contents into active framebuffer + queues virtio-gpu flush for bounded housekeeping. **Do not reintroduce synchronous virtio-gpu command waits into app syscall context** for normal uiserver presents.
- Native virtio-gpu scanout backing is DMA memory + must be mapped write-combining on CPU side. Latency regressions show as slow `present_ms` in uiserver. **Cache flag is level-specific:** 2 MiB PDEs use PAT bit 12, but split 4 KiB PTEs use the PTE PAT selector bit instead of carrying bit 12 into the physical-address field.
- `uiserver` partial dirty rects should stay split unless merged union is nearly as small as separate areas. Over-coalescing disjoint topbar/taskbar/window updates → large framebuffer copies + delayed input feedback.

## Scheduler

- Linux CFS-like: fixed tick, nanosecond vruntime, weighted share, bounded sleeper credit. Weights affect vruntime only — **never reprogram hardware timer**.
- Timer IRQ hitting a user-task kernel frame: set deferred reschedule; **do not preempt arbitrary kernel frames**.
- Task weights = microsecond vruntime budgets (default 100 µs). `uiserver` gets a longer render/present slice. `runtimed` must pass manifest `weight_micros` through `loaderd`; never replace with default.
- `.ko` module init runs as a user-service kernel frame. Long lock-free compat callbacks (`driver_register`, HID/USB/virtio probes) must call `cond_resched` at safe points so module init does not starve ready user tasks.
- `KernelSpinLock` must not be held across disk/filesystem/IPC/framebuffer-copy loops. Use `KernelWaitLock` or split the section; add `cond_resched` in long loops.
- Boot service order: driver/input policy services before UI launchers. `runtimed` waits on `devmgrd` endpoint before UI bootstrap.
