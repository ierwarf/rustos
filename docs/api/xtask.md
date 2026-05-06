# xtask API

[English](#english) | [한국어](#korean)

<a id="english"></a>

## English

`cargo xtask` is the host-side control surface for RustOS. The command enum is
defined in `tools/xtask/src/cli.rs`.

### Commands

| Command | Purpose | Main output |
| --- | --- | --- |
| `cargo xtask check` | Validate layering, package manifests, targets, and workspace checks. | No image output. |
| `cargo xtask build` | Full OS build and stage. | `build/artifacts`, `build/image`, registries. |
| `cargo xtask stage` | Copy built artifacts and overlays into the boot image. | `build/image`. |
| `cargo xtask run` | Run current staged image in QEMU. | QEMU session, `logs/debugcon.log`. |
| `cargo xtask debug` | Run QEMU with GDB stub. | `logs/rustos-debug.gdb`. |
| `cargo xtask probe-display` | Headless display probe/stress path. | Probe result and debug logs. |
| `cargo xtask clean` | Remove cargo/build outputs. | Cleaned target/build dirs. |
| `cargo xtask targets` | Install required Rust targets. | Rust target availability. |
| `cargo xtask build-efi` | Build GRUB EFI boot manager only. Requires `RUSTOS_GRUB_PUBKEY` and `RUSTOS_GRUB_SIGNING_KEY`. | `build/artifacts/EFI/BOOT/BOOTX64.EFI`, `build/artifacts/nucleus.elf.sig`. |
| `cargo xtask build-kernel` | Build Multiboot2 nucleus/kernel only. | `build/artifacts/nucleus.elf`. |
| `cargo xtask build-user` | Build userspace packages. | service/app artifacts. |
| `cargo xtask build-console-demo` | Build C demo/smoke programs. | app artifacts. |
| `cargo xtask build-driver-modules` | Build bridge driver modules. | `.ko` artifacts. |

### QEMU Options

`cargo xtask run`, `debug`, and `probe-display` accept shared options:

| Option | Meaning |
| --- | --- |
| `--profile <default|g14|nvme>` | Select QEMU machine/storage/memory profile. |
| `--accel-profile kvm` | Use KVM acceleration and host CPU profile. |
| `--usb-input` | Attach `qemu-xhci`, `usb-kbd`, and `usb-tablet`. |
| `--no-network` | Disable default usernet and `virtio-net-pci`. |
| `--debugcon <file|stdio|null>` | Route debugcon to file, terminal, or disable it. |
| `--qemu-log <int|null>` | Write QEMU interrupt trace or disable QEMU trace logging. |
| `--timeout <seconds>` | Stop QEMU after a bounded run. No timeout is applied by default. |
| `--expect <marker>` | Stop successfully once all repeated debugcon markers appear. |
| `--summarize-log` | Print high-signal debugcon markers after QEMU stops. |
| `--vfio-pci <BDF>` | Attach a host vfio-pci device. |
| `--phoenix3-passthrough` | Auto-detect and attach Phoenix3 GPU functions. |
| `--vfio-force` | Allow devices driving active host display. |

Raw QEMU args go after `--`:

```bash
cargo xtask run -- --no-reboot
```

Bounded KVM no-opt debugging uses NVMe storage by default while AHCI boundary
issues are isolated separately:

```bash
cargo xtask run --profile nvme --accel-profile kvm --usb-input --debugcon file --timeout 35 --summarize-log -- --no-reboot
```

### When To Use Each Command

- Use `check` before commits that change dependencies, manifests, or layer
  boundaries. It also validates an existing `build/artifacts/nucleus.elf` with
  `grub-file --is-x86-multiboot2` when present.
- Use `build` after changing code or staged image content. The default build
  requires GRUB public/signing key environment variables.
- Use `stage` after changing only `assets/image` or package install metadata
  when artifacts are already built.
- Use `run` for normal boot testing.
- Use `debug` when attaching GDB.
- Use `probe-display` for display, framebuffer, surface, or dirty-rect bugs.

<a id="korean"></a>

## 한국어

`cargo xtask`는 RustOS의 host-side control surface입니다. command enum은
`tools/xtask/src/cli.rs`에 정의되어 있습니다.

### Commands

| Command | Purpose | Main output |
| --- | --- | --- |
| `cargo xtask check` | layering, package manifest, target, workspace check를 검증합니다. | image output 없음 |
| `cargo xtask build` | 전체 OS build와 stage를 수행합니다. | `build/artifacts`, `build/image`, registries |
| `cargo xtask stage` | built artifact와 overlay를 boot image에 복사합니다. | `build/image` |
| `cargo xtask run` | 현재 staged image를 QEMU에서 실행합니다. | QEMU session, `logs/debugcon.log` |
| `cargo xtask debug` | GDB stub과 함께 QEMU를 실행합니다. | `logs/rustos-debug.gdb` |
| `cargo xtask probe-display` | headless display probe/stress path를 실행합니다. | probe result와 debug logs |
| `cargo xtask clean` | cargo/build output을 지웁니다. | 정리된 target/build dirs |
| `cargo xtask targets` | 필요한 Rust target을 설치합니다. | Rust target availability |
| `cargo xtask build-efi` | GRUB EFI boot manager만 빌드합니다. `RUSTOS_GRUB_PUBKEY`, `RUSTOS_GRUB_SIGNING_KEY`가 필요합니다. | `build/artifacts/EFI/BOOT/BOOTX64.EFI`, `build/artifacts/nucleus.elf.sig` |
| `cargo xtask build-kernel` | Multiboot2 nucleus/kernel만 빌드합니다. | `build/artifacts/nucleus.elf` |
| `cargo xtask build-user` | userspace package를 빌드합니다. | service/app artifacts |
| `cargo xtask build-console-demo` | C demo/smoke program을 빌드합니다. | app artifacts |
| `cargo xtask build-driver-modules` | bridge driver module을 빌드합니다. | `.ko` artifacts |

### QEMU Options

`cargo xtask run`, `debug`, `probe-display`는 같은 option을 받습니다.

| Option | Meaning |
| --- | --- |
| `--profile <default|g14|nvme>` | QEMU machine/storage/memory profile 선택 |
| `--accel-profile kvm` | KVM acceleration과 host CPU profile 사용 |
| `--usb-input` | `qemu-xhci`, `usb-kbd`, `usb-tablet` attach |
| `--no-network` | default usernet과 `virtio-net-pci` 비활성화 |
| `--debugcon <file|stdio|null>` | debugcon을 file, terminal로 보내거나 끔 |
| `--qemu-log <int|null>` | QEMU interrupt trace를 쓰거나 QEMU trace logging을 끔 |
| `--timeout <seconds>` | 제한 시간 후 QEMU를 종료. 기본값은 timeout 없음 |
| `--expect <marker>` | 반복 지정한 debugcon marker가 모두 나오면 성공으로 종료 |
| `--summarize-log` | QEMU 종료 후 high-signal debugcon marker 요약 출력 |
| `--vfio-pci <BDF>` | host vfio-pci device attach |
| `--phoenix3-passthrough` | Phoenix3 GPU function 자동 탐지/attach |
| `--vfio-force` | active host display를 구동 중인 device도 허용 |

Raw QEMU arg는 `--` 뒤에 둡니다.

```bash
cargo xtask run -- --no-reboot
```

KVM no-opt bounded debugging은 AHCI boundary issue를 별도로 격리하기 위해
우선 NVMe storage profile을 사용합니다.

```bash
cargo xtask run --profile nvme --accel-profile kvm --usb-input --debugcon file --timeout 35 --summarize-log -- --no-reboot
```

### 언제 어떤 명령을 쓰는가

- dependency, manifest, layer boundary를 바꿨다면 `check`를 사용합니다. 기존
  `build/artifacts/nucleus.elf`가 있으면 `grub-file --is-x86-multiboot2`도
  검증합니다.
- code 또는 staged image content를 바꿨다면 `build`를 사용합니다. 기본 build는
  GRUB public/signing key 환경 변수가 필요합니다.
- artifact가 이미 있고 `assets/image` 또는 install metadata만 바꿨다면 `stage`를 사용합니다.
- 일반 boot test에는 `run`을 사용합니다.
- GDB를 붙일 때는 `debug`를 사용합니다.
- display, framebuffer, surface, dirty-rect bug에는 `probe-display`를 사용합니다.
