# Linux driver-domain model

This directory builds the immutable x86_64 Linux appliance used by RustOS
Linux driver domains. It is a hardware driver provider, never the policy
owner.

```text
RustOS control domain       Linux DVM
driverd / netd / storaged   Linux LTS + firmware + supported .ko
          |                           |
          +---- L0 virtio queues -----+
                                      PCI-assigned IOMMU group
```

## Ownership

- L0 exclusively owns VM memory, interrupt remapping, IOMMU mappings, device
  assignment, revocation, and reset.
- RustOS services own admission, routing, mount, user, update, and recovery
  policy.
- A Linux DVM owns only the PCI/IOMMU group assigned to it and exports a
  normalized virtio data plane. It must not receive RustOS filesystem,
  desktop, package-manager, or general management authority.
- One PCI/IOMMU group has exactly one active owner. Failed revoke/reset is a
  fail-closed condition, never a reason to reassign the device anyway.

## Transport contract

The initial image uses unmodified upstream Linux interfaces:

- Data: virtio-net, virtio-blk, virtio-input and shared virtqueues.
- Control: virtio-console or virtio-vsock after L0 supplies an authenticated
  endpoint.
- Readiness: the included agent writes `/run/rustos-dvm/ready`; it does not
  pretend to establish a host control channel before one exists.

This avoids a Linux kernel fork. A small agent is still required for health,
device state, and authenticated control messages. High-performance page loans
or device-specific protocols are future, versioned extensions, not part of
this baseline.

## Profile limits

`rustos_linux_dvm_x86_64_defconfig` is a net/USB/auxiliary-storage baseline.
It is not a primary-GPU or boot-disk profile. Release images must pin the
kernel, module set, firmware bundle, signed module admission policy, and
source/SBOM manifest for each supported hardware profile.
