# Microkernel Overview

[English](#english) | [한국어](#korean)

<a id="english"></a>

## English

RustOS is a **hybrid microkernel**. The kernel (`nucleus.elf`) keeps only the
mechanisms that require ring0 — trap entry, paging, scheduling, IRQ/MMIO/DMA
bridges, user-copy primitives, IPC mailboxes, and a few narrow brokers for
policy that still has to mutate kernel state — and pushes everything else into
userspace services. The product target is native binary compatibility for both
Linux ELF and Windows PE executables, so the kernel exposes both a Linux-style
syscall ABI and a PE/Win32 surface through userspace policy.

### Kernel Layout

The kernel is split into crates so privileges and policy stay isolated:

- `kernel/nucleus-core`: the kernel binary entry plus shared core utilities,
  panic handling, milestone tracing, and fault-injection runtime.
- `kernel/lowlevel`: assembly trap stubs, context save/restore, syscall
  entry/exit, and CPU-local data.
- `kernel/hal`: ACPI, PCI config space, IRQ controller, SMP boot, and CPU
  feature detection.
- `kernel/mm`: physical/virtual memory, address space objects, page tables,
  MMIO mapping, and DMA helpers.
- `kernel/object`: capability-style kernel handles for services and brokers.
- `kernel/ipc-runtime`: typed IPC endpoints, shared-region cache, and message
  routing for ring0 ↔ user broker calls.
- `kernel/ps`: scheduler, threads, processes, signals (Linux thread policy is
  still here pending migration), and per-CPU run queues.
- `kernel/io-manager`: USB, AHCI/NVMe, input, display providers, network
  bridges, and the in-kernel `.ko` module loader for bridge drivers.
- `kernel/compat`: residual Windows PE/Win32 reference notes and a thin
  Linux ABI bridge that re-exports the canonical implementations from
  `kernel/ps`.
- `kernel/executive`: boot orchestration, finalize sequence, and the nucleus
  run loop.

Anything that is *not* in the list above belongs in userspace.

Ring3 migration status is read from `cargo xtask ring3-inventory`.
`migration_candidate_loc` means real remaining ring3 work. `cleanup_debt_loc`
means legacy native code that should be retired rather than migrated, and
`excluded_exception_loc` means deliberate ring0 substrate or already-ring3
reference surface.

### Boot Sequence

```text
UEFI firmware
  -> EFI/BOOT/BOOTX64.EFI (GRUB, signed, check_signatures=enforce)
  -> nucleus.elf + nucleus.elf.sig
  -> kernel_executive::boot::initialize
       gdt, idt, paging, framebuffer, heap, fault injection
       ACPI, PCI, USB, input, console, RTC, random, syscall
  -> kernel_executive::boot::finalize
       vfs init, display/input/network init, housekeeping task
       init bootstrap task spawns rootd
  -> rootd
       brings up syscalld, vfsd, loaderd, devmgrd, driverd, storaged
  -> initd
       launches runtimed (the runtime service manager)
  -> runtimed
       bootstraps uiserver synchronously (with manifest env applied),
       loads desktop/registry catalogs, then launches autostart apps
  -> uiserver
       opens the display, starts the Wayland compositor and console host,
       notifies runtimed when ready
  -> desktop/session apps (shell, wayclick, windows demos, ...)
```

`rootd` is the privilege root of userspace, modeled after an seL4 initial
task. Linux-style `initd` only runs after the foundational service brokers
are up, so the Linux runtime never has to fall back through the kernel for
bootstrap policy. Adding a Linux syscall fallback in the kernel to "make
`initd` start earlier" is a regression — the right place to add bootstrap
policy is `rootd`, a service manifest, or a narrow broker.

### Narrow Brokers

A narrow broker is a gated syscall that performs a privileged primitive on
behalf of a userspace policy owner. Brokers exist when:

- An operation requires kernel-only state (PTE mutation, IRQ unmask, IOMMU
  attach, MMIO map).
- The caller is a specific userspace service that owns the corresponding
  policy.
- A bounded contract — explicit args struct, ABI version, errno surface — is
  practical.

Examples that are live today:

- `SYS_RUSTOS_MM_BROKER`: address-space mutation owned by `syscalld`.
- `SYS_RUSTOS_PROC_PREPARE_BROKER` / `..._COMMIT_BROKER`: process creation
  owned by `loaderd`.
- `SYS_RUSTOS_PROC_SET_WINDOWS_RUNTIME_BROKER`: Windows runtime metadata
  attached during PE spawn.
- `SYS_RUSTOS_DRIVER_LOAD_MODULE_BROKER` /
  `..._DRIVER_PROBE_ALIAS_BROKER`: kernel-side module load and alias probe
  owned by `driverd`.
- `SYS_RUSTOS_IPC_*`: capability-based IPC endpoint management.

Add new brokers conservatively. Prefer extending an existing service or
adding a manifest field over carving a new syscall.

### Compatibility Direction

- Linux ELF: dynamic ELF main + interpreter loader lives in `loaderd`.
  Linux ABI surface lives in `rustos-user-abi::linux` and is re-exported
  through `kernel/ps::api` and `kernel/compat::user`; do not reintroduce
  shadow ABI files under `kernel/compat/src/user`. `syscalld` owns Linux MM
  policy and forwards through the gated MM broker.
- Windows PE: `loaderd` maps PE32+ images, resolves imports from the System32
  DLL inventory (`build/image/compat/windows/System32/*`), and registers a
  Windows runtime broker with the kernel. The PE/Win32 ABI bridge currently
  lives in `kernel/compat`, with deeper policy migration tracked as a
  follow-up.
- Wayland: `uiserver` runs a built-in Wayland compositor exposed at
  `/run/user/1000/wayland-0`. Native Wayland clients (e.g. `wayclick`) draw
  through it; console-style apps go through the runtime's console service.

<a id="korean"></a>

## 한국어

RustOS는 **hybrid microkernel**입니다. ring0가 반드시 필요한 mechanism — trap
entry, paging, scheduling, IRQ/MMIO/DMA bridge, user-copy primitive, IPC
mailbox, 그리고 kernel state를 mutation해야 하는 narrow broker — 만 커널
(`nucleus.elf`)에 두고, 나머지는 userspace service로 옮깁니다. 제품 목표는
Linux ELF와 Windows PE 실행 파일을 둘 다 native로 실행하는 것이며, 그래서
커널은 Linux 스타일 syscall ABI와 PE/Win32 surface를 userspace policy를 통해
같이 노출합니다.

### 커널 구성

ring0 권한과 policy가 섞이지 않도록 커널은 여러 crate로 나뉩니다.

- `kernel/nucleus-core`: 커널 binary entry, 공용 core utility, panic 처리,
  milestone trace, fault-injection runtime.
- `kernel/lowlevel`: assembly trap stub, context save/restore, syscall
  entry/exit, CPU-local data.
- `kernel/hal`: ACPI, PCI config space, IRQ controller, SMP boot, CPU feature
  detection.
- `kernel/mm`: physical/virtual memory, address space object, page table,
  MMIO mapping, DMA helper.
- `kernel/object`: service와 broker를 위한 capability-style handle.
- `kernel/ipc-runtime`: typed IPC endpoint, shared region cache, ring0 ↔
  user broker message routing.
- `kernel/ps`: scheduler, thread, process, signal (Linux thread policy는 아직
  여기에 남아 있음), per-CPU run queue.
- `kernel/io-manager`: USB, AHCI/NVMe, input, display provider, network
  bridge, 그리고 bridge driver `.ko` module을 위한 in-kernel module loader.
- `kernel/compat`: Windows PE/Win32 reference 주석, 그리고 `kernel/ps`의
  canonical 구현을 re-export하는 thin Linux ABI bridge.
- `kernel/executive`: boot orchestration, finalize 시퀀스, nucleus run loop.

위 목록에 없는 것은 모두 userspace에 둡니다.

ring3 migration 상태는 `cargo xtask ring3-inventory`가 기준입니다.
`migration_candidate_loc`는 실제 남은 ring3 작업, `cleanup_debt_loc`는
마이그레이션이 아니라 제거해야 할 legacy native 코드, `excluded_exception_loc`는
의도적으로 남기는 ring0 substrate 또는 이미 ring3인 reference surface입니다.

### Boot 순서

```text
UEFI firmware
  -> EFI/BOOT/BOOTX64.EFI (GRUB, 서명됨, check_signatures=enforce)
  -> nucleus.elf + nucleus.elf.sig
  -> kernel_executive::boot::initialize
       gdt, idt, paging, framebuffer, heap, fault injection
       ACPI, PCI, USB, input, console, RTC, random, syscall
  -> kernel_executive::boot::finalize
       vfs init, display/input/network init, housekeeping task
       init bootstrap task가 rootd를 spawn
  -> rootd
       syscalld, vfsd, loaderd, devmgrd, driverd, storaged 를 띄움
  -> initd
       runtimed (runtime service manager)를 실행
  -> runtimed
       uiserver를 동기 부트스트랩 (manifest env 적용),
       desktop/registry catalog 로드 후 autostart app 실행
  -> uiserver
       display open, Wayland compositor + console host 가동,
       runtimed에 ready 통보
  -> desktop/session apps (shell, wayclick, windows demos, ...)
```

`rootd`는 userspace의 권한 root이며 seL4 initial task 모델을 따릅니다.
foundational broker가 올라온 뒤에야 Linux 스타일 `initd`가 실행되므로, Linux
runtime이 bootstrap을 위해 커널 fallback에 의존할 일이 없습니다. "initd를
빨리 시작하려고 커널에 Linux syscall fallback을 넣는" 변경은 regression
입니다. bootstrap policy를 추가해야 하면 `rootd`, service manifest, 또는
narrow broker 어딘가에 둡니다.

### Narrow Broker

narrow broker는 userspace policy 소유자가 호출하는 gated syscall입니다.
다음 조건이면 narrow broker가 됩니다.

- kernel-only state(PTE mutation, IRQ unmask, IOMMU attach, MMIO map) 가
  필요한 operation.
- 호출자가 대응되는 policy를 소유한 특정 userspace service.
- 명시적인 args struct, ABI version, errno surface 등 bounded contract를
  유지할 수 있음.

현재 운영 중인 broker 예시:

- `SYS_RUSTOS_MM_BROKER`: `syscalld`가 소유하는 address-space mutation.
- `SYS_RUSTOS_PROC_PREPARE_BROKER` / `..._COMMIT_BROKER`: `loaderd`가
  소유하는 process 생성.
- `SYS_RUSTOS_PROC_SET_WINDOWS_RUNTIME_BROKER`: PE spawn 시 Windows runtime
  metadata 부착.
- `SYS_RUSTOS_DRIVER_LOAD_MODULE_BROKER` /
  `..._DRIVER_PROBE_ALIAS_BROKER`: `driverd`가 소유하는 module load와
  alias probe.
- `SYS_RUSTOS_IPC_*`: capability 기반 IPC endpoint 관리.

broker는 보수적으로 추가합니다. 새 syscall을 만들기보다 기존 service를
확장하거나 manifest field를 추가하는 쪽을 우선합니다.

### 호환성 방향

- Linux ELF: dynamic ELF main + interpreter loader는 `loaderd`. Linux ABI
  surface는 `rustos-user-abi::linux`에 있고 `kernel/ps::api` 및
  `kernel/compat::user`를 통해 re-export 됩니다. `kernel/compat/src/user`
  아래 shadow ABI 파일은 다시 만들지 않습니다. Linux MM policy는
  `syscalld`가 소유하고 gated MM broker로 forward.
- Windows PE: `loaderd`가 PE32+ image를 mapping하고
  `build/image/compat/windows/System32/*`의 DLL inventory에서 import를
  해석한 뒤 Windows runtime broker를 커널에 등록합니다. PE/Win32 ABI
  bridge는 현재 `kernel/compat`에 있으며 deeper policy migration은 별도
  과제로 추적합니다.
- Wayland: `uiserver`가 내장 Wayland compositor를
  `/run/user/1000/wayland-0`에 노출합니다. native Wayland client
  (예: `wayclick`)는 이를 통해 그리고, console 스타일 app은 runtime의
  console service를 사용합니다.
