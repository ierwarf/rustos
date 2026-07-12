# Linux driver-domain model

This directory builds the immutable x86_64 Linux appliance used by RustOS
Linux driver domains. It is a hardware driver provider, never the policy
owner.

```text
RustOS KVM guest           Linux DVM KVM guest
driverd / netd / storaged  Linux LTS + firmware + supported .ko
          |                          |
          +---- future vsock RPC ----+
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
- L0 validates the source CID and the complete DVM control contract before it
  requests health, PCI device inventory, or one bounded keyboard event. The
  control request is host-to-DVM; it is not a direct RustOS-to-DVM channel.
- `keyboard-events` is a KVM smoke-only proof: the DVM opens its allowlisted
  `virtio-keyboard` evdev node and sends an exact readiness acknowledgement;
  only then does QEMU inject a synthetic `A`. The DVM returns one Linux evdev
  key press, and L0 accepts only evdev code `30` before injecting the same
  synthetic key into RustOS's default PS/2 path. RustOS then has to show that
  `inputd` consumed a new input batch.
- This is intentionally not physical host-keyboard capture, arbitrary-key
  forwarding, or a live input data plane. RustOS still has no vsock endpoint.
  Network, block, display, GPU, and production input transports require
  separately versioned protocols and RustOS-side consumers.

This avoids a Linux kernel fork. A small agent is still required for health,
device state, and authenticated control messages. There is currently no live
RustOS/Linux vsock endpoint, PCI assignment, or production RustOS device
consumption. High-performance page loans or device-specific protocols are
future, versioned extensions, not part of this baseline.

## Profile limits

`rustos_linux_dvm_x86_64_defconfig` is a net/USB/auxiliary-storage baseline.
It is not a primary-GPU or boot-disk profile. Release images must pin the
kernel, module set, firmware bundle, signed module admission policy, and
source/SBOM manifest for each supported hardware profile.
