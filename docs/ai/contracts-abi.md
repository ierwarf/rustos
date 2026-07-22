# AI Contracts — Kernel/Service ABI

IPC service IDs, broker syscalls, handle transfer, and service routing. For package/stage/build/logging: `contracts-infra.md`.

## Kernel/Userspace ABI Surface

- Shared ABI crate: `libs/rustos-user-abi`.
- Kernel re-export: `kernel/ps/src/user/{abi,handles,sysops}.rs`. `kernel/compat` re-exports through `kernel_ps::api`; no shadow ABI/handle/user-memory sysop files.
- Device/console/UI `repr(C)` structs and ioctl numbers must live in `rustos-user-abi`. Services (`uiserver`, `runtimed`) consume that crate — never duplicate request structs or ioctl encoding.
- Evacuation policy, ring0/ring3 boundary, service ownership: live source
  `RING3-MIGRATION-REFERENCE` / `RING3-MIGRATION-COMMENTED-OUT` markers, exact
  broker call paths, and owning service contracts.
- `RING3-MIGRATION-REFERENCE` / `RING3-MIGRATION-COMMENTED-OUT` blocks are references for migration, not dormant code to revive. Do not fix breakage by uncommenting them unless the exact lines are the remaining ring0 substrate.
- For each slice, move policy/state/lifecycle behavior into the owning service, leave only narrow ring0 fd-table/user-copy/page-table/privileged-device substrate, then delete or bypass the reference block.

## Boot Initial Task

`rootd` (`services/rootd/rootd.elf`) is the first user process:

- Must avoid Linux libc/std dynamic runtime deps.
- Spawns `syscalld`, `vfsd`, `loaderd`, `procd`, then hands off to `services/initd/initd.elf`.
- Kernel boot code must not grow generic POSIX compat exceptions for `initd`; early bootstrap surface stays narrow, explicit, tied to `rootd` bringing up foundational policy services.
- Stays resident as `IPC_SERVICE_ROOTD`; serves only the versioned
  `COMMERCIAL_MAX_PROTOCOL_ROOTD_SUPERVISOR` contract and tracks
  `CoreServiceLeaseWire` via `SYS_RUSTOS_LIFECYCLE_DRAIN_BROKER`.
- Bootstrap policy services must not perform a rootd-authorized service lookup
  merely to prove that an empty maintenance queue is empty. In particular,
  procd's lifecycle drain returns before looking up syscalld when it drained no
  events. When events do exist, it resolves the endpoint before taking signal
  policy locks and releases those locks before making the syscalld call. This
  prevents the `rootd -> loaderd -> procd -> rootd` authorization cycle that
  otherwise blocks the first initd prepare transaction.

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
Service capability assignment is rootd policy: after rootd self-registration, kernel compat asks `COMMERCIAL_MAX_ROOTD_OP_SERVICE_CAPABILITY` with the registering subject PID/TID and records the returned `IPC_SERVICE_CAP_*` mask only if rootd confirms the PID matches the running lease. This includes endpoint registration through the Linux syscall ABI. Do not reintroduce a full `service_id -> capability` table in ring0.
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
it adopts only an exact-PID endpoint, keeps an endpoint-pending admitted lease in a
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
No service may hold a local policy/state lock across service discovery or a
synchronous cross-service call. Early-boot maintenance paths additionally
skip discovery when their drained work set is empty. A new bootstrap
dependency must either be covered by rootd's declared readiness order or use a
bounded, explicit ready handshake; startup scheduling luck is not readiness.

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
| 13 | service-driver (reserved) | `SERVICE_DRIVER_POLICY` | Reserved non-DVM privileged-resource coordination; no endpoint is admitted until a capability-gated broker exists |

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

RustOS accepts only bounded DVM transports: RDI3 records in a host-owned 128
KiB ivshmem input ring with 2,048 fixed 32-byte slots and exactly one MSI-X
wake vector; fixed layouts for display control/pixels and Ethernet frames. The DVM
never maps the input ring. L0 is its sole producer, RustOS is its sole
consumer, and the producer/consumer cursor cache lines are distinct. Ring0
validates fixed headers, sequence and bounds; `inputd`, `netd`, and `uiserver`
own policy above those transports. Input attachment is serialized, has a
boot-wide eight-attempt recovery budget, and may reuse its permanent MSI-X
vector only for the originally pinned PCI aperture; revoke releases the old
MMIO mapping. The sole accepted GUI topology is the V3 `RSGUI002` three-slot
GUI-DVM pool; V2, polling, a firmware framebuffer, and a native-GPU fallback
are not parsed or selected.

RDI3 preserves pointer semantics end to end. Relative evdev devices produce
bounded delta records; absolute tablets produce bounded `0..1599 x 0..899`
position records only after a complete `SYN_REPORT`. Partial X/Y reports are
staging state, identical positions are idempotent, and neither L0, ring0 nor
`inputd` may reinterpret an absolute position as a relative delta. The inputd
ingress ABI uses distinct packet and position payloads so a provider cannot
make one physical report travel through both paths.

The GUI-DVM contract fixes three page-aligned host-provisioned surfaces and 64-byte
`PRESENT`/`RELEASE` records, never guest-selected pointers, vectors, or
variable-length payloads. The ivshmem BAR contains only uncached control state;
the separate cacheable pixel device is writable only by RustOS QEMU and is
read-only/ROM in the DVM. Every READY slot is a complete immutable snapshot,
even when its PRESENT damage hint is partial. RustOS may construct that
snapshot with a damage-only patch only after reclaiming a FREE slot whose
retained pixel-content generation is the exact immediately preceding
generation and whose source mapping is unchanged; retained content generation
is not release authority. An uninitialized/stale slot, a source replacement,
or full damage forces a complete copy. The DVM module requires matching
control/pixel headers, exposes only WB read-only pixel pages to its relay, and
rejects writable VMAs. Before export, the module owns the complete fixed pixel
aperture through one `MEMORY_DEVICE_GENERIC` `dev_pagemap`; every exported PFN
must remain inside that live page-map. A raw guest-physical range that is merely
CPU-readable through `memremap()` is not sufficient DMA backing. Each immutable
slot is then exported as a read-only DMA-BUF. Standard DRM PRIME may request a bidirectional import, but the exporter
maps its scatterlist as `DMA_TO_DEVICE`; device-to-memory mapping is rejected.
Because the producer is in another VM and cannot participate in guest-side
cache maintenance, the exporter also rejects a non-coherent DMA attachment;
the enabled x86 AMD topology must report coherent DMA before
`DMA_ATTR_SKIP_CPU_SYNC` is permitted.
The physical AMD mode imports those three slots as read-only EGL DMA-BUF source
images, composes with GLES into a separate three-buffer GBM scanout pool, waits
an explicit native fence, and submits an atomic KMS page flip. It never uploads
the source with `glTexSubImage2D` and cannot report physical readiness through
the virtio staged-copy fallback. This source implementation and the model do
not substitute for the still-required physical import, page-flip, and sustained
performance capture. The consumer validates each release against the matching
host record before it writes the one outstanding control sequence. Its MSI-X
leaf may only mark pending state in IRQ context; normal context checks the exact
readiness invitation, confirmation, release generation, and ACK.
Only a completed page-flip fences the replacement front and releases the old
front; the new front is not reusable until a later fenced flip. An offline
notification clears readiness confirmation and revokes all slot authority. If a restart finds all
three slots READY, the next bounded producer attempt re-invites the newest
slot; no polling or alternate provider is selected. Unsupported multi-domain
focus records are rejected fail-closed because no focus authority is present in
this single-GUI-DVM topology. Commercial acceptance requires the source, DVM
package, launch path, conformance tests, and recovery proof to remain aligned.

GPU composition uses a separate private version-1 contract and does not expand
the application ABI. `uiserver` is the sole retained-scene, visibility,
z-order, damage, atlas-packing, and admission-policy owner. Each atlas-bearing
frame binds exactly one immutable BGRA generation from a fixed three-slot pool
and emits at most 512 commands. The atlas contains only independently
rasterized visible layer regions (for example a Wayland SHM surface, terminal
cache, glyph/icon tile, or cursor), never a CPU-precomposed final framebuffer.
Solid UI regions remain solid-quad commands instead of being baked into the
atlas. Textured commands name bounded normalized subrectangles of the one bound
atlas, so the DVM cannot select another slot or reinterpret an offset as an
address. The atlas is at most 8192×8192 and 256 MiB; three submissions may be in
flight. A frame has a 16.667 ms commercial performance target and a separate
50 ms hard execution timeout. Crossing the target keeps the previous front
buffer visible and fails the performance sample; only the hard timeout or an
explicit device/context error invalidates the epoch.

The only commands are clear, solid quad, and textured quad with bounded
fixed-point depth/rotation/tilt/perspective parameters. A batch cannot contain a
shader, GPU virtual address, DMA-BUF fd, arbitrary command-buffer byte, or
peer-selected queue. The atlas token binds one exact physical slot, mapping
generation, geometry, stride, and content epoch. Mapping generation is fixed
across all three imported slots for one provider/context epoch; slot rotation
and reuse advance sequence and content epoch without pretending to rebind the
DMA-BUF. A batch that binds a second source, uses a
slot outside `0..2`, names an empty/out-of-atlas subrectangle, or rebinds a token
is a protocol error. RustOS owns the context id, epoch, monotonic
acquire/submit/completion/release values, queue admission, and reset.
The acquire value names an exact RustOS CPU-producer release, never a
numerically plausible submit value. In physical AMD mode the root-only DVM
exporter accepts it only after the live invitation, slot, generation, sequence,
bounded batch, geometry, stride, and content epoch all match shared state. A
device-to-CPU acquire barrier then precedes creation of an already-signalled,
one-use `sync_file`; EGL imports that fd and inserts `eglWaitSyncKHR` into the
GPU command stream before the source texture can be sampled. The fd cannot be
replayed and is distinct from the later GPU render and KMS present fences.
Timeout, context loss, or revoke invalidates
every unfinished command and all source/output authority in the epoch; an old
completion cannot revive the reset context. The DVM may write only DVM-private
render targets and receives only device-read authority to RustOS sources. A
completed GPU fence releases sources; an explicit later present fence alone
retires the previous DVM-private front output. Built-in shader translation,
EGL context construction, and host pipeline creation are one context-prime
phase: admission stays closed until its explicit GPU fence completes, its
end-to-end wall time is bounded to 500,000 us, and timeout invalidates the
context. The per-frame 16.667 ms target and 50 ms hard timeout are measured
only after that bounded setup phase; prime time cannot be reported as a
completed frame. A target miss cannot be relabeled as success, but it also
cannot manufacture device loss while the bounded fence wait is still live.

The transport has two explicit, non-interchangeable evidence modes. QEMU
implements `source-path=staged-copy zero-copy=0`: only damaged atlas rectangles
are copied once into a virtio-GPU texture, after which the fixed GLES commands
perform the actual composition into a DVM-private output. This is a valid
GPU-composition test but can never satisfy the physical zero-copy gate. The
v2 prime-completion record authenticates exactly one of those modes before the
host exposes GPU readiness. RustOS caches the selected mode for the context and
must stamp that same single value into every submit; zero, unknown, ambiguous,
legacy-v1, or cross-mode records fail closed. The private `DisplayGpuInfo`
reports the selected backend mode to uiserver, but application Wayland clients
still receive no GPU command or DMA-BUF ABI.

The physical `source-path=dmabuf zero-copy=1` implementation opens the root-only
exporter, imports all three immutable atlas slots as EGLImages, samples them
read-only in the same fixed GLES vocabulary only after the exact kernel-issued
acquire `sync_file` has been server-waited, emits a native GPU completion fence,
and immediately supplies that possibly-unsignalled fd to KMS `IN_FENCE_FD`.
The relay does not serialize the GPU and vblank with a CPU pre-wait; one bounded
poll collects render completion, the DRM page-flip event, and the CRTC
out-fence before publishing completion. Generation,
content epoch, submit value, and transport sequence must all advance before a
source is accepted. The old unreachable zero-atlas branch and its
CPU-composed-frame renderer remain retired. Direct scanout without composition
is only a possible later optimization for one opaque, untransformed full-screen
source; it is not claimed by evidence-v2 or required for ordinary multi-layer
frames.
The source descriptor is currently fixed to one-plane ARGB8888, provider-owned
stride, and explicit `DRM_FORMAT_MOD_LINEAR`; the direct path requires EGL
modifier-import support. The sealed backend registry certifies only
`virtio_gpu` staged-copy and `amdgpu` direct-DMA-BUF today. The executor itself
contains no vendor-specific render/import branch. Enabling a later physical GPU
requires a new registry entry, supply/evidence gates, and a versioned descriptor
extension if its producer/consumer format-modifier intersection differs.
The DVM relay-to-agent readiness lock is mode-exact. Virtio fallback retains
schema 2 with `MODE=gpu-compositor-staged-copy`, `ZERO_COPY=0`,
`GPU_COMPOSITION=1`, and `EXPLICIT_FENCE=1`. An amdgpu-bound agent instead
requires schema 3 with `MODE=gpu-compositor-dmabuf-source`, `SOURCE_PATH=dmabuf`,
zero copy, GPU composition, explicit fence, atomic KMS, exactly three scanout
buffers, no staged damage copy, and no CPU final composition. Cross-mode
payloads fail closed while the relay holds its lock; the old direct-scanout
label remains rejected. `xtask check` cross-checks both C sources so readiness
cannot claim a transport that did not run. The display process first owns a singleton
lock, writes and fsyncs the exact payload on one locked fixed-name candidate,
and atomically renames that inode to the ready path. Ordinary failure closes
the ready lock before scheduler restoration; process exit and the RT hard limit
release all locks. The DVM agent uses the same process-owned pattern for local
health: its diagnostic `announce` command cannot create readiness, and stale,
partial, symlinked, malformed, or unlocked state fails closed.
GPU completion releases the atlas read lease. A later KMS
page-flip/present fence releases the previous front output. Neither completion
may be synthesized from command acceptance.
These ownership rules follow the consumer-owned dequeue/queue/acquire/release
shape documented by [Android BufferQueue](https://source.android.com/docs/core/graphics/arch-bq-gralloc),
the shared-buffer constraint/lifetime split used by [Fuchsia Flatland](https://fuchsia.dev/fuchsia-src/concepts/ui/scenic/life_of_a_flatland_image),
and Linux DRM's distinct
[input and output fences](https://www.kernel.org/doc/html/latest/gpu/drm-kms.html#explicit-fencing-properties).
The retained scene is published as one atomic batch, matching the transactional
commit property of
[DirectComposition](https://learn.microsoft.com/en-us/windows/win32/directcomp/architecture-and-components).
Virtio resource blobs are optional, so their presence is never assumed to turn
the QEMU staged path into zero copy; see the
[Virtio 1.3 GPU contract](https://docs.oasis-open.org/virtio/virtio/v1.3/virtio-v1.3.html#x1-3320007).

The current pre-public-ABI slice includes a source-level `uiserver` scene
compiler and a separate Linux-DVM GLES executor proof. The DVM compiles only
built-in shaders, rejects
software renderers, waits explicit acquire and completion fences, validates
output pixels plus stable/dynamic frame-hash pairs, and keeps a bounded health
submission active. QEMU uses virgl on an explicitly validated AMD host render
node in both headless and GTK modes, then requires 120 proof frames at at least
60 FPS with both GPU completion and wall-clock per-frame intervals no greater
than 16.667 ms and three fresh health samples. Pipeline creation remains under
normal scheduling. Only this finite post-prime measurement may enter Linux
`SCHED_RR` priority 8, below the display relay at 9 and input relay at 10, with
an exact 50/100 ms `RLIMIT_RTTIME`. All three DVM realtime guards first require
`SCHED_OTHER` priority 0, read back the installed RT limit before entering
`SCHED_RR`, and read back both the saved policy/priority and saved RT limit on
exit. Success and ordinary GPU-proof failure must complete that restore before
publishing evidence or entering the health loop. A display/input admission
rollback or restore mismatch is fatal to the process and cannot enter the
retry/reconnect loop. Crossing the hard limit also terminates the process. The
agent and display ready-inode locks are then released automatically; the GPU
proof publishes no success evidence before verified restoration. Its
scope declaration must say
`scope-public-abi=0`, `scope-ui-connected=0`, and `scope-scanout=0`; these are
explicit limits of this isolated proof, not runtime evidence of an endpoint's
absence. Functional fixed-command/hash/fence proof and performance acceptance
are separate: a bounded proof with `performance-target=0` may allow the display
relay to retain the last valid front, but it cannot satisfy the GPU readiness
or release gate. Therefore the proof cannot be mistaken for end-to-end RustOS
UI acceleration. The pre-scheduler-hardening bounded QEMU artifact proves the atlas compiler,
private submit transport, live AppState-to-layer adapter, fixed GLES executor,
and GPU-output-to-KMS handoff together. The relay publishes readiness only
after a validated command batch completes GPU and present fences; a
CPU-composed snapshot can no longer activate it. The app-visible 2D/3D ABI is
deliberately the next boundary, not part of this private compositor slice.
Physical AMD read-only atlas import, zero-copy source consumption, and atomic
scanout capture remain failed hardware gates rather than CPU fallbacks.

The enabled physical release profile is AMDGPU-only. Its signed schema-3 device
policy binds `amdgpu` plus PCI `1002:1900` and nominal-60-Hz throughput,
page-flip completion latency, atomic-commit latency, freshness, and consecutive
sample bounds. The relay atomically publishes one sequence-numbered sample per
second from completed DRM page-flip events; the authenticated DVM agent exposes
it through `display-evidence-v2`. That ABI additionally requires
`SOURCE_PATH=dmabuf`, GPU composition, an explicit fence, atomic KMS, exactly
three scanout buffers, and no staged damage copy. Hostd admits readiness only
after five fresh
consecutive samples and requires the sequence to keep advancing during health
checks. A wrong host/DVM identity, CPU-copy path, stale or restarted sequence,
low page-flip rate, or excessive latency revokes readiness and enters bounded
stop/reset/restore. These source checks do not replace the required physical
capture.
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
the V3 pool cannot be detached while a host slot is being copied and its fixed
`PRESENT` record is committed. A missing or revoked GUI-DVM provider returns
`Unavailable`; the normal present syscall never writes a firmware, native-GPU,
or generic framebuffer substitute.
An installed DVM Ethernet aperture is similarly not evidence of a live or
authorized network peer. The current single-DVM topology admits RustOS
transmit/receive only while the L0-authenticated RDI1 control session's exact
nonzero epoch is active. `SESSION_END` revokes that epoch synchronously; an
old end marker cannot revoke a newer epoch, and guest-writable ivshmem headers,
ready bits, counters, or frames cannot create a lease. The L0-authenticated
RDI3 lifecycle records carried by the fixed input ring never carry Ethernet
data. The aperture remains
mapped after revocation so lifecycle changes do not race an unmap, but packet
operations fail closed as `NoDevice` until a new authenticated start. A
network-only DVM is not an enabled topology: it cannot inherit the input-DVM
lease and has no packet authority without a domain-specific authenticated
lifecycle channel.
The capability-gated
`NET_BROKER_OP_PACKET_STATUS` ABI distinguishes `UNAVAILABLE` (no valid
aperture), `AWAITING_AUTHENTICATED_CONTROL` (mapped but fail-closed), and
`ACTIVE`; netd may use only a bounded wait while authentication is pending.
`DISPLAY_INFO_FLAG_DVM_SCANOUT` is a one-way provenance bit propagated from
the DVM framebuffer registration. It can only downgrade trust: it never
attests the display. `COMMERCIAL_MAX_UISERVER_OP_TRUSTED_UI_STATUS` reports
blocker bits in `value0`; callers may permit a privileged prompt only when the
value is zero. The current DVM (and native) paths always report unattested
scanout plus unattested input, so no existing provider is a trusted UI path.
The DVM flag adds the diagnostic `DVM_SCANOUT` blocker. Any path that clears a
trusted-UI blocker must independently attest both the physical scanout and
human-input source. A bounded ivshmem header, authenticated DVM agent, or
primary-provider flag alone is never such an attestation.
The reserved service-driver resource broker is not a DVM module loader and has
no currently admitted service endpoint.

The retired `compat-slowpath-ring3`, `pager-slowpath-ring3`, and
`process-slowpath-ring3` planning tables have no live ring0 source entry.
Their ownership constraints remain: syscall entry, page tables, scheduler
state, and privileged transport stay ring0 substrate while service-visible
policy remains with the owning service.

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
  policy, input readiness is `inputd` policy plus the narrow native-input
  wait/completion substrate, and non-system console-input readiness is
  `sessiond` policy. sessiond advances its generation only on an empty-to-ready
  line-discipline transition; the readiness query is non-consuming and bounded
  by the application's remaining deadline. Console output/error remain
  immediately writable. Closing a session advances the same generation and a
  subsequent query returns non-live, which compat exposes as `POLLHUP` without
  recreating the removed session. Persistent epoll admission for console descriptors is
  deliberately rejected until console handles carry a unique open-description
  identity; a reusable session or numeric fd must not masquerade as that
  lifetime. Ring0 keeps fd-table validation, epoll token
  handles, user-copy, a bounded provider-wait registry, and deadline wakeup.
  Both persistent epoll sets and syscall-scoped multi-fd poll sets use the same
  generation-based path: check, register the exact observed generations,
  recheck through ordinary service IPC while the task remains runnable, arm
  the scheduler, then verify that every registered waiter still exists before
  commit. A provider signal removes its waiter before wake, so an event in the
  recheck-to-arm window is detected without nesting an IPC block inside an
  already armed scheduler block. Every readiness IPC is capped at the shorter
  of the remaining syscall deadline and 16 ms; an infinite application wait
  therefore cannot turn a wedged provider into an unbounded kernel-service
  call. `ppoll`/`epoll_pwait` atomically
  apply their temporary signal mask, reject a non-native sigset size, and
  return `EINTR` for a pending unmasked signal; SIGKILL/SIGSTOP can never be
  added to the temporary mask.
- Legacy `SYS_RUSTOS_{STATX,STAT,READLINK,ACCESS,GETCWD,CHDIR}_METADATA`: no generic-app VFS policy in ring0 after vfsd registers. Pre-vfsd bootstrap + registered policy-service callers retain direct kernel metadata access.
- `SYS_RUSTOS_BLOCK_BROKER`: narrow boot-volume read broker, gated by `VFS_POLICY`, accepts `RustosBlockBrokerArgs`. Does not depend on `storaged`.

## Storage Surface (`storaged`)

- Gated `SYS_RUSTOS_STORAGE_LIST_BROKER` (gated by `STORAGE_POLICY`) enumerates kernel-discovered descriptors; no direct generic-app storage probing.
- Storaged accepts only the versioned `CommercialMaxProtocolRequest` contract.
  Boot extent leases are storaged policy, sourced from
  `system/registry/kernel/root-file-extents.tsv` and returned over
  `COMMERCIAL_MAX_STORAGED_OP_BOOT_EXTENT_LEASE`. Do not reintroduce generic
  ring0 boot-extent policy; ring0 storage brokers remain descriptor/block
  substrate only.
- Root extent manifests are bootloader-supplied data, not ring0 filesystem policy. GRUB signature enforcement authenticates `system/registry/kernel/root-file-extents.tsv` before loading it as the `rustos-root-extents` multiboot2 module, and `BootInfo.boot_extent_manifest` points at that immutable memory. Each row binds an exact path and extent coverage to ordered `sha256_chunks` digests over 64-KiB file chunks. Kernel boot may parse that manifest and perform physical extent reads, but must reject duplicate paths, overlapping/inexact extents, digest-count mismatch, and content mismatch; it must not need FAT directory traversal just to discover the manifest.
- Ring0 boot-volume FAT traversal is not part of the direct boot-file path. Entering `KernelVfsReady` must preload the root extent table from `BootInfo.boot_extent_manifest`; if the manifest is absent or a path is missing from it, direct boot-volume helpers fail closed. Directory traversal and generic FAT fallback must stay out of ring0 so namespace policy stays in `vfsd`/`storaged`.
- AHCI/NVMe post-bootstrap selection, inventory, partition, and extent policy lives in `storaged`/`vfsd`, but physical boot-volume block reads still require kernel io-manager transport substrate. Do not delete AHCI MMIO/DMA command execution until an explicit ring3 service-driver protocol can perform real block I/O before `rootd` and `vfsd` need it.

## Input Surface (`inputd`)

- Linux input reads call `InputdIpcRequest` with `INPUTD_IPC_OP_READ`.
- Compat input-device reads call `INPUTD_IPC_OP_AUTHORIZE_READ` before
  `INPUTD_IPC_OP_READ`; inputd consumes the authorization by `(pid, tid, fd,
  access)` so raw reads are not accepted as standalone event-drain authority.
- inputd owns a single System-class `inputd-dvm-ingress` worker. It waits in
  `SYS_RUSTOS_INPUT_WAIT_BROKER`, which arms before sleeping and rechecks raw
  producer/consumer cursors after registration; the MSI-X leaf only wakes that
  worker. Its dedicated kernel wake slot is separate from the finite generic
  application poll-waiter set, and a dead predecessor is reclaimed before an
  inputd restart may arm it. The worker alone calls gated
  `SYS_RUSTOS_INPUT_INGEST_BROKER`, drains at most
  `INPUTD_INGEST_MAX_EVENTS` records per turn, then yields before a recovery
  batch. Thus a stalled or absent application reader cannot fill the fixed DVM
  ring, while no periodic polling loop is introduced. L0 signals the one
  MSI-X doorbell only on an empty-to-nonempty transition (or an authenticated
  cleanup record); the post-arm cursor recheck makes a suppressed duplicate
  edge safe and prevents one interrupt/context switch per pointer frame.
- `INPUTD_IPC_OP_STATS` (the kernel's poll recheck) and an authorized
  `INPUTD_IPC_OP_READ` also refresh ingress under the same inputd queue lock,
  closing the wake/read race without becoming the sole progress path. Inputd
  reports only its service-owned policy queue, returns a bounded
  `InputdReadResponse` capped at 32 KiB, and uses fixed-size ingress scratch.
  The non-consuming `STATS` probe has a 16 ms IPC reply deadline, so a wedged
  inputd cannot freeze the UI poll loop behind the generic service timeout; a
  retry sees either unchanged ingress or the already-transferred policy record.
  The endpoint server still blocks in `SYS_RUSTOS_IPC_RECV` while idle.
- RustOS-native input polls and uiserver use readiness-then-read safely:
  `INPUTD_IPC_OP_STATS` and `INPUTD_IPC_OP_READ` both refresh ingress before
  observing policy state, and generic poll/epoll rechecks service policy after
  every generation wake before returning an event. The latency-sensitive
  uiserver reader performs a zero-time
  poll, whose non-consuming `STATS` request has the 16 ms IPC bound above,
  before entering the stateful authorized `READ`; it never starts that generic
  30-second service transaction merely to discover an empty queue. The reader
  retains its cumulative 4 ms cadence, so missed slots never accumulate burst
  credit. Inputd now also publishes a monotonic service-owned readiness
  generation when its policy queue changes from empty to nonempty. Generic
  poll/epoll observes that generation without moving input policy or transport-
  consumer authority out of inputd.
- Inputd is one provider of the common cross-service wait-set ABI. vfsd owns a
  bounded interest registry keyed by provider object identity and exact service
  epoch; netd and inputd own readiness truth and monotonic generations. Compat
  performs check, bounded waiter registration, provider recheck while runnable,
  scheduler arm, exact waiter-presence recheck, commit, and a final
  authoritative provider recheck. It supplies finite/infinite
  timeout and signal-cancellation substrate but never inspects a
  service-private queue. Empty transient poll sets still use the same scheduler
  arm plus deadline/signal wake rather than spinning.
- Wait interests follow open-description lifetime rather than reusable numeric
  descriptors. Socket and epoll service references are acquired by dup/fork,
  released by close/CLOEXEC/process exit, and purged after the final reference.
  IPC descriptor export likewise acquires the service-backed open-description
  reference before publication; successful installation adopts it, while
  cancellation or rejection moves it to the process substrate's bounded
  deferred-drop queue. That queue shares the 1,024-entry admission ceiling
  with live transfers, and housekeeping releases at most 32 entries per turn
  through bounded provider cleanup, so task retirement cannot orphan input or
  socket readiness authority.
  A provider restart advances endpoint authority, wakes matching waiters, and
  makes the old epoch fail closed instead of accepting a reused token. Unsupported
  edge-trigger and one-shot flags are rejected until their service-owned
  rearm contracts exist; they are never silently approximated.
- The source/model boundary is implemented, but runtime acceptance still needs
  the current change set's bounded QEMU/KVM event and timeout evidence. See
  `physical-gpu-status.md` for that evidence boundary.
- KVM latency evidence records input arrival at uiserver queue consumption,
  before a GPU backpressure retry may leave the turn. The previous end-of-turn
  sample counted consumed input and cursor motion but skipped their timestamp
  on that retry, fabricating 100+ ms gaps. One-second profile rollover preserves
  the prior arrival timestamp so a real cross-window stall still fails closed.
- inputd must coalesce lossy DVM pointer motion to the latest delta while
  preserving keyboard and pointer-button edges. Linux key translation and
  modifier/text state remain inputd policy; RustOS receives only bounded,
  authenticated relay records.
- Input IPC ABI version 2 extends `InputIngressWire` with a distinct,
  report-atomic DVM absolute-pointer position. It is never reinterpreted as a
  relative delta: complete `SYN_REPORT` positions are bounded to the declared
  surface and identical positions are idempotent. The relative-pointer kind
  remains separate for truly relative devices. Native PS/2 and raw HID-report
  ingress are not accepted ABI variants; an unknown kind, access mode, ABI
  version, or provenance flag is discarded rather than translated into a
  fabricated event.
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

Before any mapping broker call, parsed ELF64 and PE64 regions cross the shared
`rustos-image-admission` gate. Every region must remain inside the single
process window, have a non-overflowing nonzero extent, not overlap another
region, and satisfy W^X. A main-image entry must fall inside an executable
region; only a PE DLL may explicitly use entry zero. The same crate parses the
bounded ELF64/PE64 layout bytes and validates PE64 base relocation and import
tables against an isolated image snapshot. Loaderd retains format policy and
exact file reads, while a short read in the process broker aborts rather than
committing a zero-filled file tail. The TLA+ models, source-level tests, and
`fuzz-host --target image-admission` are complementary gates.

**ELF:** loaderd emits `PT_LOAD` mappings for main image + `PT_INTERP` via `MAP_FILE_BROKER`. Static-PIE biases: main = `PROC_BROKER_USER_SPACE_BASE + 0x0040_0000`, interpreter = `+ 0x0200_0000`. Must use `SYS_RUSTOS_PROC_SET_LINUX_RUNTIME_BROKER` for minimal launch metadata (entry, phdr, phnum, phent, brk_start, interpreter_base) — **not** raw blob streaming via `SET_IMAGE_BLOB_BROKER`. Kernel derives launch state from this metadata and the pre-built address space.

**PE64:** PE validation, section materialization, base relocation, import/export resolution, staged system-DLL registry lookup, PEB/TEB/runtime blob construction all happen in loaderd before commit. The bounded section-header table is fetched with one `pread64`, not one policy/VFS roundtrip per section. PE64 commit includes `SYS_RUSTOS_PROC_SET_WINDOWS_RUNTIME_BROKER` after all `MAP_DATA_BROKER` ops and before `COMMIT_BROKER`. Kernel validates metadata + spawns the materialized address space but **must not** reintroduce PE import/export/system-DLL policy.

Commit (`COMMIT_BROKER`) builds the child address space from recorded mappings.
When a broker request supplies `console_session = 0`, the kernel inherits the
exact live caller (or target thread) session. Missing process/thread state is
`EINVAL`; it must never manufacture the privileged system session.
By default, commit-broker spawns do not request immediate deferred reschedule so
`loaderd` can reply to its caller before the spawned child runs startup policy.
Supervisors (`rootd` for initd, `initd` for post-init services, and `runtimed`
for every catalog launch) set `LOADER_SPAWN_FLAG_DEFER_START`. `runtimed`
records ordinary app ownership before `LOADER_OP_ACTIVATE`; endpoint-owning
services also complete lease admission before activation and are then confirmed
through the exact-PID endpoint wait syscall. Activation is single-use, fails
closed for an unknown, exited, already-running, or non-suspended PID, and
publishes one spawn-specific scheduler handoff only after this supervisor
commit point.

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
use this direct bootstrap handoff. IPC enqueue/reply
completion wakes the receiver/caller and may set a direct pick hint for any
currently ready and
schedulable target; already-runnable service endpoints still need a handoff when
the caller queued work before the service re-entered receive. Synchronous IPC
enqueue only sets the receiver handoff hint; the caller must arm and re-poll its
reply wait before yielding so a fast service reply cannot race a not-yet-armed
waiter. Generic IPC hints are caller-local and the newest eligible receiver
replaces older generic hints, even across scheduling classes; stale high-class
service hints must not block the service that the current caller is waiting on.
A generic receiver hint is consumed when the server blocks or the scheduler
next selects work; it must not switch away from a live syscall continuation.

The separate local-socket completion path accepts a netd latency flag only
from the currently registered netd endpoint, only for the exact versioned
response length, and only for successful local data/event transfer or the one
armed nonblocking-receive drain. It queues only ready User tasks in a bounded,
deduplicated FIFO and requests a user-return reschedule edge. Unknown flags,
foreign endpoints, System targets, stale tasks, and malformed replies cannot
create scheduler authority. The 100 us PIT edge is an upper bound if the
normal user-return scheduler check is missed; no handoff preempts an arbitrary
kernel frame.

## Network Surface (`netd`)

Routed Linux ops after bootstrap: `socket`, `socketpair`, socket `dup`/`dup2`/`dup3`, socket `close`, socket `read`/`write`/`writev`, `bind`, `listen`, `accept`/`accept4`, `connect`, `sendto`, `recvfrom`, `sendmsg`, `recvmsg`, `getsockname`, `getpeername`, `setsockopt`, `getsockopt`, `shutdown`.

netd invokes gated `SYS_RUSTOS_NET_BROKER` with target pid. Net broker arg struct carries six 64-bit syscall arg slots. Kernel performs handle install and target user-memory validation/copy; AF_UNIX socket lifecycle, binding/listen queues, byte queues, and socket option policy belong to netd. Version-2 `NetdIpcRequest`/`Response` frames copy an exact 120/32-byte header plus only the actual bounded payload, rather than a fixed maximum-size buffer. They carry `socket_token`, fd `status_flags`, and a bounded inline payload for this service-owned socket path.

Four fixed netd request receivers prevent an unrelated caller from serializing
all local AF_UNIX work. Blocking INET connect/send/recv waits run in a separate
fixed eight-worker pool that releases the shared network-state lock between
polls; there is no thread-per-request fallback. All socket poll/epoll waits now
query netd readiness generation, register the shared bounded wait token, and
re-query before sleeping and after wake. Because the DVM packet ring currently
exposes no userspace interrupt edge, netd owns one fixed 1 ms INET ingress
worker; one bounded smoltcp poll per turn publishes a generation only when
smoltcp reports a socket-state transition. The worker admits at most 32
ingress packets before yielding. This lets new external packets wake
an already-sleeping generic wait without moving network policy into ring0.

This removes avoidable copies and waits, but standard AF_UNIX data still makes
a synchronous compat-to-netd service round trip for every send/receive. The
current WayClick KVM gate proves that this path does not yet sustain 55 FPS.
After removing uiserver's cursor-only early-present lane and moving frame
callbacks onto a previous-presentation permit, the signed post-cleanup
artifact's settled standard-client windows reach 35.894--41.776 FPS,
compositor callback wait is normally 2--4 ms, and the private GPU proof reaches
110.389 FPS with 9.057/14.392 ms average/maximum GPU time.
The remaining limit is therefore not a private rendering API, ordinary
`wl_shm` copy, or the GPU proof. The generic wait-set now gives ordinary Linux
applications a bounded aggregate wait over vfsd-owned interests backed by
netd/inputd readiness generations, exact provider epochs, and monotonic
deadlines. It deliberately leaves socket data and namespace/options policy in
netd and does not add a WayClick-specific route. Wayland-server's existing
backend epoll fd aggregates the changing client set; uiserver duplicates that
open description into a demoted readiness worker, merges its single-capacity
wake with the input reader's wake, and rearms only after main-loop dispatch. A
client commit can therefore wake the main loop before its runtime deadline.
Runtime/QEMU evidence remains a release gate; source/model completion alone
does not satisfy the 55 FPS gate.

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
- `SYS_RUSTOS_SCHED_DEMOTE_SELF` is a narrow scheduler substrate for an
  already-running user thread to irreversibly surrender its inherited base
  System class. It takes no priority argument, cannot promote any thread, and
  preserves only live reply-scoped IPC inheritance until that reply's normal
  release. uiserver uses it for background and untrusted client-accept workers;
  failure terminates the UI process instead of letting those workers contend
  with input/present at System priority.
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
- Task weights = microsecond vruntime budgets (default 100 µs). `uiserver` gets a longer render/present slice, and latency-critical brokers it calls in-frame, especially `inputd`, must stay in the same order of weight so UI loops do not block behind input IPC. Runtime catalog metadata is not a realtime capability: `runtimed` pins only the exact UI executable to System weight and clamps every other launch below System admission before calling `loaderd`.
- The max-burst guard rotates to another ready peer within the current
  scheduling class even when the current task's weighted vruntime still wins.
  Separately, after two consecutive System dispatches, one ready User task must
  run before System selection resumes. Each ready User task also has an 8 ms
  age bound, so a single busy User task cannot consume every reserved turn.
  Authenticated User-only event handoffs use a deduplicated 16-entry FIFO and
  are capped at eight consecutive picks. These are the explicit recovery and
  application CPU reservations under DVM/UI load.
- Normal and voluntary-yield task selection each scan the fixed 128-slot table
  once and retain one minimum-vruntime candidate per class. Do not restore a
  separate full-table pass for each empty higher class: effective class
  resolution also walks live reply donations, so repeated scans multiply the
  timer-tick cost under User-only and recovery workloads.
- Strict admission for bootstrap syscall/VFS/loader/process/pager brokers comes
  only from `rootd`'s fixed manifest. Dynamic package metadata cannot set
  `TASK_WEIGHT_INTERACTIVE_FLAG`; admitted ready brokers are covered by the
  bounded System-class wait rail.
- `KernelSpinLock` must not be held across disk/filesystem/IPC/framebuffer-copy loops. Use `KernelWaitLock` or split the section; add `cond_resched` in long loops.
- Boot service order: driver/input/storage policy services before UI launchers. `runtimed` waits on `devmgrd` and `storaged` endpoints before UI bootstrap.
