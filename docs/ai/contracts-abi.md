# AI Contracts — Kernel/Service ABI

IPC service IDs, broker syscalls, handle transfer, and service routing. For package/stage/build/logging: `contracts-infra.md`.
Cross-owner composition is indexed by `system-flows.md` and
`formal/system-flows.tsv`; this file remains the detailed owner/wire contract.

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
  while rebasing private maintenance state. Rootd, procd, and syscalld therefore
  own independent bounded lifecycle fan-out queues. Procd consumes exit evidence
  only for signal policy; syscalld consumes the same evidence directly before
  interpreting its next request and owns credential/MM-policy cleanup. Neither
  service calls the other or rootd during the drain, so queued recovery evidence
  cannot close a `rootd -> loaderd -> procd -> rootd` authorization cycle.
- Every lifecycle drain requires the exact current ABI version and zero reserved
  fields. Rootd overflow is sticky and terminal. Procd/syscalld overflow rebases
  only the affected private queue after its owner clears all cached per-process
  authority.

## IPC Service Registry

All raw and handle-carrying `SYS_RUSTOS_IPC_CALL*` waits use the same finite
30-second service ceiling. Timeout cancels the exact reply identity and drops
any transferred-handle authority; a late reply is invalid. Shorter provider or
application deadlines remain authoritative and may not be widened to this
ceiling. Server receive loops may wait for new work indefinitely because they
hold no caller reply, but every accepted synchronous request must reach reply,
timeout/cancel, peer-close, or owner-exit cleanup.

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
Rootd self-registration is a boot-only trust transition, not a permanent
fallback. The first successful rootd publication seals its non-reusable process
identity for the boot; revoke or exit never allows a foreign process to claim
`IPC_SERVICE_ROOTD`. A rootd failure is therefore fail-stop until reboot unless
a future explicit, authenticated replacement protocol is added.
Every service publication first proves that the endpoint is owned by the
publishing process. Non-root registration also binds rootd's authorization to
the exact rootd endpoint epoch and rechecks that epoch under the registry
mutation lock at commit; restart/revoke/exit between check and publish returns
`EAGAIN` and cannot publish stale authority.
Numeric endpoint IDs are not ambient call authority. A successful service
lookup records a bounded process-local grant for that exact service publication
epoch; ordinary IPC calls and calls carrying handles must present the endpoint
under that grant. Revoke/republication invalidates it by epoch, and process exit
removes it. Endpoints outside the live service registry are callable only by
their owner process. Raw endpoint values are not inherited/transferable
capabilities; future cross-process endpoint transfer must use an explicit
typed-handle grant and lifecycle contract.
Runtimed's Unix control socket is also an authority boundary, not a trusted
local channel. It reads `SO_PEERCRED` and ignores caller-supplied identity.
Snapshot, launch, and terminate require either the current live uiserver
service owner or a running process whose immutable launch record carries the
signed `logical_admin` bit. `UI ready` is uiserver-only. Socket path permissions
are defense in depth and never replace this per-request authorization.
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
Rootd receives supervisor requests through endpoint-owner-only sender-stamped
receive syscalls; both pre-init and post-init supervisor turns must drain
lifecycle/restart state before using `SYS_RUSTOS_IPC_TRY_RECV_WITH_SENDER`, so
a failed child cannot hide behind an indefinite rootd IPC wait. `SERVICE_CAPABILITY`,
`SERVICE_LOOKUP`, and `READINESS_SIGNAL` must reject payload `subject_pid/tid`
values that do not match the kernel-stamped sender PID/TID. The sender-stamped
receive path first re-authorizes the exact endpoint owner and is not a generic
app IPC ABI; kernel-stamped sender identity does not grant policy capability.
Rootd's supervisor loop must yield
between nonblocking receive turns rather than sleep-polling, because service
endpoint registration synchronously depends on rootd capability replies.
No service may hold a local policy/state lock across service discovery or a
synchronous cross-service call. Early-boot maintenance paths additionally
skip discovery when their drained work set is empty. A new bootstrap
dependency must either be covered by rootd's declared readiness order or use a
bounded, explicit ready handshake; startup scheduling luck is not readiness.
Rootd must also remain the sole consumer of its supervisor endpoint while a
loaderd launch is in flight. Because loaderd executable open can synchronously
enter `vfsd`, and vfsd commits open-description state through rootd's
checkpoint protocol, rootd may not block its supervisor thread in
`SYS_RUSTOS_IPC_CALL` to loaderd without another receiver. It delegates the
sole supervisor endpoint turn to exactly one same-process, fixed-stack worker
while the original rootd thread performs the loader call. The worker blocks in
endpoint receive, drains lifecycle evidence, and services nested loaderd/vfsd
requests without scheduler polling. After the loader call publishes the exact
PID or errno, the original thread issues the sender-stamped
`COMMERCIAL_MAX_ROOTD_OP_LOADER_WORKER_COMPLETE` wake on the same endpoint;
only that same-process request may advance `RESULT_READY` to `COMPLETE`, and
the original thread does not touch the borrowed supervisor state until the
worker publishes `EXITED`. The worker receives no new capability and concurrent
jobs fail closed; direct spawn and deferred checkpointing are not permitted
substitutes.

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
`SYS_RUSTOS_ENTROPY_BROKER` is limited to the Linux-syscall and network-policy
capabilities and returns at most one inline-IPC payload of bytes from the
boot-seeded ChaCha20 substrate. Initialization is one-shot and rejects an
absent/all-zero seed. A private master stream derives every child seed from
master output; consumers never clone the master or form related keys by
combining the boot seed with public PID/TID/counter state. Syscalld still owns
Linux `getrandom` flag, length, and error policy; netd still owns token
collision/admission policy. PID/TID/counter-derived pseudo-random output is
forbidden for credentials, object capabilities, ASLR material, and Linux
`getrandom`.
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
- Sender binding is mandatory for every published policy-service ingress, not
  only rootd. Direct requests require a nonzero exact PID/TID match.
  Service-to-service delegation is explicit: the receiver rechecks
  `SYS_RUSTOS_IPC_VALIDATE_SERVICE_OWNER` for the kernel-stamped sender PID on
  every delegated request and never caches the result across exit, revoke, or
  endpoint republish. Sender identity, lookup admission, object rights, object
  generation, and exact response binding are cumulative checks. The complete
  ingress registry is `formal/trust-boundaries.tsv`.
- IPC stability changes are substrate only. Service restart, admission, routing,
  and policy decisions stay in `rootd`/`syscalld`/`vfsd`/`loaderd`/other owning
  services, not in new ring0 policy tables.

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
- VFS device open responses use `VfsIpcResponse.aux` for device access metadata such as `INPUTD_ACCESS_*`; ring0 read paths must not classify `/dev/input*` by path string after the remote handle is installed.
- Linux `openat` installs `KernelHandle::RemoteVfs` for regular files + directories after vfsd registration.
- Linux `dup`/`dup2`/`dup3` acquire the exact service-side
  reference before publishing the local duplicate. `close` removes the local
  descriptor first, then performs token-addressed provider release; a wedged
  vfsd/netd cannot retain a reusable numeric fd or block process retirement.
  All close/dup/fork/CLOEXEC/exit provider reference operations and matching
  epoll purges use the 16 ms cancellable internal IPC path. vfsd is the only
  intended caller of `SYS_RUSTOS_FD_*_BROKER`; gated by `VFS_POLICY`.
  Generic apps must not call those brokers directly.
  Fork clones the address space and fd table in one process-state snapshot;
  provider refs are acquired from that frozen child table before publication.
  Resnapshotting the live parent afterward is forbidden because a sibling
  close/reuse could bind the child fd to one object and the provider ref to
  another.
  Exec cleanup is likewise derived only from the exact `KernelHandle` values
  removed by the atomic process-table CLOEXEC commit; a pre-commit CLOEXEC
  snapshot is not cleanup authority.
- A devmgrd ioctl route is policy advice, not fd identity. Compat must resolve
  the live fd-table entry again before dispatch: sessiond TTY operations require
  an actual `KernelHandle::Console`, so a closed/reused ordinary file fd
  receives `ENOTTY` and cannot read or mutate the caller's console policy.
- `LinuxSyscallOffloadRequest.arg0..arg3` carry 64-bit fd-control args (target fd, cmd, arg, flags). **Do not pack pointer/flag values into the 32-bit `mask` field.**
- `mount`/`umount2` route to vfsd → gated `SYS_RUSTOS_VFS_*_BROKER` for kernel mount-table mutation. Do not reintroduce direct generic-app `linux_ops::mount`/`umount2` paths.
- `poll`/`ppoll`/`epoll_*` route generic fd readiness policy and epoll
  interest state through `VFS_IPC_OP_POLL_QUERY`; socket readiness is `netd`
  policy, input readiness is `inputd` policy plus the narrow native-input
  wait/completion substrate, and non-system console-input readiness is
  `sessiond` policy. sessiond advances its generation only on an empty-to-ready
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
  EPOLL_CTL_ADD/MOD pins the exact provider or console open description across
  the vfsd mutation. Releasing that transaction guard performs normal
  last-close purge, so a concurrent final close cannot purge first and then
  leave a newly inserted interest for a retired object.
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
  descriptions, cursors, checkpointing, and the overlay decision; digest and
  exact-entry admission remain in the bounded ring0 reader.

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
  before storage-DVM readiness (`/lib64/ld-linux-x86-64.so.2`, `libc.so.6`,
  and `libgcc_s.so.1`) in addition to bootstrap services and registries.
  Missing runtime members fail image staging; boot never falls through to a
  physical controller or an unverified host filesystem.
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
  not acceptable evidence for this boundary.
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
- AHCI admission includes the complete minimal Linux driver closure: `ahci`
  owns the PCI controller and signed `sd_mod` must publish its one whole-disk
  block namespace before the relay starts. NVMe uses its native namespace
  driver directly. Missing class-driver or namespace publication fails closed.
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
  Storaged retries INFO behind that atomic waiter for at most 15 seconds in
  one-second slices. Withdrawal after a successful observation is revoke, and
  timeout never selects early-system or a physical-controller fallback.
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
  shared ring, the Linux DVM completes the backing-device flush, and storaged
  consumes the exact completion. Storage-DVM KVM acceptance requires this
  proof in addition to both peer readiness flags and exact geometry.
- Storaged may satisfy repeated FAT metadata and executable reads from at most
  eight validated 64-KiB read-ahead windows (512 KiB total), but it first
  revalidates live DVM geometry and the exact caller generation on every
  request. A hit must be wholly contained in one exact-generation window. A
  miss submits one forward window no larger than the fixed DVM slot;
  overlapping replacements cannot coexist, a different generation atomically
  replaces the complete cache epoch, and every write clears all windows before
  submission. Transport-info failure also clears them. Cached bytes therefore
  cannot survive revoke, restart, or mutation, and the optimization adds no
  controller authority or hidden fallback to vfsd.
- Storaged invokes RustOS-private syscalls only through the raw
  `rustos-svc-runtime` entrypoints. A libc wrapper must not collapse raw
  negative results into `-1` plus TLS `errno`; transient `EAGAIN`, absence,
  revocation, timeout, and protocol failure remain distinct across the full
  kernel-to-vfsd path.
- Rootd's least-authority service graph contains the explicit
  `vfsd -> storaged` edge. Before storaged publication, lookup preserves the
  transient registry errno; denial preserves `EACCES`. Kernel lookup validates
  canonical error responses before applying success-only value invariants, so
  neither state can be fabricated as `EINVAL` and cached as permanent.
- Runtimed keeps `uiserver` outside the early-system bootstrap image. A
  transient loader or DVM-volume error schedules a bounded 500-ms supervisor
  retry; only the existing permanent-launch errno set disables the entry.
  Successful spawn ownership prevents duplicates while endpoint readiness is
  pending.
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
  bounded interest registry keyed by the target fd plus stable provider object
  identity, matching Linux's fd/open-description pair; the observed service
  epoch is mutable registration state, so an explicit MOD can rebind an
  existing interest after provider restart without creating an undeletable
  duplicate. Netd and inputd own readiness truth and monotonic generations. Compat
  performs check, bounded waiter registration, provider recheck while runnable,
  scheduler arm, exact waiter-presence recheck, commit, and a final
  authoritative provider recheck. It supplies finite/infinite
  timeout and signal-cancellation substrate but never inspects a
  service-private queue. Empty transient poll sets still use the same scheduler
  arm plus deadline/signal wake rather than spinning.
  A 16 ms internal provider-query timeout never becomes the Linux-visible
  application timeout: readiness already found elsewhere in the same scan is
  returned immediately, otherwise the scan retries until the caller's original
  deadline or an unmasked signal settles it.
- Wait interests follow open-description lifetime rather than reusable numeric
  descriptors. Socket and epoll service references are acquired by dup/fork,
  released by close/CLOEXEC/process exit, and purged after the final reference.
  Remote VFS descriptor references are different: the kernel fd table is their
  sole refcount authority, so dup/fork/transfer performs no ambiguous remote
  increment. Vfsd receives only initial open and idempotent final close.
  Dup/fcntl snapshots one source handle token, acquires its provider reference,
  then revalidates that token under the fd-table lock before commit. Exact-fd
  replacement returns the handle actually retired at that linearization point,
  so concurrent close/reuse cannot redirect either acquisition or cleanup.
  IPC descriptor export likewise acquires the service-backed open-description
  reference before publication; successful installation adopts it, while
  cancellation or rejection moves it to the process substrate's bounded
  deferred-drop queue. That queue shares the 1,024-entry admission ceiling
  with live transfers, and housekeeping releases at most 32 entries per turn
  through bounded provider cleanup, so task retirement neither waits without a
  bound nor silently drops locally queued cleanup.
  Every remaining provider-side reference mutation carries a kernel-minted
  128-bit operation ID. Netd reserves a bounded replay slot before applying a
  mutation, replays the exact result, rejects operation-ID aliases, and retains
  completion until the kernel ACK. Compat retries the same operation ID and
  keeps unresolved mutation/ACK work in a bounded fail-closed reconciliation
  queue. Vfsd epoll mutations are committed to rootd's authenticated service
  checkpoint before local publication and exact retries cannot advance a
  revision twice. Queue exhaustion is `ENOSPC`, never fabricated success or
  silent eviction.
  A provider restart advances endpoint authority, wakes matching waiters, and
  reports the affected interest as `ERR|HUP` instead of failing the aggregate
  wait or accepting a reused token. The stale epoch remains revoked until an
  explicit MOD rebinds it or DEL removes it; DEL validates only the stable
  fd/open-description key and remains available while the provider is down.
  Unsupported
  edge-trigger and one-shot flags are rejected until their service-owned
  rearm contracts exist; they are never silently approximated.
  On vfsd restart, rootd returns the versioned opaque vfsd checkpoint only to
  the current authenticated vfsd lease. The replacement reconstructs epoll
  refcounts and every provider/object/epoch/events/data interest before it
  creates or publishes its endpoint; malformed, duplicate, stale, or partial
  replay fails closed. Ring0 does not mirror service policy as a fallback.
  Crash-injection runtime evidence is still required before release acceptance.
  Ordinary remote VFS open descriptions now use the same restart boundary.
  Vfsd durably stages the parent and every normalized-path chunk before the
  live record binds kind, cursor, length/content identity, and mutable status
  flags to the kernel-minted capability. If the OPEN response remains
  uncertain, compat closes the exact proposed capability; vfsd can tombstone
  either a staging parent or the completed handle, so neither becomes an
  ownerless fallback object. Sequential read/getdents leaves a
  prepared cursor result until compat commits after successful user copyout or
  cancels on failure. Kernel fd lookup and temporary reference acquisition are
  one atomic close-exclusion step; dup/fork/transfer references remain
  kernel-owned. Final close preserves its exact tombstone across reply loss and
  service restart, and only a separate kernel visibility ACK authorizes exact-
  proof rootd compaction. A stale proof cannot erase a reused key. Source,
  focused tests, and `vfs-open-description-recovery` cover these invariants;
  current-image crash injection and long-churn runtime evidence remain release
  acceptance gates rather than being replaced by a path-reopen fallback.
- Standard input, output, and error descriptors are ordinary Arc-backed console
  open descriptions rather than numeric fd exceptions. Sessiond-backed output
  and error report `POLLOUT` only while their exact session remains live and
  report `POLLHUP` after revoke. An empty `O_NONBLOCK` console read returns
  `EAGAIN` from the first bounded sessiond reply and never re-enters the blocking
  retry loop. Session close is terminal for that session generation: stale
  read, write, TTY, and input-injection requests return `ENODEV` and cannot
  recreate service state or turn a HUP token live again.
  Console liveness counts only descriptor-table ownership: temporary syscall
  snapshots do not postpone the final-close purge or suppress its waiter wake.
- The latest bounded 30-second QEMU witness predates the current entropy,
  checkpoint, and transactional-FD changes. It reached initd and repeatedly
  delivered the expected no-input-device child exits without a
  rootd/loaderd/procd authorization cycle, panic, or stalled lifecycle drain,
  but it must not be cited as runtime evidence for this change set. A newly
  signed image and current-source boot/crash-injection capture remain required.
  The repository KVM launcher also passes its dry-run contract checks, while
  the physical live-KVM gate remains separately unclaimed because host
  admission correctly rejects the available NVIDIA render node. See
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
- Loader request ABI v2 requires `requester_pid` to equal the kernel-stamped
  IPC sender. Process broker ABI v2 carries that identity into deferred
  commit: ring0 binds the suspended target PID to the exact requester in a
  bounded registry. ACTIVATE consumes that pair once; foreign callers and
  replays fail, loaderd restart cannot transfer or erase the authority, and
  requester exit revokes the entry and retires the still-suspended target.
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

A prepared remote file mapping may use a larger internal copy buffer, but each
fallback read from vfsd is capped to the versioned VFS IPC payload.
Bootstrap-backed reads may fill the larger buffer directly. VFS IPC version 4
uses the maximum 64-KiB inline message with a fixed 40-byte response header and
the remaining 65,496 bytes as payload, so one kernel copy window requires at
most two bounded reads. The response layout and exact maximum size are source
assertions; a larger request is chunked instead of becoming an
executable-specific `EOVERFLOW`.

Commit (`COMMIT_BROKER`) builds the child address space from recorded mappings.
When a broker request supplies `console_session = 0`, the kernel inherits the
exact live caller (or target thread) session. Missing process/thread state is
`EINVAL`; it must never manufacture the privileged system session.
One loader request is one synchronous reply-capability transaction. Image
reads and nested policy IPC remain ordinary preemption/block points, but
loaderd must not insert voluntary scheduler yields between mapping,
runtime-metadata publication, and commit while retaining that reply. Such
yields neither release ownership nor improve correctness and can expand a
bounded launch by whole scheduler fairness windows under concurrent System
services. The idle/failed receive loop may still yield because it owns no
caller transaction.
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

Every syscall captures the entering user's complete SIMD/FPU image in a
syscall-scoped snapshot distinct from the scheduler's per-task SIMD slot. If a
blocking syscall yields, the task slot is allowed to hold the suspended kernel
continuation's SIMD scratch state; syscall return must restore only the
entering-user snapshot. Reusing the scheduler slot for both lifetimes violates
the syscall register-preservation ABI and can corrupt userspace request
structures assembled with XMM/YMM registers after the wait.

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
- Privileged loader requests use service roles, never caller-supplied PIDs.
  `SPAWN_EXEC` requires the current rootd, initd, or sessiond endpoint owner;
  `EXEC_TARGET` requires procd. Initd's endpoint is identity-only and has no
  receive surface. Loaderd checks on ingress and ring0 repeats the check at the
  final commit, after image loading, so role restart/revoke invalidates the
  request before flags, weight, console session, or target image can commit.
- Post-init readiness is not a PID announcement. Rootd requires the exact
  declared executable path and asks a rootd-only process broker to prove the
  still-unconsumed deferred-spawn `(target_pid, reporter_pid)` binding before
  publishing the lease. Capability registration then independently requires
  the reported PID to own its endpoint; reporter exit revokes descendants.
- `KernelSpinLock` must not be held across disk/filesystem/IPC/framebuffer-copy loops. Use `KernelWaitLock` or split the section; add `cond_resched` in long loops.
- Boot service order: driver/input/storage policy services before UI launchers. `runtimed` waits on `devmgrd` and `storaged` endpoints before UI bootstrap.
