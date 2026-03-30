# RustOS

## 개요

현재 기본 개발 흐름은 `cargo xtask build` / `cargo xtask run` 과 VS Code launch 구성을 기준으로 맞춰져 있습니다. 부팅 볼륨의 기준 디렉터리는 `build/image` 이고, 실제 UEFI 기본 엔트리는 `build/image/EFI/BOOT/BOOTX64.EFI` 입니다.

## 디렉터리 역할

- `assets/image/`: staged 부팅 이미지에 그대로 덮어쓸 정적 overlay
- `boot/bootloader/`: UEFI 부트로더와 boot protocol
- `boot/prekernel/`: `kernel.elf` 적재와 초기 KASLR 적용
- `core/kernel/`: 커널 본체
- `system/packages/`: native 서비스와 앱 소스
- `compat/windows/user/`: Windows userland 호환 DLL/runtime 소스
- `samples/`: demo, smoke, validation 프로그램 소스
- `tests/`: 통합 테스트 crate
- `tools/xtask/`: host-side build, stage, run orchestrator
- `boot/`: boot chain 바이너리와 boot-owned 공용 crate
- `drivers/`: first-party `.ko` 드라이버와 driver ABI/runtime 공용 crate
- `docs/`: 구조 문서와 README 이미지 자산
- `vendor/`: git 에 유지하는 외부 바이너리 자산과 prebuilt `.ko`, OVMF
- `build/artifacts/`: 컴파일 산출물 보관소
- `build/image/`: 실제 부팅 볼륨 루트
- `build/logs/`: QEMU interrupt/debug 로그

추가 구조 설명은 [`docs/structure.md`](docs/structure.md) 에 정리돼 있습니다.

## 준비

```bash
sudo apt update
sudo apt install -y rustup gcc nasm qemu-system-x86 ovmf

rustup default nightly
rustup component add rust-src llvm-tools-preview
rustup target add x86_64-unknown-uefi
```

## VS Code

공용 launch 는 `QEMU`, `KVM`, `G14` 세 가지입니다.

- `QEMU`: 실행 전 `Build OS (QEMU)`를 돌리고, `opt-level=0` 빌드 후 QEMU를 실행합니다.
- `KVM`, `G14`: 기본 release 최적화 빌드를 사용합니다.
- QEMU interrupt 로그는 기본적으로 `build/logs/qemu_interrupt.log` 에 남깁니다.

<img src="docs/assets/openfolder.png" alt="open folder" width="300" />

<img src="docs/assets/run.png" alt="run" width="300" />

## 터미널

```bash
cargo xtask build
cargo xtask run
```

`cargo xtask run` 은 staged image를 자동으로 새로 만들지 않으므로, 보통 `cargo xtask build` 다음에 실행합니다.

유용한 명령:

- `cargo xtask check`
- `cargo xtask stage`
- `cargo xtask clean`
- `cargo test -p module-tests`

## 자산 배치 규칙

- first-party 드라이버 소스는 `drivers/...` 에 둡니다.
- deployable 패키지 정책은 각 패키지 루트의 `RUSTOS.package.toml` 이 source of truth 입니다.
- third-party 또는 prebuilt `.ko`, firmware, OVMF 는 `vendor/...` 에 둡니다.
- staged image에 정적으로 포함할 overlay 파일만 `assets/image/...` 에 둡니다.
- `assets/image` 아래 경로는 boot volume 내부 경로와 동일해야 합니다.

예시:

- `vendor/modules/input/usbhid.ko` -> stage 시 `system/drivers/input/usbhid.ko`
- `system/packages/uiserver/RUSTOS.package.toml` -> stage 시 `system/packages/uiserver/uiserver.elf`
- `samples/windows/userdemo2/RUSTOS.package.toml` -> stage 시 `samples/windows/userdemo2/userdemo2.exe`
- `vendor/ovmf/OVMF.fd` -> QEMU firmware
- `assets/image/system/config/foo.txt` -> `build/image/system/config/foo.txt`

## 로그와 산출물

- build 산출물은 `build/` 아래만 사용합니다.
- QEMU debugcon 로그는 필요 시 `build/logs/debugcon.log` 에 기록됩니다.
- repo 루트에는 실행 로그나 firmware 바이너리를 직접 두지 않습니다.
