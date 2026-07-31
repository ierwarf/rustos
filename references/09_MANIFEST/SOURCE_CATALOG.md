# Source catalog

이 목록은 공식 저장소·공식 문서 우선이다. `mode=fetch` 항목은 전체 소스를 ZIP에 복제하지 않고 공식 원본에서 재현 가능하게 받는다.

| Name | URL | Ref | Focus | Mode |
|---|---|---|---|---|
| Linux | https://github.com/torvalds/linux | master | kernel, ABI, process docs, LKMM | offline+fetch |
| Qubes docs | https://github.com/QubesOS/qubes-doc | main | architecture, qrexec, TCB | offline+fetch |
| Qubes core-admin | https://github.com/QubesOS/qubes-core-admin | main | qube lifecycle, admin API, storage | fetch |
| Qubes qrexec | https://github.com/QubesOS/qubes-core-qrexec | main | cross-domain RPC | fetch |
| Xen | https://github.com/xen-project/xen | master | hypervisor ABI, grant/event | fetch |
| seL4 | https://github.com/seL4/seL4 | master | capability microkernel | offline+fetch |
| seL4 l4v | https://github.com/seL4/l4v | master | formal proofs | offline+fetch |
| seL4 RFCs | https://github.com/seL4/rfcs | master | architecture change process | offline+fetch |
| Fuchsia | https://fuchsia.googlesource.com/fuchsia | main | Zircon, Starnix, components | fetch |
| FreeBSD src | https://github.com/freebsd/freebsd-src | main | Linuxulator, kernel | fetch |
| NetBSD src | https://github.com/NetBSD/src | trunk | compat_linux, kernel | fetch |
| illumos | https://github.com/illumos/illumos-gate | master | lx brand, zones | fetch |
| gVisor | https://github.com/google/gvisor | master | userspace kernel, Linux ABI | fetch |
| Wine | https://github.com/wine-mirror/wine | master | Windows ABI, WoW64 | fetch |
| ReactOS | https://github.com/reactos/reactos | master | NT-compatible OS | fetch |
| MINIX 3 | https://github.com/Stichting-MINIX-Research-Foundation/minix | master | microkernel servers | fetch |
| HelenOS | https://github.com/HelenOS/helenos | master | IPC/VFS/drivers | fetch |
| Redox kernel | https://github.com/redox-os/kernel | master | Rust kernel | fetch |
| Genode | https://github.com/genodelabs/genode | master | component framework | fetch |
| Fiasco.OC | https://github.com/kernkonzept/fiasco | master | L4 microkernel | fetch |
| L4Re core | https://github.com/kernkonzept/l4re-core | master | L4 runtime/services | fetch |
| TLA+ Examples | https://github.com/tlaplus/Examples | master | distributed/concurrent specs | offline+fetch |
| Apalache | https://github.com/apalache-mc/apalache | main | symbolic TLA+ checking | fetch |
| Kani | https://github.com/model-checking/kani | main | Rust model checking | fetch |
| Loom | https://github.com/tokio-rs/loom | master | Rust concurrency exploration | offline+fetch |
| CBMC | https://github.com/diffblue/cbmc | develop | C bounded model checking | fetch |
| syzkaller | https://github.com/google/syzkaller | master | kernel syscall fuzzing | fetch |
| herdtools7 | https://github.com/herd/herdtools7 | master | memory model/litmus | fetch |
| RISC-V ISA | https://github.com/riscv/riscv-isa-manual | main | architecture spec | fetch |
| VirtIO spec | https://github.com/oasis-tcs/virtio-spec | master | virtual devices | fetch |
| Devicetree spec | https://github.com/devicetree-org/devicetree-specification | main | hardware description | fetch |
| Arm ABI | https://github.com/ARM-software/abi-aa | main | calling/ELF ABI | fetch |