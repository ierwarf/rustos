# Linux driver-domain model

This directory builds the immutable x86_64 Linux appliance used by RustOS
Linux driver domains. It is a hardware driver provider, never the policy
owner.

```text
RustOS HVM                 Linux DVM
driverd / netd / storaged  Linux LTS + firmware + supported .ko
          |                          |
          +-- Xen frontend/backend --+
                                     PCI-assigned IOMMU group
```

## Ownership

- L0 exclusively owns VM memory, interrupt remapping, IOMMU mappings, device
  assignment, revocation, and reset.
- RustOS services own admission, routing, mount, user, update, and recovery
  policy.
- A Linux DVM owns only the PCI/IOMMU group assigned to it and can export a
  normalized data plane only after a versioned Xen frontend/backend is
  implemented. It must not receive RustOS filesystem,
  desktop, package-manager, or general management authority.
- One PCI/IOMMU group has exactly one active owner. Failed revoke/reset is a
  fail-closed condition, never a reason to reassign the device anyway.

## Transport contract

The initial image uses unmodified upstream Linux interfaces inside the DVM:

The shape follows the useful part of Qubes qrexec: independent domain-local
agents, a versioned handshake over a narrow Xen transport, and an L0 policy
broker that approves a capability before either endpoint opens a service. It
does not reuse qrexec's command-execution protocol and does not grant RustOS
control-domain authority.

- DVM-local devices may use standard Linux virtio drivers, but they are not a
  RustOS guest-to-guest data plane.
- The immutable `control-plane-v1.env` contract is carried in the DVM image,
  hashed into its artifact manifest, and written by the agent to
  `/run/rustos-dvm/ready`. It declares `state=pretransport`.
- Planned inter-domain transport is Xen vchan plus grant tables/event channels
  with explicit RustOS frontend and Linux backend endpoints. `pretransport`
  means none of those endpoints, no control request, and no device queue is
  live yet.
- RustOS HVM does only CPUID discovery and a W^X hypercall-page install at
  boot. It invokes no Xen hypercall and is not a Xen frontend; the page is a
  prerequisite for the future, separately reviewed endpoint implementation.
- The future control endpoint is authenticated with L0-bound domain identity
  before it exposes an approved capability. Health and device inventory are the
  only initial capabilities; network, block, input, display, and GPU data
  planes require separately versioned protocols.

This avoids a Linux kernel fork. A small agent is still required for health,
device state, and authenticated control messages. There is currently no
RustOS Xen frontend, Linux Xen backend binding, PCI assignment, or RustOS
device consumption. High-performance page loans or device-specific protocols
are future, versioned extensions, not part of this baseline.

## Profile limits

`rustos_linux_dvm_x86_64_defconfig` is a net/USB/auxiliary-storage baseline.
It is not a primary-GPU or boot-disk profile. Release images must pin the
kernel, module set, firmware bundle, signed module admission policy, and
source/SBOM manifest for each supported hardware profile.
