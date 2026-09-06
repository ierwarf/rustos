# RustOS

[![Rust](https://github.com/ierwarf/rustos/actions/workflows/rust.yml/badge.svg?branch=master)](https://github.com/ierwarf/rustos/actions/workflows/rust.yml?branch=master)
[![Docs](https://img.shields.io/badge/docs-mdBook-informational)](https://ierwarf.github.io/rustos/)
[![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)

> An experimental Rust-first hybrid microkernel operating system with userspace
> services, Linux ELF / Windows PE compatibility work, a Wayland desktop, and a
> Linux driver domain.

![RustOS desktop UI](docs/assets/debug.png)

[English](#english) · [한국어](#korean) · [Documentation](https://ierwarf.github.io/rustos/)

---

<a id="english"></a>

## English

### Overview

RustOS is a Rust-built **hybrid microkernel** organized as an operating-system
platform rather than a single kernel crate. Ring0 keeps the mechanisms that
need kernel privilege—trap entry, paging, scheduling, IRQ/MMIO/DMA bridges,
and a small set of gated brokers—while higher-level policy is moved into
userspace services.

The project targets native binary compatibility for **Linux ELF** and
**Windows PE** programs. `uiserver` includes a Wayland compositor that provides
the desktop surface for native and compatibility clients.

### Highlights

- **Hybrid microkernel design** — privileged mechanisms stay in ring0 while
  policy and higher-level functionality move to userspace services.
- **Rust-first workspace** — layered kernel crates, service binaries, shared
  libraries, tools, and manifests live in one development workspace.
- **Dual compatibility direction** — Linux ELF and Windows PE compatibility
  are developed against the same OS architecture rather than as separate
  systems.
- **Wayland desktop** — the built-in compositor in `uiserver` owns the desktop
  surface.
- **Manifest-driven images** — `cargo xtask` validates, builds, stages, and
  assembles the UEFI boot image.
- **Linux driver domain** — KVM smoke testing can boot RustOS together with a
  pinned Buildroot Linux DVM and exercise its bounded authenticated control
  probe.

### Build flow

```text
cargo xtask build
        │
        └──> build/rustos-boot.img
                 │
                 └──> GRUB EFI -> RustOS
```

The staged boot volume lives at `build/image`, the generated disk image is
`build/rustos-boot.img`, and the default UEFI entry is
`build/image/EFI/BOOT/BOOTX64.EFI`.

### Quick start

See [Getting Started](docs/getting-started.md) for host prerequisites and the
full setup procedure.

```bash
cargo xtask check
cargo xtask build
cargo xtask kvm-smoke --dry-run
```

For the Linux driver-domain smoke path on a host with `/dev/kvm` and
`qemu-system-x86_64`:

```bash
cargo xtask build-dvm
cargo xtask kvm-smoke --expect 'runtimed: bootstrap ui done'
```

The DVM currently exposes the authenticated `agent-v1-control` smoke path for
health/PCI inventory and a bounded synthetic-key probe. This validates the
control/smoke path only; a general RustOS↔DVM device data plane, arbitrary key
forwarding, NIC/storage transport, and passthrough validation are separate
work.

### Architecture at a glance

| Area | Role |
| --- | --- |
| Boot | GRUB EFI boot path and generated UEFI disk image |
| Ring0 | Trap entry, paging, scheduling, IRQ/MMIO/DMA bridges, gated brokers |
| Userspace | OS policy and higher-level services |
| UI | `uiserver` Wayland compositor and desktop surface |
| Compatibility | Linux ELF and Windows PE compatibility work |
| Driver domain | Buildroot Linux DVM plus authenticated control smoke path |
| Tooling | `cargo xtask`, manifests, staging, verification, and KVM orchestration |

### Project status

| Area | Status |
| --- | --- |
| GRUB EFI boot manager | Present |
| Layered kernel crates | Present |
| Manifest-driven image staging | Present |
| Userspace services | Present |
| Wayland/framebuffer desktop UI | Present |
| Linux ELF compatibility | In progress |
| Windows PE compatibility | In progress |
| Linux DVM control smoke path | Present |
| General RustOS↔DVM device data plane | In progress |
| Public release artifacts | Planned |

### Documentation

A good reading order is:

1. [Getting Started](docs/getting-started.md)
2. [Execution Flow](docs/execution-flow.md)
3. [Microkernel Overview](docs/architecture/microkernel-overview.md)
4. [Userspace Services](docs/architecture/services.md)
5. [Userspace Compatibility](docs/architecture/userspace-compat.md)
6. [UI Server & Wayland](docs/architecture/ui-server.md)
7. [Structure Guide](docs/structure.md)
8. [Kernel API](docs/api/kernel.md)
9. [xtask API](docs/api/xtask.md)
10. [Logging Guide](docs/logging.md)
11. [AI Agent Reference](docs/ai/README.md)

Full documentation is also published through the
[mdBook site](https://ierwarf.github.io/rustos/).

---

<a id="korean"></a>

## 한국어

### 개요

RustOS는 Rust로 작성된 **hybrid microkernel** 운영체제이며, 단일 kernel
crate가 아니라 하나의 OS 개발 플랫폼 형태로 구성되어 있습니다. ring0에는
trap entry, paging, scheduling, IRQ/MMIO/DMA bridge, 소수의 gated broker처럼
커널 권한이 필요한 mechanism만 남기고, 상위 정책과 기능은 userspace
service로 옮기는 방향을 사용합니다.

프로젝트는 **Linux ELF**와 **Windows PE** 프로그램의 native binary
compatibility를 목표로 합니다. `uiserver`에는 native/compatibility client가
사용하는 desktop surface를 제공하는 Wayland compositor가 포함되어 있습니다.

### 주요 특징

- **Hybrid microkernel 구조** — 특권 mechanism은 ring0에 두고 정책과 상위
  기능은 userspace service로 분리합니다.
- **Rust-first workspace** — 계층화된 kernel crate, service binary, 공용
  library, tooling, manifest를 하나의 개발 workspace에서 관리합니다.
- **이중 호환성 방향** — Linux ELF와 Windows PE 호환 경로를 동일한 OS
  architecture 위에서 개발합니다.
- **Wayland desktop** — `uiserver`의 내장 compositor가 desktop surface를
  담당합니다.
- **Manifest 기반 image pipeline** — `cargo xtask`가 검증, 빌드, staging,
  UEFI boot image 생성을 담당합니다.
- **Linux driver domain** — KVM smoke에서 RustOS와 고정된 Buildroot Linux
  DVM을 함께 부팅하고 제한된 인증 control probe를 검증할 수 있습니다.

### 빌드 흐름

```text
cargo xtask build
        │
        └──> build/rustos-boot.img
                 │
                 └──> GRUB EFI -> RustOS
```

staged boot volume은 `build/image`, 생성되는 disk image는
`build/rustos-boot.img`, 기본 UEFI entry는
`build/image/EFI/BOOT/BOOTX64.EFI`입니다.

### 빠른 시작

host 준비와 전체 설치 절차는 [시작하기](docs/getting-started.md)를 참고하세요.

```bash
cargo xtask check
cargo xtask build
cargo xtask kvm-smoke --dry-run
```

`/dev/kvm`과 `qemu-system-x86_64`를 사용할 수 있는 host에서 Linux DVM smoke
경로를 실행하려면:

```bash
cargo xtask build-dvm
cargo xtask kvm-smoke --expect 'runtimed: bootstrap ui done'
```

현재 DVM은 health/PCI inventory와 제한된 synthetic-key probe를 위한 인증된
`agent-v1-control` smoke 경로를 제공합니다. 이는 control/smoke 경로만
검증합니다. 범용 RustOS↔DVM device data plane, 임의 키 forwarding,
NIC/storage transport, passthrough 검증은 별도의 진행 항목입니다.

### 아키텍처 한눈에 보기

| 영역 | 역할 |
| --- | --- |
| Boot | GRUB EFI boot 경로와 생성된 UEFI disk image |
| Ring0 | Trap entry, paging, scheduling, IRQ/MMIO/DMA bridge, gated broker |
| Userspace | OS 정책과 상위 service |
| UI | `uiserver` Wayland compositor와 desktop surface |
| Compatibility | Linux ELF / Windows PE 호환 경로 |
| Driver domain | Buildroot Linux DVM과 인증된 control smoke path |
| Tooling | `cargo xtask`, manifest, staging, verification, KVM orchestration |

### 현재 상태

| 영역 | 상태 |
| --- | --- |
| GRUB EFI boot manager | 구현됨 |
| Layered kernel crates | 구현됨 |
| Manifest 기반 image staging | 구현됨 |
| Userspace services | 구현됨 |
| Wayland/framebuffer desktop UI | 구현됨 |
| Linux ELF compatibility | 진행 중 |
| Windows PE compatibility | 진행 중 |
| Linux DVM control smoke path | 구현됨 |
| 범용 RustOS↔DVM device data plane | 진행 중 |
| Public release artifacts | 예정 |

### 문서

추천 읽기 순서:

1. [시작하기](docs/getting-started.md)
2. [실행 흐름](docs/execution-flow.md)
3. [Microkernel Overview](docs/architecture/microkernel-overview.md)
4. [Userspace Services](docs/architecture/services.md)
5. [Userspace Compatibility](docs/architecture/userspace-compat.md)
6. [UI Server & Wayland](docs/architecture/ui-server.md)
7. [Structure Guide](docs/structure.md)
8. [Kernel API](docs/api/kernel.md)
9. [xtask API](docs/api/xtask.md)
10. [로깅 가이드](docs/logging.md)
11. [AI Agent Reference](docs/ai/README.md)

전체 문서는 [mdBook 사이트](https://ierwarf.github.io/rustos/)에서도 볼 수 있습니다.

---

## License

RustOS is distributed under the [MIT License](LICENSE).
