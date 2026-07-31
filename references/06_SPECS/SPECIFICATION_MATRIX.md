# 명세서 매트릭스

| 영역 | 우선 명세 | 구현 시 핵심 |
|---|---|---|
| x86-64 user ABI | x86-64 psABI, ELF gABI | calling convention, stack, relocations, TLS, signal frame |
| Arm ABI | ARM-software/abi-aa | AAPCS, ELF, exception/unwind, SMC conventions |
| RISC-V | riscv-isa-manual, psABI | privileged state, fences, page tables, calling convention |
| UEFI | UEFI specification | boot services lifetime, memory map key, runtime services |
| ACPI | ACPI specification | tables, AML, power states, MADT/IOMMU |
| VirtIO | OASIS virtio spec | feature negotiation, virtqueue barriers, reset |
| Devicetree | devicetree specification | cell sizes, address translation, binding schemas |
| PCI/PCIe | PCI-SIG specs | enumeration, BAR, MSI/MSI-X, FLR, AER |
| USB/xHCI | USB-IF/xHCI specs | rings, cycle bit, ownership, event ordering |
| NVMe | NVM Express specs | queue lifecycle, reset, namespaces, flush |
| ELF/DWARF | gABI, DWARF | loader, unwind, debug, core dump |
| POSIX | The Open Group/Austin Group | process, files, signals, threads; Linux extensions separate |
| TPM | TCG TPM 2.0 | measured boot, policy sessions, NV counters |

재배포 제한이 불명확한 PDF는 `hardware_links/OFFICIAL_SPEC_LINKS.md`에 링크만 둔다.
