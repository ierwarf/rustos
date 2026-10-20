# Driver-domain wire and ownership contract

[Contract index](../contracts-abi.md).

## Driver domain contract

Linux kernel modules execute only inside the Linux DVM. RustOS neither stages nor
loads module images, probes module aliases, or retains native USB, PS/2, or
virtio-net fallback providers. A missing DVM transport leaves that device
unavailable; it must not select an alternate in-kernel provider.

RustOS accepts only bounded DVM transports: RDI1 version-3 records in a host-owned 128
KiB ivshmem input ring with 2,048 fixed 32-byte slots and exactly one MSI-X
wake vector; fixed layouts for display control/pixels and Ethernet frames. The DVM
never maps the input ring. L0 is its sole producer, RustOS is its sole
consumer, and the producer/consumer cursor cache lines are distinct. Ring0
validates only aperture/header/cursor bounds and copies fixed records stamped
with the observed transport generation. `inputd` validates RDI1
magic/version/checksum, epoch, sequence, key/pointer bounds, and session
lifecycle; `netd`, `storaged`, and `uiserver` own their device policy above the
generic transport substrate. Input attachment is serialized, has a
boot-wide eight-attempt recovery budget, and may reuse its permanent MSI-X
vector only for the originally pinned PCI aperture; revoke releases the old
MMIO mapping. The sole accepted GUI topology is the V3 `RSGUI002` three-slot
GUI-DVM pool; V2, polling, a firmware framebuffer, and a native-GPU fallback
are not parsed or selected.

The block aperture and RDI1 input ring are atomic shared RAM, not generic
MMIO: RustOS admits only an exact prefetchable non-I/O BAR and maps it WB;
the Linux block relay requires the same `IORESOURCE_PREFETCH` resource and
clears x86 PAT cache bits before `remap_pfn_range`, while L0's input producer
uses ordinary `MAP_SHARED`. The MMIO registry rejects every overlapping
physical interval with a different cache mode before choosing either the
direct-map or high mapping window, including an interval straddling the
direct-map limit. Publication metadata reserves before either direct-map or
high-window mutation; direct-map override, new-page, and retained-index
metadata each reserve their worst-case page count before a PTE change. No
`mmio::map` boolean selector exists. Each AP rejects a non-WB preexisting
PAT0 rather than retyping an active selector, then programs PAT2=UC/PAT4=WC
and exactly reads back the full expected PAT MSR before
`OnlineParked`/private-ready publication; WB is the empty selector. These
rules cover block/input control memory only: display pixel memory remains its
separate cache-mode contract and uses the io-manager WC mapper. Raw
direct-map retyping is limited to kernel-mm's permanent boot-MMIO path and
io-manager's owned mapping transaction: the BSP retypes the existing APIC leaf
UC before APIC/PIC use, while generic high-window mapping rejects every
physical address below the direct-map limit.

When the RDI1 version-3 ring reaches its normal reserve-preserving limit, L0 waits for
consumer credit for at most 50 ms rather than dropping a valid user frame or
spinning. Progress resumes on the observed consumer cursor; no credit before
the deadline is a fail-closed transport-health failure. Cleanup uses its
separately reserved lifecycle capacity and follows the same bounded rule.

RDI1 version 3 preserves pointer semantics end to end. Relative evdev devices produce
bounded delta records; absolute tablets produce bounded `0..1599 x 0..899`
position records only after a complete `SYN_REPORT`. Partial X/Y reports are
staging state, identical positions are idempotent, and neither L0 nor `inputd`
may reinterpret an absolute position as a relative delta. Ring0 has no pointer
semantics at all. The inputd
ingress ABI uses distinct packet and position payloads so a provider cannot
make one physical report travel through both paths.

The GUI-DVM contract fixes three page-aligned host-provisioned surfaces and 64-byte
`PRESENT`/`RELEASE` records, never guest-selected pointers, vectors, or
variable-length payloads. The ivshmem BAR contains only uncached control state;
the separate cacheable pixel device is writable only by RustOS QEMU and is
read-only/ROM in the DVM. Every READY slot is a complete immutable snapshot,
even when its PRESENT damage hint is partial. RustOS may construct that
snapshot with a damage-only patch only after reclaiming a FREE slot whose
retained pixel-content generation is either the exact immediately preceding
generation or is covered by the complete contiguous damage history retained
for the fixed three-slot pool, and whose source mapping is unchanged. Retained
content generation is not release authority. An uninitialized slot, missing or
invalid history, a source/topology replacement, or full damage forces a
complete copy. The DVM module requires matching
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
The exclusive RustOS install transaction sends its initial nonzero/even prime
invitation before publishing the installed transport. This control-only request
creates no frame slot or presentation evidence: mandatory GPU startup must not
wait for a CPU-composed frame to request its own provider proof. Normal discovery
only drains the response; it cannot overwrite a newer invitation. Matching
context/epoch/fence and invitation ACK remain prerequisites for readiness.
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
are copied once from uiserver's complete service-owned atlas into the exact
selected host-created transport slot. Display GPU ABI v5 maps only that
slot-scoped pixel range into uiserver with the same write-combine memory type
as its ring0 alias; command/control pages and other slots remain unreachable.
The commit request carries no user pointer. Ring0 validates the surface
capability, slot, generation, bounded damage, and command batch, publishes
metadata, and signals the DVM without touching atlas pixels. A generic
ring0 user-copy path or a separately allocated intermediate surface is
forbidden.
When a rotated slot needs several contiguous epochs, uiserver retains disjoint
damage rectangles and unions only overlapping rectangles. It must not turn
distant cursor and client damage into their atlas-sized bounding rectangle.
If the accumulated set exceeds `DVM_GPU_ATLAS_MAX_DAMAGE_RECTS`, the only
allowed fallback is an explicit full-slot snapshot before publication.
Steady-state selection prefers the exact predecessor, otherwise the cheapest
complete reconstruction whose damage is at most one eighth of the atlas.
When every released slot is older or more expensive, submission returns
backpressure and the retained visual update is coalesced until a recent slot
completes. It does not publish a partial snapshot or amplify that update into
an avoidable full-slot copy.
Large slot rows use aligned non-temporal stores into the write-combine mapping,
and one store fence completes the copy before the commit ioctl publishes its
generation. Small damage retains the ordinary bounded copy path.
Console texture content and scene topology have separate generations.
Output or terminal-cursor changes rebuild and copy only the bound console
rectangle. Window position changes only the destination of the exact retained
texture command and must not rebuild or copy atlas pixels. The binding includes
the authenticated layer index and source rectangle; a mismatch fails closed
into a structural rebuild. Window identity, dimensions, visibility, focus,
title, or ordering changes force that rebuild. Content versions and `x/y`
position alone must not alter the structural reuse signature.
Wayland pointer motion is coalesced to the latest coordinates on the same
15 ms frame cadence. Focus transitions still emit immediate enter/leave
events, and button delivery first flushes any pending motion so observable
protocol order and click coordinates remain exact.
Runtimed's pre-catalog uiserver launch uses a sealed two-variable bootstrap
environment and an empty argument list. Later catalog admission must match the
same executable, weight, privilege flags, arguments, and ordered environment
exactly; mismatch fails the catalog instead of silently drifting. The boot
path must not enumerate the complete applications directory before uiserver.
The DVM then uploads those damaged rectangles into a virtio-GPU texture, after
which the fixed GLES commands perform the actual composition into a
DVM-private output. This is a valid
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
authorized network peer. `inputd` authenticates the RDI1 session lifecycle and
sends an identity-stamped request to `netd`; netd validates the live inputd
service owner and chooses the exact nonzero packet generation. Only netd may
grant/revoke that generation through its capability-gated packet broker.
Ring0 enforces nonzero grant, exact revoke, startup reset, and bounded ring
memory access but knows nothing about input-session semantics. An old end
marker cannot revoke a newer generation, and guest-writable ivshmem headers,
ready bits, counters, or frames cannot create a lease. Lifecycle records never
carry Ethernet data. The aperture remains
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
The former driverd/service-driver broker surface is deleted. Its numeric slots
are permanent fail-closed tombstones, not reserved future service endpoints.

The retired `compat-slowpath-ring3`, `pager-slowpath-ring3`, and
`process-slowpath-ring3` planning tables have no live ring0 source entry.
Their ownership constraints remain: syscall entry, page tables, scheduler
state, and privileged transport stay ring0 substrate while service-visible
policy remains with the owning service.

