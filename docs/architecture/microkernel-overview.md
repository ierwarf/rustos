# Microkernel Overview

<a id="english"></a>

## English

RustOS keeps a deliberately small privileged layer and moves runtime policy to
services. It preserves Linux ELF and Windows PE application ABIs while using a
Linux Driver VM (DVM) for Linux driver compatibility.

```text
UEFI/GRUB -> nucleus -> rootd -> core services -> initd -> runtimed/uiserver
                         |
                         +-> fixed DVM transports
                             input: RDI2 over COM2
                             display: validated ivshmem framebuffer
                             network: validated ivshmem frame ring
                         |
                         +-> Linux DVM owns DRM/KMS, evdev, virtio-net, modules
```

Ring0 owns boot, scheduler, page tables, user-copy, capability enforcement,
boot-volume I/O, and fixed transport validation. It does not load Linux
modules, enumerate a direct virtio network device, or provide USB/PS2/display
fallbacks. If a DVM transport is missing or invalid, the affected device fails
closed.

`inputd`, `uiserver`, and `netd` own input, display, and networking policy.
The kernel validates sizes, versions, sequence/state transitions, and memory
bounds before handing a record or frame to those services.

<a id="korean"></a>

## 한국어

RustOS는 privileged layer를 좁게 유지하고 runtime policy를 service로 보냅니다.
Linux ELF와 Windows PE application ABI는 유지하며 Linux driver compatibility는
Linux Driver VM(DVM)으로 처리합니다.

```text
UEFI/GRUB -> nucleus -> rootd -> core service -> initd -> runtimed/uiserver
                         |
                         +-> 고정 DVM transport
                             input: COM2 위 RDI2
                             display: 검증된 ivshmem framebuffer
                             network: 검증된 ivshmem frame ring
                         |
                         +-> Linux DVM이 DRM/KMS, evdev, virtio-net, module 소유
```

Ring0는 boot, scheduler, page table, user-copy, capability enforcement,
boot-volume I/O, 고정 transport 검증만 담당합니다. Linux module을 load하지 않고,
direct virtio network device나 USB/PS2/display fallback도 제공하지 않습니다.
DVM transport가 없거나 잘못되면 해당 device는 fail closed 됩니다.

Input/display/network policy는 `inputd`, `uiserver`, `netd`가 소유합니다.
Kernel은 record/frame을 service에 넘기기 전에 size, version, sequence/state,
memory bounds를 검증합니다.
