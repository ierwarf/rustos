# Linux driver-domain model

This directory builds the immutable x86_64 Linux appliance used by RustOS
Linux driver domains. It is a hardware driver provider, never the policy
owner.

```text
RustOS KVM guest           Linux DVM KVM guest
inputd/netd/uiserver       Linux LTS + firmware + supported .ko
          |                          |
          +---- L0 versioned brokers -+
                                      PCI-assigned IOMMU group
```

## Ownership

- The KVM host exclusively owns VM memory, interrupt remapping, IOMMU mappings,
  device assignment, revocation, and reset.
- RustOS services own admission, routing, mount, user, update, and recovery
  policy.
- A Linux DVM owns only the PCI/IOMMU group assigned to it and can export a
  normalized data plane only after a versioned KVM transport endpoint is
  implemented. It must not receive RustOS filesystem,
  desktop, package-manager, or general management authority.
- One PCI/IOMMU group has exactly one active owner. Failed revoke/reset is a
  fail-closed condition, never a reason to reassign the device anyway.

## L0 launch-plan and VFIO lease

`rustos-hostd discover` reads the host IOMMU topology and `rustos-hostd
preflight --plan ...` validates a `launch-plan-v1.env` contract. The plan must
name the complete actual IOMMU group, not a single desired PCI function, and
must reject any host-protected PCI BDF in that group. Both commands are
read-only: neither unbinds a driver, assigns VFIO, resets hardware, nor starts
a DVM. Hostd also derives display safety from live sysfs: `boot_vga=1` and any
connected DRM connector are unconditional assignment failures, even if an
operator omitted that BDF from the plan's protected set. Activated acquisition
repeats the live check after durable prepare and immediately before unbind.
`rustos-hostd preflight-physical` additionally verifies the exact production
QEMU, display-only policy, schema-8 bundle, writable `/dev/iommu`, and a
successful empty-IOAS allocate/destroy ioctl probe without changing a binding.
It also requires a 4 GiB soft `RLIMIT_MEMLOCK` budget before any VFIO mutation,
covering the fixed 2 GiB guest RAM, display backing, and bounded pinning
headroom. Supervision repeats the limit check before reset and launch.
For an `amdgpu` display it also parses the bounded host ACPI VFCT table before
binding, requires its ACPI checksum and one exact bus/device/function/vendor/
device image, accepts only either an exact populated subsystem pair or the
firmware-defined all-zero absent pair, and validates the image's 0x55aa and
ATOM headers.
It also derives the maximum impact scope of the enabled reset ordering.
`bus`/`cxl_bus` is accepted only when every function on the affected bus is in
the same admitted lease; missing, unknown, or escaping reset scope fails closed
because IOMMU-group isolation alone does not prove reset-domain isolation.
Before binding, hostd also requires the vfio-pci `disable_idle_d3` parameter to
be latched on new devices. It clears and verifies PCI bus-master immediately
after bind and around each reset, so an idle power-state restore cannot leave
DMA enabled before the non-identity IOMMUFD mapping exists.
`acquire --activate` repeats that complete reversible gate
after signed-release admission and before it writes prepared state or detaches
the group from its host drivers.
Supervision repeats the VFCT parse before reset, rewrites only the validated
image's BDF to the fixed guest slot `0000:00:08.0`, recomputes the ACPI
checksum, verifies that the VBIOS payload is byte-identical, and fsyncs the
complete relocated table into the owner-private per-launch directory. QEMU
receives only that immutable table through `-acpitable`; the VFIO function is
pinned at the same guest slot. This is required for APUs whose VBIOS is part
of system firmware rather than a readable PCI ROM BAR.

`rustos-hostd relay-input` accepts the strict schema-2 input-only policy. The
physical-device supervisor requires `driver-domain-policy-v3`: its domain ID
must match the validated plan, its signed bytes bind the exact production QEMU
SHA-256 plus `amdgpu` `1002:1900`, and it fixes the nominal-60-Hz page-flip and
latency evidence thresholds. Input uses `input-ring-msix`; the commercial
display slice uses `display-dmabuf-kms`. Physical network and block remain disabled.

`rustos-hostd acquire --activate` requires a detached signature verified by a
pinned keyring. The release binds the exact plan, DVM artifacts, schema-3 device
policy, fleet, and validity window before hostd writes a private `prepared`
lease, snapshots every original driver/override, binds the complete group to
`vfio-pci`, and atomically marks it active. Unsigned binding is unavailable.

`rustos-hostd supervise` then resets the complete group, requires `/dev/iommu`,
launches the signed QEMU with 2 GiB guest RAM, one IOMMUFD, and every group
function attached to it, and authenticates the DVM control channel within 30
seconds. The 2 GiB fixed profile leaves bounded unpacking headroom for the
approximately 453 MiB release initramfs; the retired 1 GiB profile failed
part-way through unpacking on the physical GPU run. Readiness then
requires five fresh relay-owned `display-evidence-v2` samples proving a
read-only DMA-BUF source import, GPU composition into a separate three-buffer
GBM scanout pool, explicit fence completion, atomic KMS page flips, zero relay
CPU copy, the exact AMD identity, and the signed throughput/page-flip/commit
latency bounds. A pre-exec
gate keeps QEMU from opening VFIO until the exact PID/start-time and all bound
digests are fsync'd in the private runtime record. The launcher accepts only
canonical trusted-owner launch files under non-mutable directory chains and
    computes their SHA-256 internally. The release image includes `acpid` with
    an exact power-button to `/sbin/poweroff` action. Hostd uses a private QMP
    socket to negotiate capabilities, request `system_powerdown`, and wait ten
    seconds for actual QEMU exit; command acceptance alone is not evidence.
    TERM/KILL are bounded fallback only. Crash recovery rechecks the start token
    after `pidfd_open`, tries the same QMP/ACPI path, and signals only through that pidfd;
original drivers return only after child reap and a second group reset. Reset
failure retains `vfio-pci` plus the durable quarantine evidence. `recover`
signals only an exact PID/start-time match and follows the same order.
A signaled or nonzero QEMU exit remains a failed supervision result after safe
restoration and cannot be reported as a completed physical-DVM run.

## Transport contract

The initial image uses unmodified upstream Linux interfaces inside the DVM:

The shape follows the useful part of Qubes qrexec: independent domain-local
agents, a versioned handshake over a narrow KVM vsock transport, and a host
policy broker that approves a capability before either endpoint opens a
service. It does not reuse qrexec's command-execution protocol and does not
grant RustOS host authority.

- DVM-local devices may use standard Linux virtio drivers, but they are not a
  RustOS guest-to-guest data plane.
- The immutable `control-plane-v1.env` contract is carried in the DVM image,
  hashed into its artifact manifest, and written by the initialized serving
  agent to a complete locked inode before it is atomically installed at
  `/run/rustos-dvm/ready`. The agent retains the inode lock for its lifetime;
  local health validates the exact payload and metadata and rejects stale or
  unlocked state after every exit path. The diagnostic `announce` command does
  not publish readiness. In `state=control`, the agent connects only to the
  L0 host's KVM-vsock listener using its launch-assigned CID.
- L0 validates the source CID and complete DVM contract, then sends a fresh
  challenge. The agent must return `dvm-agent-hmac-sha256-v1`, an HMAC-SHA256
  proof over the challenge and exact HELLO bytes using the per-launch 256-bit
  secret. Its first four bytes also derive the per-launch private KVM-vsock
  listener port on both sides, so a same-CID ordinary process cannot reserve
  the pre-authentication setup slot. L0 writes `WELCOME` only after that proof
  succeeds; all failed or
  timed-out proofs close the connection without a probe or input relay. The
  secret is generated and retained by L0, injected as QEMU fw_cfg, and read
  through its root-only `raw` attribute. This authenticates the DVM control
  agent against ordinary same-CID guest processes; guest root and the
  hypervisor remain explicit TCB boundaries. Only then does L0 request health,
  PCI inventory, and the `input-stream` service. The agent discovers
  a keyboard through `KEY_A`/`KEY_Z`/`KEY_SPACE` capabilities and a relative
  pointer through `REL_X`/`REL_Y`/`BTN_LEFT`; it does not trust a QEMU device
  name. It reports key records plus one coalesced pointer packet per
  `SYN_REPORT`. L0 accepts only bounded key code/action, signed motion/wheel,
  and five-button fields, assigns its own monotonic sequence, and forwards one
  fixed RDI3 frame through an L0-owned fixed input ring and one RustOS-only
  MSI-X wake vector. The DVM never maps that ring, learns a RustOS address, or
  gains a generic RPC path.
- RustOS accepts a nonzero relay epoch and contiguous sequence only, then
  carries keyboard/pointer ingress to ring-3 `inputd`. `inputd` alone
  translates key layout/modifier/text state and merges DVM pointer buttons
  separately from native fallback providers. On a malformed, overflowed, or
  disconnected DVM stream, L0 releases its tracked keys/buttons and sends a
  session end; `inputd` clears remaining DVM-only state. QMP and synthetic
  PS/2 injection are not part of this path. KVM smoke proves the authenticated
  relay endpoint, not a fabricated key; a real event requires an input
  controller assigned to DVM. In the current combined-DVM profile, that same
  L0-authenticated start/end epoch is also a lifecycle lease for RustOS's
  independently bounded Ethernet ivshmem provider. It does not carry Ethernet
  data: RustOS requires both the fixed mapped ring and this live lease, and an
  exact end makes later packet operations fail closed. DVM-writable ring state
  cannot create or extend the lease; a network-only DVM is not an enabled
  topology because it lacks its own authenticated lifecycle channel.
- `rustos-hostd relay-input` is a reconnecting L0 service by default. A DVM
  agent reconnect or a RustOS serial endpoint restart creates a fresh epoch;
  the diagnostic `--once` mode is the only one that exits on the first error.
- This low-rate relay is intentionally not a network, block, display, or GPU
  data plane. Its authenticated session markers may gate the current combined
  DVM's separately bounded Ethernet backend, but no network payload uses this
  relay. Those classes require separately versioned paravirtual
  frontends/backends with queue, DMA, cancellation, reset, and revocation
  semantics. The common pattern remains: DVM identifies the device, L0
  validates ownership and protocol, then RustOS policy services consume the
  narrow backend interface.

The commercial KVM profile always enables `--gui-dvm-surfaces`. L0 creates the
fixed V3 `RSGUI002` three-slot transport: ivshmem carries only uncached control
records and MSI-X, while a separate cacheable pixel pool is writable by
RustOS and read-only in the Linux GUI-DVM. RustOS is observed as ivshmem peer 0
before the DVM becomes peer 1, and Linux alone receives its private
virtio-GPU DRM/KMS device. RustOS has no direct virtio-GPU module or native-GPU
presentation path: `uiserver` submits only to the validated DVM pool, and a
missing, malformed, or revoked provider is `Unavailable` rather than a
boot-framebuffer or generic-provider fallback. The non-DVM direct-GPU test
profile is diagnostic-only and cannot satisfy commercial GUI acceptance.

The private compositor transport is deliberately narrower than an application
graphics ABI. One atomic frame binds one immutable atlas generation from a
three-slot RustOS-owned pool plus a bounded clear/solid/textured command list.
The display provider owns pitch: uiserver accepts padding only when the surface
pitch exactly matches the provider, remains pixel-aligned, and bounds
`stride * height`. This follows the Linux DMA-BUF exchange rule that allocation
may preserve the requested width while returning a wider aligned stride, and
the Wayland `wl_shm` rule that a buffer carries an explicit row stride rather
than an implied `width * bpp` value. Atlas allocation/mapping runs in a bounded
worker; the existing CPU-presented surface remains live until the current-epoch
prime, retained scene, and first GPU+KMS completion atomically promote the path.
The prime is workload-representative rather than a clear-only readiness token:
before advertising the epoch, the DVM performs one full provider-stride upload
from a private zero-filled texture, the fixed textured-quad GLES path, an EGL
native completion fence, and the initial atomic KMS present under one 500 ms
setup deadline. The physical prime deliberately does not sample a RustOS
DMA-BUF before the first validated producer release. Steady frames
remain independently limited to 16.667 ms. Thus shader or full-upload first-use
cost cannot destroy an epoch after it was reported ready.
Initialization or first-frame timeout is terminal, and malformed layers never
hide behind the transient retained-scene wait.
In QEMU, changed atlas rectangles take one explicit staged upload into the
virtio-GPU texture and evidence is labelled `source-path=staged-copy
zero-copy=0`. On a registered physical backend, the same logical source is a read-only DMA-BUF
import. Prime-completion ABI v2 authenticates that selected source mode before
the host can publish a frame; every submit record must carry the same single
mode for the life of that context. The sealed backend registry currently admits
only `virtio_gpu` with staged copy and `amdgpu` with direct DMA-BUF. The shared
`rustos-dvm-gpu-backends.h` registry is consumed by both the proof executor and
display relay, and evidence names the exact `backend-class` plus
`certification=registered`; the KVM gate checks those fields against the
selected host profile instead of assuming virgl-on-AMD. Multiple eligible
render nodes are rejected as ambiguous rather than silently picking one. Render,
import, fence, and KMS mechanisms are driver-neutral; a later physical driver
requires a separately reviewed registry entry and hardware evidence, never an
optimistic fallback. The current DMA-BUF contract is deliberately one-plane
ARGB8888 with an explicit `DRM_FORMAT_MOD_LINEAR` modifier and requires EGL
modifier import support. Supporting another layout requires a new versioned
format/plane/modifier descriptor and producer/consumer intersection rather than
driver-name inference.
Prime and steady-state evidence use separate checked 1024-byte and 2048-byte
serialization bounds. Overflow is terminal and removes both evidence files;
the KVM runner reports that publication failure immediately rather than
converting it into a generic readiness timeout.

This follows the common commercial split: buffer allocation and usage are
negotiated separately from the rendering API, the exported buffer is opaque,
and acquire/release fences carry ownership between producer, GPU, and display.
Linux DRM validates format/modifier pairs and atomic plane state; Android's
gralloc/BufferQueue/HWC split likewise negotiates usage and returns completion
fences; virtio-GPU uses its own negotiated resource/blob features instead of
being mislabeled as native DMA-BUF zero copy.

The kernel validates the exact live source mapping generation and acquire
value. That mapping generation remains fixed for the three imported atlas slots
during one provider/context epoch; each publication instead advances sequence
and content epoch. The relay rejects an in-epoch mapping-generation change
without mistaking normal slot rotation or reuse for a DMA-BUF rebind. It
materializes the completed CPU release as a non-replayable `sync_file`, and EGL
server-waits it before GLES sampling. The display adapter owns the complete
fixed pixel aperture as one `MEMORY_DEVICE_GENERIC` `dev_pagemap`; every export
holds that owner live and rejects an out-of-range PFN. Plain guest-physical
`memremap()` visibility is never treated as DMA-BUF backing. Because the producer is outside the DVM,
the exporter admits only a DMA-coherent attachment before using its persistent
read-only mapping; a non-coherent future topology fails closed. Source and model checks can establish this
implementation, but `source-path=dmabuf zero-copy=1` is commercial evidence only
after physical import, page-flip, and sustained-rate capture pass. The QEMU path
can prove real GPU composition but never physical zero copy. Atlas reuse follows
the GPU completion fence; old output reuse follows the later KMS page-flip fence.
The relay sends the possibly-unsignalled render fence directly as KMS
`IN_FENCE_FD` and observes render, page-flip, and out-fence completion together;
it does not CPU-wait before commit and thereby miss the next vblank.
Ring0 validates fixed records and slot epochs only; scene policy, packing, and
fallback rejection remain in `uiserver`, while GLES/KMS execution remains in
the DVM.

The relay reports one-second windows from the per-frame completion point, not
from an outer queue-drain boundary. Therefore a continuously non-empty queue
cannot starve evidence. A passing virtual window has matching page-flip,
GPU-fence, and present-fence counts, zero relay CPU-copy time, at least 60 FPS,
at most 12 ms average GPU/atomic work, and at most 16.667 ms maximum GPU render
time. Per-frame serial tracing is rate-limited and is not the acceptance
counter. This matches DRM's explicit in/out-fence ordering; it does not convert
QEMU staged copy into physical zero copy.

DVM-local display health is process-owned rather than file-presence based. A
singleton lock rejects a second relay; after a confirmed peer and completed GPU
frame, the relay writes and fsyncs the exact state on one locked candidate inode
and atomically installs it as `display-ready.lock`. The agent accepts the state
only while that inode remains locked. Ordinary failure closes the ready lock
before scheduler restoration, while process exit and the RT hard limit release
all locks. The fixed candidate name bounds pre-rename crash residue.

The private compositor uses a cumulative 16 ms submission cadence when no
provider vblank clock is exported. Input continues to coalesce while an early
submission receives backpressure; it never falls through to CPU presentation.
Missed cadence slots are skipped instead of accumulated, so a stalled frame
cannot be followed by a GPU burst. This keeps a small margin above the 60 FPS
gate without the previously observed 90-100 FPS saturation.

References: [Linux DMA-BUF buffer exchange](https://docs.kernel.org/userspace-api/dma-buf-alloc-exchange.html),
[Linux DMA-BUF sharing and synchronization](https://docs.kernel.org/driver-api/dma-buf.html),
[Linux atomic KMS](https://docs.kernel.org/gpu/drm-kms.html),
[virtio-GPU](https://docs.oasis-open.org/virtio/virtio/v1.3/virtio-v1.3.html),
[Android BufferQueue/gralloc](https://source.android.com/docs/core/graphics/arch-bq-gralloc),
[Wayland `wl_shm` buffer stride](https://wayland.freedesktop.org/docs/html/apa.html),
[Linux DRM/KMS userspace API](https://docs.kernel.org/gpu/drm-uapi.html),
[Khronos EGL native-fence contract](https://registry.khronos.org/EGL/extensions/ANDROID/EGL_ANDROID_native_fence_sync.txt),
and [Android SurfaceFlinger/HWC ownership](https://source.android.com/docs/core/graphics/surfaceflinger-windowmanager).

Linux 6.12 virtio-gpu rejects the foreign SG-table DMA-BUF needed by the
zero-copy source path, and current QEMU vmware-svga cannot bind vmwgfx because
pitchlock is absent. Therefore virtual KVM cannot close the physical source
import/atomic-scanout gate; the enabled AMD profile requires the physically
assigned amdgpu device and permits no CPU-copy fallback. The current Blackwell
target is packaged from the SHA-256-pinned 580.173.02 official release: only
the open `nvidia`, `nvidia-modeset`, and `nvidia-drm` modules plus matching
`gsp_ga10x.bin`/`gsp_tu10x.bin` are installed. UVM, CUDA, the proprietary
kernel flavor, and the NVIDIA userspace graphics stack are absent. The init
service loads only a kernel-produced display-class PCI modalias and refuses the
relay if NVIDIA KMS does not initialize completely. Firmware redistribution
authorization and target-hardware evidence remain mandatory release gates.
Artifact-manifest schema 8 makes this supply contract executable. Its exact
25-key vocabulary pins Buildroot 2026.05, Linux 6.12.94, NVIDIA-open 580.173.02,
the source and boot-artifact hashes, the allowed display modules, redistribution
posture, the enforced module-signing policy, its certificate, the exact kernel
configuration that proves enforcement, and the authenticated control-plane
contract. The manifest and its seven named payloads form one co-located,
self-contained release bundle; `hostd` does not accept an out-of-bundle config,
source lock, certificate, or control contract. Unknown, duplicate, or omitted
manifest keys, any key outside the control contract's exact six-key vocabulary,
and any companion-file hash mismatch are admission failures in `hostd`,
`xtask`, and the staging verifier. Staging requires a fresh destination below
owner-controlled, non-symlinked, non-group/world-writable ancestors and publishes
only by atomic rename after a second complete verification.
The build and `make verify` run Linux's signature extractor over every
installed `.ko`, then ask OpenSSL CMS to verify the detached module payload
against the bound X.509 certificate. A trailer-shaped but invalid or
foreign-signed module therefore cannot pass the artifact gate.
The AMD `1002:1900` image additionally prunes the broad upstream AMDGPU firmware
package to the 12 regular files named by the sealed board profile. Verification
requires that exact filename set and the per-file SHA-256 values in
`sources.lock`; an extra firmware blob is a supply-contract failure, not benign
payload. The kernel feature envelope requires ZONE_DEVICE page ownership,
DMA-BUF/sync, and AMD DC while forbidding guest VFIO/IOMMUFD, generic DMA heaps,
userptr, and diagnostic DRM providers.
The per-image signing private key never enters the artifact directory. Build
and verification require it to remain a non-symlinked, build-user-owned 0600
regular file at the kernel's pinned `certs/signing_key.pem` path; the outer
signed release authorization binds the exported public certificate and exact
kernel/rootfs hashes.

This avoids a Linux kernel fork while retaining a narrow agent for health,
device state, and authenticated control messages. Input, Ethernet, and GUI
each have bounded, versioned transport contracts; PCI assignment remains
gated by the durable VFIO lease. Any later page-loan or device-specific
NIC/block/GPU transport is disabled by default and release-blocked until it
has its own queue, DMA/IOMMU, cancellation, reset, revocation, conformance,
and runtime evidence.

## Profile limits

`rustos_linux_dvm_x86_64_defconfig` is a display-DVM plus virtual
net/USB/auxiliary-storage baseline. It may own a dedicated assigned GPU, but is
not a host-primary-GPU or boot-disk profile. Manifest schema 8 exposes the
exact NVIDIA release digest, non-redistribution status, and admitted KMS module
set in addition to binding the complete source lock and the certificate for
the kernel-enforced signed-module policy. Release images must pin the kernel,
module set, firmware bundle, signed module admission policy, and source/artifact
manifest for each supported hardware profile. Schema 8 currently provides the
locked source/artifact manifest only; it does not claim a CycloneDX SBOM or
Buildroot `legal-info` bundle.

The build wrapper treats cache ownership as part of that profile contract.
Buildroot/toolchain identity is the only ordinary full-output lane; Linux
source/config changes rebuild Linux and every kernel-signed module, local relay
source changes rebuild that package, and overlay/firmware post-build policy
regenerates only the rootfs. `build-plan` exposes the decision before mutation,
and configuration stamps support safe resume after Kconfig reconciliation.
Kernel/package/rootfs currency stamps are committed only after config,
signature, manifest, and artifact verification succeeds. A compiler-cache hit
or incremental compile is build evidence only, never runtime or hardware
evidence. Independent signed builds need not be byte-identical because the
module-signing identity is per image; the certificate-bound verification and
locked-input manifest are the release proof.
