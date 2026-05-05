# Structure Guide

[English](#english) | [한국어](#korean)

<a id="english"></a>

## English

This document defines the repository layout rules for RustOS. These rules are
not only documentation: the important dependency and package taxonomy checks
are enforced by `cargo xtask check`.

### Directory Roles

- `boot/`: bootloader, prekernel, and boot protocol crates
- `kernel/`: kernel entry crate plus internal kernel subsystem crates
- `services/`: userspace system services such as `initd`, `runtimed`, and `uiserver`
- `apps/`: demo, smoke, desktop, and compatibility test applications
- `compat/`: compatibility layer sources, including Windows userspace support
- `libs/`: general-purpose crates shared across product layers
- `tests/`: integration and module test crates
- `tools/`: host-side build, stage, run, and validation tools
- `assets/image/`: static overlay copied directly into the staged boot image
- `assets/ui/`: UI fonts and related UI assets
- `build/artifacts/`: build artifact cache
- `build/image/`: staged boot volume root
- `logs/`: QEMU, debugcon, interrupt, and helper logs
- `drivers/bridges/`: kernel-address-space bridge drivers and `.ko` modules
- `drivers/user/`: userspace driver/service ELF packages
- `drivers/libs/`: driver ABI, runtime, and helper crates
- `vendor/`: external binaries, firmware, prebuilt `.ko` files, and OVMF
- `config/`: shared configuration such as logging policy
- `docs/`: architecture, workflow, and operator documentation

### Placement Rules

- Deployment policy belongs in each package root's `RUSTOS.package.toml`.
- Choose the owning layer first: `boot`, `kernel`, `services`, `apps`,
  `compat`, `drivers/bridges`, `drivers/user`, `libs`, or `drivers/libs`.
- Boot-chain shared logic belongs under `boot/`.
- Driver ABI/runtime logic or bus/device helpers belong under `drivers/libs/`.
- Code that needs hardware IRQ/MMIO/DMA bridge access belongs under
  `drivers/bridges/`.
- Restartable policy-oriented logic belongs under `services/` or
  `drivers/user/`.
- Only code that needs hardware, ABI entrypoints, or core kernel privileges
  belongs under `kernel/`.
- Static files that should appear in the boot volume go under `assets/image/`.
- External binary inputs that the repository keeps should go under `vendor/`.
- Generated outputs must stay under `build/` or `logs/`.

### Layering Rules

- `kernel/src` is entry-only. Real kernel functionality is split across
  `kernel/nucleus-core`, `kernel/lowlevel`, `kernel/hal`, `kernel/mm`,
  `kernel/object`, `kernel/ipc-runtime`, `kernel/ps`, `kernel/io-manager`,
  `kernel/compat`, and `kernel/executive`.
- Kernel-internal crate dependencies must not flow from lower layers back into
  higher layers. `cargo xtask check` validates these relationships.
- `libs/` may depend only on boot protocol crates and other `libs/` crates.
  It must not depend on implementation crates from `kernel/`, `services/`,
  `apps/`, or `drivers/bridges/`.
- `drivers/libs/` may depend on `libs/`, `drivers/libs/`, and required boot
  protocol crates. It must not depend on bridge driver implementations.
- `services/` may depend on `libs/`, `compat/`, and `drivers/libs/`.
- `apps/` may depend on `libs/` and `compat/`.
- `drivers/bridges/` may depend on kernel ABI surfaces, `libs/`, and
  `drivers/libs/`.
- `tests/` may reference product crates, but tests are still included in the
  layering checks.

### Runtime Dependencies

- A deployable unit that needs another unit to run or be exposed first declares
  `runtime_deps = ["package-id"]` in `RUSTOS.package.toml`.
- `runtime_deps` values are package `id` values, not install paths and not
  desktop file ids.
- `cargo xtask check` validates dependency existence, profile inclusion,
  self-dependencies, and dependency cycles for the selected profile.
- Stage writes dependency metadata into the startup/runtime registries as
  `deps=` fields. Launcher ordering enforcement is handled separately.

### Vendor, Assets, And Build Outputs

- `vendor/`: external source inputs preserved by the repository
- `assets/image/`: files copied into the staged image
- `build/`: reproducible build and stage output
- `logs/`: run/debug output

Do not put generated artifacts, QEMU run output, or local debug captures in
source directories.

### Adding A New Module

1. If it is a source-based first-party kernel bridge, add a crate under
   `drivers/bridges/...`.
2. If it is a userspace driver or service, add it under `drivers/user/...` or
   `services/...`.
3. If shared driver API/helper code is needed, use `drivers/libs/...`.
4. If it is deployable, add `RUSTOS.package.toml` at the package root and
   declare `kind`, `execution_domain`, `startup`, and `install.path`.
5. Run `cargo xtask check` to validate layering and package taxonomy.
6. Run `cargo xtask build`, then inspect generated registries under
   `build/image/system/registry/...` if startup, desktop, or driver policy
   changed.
7. Update this document and [README.md](../README.md) when path rules or
   ownership boundaries change.

<a id="korean"></a>

## 한국어

이 문서는 RustOS 저장소 배치 규칙을 정의합니다. 이 규칙은 단순 문서가
아니며, 중요한 dependency와 package taxonomy 검사는 `cargo xtask check`로
강제됩니다.

### 디렉터리 역할

- `boot/`: bootloader, prekernel, boot protocol crate
- `kernel/`: kernel entry crate와 내부 kernel subsystem crate
- `services/`: `initd`, `runtimed`, `uiserver` 같은 userspace system service
- `apps/`: demo, smoke, desktop, compatibility test application
- `compat/`: Windows userspace 지원을 포함한 호환 계층 소스
- `libs/`: 여러 제품 계층이 공유하는 일반-purpose crate
- `tests/`: integration/module test crate
- `tools/`: host-side build, stage, run, validation 도구
- `assets/image/`: staged boot image에 그대로 복사되는 정적 overlay
- `assets/ui/`: UI font와 관련 UI asset
- `build/artifacts/`: build artifact cache
- `build/image/`: staged boot volume root
- `logs/`: QEMU, debugcon, interrupt, helper log
- `drivers/bridges/`: kernel address space에 남는 bridge driver와 `.ko` module
- `drivers/user/`: userspace driver/service ELF package
- `drivers/libs/`: driver ABI, runtime, helper crate
- `vendor/`: 외부 바이너리, firmware, prebuilt `.ko`, OVMF
- `config/`: logging policy 같은 공유 설정
- `docs/`: architecture, workflow, operator 문서

### 코드 배치 규칙

- 배포 단위 정책은 각 package root의 `RUSTOS.package.toml`에 둡니다.
- 먼저 소유 계층을 결정합니다: `boot`, `kernel`, `services`, `apps`,
  `compat`, `drivers/bridges`, `drivers/user`, `libs`, `drivers/libs`.
- boot chain 공용 로직은 `boot/` 아래에 둡니다.
- driver ABI/runtime 로직이나 bus/device helper는 `drivers/libs/` 아래에 둡니다.
- hardware IRQ/MMIO/DMA bridge 접근이 필요한 코드는 `drivers/bridges/`
  아래에 둡니다.
- 재시작 가능하고 정책 중심인 로직은 `services/` 또는 `drivers/user/`
  아래에 둡니다.
- hardware, ABI entrypoint, core kernel 권한이 필요한 코드만 `kernel/`
  아래에 둡니다.
- boot volume에 들어갈 정적 파일은 `assets/image/` 아래에 둡니다.
- 저장소가 보관하는 외부 binary input은 `vendor/` 아래에 둡니다.
- 생성 산출물은 `build/` 또는 `logs/` 아래에만 둡니다.

### 계층 규칙

- `kernel/src`는 entry-only입니다. 실제 kernel 기능은
  `kernel/nucleus-core`, `kernel/lowlevel`, `kernel/hal`, `kernel/mm`,
  `kernel/object`, `kernel/ipc-runtime`, `kernel/ps`, `kernel/io-manager`,
  `kernel/compat`, `kernel/executive`로 나눕니다.
- kernel 내부 crate dependency는 낮은 계층에서 높은 계층으로 역류하면
  안 됩니다. `cargo xtask check`가 이 관계를 검사합니다.
- `libs/`는 boot protocol crate와 다른 `libs/` crate에만 의존할 수
  있습니다. `kernel/`, `services/`, `apps/`, `drivers/bridges/` 구현 crate를
  끌어오면 안 됩니다.
- `drivers/libs/`는 `libs/`, `drivers/libs/`, 필요한 boot protocol crate에만
  의존할 수 있습니다. bridge driver 구현에 의존하면 안 됩니다.
- `services/`는 `libs/`, `compat/`, `drivers/libs/`에 의존할 수 있습니다.
- `apps/`는 `libs/`와 `compat/`에 의존할 수 있습니다.
- `drivers/bridges/`는 kernel ABI surface, `libs/`, `drivers/libs/`에 의존할
  수 있습니다.
- `tests/`는 product crate를 참조할 수 있지만 layering check 대상에
  포함됩니다.

### 런타임 의존성

- 실행 또는 노출 순서가 필요한 배포 단위는 `RUSTOS.package.toml`에
  `runtime_deps = ["package-id"]`를 선언합니다.
- `runtime_deps` 값은 install path나 desktop file id가 아니라 package
  `id`입니다.
- `cargo xtask check`는 선택된 profile 기준으로 dependency 존재 여부, 같은
  profile 포함 여부, 자기 자신 의존, 순환 의존을 검사합니다.
- stage는 startup/runtime registry에 `deps=` metadata를 기록합니다.
  launcher 실행 순서 강제는 별도 단계에서 처리합니다.

### Vendor, Assets, Build 구분

- `vendor/`: 저장소가 보관하는 외부 source input
- `assets/image/`: staged image에 복사되는 파일
- `build/`: 재생성 가능한 build/stage output
- `logs/`: run/debug output

생성 artifact, QEMU 실행 output, local debug capture를 source directory에
두지 마세요.

### 새 모듈 추가 절차

1. source 기반 first-party kernel bridge라면 `drivers/bridges/...`에 crate를
   추가합니다.
2. userspace driver 또는 service라면 `drivers/user/...` 또는 `services/...`
   아래에 추가합니다.
3. 공용 driver API/helper code가 필요하면 `drivers/libs/...`를 사용합니다.
4. deployable 단위라면 package root에 `RUSTOS.package.toml`을 추가하고
   `kind`, `execution_domain`, `startup`, `install.path`를 선언합니다.
5. `cargo xtask check`로 layering과 package taxonomy를 검증합니다.
6. `cargo xtask build`를 실행한 뒤 startup, desktop, driver 정책이 바뀌었다면
   `build/image/system/registry/...` 아래 generated registry를 확인합니다.
7. 경로 규칙이나 ownership boundary가 바뀌면 이 문서와
   [README.md](../README.md)를 같이 갱신합니다.
