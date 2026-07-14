# AI Contracts — Kernel/Service ABI

IPC service IDs, broker syscalls, handle transfer, and service routing. For package/stage/build/logging: `contracts-infra.md`.

## Kernel/Userspace ABI Surface

- Shared ABI crate: `libs/rustos-user-abi`.
- Kernel re-export: `kernel/ps/src/user/{abi,handles,sysops}.rs`. `kernel/compat` re-exports through `kernel_ps::api`; no shadow ABI/handle/user-memory sysop files.
- Device/console/UI `repr(C)` structs and ioctl numbers must live in `rustos-user-abi`. Services (`uiserver`, `runtimed`) consume that crate — never duplicate request structs or ioctl encoding.
- Evacuation policy, ring0/ring3 boundary, service ownership: live source
  `RING3-MIGRATION-REFERENCE` / `RING3-MIGRATION-COMMENTED-OUT` markers plus
  `cargo xtask ring3-inventory`.
- `RING3-MIGRATION-REFERENCE` / `RING3-MIGRATION-COMMENTED-OUT` blocks are references for migration, not dormant code to revive. Do not fix breakage by uncommenting them unless the exact lines are the remaining ring0 substrate.
- For each slice, move policy/state/lifecycle behavior into the owning service, leave only narrow ring0 fd-table/user-copy/page-table/privileged-device substrate, then delete or bypass the reference block.
- Inventory interpretation: `excluded_exception_loc` is deliberate ring0 or
  already-ring3 reference surface, `cleanup_debt_loc` is legacy code to retire
  rather than migrate, and `migration_candidate_loc` is the remaining real
  ring3 migration candidate set.

## Boot Initial Task

`rootd` (`services/rootd/rootd.elf`) is the first user process:

- Must avoid Linux libc/std dynamic runtime deps.
- Spawns `syscalld`, `vfsd`, `loaderd`, `procd`, then hands off to `services/initd/initd.elf`.
- Kernel boot code must not grow generic POSIX compat exceptions for `initd`; early bootstrap surface stays narrow, explicit, tied to `rootd` bringing up foundational policy services.
- Stays resident as `IPC_SERVICE_ROOTD`; serves `ROOTD_IPC_OP_STATUS`, `ROOTD_IPC_OP_LEASE_LIST`; tracks `CoreServiceLeaseWire` via `SYS_RUSTOS_LIFECYCLE_DRAIN_BROKER`.

## IPC Service Registry

Endpoints registered via `SYS_RUSTOS_IPC_REGISTER_SERVICE_ENDPOINT`, looked up by stable `IPC_SERVICE_*` id. Registering endpoint `0` revokes; later lookups fail closed.
Before final endpoint cleanup, a terminating process is marked exiting. Endpoint
publication, explicit revoke, and exit cleanup share one registry mutation
critical section; registration rechecks that marker inside it and fails with
`ESRCH`. This prevents concurrent registrars or an exiting process from
publishing a stale endpoint after the cleanup scan. Endpoint lookup and
capability checks also fail closed when the recorded owner is already marked
exiting, including the interval before the cleanup stores become visible.
The kernel compat service table records low-volume structured milestones for
endpoint lifecycle transitions: `ipc-service-register`,
`ipc-service-register-denied`, `ipc-service-register-busy`,
`ipc-service-revoke`, `ipc-service-revoke-denied`, and
`ipc-service-exit-revoke`. These are diagnostic state only; rootd remains the
service admission and capability policy owner. Endpoint revoke is owner-only
unless the caller holds `ROOT_SUPERVISOR`.
`SYS_RUSTOS_IPC_LOOKUP_SERVICE_ENDPOINT` is service/supervisor discovery: root-supervisor callers may use the kernel table directly; other callers are admitted through rootd `COMMERCIAL_MAX_ROOTD_OP_SERVICE_LOOKUP`. Core services and post-init services are admitted only by matching a running rootd lease. Generic apps use Linux/Win32 ABI routes and kernel compat helpers, not raw policy-service endpoint lookup.
Service capability assignment is rootd policy: after rootd self-registration, kernel compat asks `COMMERCIAL_MAX_ROOTD_OP_SERVICE_CAPABILITY` with the registering subject PID/TID and records the returned `IPC_SERVICE_CAP_*` mask only if rootd confirms the PID matches the running lease. This includes the legacy Linux-syscall endpoint registration path. Do not reintroduce a full `service_id -> capability` table in ring0.
Post-init lease reporting is ring3-owned: `initd` reports successfully spawned
policy services to rootd with `COMMERCIAL_MAX_ROOTD_OP_READINESS_SIGNAL`
(`arg0=IPC_SERVICE_*`, `arg1=pid`, `path=exec`). rootd uses that lease registry
when answering kernel compat `SERVICE_CAPABILITY` / `SERVICE_LOOKUP` requests.
Services launched by another supervisor must be reported by that supervisor
before the child depends on service endpoint registration; `runtimed` reports
the `uiserver` lease. There is no post-init capability or lookup allowlist
fallback. `READINESS_SIGNAL` is a supervisor-spawn lease admission signal, not
proof that the child has registered its own policy endpoint; endpoint readiness
is still determined by `SYS_RUSTOS_IPC_REGISTER_SERVICE_ENDPOINT` /
`SYS_RUSTOS_IPC_LOOKUP_SERVICE_ENDPOINT`. Supervised launches use the
deferred-start transaction: loaderd creates the child suspended, the supervisor
admits the exact PID lease in rootd, loaderd activates the child, and the
supervisor waits with `SYS_RUSTOS_IPC_WAIT_SERVICE_ENDPOINT`. The wait is
race-free, deadline bounded, keyed by service id plus expected PID, and wakes on
registration or child exit. Polling endpoint lookup is not a readiness model.
rootd must authorize the reporter
for each post-init lease: `initd` reports netd/devmgrd/inputd/storaged
and runtimed-as-sessiond, while running sessiond/runtimed reports uiserver.
Reports for an already-running lease are idempotent only for the same PID and
must reject attempts to overwrite the lease with a different PID. rootd also
records the kernel-stamped reporter PID. A newly restarted `initd` reconciles
its five service leases through `COMMERCIAL_MAX_ROOTD_OP_POST_INIT_LEASE_QUERY`:
it adopts only an exact-PID endpoint, keeps an endpoint-less legacy lease in a
bounded 30-second recovery window, then requests
`COMMERCIAL_MAX_ROOTD_OP_POST_INIT_LEASE_RECLAIM` for the exact stale PID.
Only the current initd may query/reclaim its own service classes; sessiond's
uiserver child is reclaimed first with its parent. Reclaim uses the
root-supervisor-only `SYS_RUSTOS_ROOTD_TERMINATE_BROKER`, which retires all
threads, marks the process exiting, revokes its endpoints, clears loader
broker state, records one fixed SIGKILL lifecycle exit, and signals its parent.
It is teardown substrate, not a generic kill syscall or a kernel restart
policy table.
rootd receives supervisor requests through root-supervisor-only sender-stamped
receive syscalls; both pre-init and post-init supervisor turns must drain
lifecycle/restart state before using `SYS_RUSTOS_IPC_TRY_RECV_WITH_SENDER`, so
a failed child cannot hide behind an indefinite rootd IPC wait. `SERVICE_CAPABILITY`,
`SERVICE_LOOKUP`, and `READINESS_SIGNAL` must reject payload `subject_pid/tid`
values that do not match the kernel-stamped sender PID/TID. The sender-stamped
recv path is not a generic app IPC ABI. rootd's supervisor loop must yield
between nonblocking receive turns rather than sleep-polling, because service
endpoint registration synchronously depends on rootd capability replies.

| ID | Service | Capability (`IPC_SERVICE_CAP_*`) | Owns |
|----|---------|----------------------------------|------|
| 1 | `syscalld` | `LINUX_SYSCALL_POLICY` | Linux cold validation, credentials, rlimits, clock/random/MM policy, Win32 syscalls |
| 2 | `vfsd` | `VFS_POLICY` | File namespace, cwd, dir/file cursors, mount/umount, metadata |
| 3 | `netd` | `NET_POLICY` | Socket namespace + all socket syscalls (AF_UNIX too) |
| 4 | `devmgrd` | `DEVICE_POLICY` | `/dev` registry, device open, ioctl auth |
| 6 | `loaderd` | `PROCESS_LOADER` | ELF/PE image policy, mapping, launch |
| 7 | `storaged` | `STORAGE_POLICY` | Block inventory after registration |
| 8 | `inputd` | `INPUT_POLICY` | Input ingest, read payload |
| 9 | `procd` | `PROCESS_POLICY` | exec/fork/wait/signal |
| 10 | `rootd` | `ROOT_SUPERVISOR` | Core-service leases, restart budgets |
| 11 | `sessiond` (reserved) | `SESSION_POLICY` | Console/TTY/session |
| 12 | `pagerd` (reserved) | `PAGER_POLICY` | Backing/page-cache |
| 13 | service-driver (reserved) | `SERVICE_DRIVER_POLICY` | Future non-DVM privileged-resource coordination |

Broker authorization checks the caller's registered service capability — **not** its executable path.
Until a standalone `pagerd` lease exists, `syscalld` may register the reserved `IPC_SERVICE_PAGERD` endpoint and receive `PAGER_POLICY`; rootd must treat that as an explicit compatibility delegation, not a generic multi-service registration rule.
`initd` must not be spawned until that delegated pager endpoint is registered;
otherwise dynamic loader page-fault/backing policy can deadlock behind rootd's
own initd spawn IPC.
## Driver domain contract

Linux kernel modules execute only inside the Linux DVM. RustOS neither stages nor
loads module images, probes module aliases, or retains native USB, PS/2, or
virtio-net fallback providers. A missing DVM transport leaves that device
unavailable; it must not select an alternate in-kernel provider.

RustOS accepts only bounded DVM transports: RDI2 over COM2 for input, fixed
ivshmem layouts for display and Ethernet frames. The Linux DRM/KMS relay inside
the DVM remains the display owner. Ring0 validates fixed headers, sequence and
bounds; `inputd`, `netd`, and `uiserver` own policy above those transports.
Before L0 writes `WELCOME` or opens a control relay, the launch-bound Linux DVM
agent must prove possession of the per-launch 256-bit secret with
`dvm-agent-hmac-sha256-v1`: HMAC-SHA256 over a fresh L0 challenge and the exact
HELLO bytes. The secret is generated by the L0 launcher, retained in its
owner-private runtime directory, and supplied only through QEMU fw_cfg's
root-only `raw` attribute. Its first four bytes also derive a nonzero private
KVM-vsock listener port, identically in L0 and the root-only agent; the port
is an availability capability, not an authentication replacement. An ordinary
same-CID process cannot reserve the pre-authentication setup slot without
reading that secret. CID and the image-bound control contract are necessary but
never sufficient identity. A DVM guest-root compromise remains
inside the DVM trusted-computing base and must be contained by its device/IOMMU
boundary; this handshake prevents ordinary same-CID guest processes from
impersonating the agent.
The display backend serializes provider replacement with an active DVM present:
the shared display generation must return even before its header is detached,
so a DVM consumer never observes a retired aperture permanently in-progress.
An installed DVM Ethernet aperture is similarly not evidence of a live or
authorized network peer. The current single-DVM topology admits RustOS
transmit/receive only while the L0-authenticated RDI1 control session's exact
nonzero epoch is active. `SESSION_END` revokes that epoch synchronously; an
old end marker cannot revoke a newer epoch, and guest-writable ivshmem headers,
ready bits, counters, or frames cannot create a lease. COM2 carries these
bounded lifecycle markers only, never Ethernet data. The aperture remains
mapped after revocation so lifecycle changes do not race an unmap, but packet
operations fail closed as `NoDevice` until a new authenticated start. A future
network-only DVM must introduce a domain-specific authenticated lifecycle
channel rather than inheriting the input-DVM lease. The capability-gated
`NET_BROKER_OP_PACKET_STATUS` ABI distinguishes `UNAVAILABLE` (no valid
aperture), `AWAITING_AUTHENTICATED_CONTROL` (mapped but fail-closed), and
`ACTIVE`; netd may use only a bounded wait for the middle transitional state.
`DISPLAY_INFO_FLAG_DVM_SCANOUT` is a one-way provenance bit propagated from
the DVM framebuffer registration. It can only downgrade trust: it never
attests the display. `COMMERCIAL_MAX_UISERVER_OP_TRUSTED_UI_STATUS` reports
blocker bits in `value0`; callers may permit a privileged prompt only when the
value is zero. The current DVM (and native) paths always report unattested
scanout plus unattested input, so no existing provider is a trusted UI path.
The DVM flag adds the diagnostic `DVM_SCANOUT` blocker. A future path must
independently attest both the physical scanout and human-input source before
clearing either blocker. A bounded ivshmem header, authenticated DVM agent, or
primary-provider flag alone is never such an attestation.
The reserved service-driver resource broker is not a DVM module loader and has
no currently admitted service endpoint.

Residual app-ABI policy batches (`compat-slowpath-ring3`,
`pager-slowpath-ring3`, and `process-slowpath-ring3`) are planning references.
They do not move syscall entry, page tables, scheduler state, or privileged
transport out of ring0.

## Handle Transfer

- Bounded cap-transfer via `kernel_ipc_runtime::api::KernelTransferredHandle` + `*_with_handles` endpoint APIs.
- Byte-only recv/take wrappers must fail with `BufferTooSmall` when a queued message contains transferred handles. Older paths must never silently drop capabilities.
- Transferred handles require nonzero transfer ticket + `HandleRights::allows_transfer()` true.
- Pending transferred-handle entries are owned by the process-handle substrate,
  not an isolated compat cache. An endpoint message that is cancelled,
  peer-closed before receive, rejected after receive-output validation, or
  abandoned by caller task exit must return every opaque descriptor for exactly
  one substrate drop. Successful receive installs the whole validated batch;
  duplicate/stale descriptors and partial installation must fail closed.
- Supervisor services polling independent brokers must use `SYS_RUSTOS_IPC_TRY_RECV`, not blocking `SYS_RUSTOS_IPC_RECV`.
- FD-table transfer goes through `kernel_ps::api::TransferredHandleEntry` + `HandleTable::{duplicate_for_transfer, install_transferred}`. Source class + rights must permit descriptor transfer; directory FDs are file capabilities and transferable for VFS migration.
- Userspace handle-aware IPC: `SYS_RUSTOS_IPC_{CALL,RECV,REPLY}_WITH_HANDLES` with `Ipc*WithHandlesArgs`. Send handles = Linux fd arrays; received handles install into receiver fd table and return as `i32` fd arrays + `u16` count. `recv_fd_count_ptr` mandatory even when no handles returned. Counts bounded by `IPC_MAX_TRANSFER_HANDLES`.

## IPC Wait Discipline

- A user-created endpoint and every reply capability queued through it are
  owned by the creating **process**, not a single creating task. Any worker in
  that process may receive/reply; a different process must receive `EPERM`
  without consuming the request or reply. Process exit closes all of its
  process-owned endpoints and wakes pending callers; individual task exit only
  removes that task's waiter/caller state. Do not weaken this to globally
  guessable endpoint or reply IDs, and do not regress service worker threads
  such as `uiserver-display-policy` to creator-task-only ownership.
- Public `SYS_RUSTOS_IPC_CALL` and handle-transfer call syscalls keep blocking
  Send/Receive/Reply semantics. Do not add a public timeout ABI for generic
  Linux ELF or Windows PE callers.
- Kernel compat calls into policy services may use a bounded internal deadline.
  On timeout, compat cancels the reply cap so queued and already-received
  endpoint calls cannot be completed by a late reply. Timeout cancellation
  records an `ipc-reply-timeout` milestone with the reply cap, caller task, and
  cancellation status.
- Endpoint process-owner teardown wakes both pending callers and tasks blocked
  in receive on that endpoint. Task exit must also prune stale receiver
  waiters owned by the exiting task from every endpoint.
- A single-endpoint service may block in `SYS_RUSTOS_IPC_RECV`. Supervisors
  with multiple independent event sources must drain with
  `SYS_RUSTOS_IPC_TRY_RECV` / `rustos_svc_runtime::ipc::try_recv` and use a
  bounded yield/sleep between drain passes.
- root-supervisor services that authorize subjects may use
  `SYS_RUSTOS_IPC_{TRY_RECV,RECV}_WITH_SENDER` to receive the caller PID/TID
  stamped by the kernel. Payload subject fields are not trusted unless they
  match this sender identity. Use the blocking variant only when the supervisor
  has no independent event source that must be polled before the next IPC.
- IPC stability changes are substrate only. Service restart, admission, routing,
  and policy decisions stay in `rootd`/`syscalld`/`vfsd`/`loaderd`/other owning
  services, not in new ring0 policy tables.

## VFS Surface (`vfsd`)

- Protocol: `VfsIpcRequest`/`VfsIpcResponse` (separate from `LinuxSyscallOffloadRequest`) for service-owned handles + chunked I/O.
- Kernel fd tables mirror service-owned objects as `KernelHandle::RemoteVfs`.
- VFS device open responses use `VfsIpcResponse.aux` for device access metadata such as `INPUTD_ACCESS_*`; ring0 read paths must not classify `/dev/input*` by path string after the remote handle is installed.
- Linux `openat` installs `KernelHandle::RemoteVfs` for regular files + directories after vfsd registration.
- Linux `close`/`dup`/`dup2`/`dup3`/`fcntl`/`getdents64` route through vfsd before app fd-table mutation. vfsd is the only intended caller of `SYS_RUSTOS_FD_*_BROKER`; gated by `VFS_POLICY`. Generic apps must not call directly.
- `LinuxSyscallOffloadRequest.arg0..arg3` carry 64-bit fd-control args (target fd, cmd, arg, flags). **Do not pack pointer/flag values into the 32-bit `mask` field.**
- `mount`/`umount2` route to vfsd → gated `SYS_RUSTOS_VFS_*_BROKER` for kernel mount-table mutation. Do not reintroduce direct generic-app `linux_ops::mount`/`umount2` paths.
- `poll`/`ppoll`/`epoll_*` route generic fd readiness policy and epoll
  interest state through `VFS_IPC_OP_POLL_QUERY`; socket readiness is `netd`
  policy and input readiness is `inputd` policy plus the narrow native-input
  wait/completion substrate. Ring0 keeps fd-table validation, epoll token
  handles, user-copy, and bounded timeout sleeping.
- Legacy `SYS_RUSTOS_{STATX,STAT,READLINK,ACCESS,GETCWD,CHDIR}_METADATA`: no generic-app VFS policy in ring0 after vfsd registers. Pre-vfsd bootstrap + registered policy-service callers retain direct kernel metadata access.
- `SYS_RUSTOS_BLOCK_BROKER`: narrow boot-volume read broker, gated by `VFS_POLICY`, accepts `RustosBlockBrokerArgs`. Does not depend on `storaged`.

## Storage Surface (`storaged`)

- Gated `SYS_RUSTOS_STORAGE_LIST_BROKER` (gated by `STORAGE_POLICY`) enumerates kernel-discovered descriptors; no direct generic-app storage probing.
- `StoragedRequest`/`Response` exposes `STORAGED_OP_ROOT_STATUS`, `STORAGED_OP_BOOT_EXTENT_LOOKUP`.
- Boot extent leases are storaged policy, sourced from `system/registry/kernel/root-file-extents.tsv` and returned over `STORAGED_OP_BOOT_EXTENT_LOOKUP`. Do not reintroduce generic ring0 boot-extent policy; ring0 storage brokers remain descriptor/block substrate only.
- Root extent manifests are bootloader-supplied data, not ring0 filesystem policy. GRUB loads `system/registry/kernel/root-file-extents.tsv` as the `rustos-root-extents` multiboot2 module and `BootInfo.boot_extent_manifest` points at that memory. Kernel boot may parse that manifest and perform physical extent reads, but must not need FAT directory traversal just to discover the manifest.
- Ring0 boot-volume FAT traversal is not part of the direct boot-file path. Entering `KernelVfsReady` must preload the root extent table from `BootInfo.boot_extent_manifest`; if the manifest is absent or a path is missing from it, direct boot-volume helpers fail closed. Directory traversal and generic FAT fallback must stay out of ring0 so namespace policy stays in `vfsd`/`storaged`.
- AHCI/NVMe post-bootstrap selection, inventory, partition, and extent policy lives in `storaged`/`vfsd`, but physical boot-volume block reads still require kernel io-manager transport substrate. Do not delete AHCI MMIO/DMA command execution until an explicit ring3 service-driver protocol can perform real block I/O before `rootd` and `vfsd` need it.

## Input Surface (`inputd`)

- Linux input reads call `InputdIpcRequest` with `INPUTD_IPC_OP_READ`.
- Compat input-device reads call `INPUTD_IPC_OP_AUTHORIZE_READ` before
  `INPUTD_IPC_OP_READ`; inputd consumes the authorization by `(pid, tid, fd,
  access)` so raw reads are not accepted as standalone event-drain authority.
- inputd drains authenticated RDI2 records via gated
  `SYS_RUSTOS_INPUT_INGEST_BROKER`, returns bounded `InputdReadResponse`
  capped at 32 KiB, and uses a bounded 4 ms ingest turn. It must sleep when
  both IPC and DVM input are idle and reuse its fixed-size ingress scratch.
- RustOS-native input readers should prefer short nonblocking `read()` attempts
  over a separate readiness-then-read cycle. `INPUTD_IPC_OP_READ` already drains
  ingest first, so routing through read avoids stale `STATS`/`poll` readiness
  decisions while keeping HID decode policy in inputd.
- inputd must coalesce lossy DVM pointer motion to the latest delta while
  preserving keyboard and pointer-button edges. Linux key translation and
  modifier/text state remain inputd policy; RustOS receives only bounded,
  authenticated relay records.
- Absolute pointer coordinates are a two-part contract. `inputd` owns HID
  logical axis decoding and event coalescing; `uiserver` owns the current
  display/output extent and publishes it to inputd with
  `INPUTD_IPC_OP_SET_POINTER_SURFACE` after display surface generation is
  stable. Do not hardcode fallback display sizes in inputd. If the pointer
  surface is not configured yet, inputd must not enqueue fabricated absolute
  positions.
- Ring0 performs only current-process user-copy of service-returned bytes.
- `INPUTD_IPC_OP_AUTHORIZE_READ` + `SYS_RUSTOS_INPUT_STATS_BROKER` remain compat/observability surfaces while remaining event queue is evacuated.

## Console / TTY Surface (`runtimed` session policy)

- `runtimed::session::SessionRuntime` owns Linux-style line discipline for
  console-hosted programs: canonical edit buffer, echo, cursor edit keys,
  noncanonical byte translation, and termios state.
- `uiserver` forwards focused keyboard `InputEvent`s to
  `CONSOLE_IOCTL_SEND_INPUT_EVENT`; it must not implement shell line editing.
- `runtimed` interprets key meanings through `keyboard-core::KeyCode`, not
  duplicated numeric constants. `InputEvent.text` is the source of printable
  bytes; Enter/backspace/arrows use `KeyCode`.
- Kernel TTY substrate remains a bootstrap/user-copy fallback only. Do not add
  new canonical editing or focus policy to ring0.

## Device Surface (`devmgrd`)

- `/dev` registry exposed to vfsd via `DevmgrdIpcRequest`/`Response`: `DEVMGRD_IPC_OP_LOOKUP`, `DEVMGRD_IPC_OP_READDIR`.
- vfsd may mirror only the explicit pre-devmgrd bootstrap nodes: `console0`, `display0`, `input0`, `input/event0`, `dri/card0`. **Do not reintroduce wildcard `/dev/*` success path.**
- After devmgrd registration, `/dev/...` file opens route through `devmgrd` `DEVMGRD_IPC_OP_OPEN` → `SYS_RUSTOS_DEVICE_OPEN_BROKER` → transferred device fd in reply. `/dev` directories stay vfsd-owned; devmgrd decides which device file paths exist.
- Device-open uses `DevmgrdDeviceOpenRequest`/`Response` with typed `DeviceId + access + rights`. Broker must install fd with the **exact reduced `DeviceHandleRights` chosen by devmgrd**, not default native-device rights. Broker must not infer policy from paths.
- `ioctl` route ownership lives in devmgrd: kernel compat asks `DEVMGRD_IPC_OP_IOCTL_ROUTE` and follows `DEVMGRD_IOCTL_ROUTE_DEVMGRD`, `DEVMGRD_IOCTL_ROUTE_SESSIOND_TTY`, `DEVMGRD_IOCTL_ROUTE_SESSIOND_COMMIT`, or `DEVMGRD_IOCTL_ROUTE_DIRECT`. Do not add new ioctl policy match tables to ring0.
- Policy-sensitive `ioctl` routes through devmgrd → `SYS_RUSTOS_DEVICE_IOCTL_BROKER` (gated by `DEVICE_POLICY`). Direct ioctl fallback allowed only pre-devmgrd. Hot data-path ioctls (display present) may stay direct broker calls to avoid per-frame policy IPC.

## Process Loader Surface (`loaderd` + `procd`)

- Runtime launches route through `loaderd` (`IPC_SERVICE_LOADERD`), not direct `SYS_RUSTOS_SPAWN_EXEC`.
- `SYS_RUSTOS_PROC_*_BROKER` calls fail with `EACCES` unless caller owns `PROCESS_LOADER`.
- `SYS_RUSTOS_SPAWN_EXEC` restricted to `rootd` spawning the fixed bootstrap allowlist (`syscalld`, `vfsd`, `loaderd`, `procd`, `initd`). Fails closed for `initd`, generic apps, broad service restarts. rootd may use direct spawn only during fixed bootstrap + `loaderd` recovery; post-bootstrap restarts of other leases must call loaderd.
- Linux `execve` → `procd` (target auth) → `loaderd` (image materialization). If loader materialization fails, procd must cancel the exec ticket via `SYS_RUSTOS_PROC_CANCEL_EXEC_BROKER` before replying.
- An exec ticket binds one live, non-exiting Linux `(target_pid, target_tid)` pair. Cancel and exec-target validate that exact stored pair before consuming the ticket; a mismatched request must leave it live. The successful exec-target path publishes its register handoff before replacing the target image. Normal/signal process exit, a non-final target-thread exit, and sibling retirement caused by Linux exec remove stale ticket or handoff state.
- `loaderd` must attempt `ABORT_BROKER` after every rejected commit/exec-target call. Commit is normally terminal, but this closes early rejection paths (such as an already-pending target handoff) before they can retain a bounded prepare slot.
- **Do not move** executable-format, import/export, or DLL namespace policy back into the kernel.

### Process Broker Session

Start: `SYS_RUSTOS_PROC_PREPARE_BROKER` with `PROC_BROKER_ABI_VERSION` + explicit format (`PROC_BROKER_FORMAT_ELF64` or `PROC_BROKER_FORMAT_PE64`). Returned `prepare_handle` is owned by the loader process; supply to `SYS_RUSTOS_PROC_COMMIT_BROKER` or `_ABORT_BROKER`.

Prepare state is owner-bound and bounded. `COMMIT_BROKER` consumes the prepare
handle before its later launch validation, so both successful and rejected
commit attempts are terminal for that handle. Normal loader process exit and
signal-driven process exit must remove every still-uncommitted prepare state;
its pinned mapping metadata must not survive a crashed loader.

Mapping ops use `PROC_BROKER_MAP_{READ,WRITE,EXEC,PRIVATE}` flags and record non-overlapping page-aligned mappings:

- `SYS_RUSTOS_PROC_MAP_ZEROED_BROKER`
- `SYS_RUSTOS_PROC_MAP_DATA_BROKER`
- `SYS_RUSTOS_PROC_MAP_FILE_BROKER` + batch variant `_BATCH_BROKER` — **fd/cap-backed only**. Kernel resolves fd to pinned `KernelHandle` at registration; no path re-open at commit. Backing must be file-kind (`VfsFile`, `RemoteVfs(File)`, `Memfd`); directory/device/socket fd rejected with `EINVAL`/`EACCES`.

**ELF:** loaderd emits `PT_LOAD` mappings for main image + `PT_INTERP` via `MAP_FILE_BROKER`. Static-PIE biases: main = `PROC_BROKER_USER_SPACE_BASE + 0x0040_0000`, interpreter = `+ 0x0200_0000`. Must use `SYS_RUSTOS_PROC_SET_LINUX_RUNTIME_BROKER` for minimal launch metadata (entry, phdr, phnum, phent, brk_start, interpreter_base) — **not** raw blob streaming via `SET_IMAGE_BLOB_BROKER`. Kernel derives launch state from this metadata and the pre-built address space.

**PE64:** PE validation, section materialization, base relocation, import/export resolution, staged system-DLL registry lookup, PEB/TEB/runtime blob construction all happen in loaderd before commit. PE64 commit includes `SYS_RUSTOS_PROC_SET_WINDOWS_RUNTIME_BROKER` after all `MAP_DATA_BROKER` ops and before `COMMIT_BROKER`. Kernel validates metadata + spawns the materialized address space but **must not** reintroduce PE import/export/system-DLL policy.

Commit (`COMMIT_BROKER`) builds the child address space from recorded mappings.
By default, commit-broker spawns do not request immediate deferred reschedule so
`loaderd` can reply to its caller before the spawned child runs startup policy.
Supervisors (`rootd` for initd, `initd` for post-init services, and `runtimed`
for uiserver) set `LOADER_SPAWN_FLAG_DEFER_START` and complete lease admission
before `LOADER_OP_ACTIVATE`. Activation is single-use and fails closed for an
unknown, exited, already-running, or non-suspended PID. Endpoint-owning children
must then be confirmed through the exact-PID endpoint wait syscall.

`LOADER_SPAWN_FLAG_IMMEDIATE_HANDOFF` remains an ABI option for a child with a
pre-admitted ownership contract; it is not valid for normal supervised service
bootstrap.
Immediate handoff uses a spawn-specific scheduler hint that is consumed before
generic IPC reply hints, so the loader's reply to the supervisor cannot
overwrite the freshly-created child. The spawn broker must set this hint without
requesting or taking a reschedule before returning to `loaderd`. Spawn handoff
is an explicit bootstrap transfer and may be consumed by the next scheduler
dispatch, including deferred syscall-exit dispatch; it may bypass the generic
IPC strict-class guard, otherwise a ready System-class task can indefinitely
defer a child-first handoff. A service that lacks a pre-admitted lease must not
use this legacy path. IPC enqueue/reply
completion wakes the receiver/caller and may set a direct pick hint for any
currently ready and
schedulable target; already-runnable service endpoints still need a handoff when
the caller queued work before the service re-entered receive. Synchronous IPC
enqueue only sets the receiver handoff hint; the caller must arm and re-poll its
reply wait before yielding so a fast service reply cannot race a not-yet-armed
waiter. Generic IPC hints are caller-local
and the newest eligible receiver replaces older generic hints, even across
scheduling classes; stale high-class service hints must not block the service
that the current caller is waiting on.
Both paths request a deferred reschedule at syscall exit; handoff hints must not
wait for a later timer tick or a service's next blocking receive.

## Network Surface (`netd`)

Routed Linux ops after bootstrap: `socket`, `socketpair`, socket `dup`/`dup2`/`dup3`, socket `close`, socket `read`/`write`/`writev`, `bind`, `listen`, `accept`/`accept4`, `connect`, `sendto`, `recvfrom`, `sendmsg`, `recvmsg`, `getsockname`, `getpeername`, `setsockopt`, `getsockopt`, `shutdown`.

netd invokes gated `SYS_RUSTOS_NET_BROKER` with target pid. Net broker arg struct carries six 64-bit syscall arg slots. Kernel performs handle install and target user-memory validation/copy; AF_UNIX socket lifecycle, binding/listen queues, byte queues, and socket option policy belong to netd. `NetdIpcRequest`/`Response` carry `socket_token`, fd `status_flags`, and a bounded inline payload for this service-owned socket path.

Blocking INET connect/send/recv waits run as bounded netd workers that release
the shared network-state lock between polls. The single policy receive loop
must remain available for AF_UNIX and readiness traffic; no blocking INET
request may hold that loop or its state lock across a timer sleep.

## Process Policy Surface (`procd`)

procd owns Linux `execve`, `fork`/`clone`, `wait4`, `rt_sigaction`, `rt_sigprocmask`, `sigaltstack`, `tgkill`, signal selection.

`wait4` routes through procd for ownership validation; kernel still performs narrow process-table wait + status/rusage copyout.

Kernel keeps only: user-copy, address-space replacement, scheduler mutation, pending-signal wakeup, Linux x86_64 `rt_sigframe`/`rt_sigreturn`.

## Syscalld Residual Surface

- Per-process credentials + `RLIMIT_STACK` policy DB: source of truth for Linux-visible `get*id`, `set*id`, `prlimit64`. Kernel process credentials = gated bootstrap/security primitive; **must not** be mutated by Linux `set*id`.
- Linux time admission policy for `nanosleep`, `clock_gettime`,
  `clock_nanosleep`, and `ppoll` timespec validation belongs to `syscalld`.
  Ring0 keeps only current-task user-copy plus RTC/tick sleep and clock
  substrate.
- Linux futex WAIT and WAIT_BITSET accept validated relative/absolute timeout
  timespecs and block on the RTC waiter substrate; timeout-bearing futex calls
  must not return `ENOSYS` or spin in libc retry loops. Any waiter table read by
  an IRQ handler must be mutated from process context with interrupts excluded
  while its spin lock is held.
- `exit_group` and default fatal-signal termination retire every thread in the
  process and fail every thread-owned IPC wait/endpoint before the current
  thread halts. Never revoke process service endpoints while sibling threads
  remain runnable.
- uiserver opens input nonblocking and waits through bounded `poll`; RustOS must
  not replace readiness with unconditional success followed by high-rate inputd
  reads.
- Linux `memfd_create`: policy validation in syscalld; kernel performs handle install + read/write/truncate/seal (current handles, user memory).
- Windows syscall policy: `Win32SyscallOffloadRequest`/`Response` + `SYSCALL_OFFLOAD_OP_WIN32_*` range. Kernel dispatcher calls service policy first, validates ABI/status, and must fail closed with a non-success NTSTATUS on malformed or denied responses before performing only the narrow privileged action.

## Commercial-Max Protocol

`rustos-user-abi::syscall::CommercialMaxProtocol*` reserves versioned protocol/op
ids for core services. Retired driverd values remain numeric ABI reservations;
they do not admit a service or a module-loading path.

**Shared ABI scaffolding only** — ring0 exposes new privileged actions only when a narrow broker is implemented and capability-gated.

## Display Surface

- `device::DisplayInfo.flags`: `DISPLAY_INFO_FLAG_PRIMARY_PROVIDER` distinguishes a real primary provider from GRUB/firmware framebuffers (default = early console + panic output only).
- Firmware framebuffer data is diagnostic-only and is never a presentation
  fallback. The only accepted normal provider is the validated DVM aperture.
- Driver framebuffer registration accepts only the DVM primary-provider flag;
  do not infer ownership from geometry or `display_info()` presence.
- Surface present = kernel fast path: copies validated shared-surface contents into active framebuffer + queues provider flush for bounded housekeeping. **Do not reintroduce synchronous virtio-gpu command waits into app syscall context** for normal uiserver presents.
- The KVM virtio-gpu path is Linux DVM DRM/KMS over the fixed display aperture.
  RustOS has no in-kernel virtio-gpu `.ko` path and must not regain a second
  display provider.
- If the DVM provider is unavailable, normal presentation fails closed. It
  must not synthesize a boot-framebuffer or direct-GPU fallback.
- A DVM-backed physical scanout is usable for normal desktop rendering but is
  not a trusted-attention display. The trusted-UI status endpoint fails closed
  until an independently attested scanout and input path are deployed; drawing
  an overlay in `uiserver` does not change that boundary.
- `uiserver` partial dirty rects should stay split unless merged union is nearly as small as separate areas. Over-coalescing disjoint topbar/taskbar/window updates → large framebuffer copies + delayed input feedback.

## Scheduler

- Linux CFS-like: fixed tick, nanosecond vruntime, weighted share, bounded sleeper credit. Weights affect vruntime only — **never reprogram hardware timer**.
- Timer IRQ hitting a user-task kernel frame: set deferred reschedule; **do not preempt arbitrary kernel frames**.
- Task weights = microsecond vruntime budgets (default 100 µs). `uiserver` gets a longer render/present slice, and latency-critical brokers it calls in-frame, especially `inputd`, must stay in the same order of weight so UI loops do not block behind input IPC. `runtimed` must pass manifest `weight_micros` through `loaderd`; never replace with default.
- The max-burst guard rotates to another ready peer within the current
  scheduling class even when the current task's weighted vruntime still wins;
  this is the last-resort rail for long UI/input kernel frames without breaking
  strict System/User/Idle band ordering.
- `KernelSpinLock` must not be held across disk/filesystem/IPC/framebuffer-copy loops. Use `KernelWaitLock` or split the section; add `cond_resched` in long loops.
- Boot service order: driver/input/storage policy services before UI launchers. `runtimed` waits on `devmgrd` and `storaged` endpoints before UI bootstrap.
