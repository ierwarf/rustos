# Environment Variables Reference

[English](#english) | [한국어](#korean)

<a id="english"></a>

## English

| Variable | Default | Purpose |
| --- | --- | --- |
| `ROOT_DIR` | repo root | Override repository root. |
| `WORKSPACE_MANIFEST` | `Cargo.toml` | Override workspace manifest path. |
| `CARGO_TARGET_DIR` | `target` | Override Cargo target dir. |
| `CARGO` | `cargo` | Cargo executable. |
| `RUSTUP` | `rustup` | rustup executable. |
| `CC` | `gcc` | C compiler. |
| `MINGW_CC` | `x86_64-w64-mingw32-gcc` | Windows PE compiler. |
| `KVM_QEMU_BIN` | `qemu-system-x86_64` | QEMU command used for KVM smoke. |
| `KERNEL_TARGET` | `x86_64-unknown-linux-gnu` | Kernel/userspace target. |
| `GRUB_MKSTANDALONE` | `grub-mkstandalone` | GRUB standalone EFI builder. |
| `GRUB_FILE` | `grub-file` | Multiboot2 artifact validator. |
| `GPG` | `gpg` | GPG executable used for detached kernel signatures. |
| `RUSTOS_GRUB_PUBKEY` | `build/dev-grub.pub` | Binary GPG public key file embedded into GRUB, produced with `gpg --export`. |
| `RUSTOS_GRUB_SIGNING_KEY` | `RustOS Dev GRUB <rustos-dev-grub@example.invalid>` | GPG key id or fingerprint used to sign `nucleus.elf`; `xtask build` creates the default development key when missing. |
| `RUSTOS_GPG_HOME` | `build/dev-grub-gpg` | Optional GPG home for signing. |
| `RUSTOS_GRUB_SBAT` | empty | Optional SBAT metadata file passed to `grub-mkstandalone`. |
| `RUSTOS_GRUB_MODULES` | secure boot module set | Optional GRUB module list override. |
| `BUILD_DIR` | `build` | Build output root. |
| `IMAGE_DIR` | `build/image` | Staged image root. |
| `OVMF_PATH` | `vendor/firmware/ovmf/OVMF.fd` | Firmware image. |
| `RUSTOS_UI_BOOT_TRACE` | disabled | Enable uiserver local boot trace. |
| `RUSTOS_UI_PROFILE` | disabled | Enable uiserver profiling lines. |
| `RUSTOS_UI_CONFIG` | `/assets/ui/ui.conf` | Development override of the uiserver UI config path (font file, size and charset per text role, and the home screen clock UTC offset). runtimed launches uiserver with a fixed environment, so the image relies on the default. |
| `RUSTOS_WAYCLICK_PROFILE` | disabled | Enable WayClick frame-lifecycle profiling; `xtask kvm-smoke --min-ui-fps` changes only its private KVM disk copy. |

Most build variables are parsed by `tools/xtask/src/config/mod.rs`. KVM
execution is in `tools/xtask/src/kvm.rs`; runtime admission requires QEMU,
read/write KVM/vhost-vsock, and the virtual display's exact AMDGPU render node
([host prerequisites](../getting-started.md#kvm-parallel-boot-smoke)). It never
changes the host hypervisor configuration; dry-run does not admit devices.

<a id="korean"></a>

## 한국어

| Variable | Default | Purpose |
| --- | --- | --- |
| `ROOT_DIR` | repo root | repository root override |
| `WORKSPACE_MANIFEST` | `Cargo.toml` | workspace manifest path override |
| `CARGO_TARGET_DIR` | `target` | Cargo target dir override |
| `CARGO` | `cargo` | Cargo executable |
| `RUSTUP` | `rustup` | rustup executable |
| `CC` | `gcc` | C compiler |
| `MINGW_CC` | `x86_64-w64-mingw32-gcc` | Windows PE compiler |
| `KVM_QEMU_BIN` | `qemu-system-x86_64` | KVM smoke에 사용하는 QEMU command |
| `KERNEL_TARGET` | `x86_64-unknown-linux-gnu` | kernel/userspace target |
| `GRUB_MKSTANDALONE` | `grub-mkstandalone` | GRUB standalone EFI builder |
| `GRUB_FILE` | `grub-file` | Multiboot2 artifact validator |
| `GPG` | `gpg` | detached kernel signature 생성용 GPG executable |
| `RUSTOS_GRUB_PUBKEY` | `build/dev-grub.pub` | `gpg --export`로 만든 GRUB embed용 binary GPG public key file |
| `RUSTOS_GRUB_SIGNING_KEY` | `RustOS Dev GRUB <rustos-dev-grub@example.invalid>` | `nucleus.elf` 서명에 사용할 GPG key id/fingerprint. 없으면 `xtask build`가 기본 개발 키를 생성 |
| `RUSTOS_GPG_HOME` | `build/dev-grub-gpg` | signing에 사용할 optional GPG home |
| `RUSTOS_GRUB_SBAT` | empty | `grub-mkstandalone`에 넘길 optional SBAT metadata file |
| `RUSTOS_GRUB_MODULES` | secure boot module set | optional GRUB module list override |
| `BUILD_DIR` | `build` | build output root |
| `IMAGE_DIR` | `build/image` | staged image root |
| `OVMF_PATH` | `vendor/firmware/ovmf/OVMF.fd` | firmware image |
| `RUSTOS_UI_BOOT_TRACE` | disabled | uiserver local boot trace 활성화 |
| `RUSTOS_UI_PROFILE` | disabled | uiserver profiling line 활성화 |
| `RUSTOS_UI_CONFIG` | `/assets/ui/ui.conf` | uiserver UI 설정 경로의 개발용 override(text role별 font 파일·크기·charset, home 화면 시계 UTC offset). runtimed가 uiserver를 고정 환경으로 실행하므로 image는 기본값을 사용 |
| `RUSTOS_WAYCLICK_PROFILE` | disabled | WayClick frame lifecycle profile 활성화. `xtask kvm-smoke --min-ui-fps`는 private KVM disk copy만 변경 |

대부분의 build variable은 `tools/xtask/src/config/mod.rs`에서 읽습니다. KVM
실행은 `tools/xtask/src/kvm.rs`에 있으며 QEMU, read/write KVM/vhost-vsock,
가상 display용 정확한 AMDGPU render node가 필요합니다
([host 조건](../getting-started.md#kvm-병렬-부팅-smoke)). Dry-run은 장치를 admission하지 않습니다.
xtask가 host hypervisor 설정을 바꾸지는 않습니다.
