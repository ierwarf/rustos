# 구조 정리 기준

## 디렉터리 역할

- `boot/`: bootloader, prekernel, boot protocol
- `kernel/`: scheduler, VM, trap/IRQ, syscall, task, handle, broker 같은 커널 메커니즘
- `services/`: `initd`, `uiserver` 같은 system service ELF
- `apps/`: demo, smoke, desktop app ELF
- `compat/`: 호환 계층 소스
- `libs/`: 여러 제품 계층이 공유하는 일반-purpose crate
- `tests/`: 통합 테스트
- `tools/`: host-side build/run 도구
- `assets/image/`: 부팅 이미지 overlay
- `build/artifacts/`: 컴파일 결과 보관
- `build/image/`: staged 부팅 볼륨 루트
- `logs/`: QEMU 및 디버그 로그
- `drivers/bridges/`: kernel address space에 남는 `.ko` bridge
- `drivers/user/`: user-mode driver/service ELF
- `drivers/libs/`: driver ABI/runtime/helper 공용 crate
- `vendor/`: 외부 바이너리, firmware, prebuilt `.ko`, OVMF

## 새 코드 배치 규칙

- 배포 단위 정책은 각 패키지 루트의 `RUSTOS.package.toml` 에 둡니다.
- 먼저 소유 계층을 결정합니다: `boot`, `kernel`, `services`, `apps`, `compat`, `drivers/bridges`, `drivers/user`, `libs`, `drivers/libs`.
- boot chain 공용 로직은 `boot/` 아래 crate에 둡니다.
- driver ABI/runtime 또는 특정 버스/디바이스 공용 로직은 `drivers/libs/` 아래 crate에 둡니다.
- 하드웨어 IRQ/MMIO/DMA bridge가 필요한 코드는 `drivers/bridges/` 아래에 둡니다.
- 재시작 가능하고 정책 중심인 로직은 `services/` 또는 `drivers/user/` 아래에 둡니다.
- 하드웨어나 ABI entrypoint가 필요한 코드만 `kernel/` 아래에 둡니다.
- 단순 staged asset 은 `assets/image/` 로 갑니다.
- 외부에서 가져온 바이너리 파일은 `vendor/` 로 갑니다.

## 계층 규칙

- `kernel/src` 는 entry-only 이며, 실제 커널 기능은 `kernel/nucleus-core`, `kernel/lowlevel`, `kernel/hal`, `kernel/mm`, `kernel/object`, `kernel/ipc-runtime`, `kernel/ps`, `kernel/io-manager`, `kernel/compat`, `kernel/executive` 로 나눕니다.
- kernel 내부 crate 의존은 낮은 계층에서 높은 계층으로 역류하지 않도록 `cargo xtask check` 에서 세분화해 검사합니다.
- `libs/` 는 `boot/` protocol crate 와 다른 `libs/` 에만 의존합니다. `kernel/`, `services/`, `apps/`, `drivers/bridges/` 구현 crate를 끌어오면 안 됩니다.
- `drivers/libs/` 는 `libs/`, `drivers/libs/`, 필요한 boot protocol 에만 의존합니다. bridge driver 구현에 의존하면 안 됩니다.
- `services` 는 `libs/`, `compat/`, `drivers/libs/` 에만 의존합니다.
- `apps` 는 `libs/` 와 `compat/` 에만 의존합니다.
- `drivers/bridges` 는 `kernel/` ABI 와 `libs/`, `drivers/libs/` 에만 의존합니다.
- `tests/` 는 product crate 를 참조할 수 있지만, 계층 검사의 대상에 포함됩니다.
- 이 규칙은 문서가 아니라 `cargo xtask check` 의 layering check 로 강제합니다.

## 런타임 의존성

- 실행/노출 순서가 필요한 배포 단위는 `RUSTOS.package.toml` 에 `runtime_deps = ["package-id"]` 를 선언합니다.
- `runtime_deps` 값은 install path 나 desktop id 가 아니라 package `id` 입니다.
- `cargo xtask check` 는 선택된 profile 기준으로 dependency 존재 여부, 같은 profile 포함 여부, 자기 자신 의존, 순환 의존을 검사합니다.
- stage 단계는 startup/runtime registry 에 `deps=` metadata 를 기록합니다. 현재 런처 실행 순서 강제는 별도 단계에서 처리합니다.

## vendor / assets / build 구분

- `vendor`: 저장소가 보관하는 외부 원본
- `assets/image`: stage 시 복사되는 정적 overlay
- `build`: 언제든 재생성 가능한 산출물

## 새 모듈 추가 절차

1. source 기반 first-party kernel bridge 면 `drivers/bridges/...` 에 crate를 추가합니다.
2. user-mode driver/service 면 `drivers/user/...` 또는 `services/...` 에 crate를 추가합니다.
3. 공용 driver API/helper 가 필요하면 `drivers/libs/...` 를 사용합니다.
4. deployable 단위라면 해당 패키지 루트에 `RUSTOS.package.toml` 을 추가하고 `kind`, `execution_domain`, `startup` 을 명시합니다.
5. `cargo xtask check` 로 layering/package taxonomy 검사를 통과시키고, `cargo xtask build` 후 `build/image/system/registry/...` generated registry를 확인합니다.
6. README나 이 문서에 경로 규칙이 바뀌면 같이 갱신합니다.
