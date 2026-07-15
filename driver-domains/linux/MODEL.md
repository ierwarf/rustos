# Linux driver-domain model

This directory builds the immutable x86_64 Linux appliance used by RustOS
Linux driver domains. It is a hardware driver provider, never the policy
owner.

```text
RustOS KVM guest           Linux DVM KVM guest
driverd / netd / storaged  Linux LTS + firmware + supported .ko
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
a DVM.

`rustos-hostd relay-input` additionally requires a `driver-domain-policy-v1`
file. Its domain ID must match the validated launch plan and it enables exactly
one named transport per device class. The present input transport is
`input-ring-msix`; network, block, and display are deliberately `disabled` until
their own data-plane contracts exist. This prevents a convenient input relay
from silently becoming an unbounded driver proxy.

`rustos-hostd acquire` is also read-only by default. Its explicit laboratory
path requires both `--activate` and `--allow-unsigned-test-bind`; it writes a
private `prepared` lease before changing any sysfs binding, snapshots every
original driver and `driver_override`, binds the whole validated group to
`vfio-pci`, then atomically marks the lease `active`. If acquisition fails, it
rolls back in reverse order but retains the `prepared` record for explicit
recovery. `release --activate` restores a `prepared` or `active` lease and
removes its record only after restoration succeeds; a crash or failed restore
also intentionally leaves the record for recovery. Lease files
are owner-private and their directory is owner-private before use.

Unsigned laboratory binding is deliberately not a production path. A release
manifest must cryptographically bind the validated plan, DVM artifact hashes,
and policy before normal hardware lifecycle activation is enabled. Do not use
the laboratory flag for the L0 boot disk, active display/GPU, Wi-Fi, or any
other host-critical IOMMU group.

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
  hashed into its artifact manifest, and written by the agent to
  `/run/rustos-dvm/ready`. In `state=control`, the agent connects only to the
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

This avoids a Linux kernel fork while retaining a narrow agent for health,
device state, and authenticated control messages. Input, Ethernet, and GUI
each have bounded, versioned transport contracts; PCI assignment remains
gated by the durable VFIO lease. Any later page-loan or device-specific
NIC/block/GPU transport is disabled by default and release-blocked until it
has its own queue, DMA/IOMMU, cancellation, reset, revocation, conformance,
and runtime evidence.

## Profile limits

`rustos_linux_dvm_x86_64_defconfig` is a net/USB/auxiliary-storage baseline.
It is not a primary-GPU or boot-disk profile. Release images must pin the
kernel, module set, firmware bundle, signed module admission policy, and
source/SBOM manifest for each supported hardware profile.
