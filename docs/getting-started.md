# Getting Started

[English](#english) | [한국어](#korean)

<a id="english"></a>

## English

### Build prerequisites

Ubuntu/Debian baseline:

```bash
sudo apt update
sudo apt install -y rustup gcc nasm mingw-w64 grub-efi-amd64-bin grub-common gnupg libelf-dev

rustup default nightly
rustup component add rust-src llvm-tools-preview
rustup target add x86_64-unknown-linux-gnu
```

`libelf-dev` is required only when building the Linux DVM. RustOS uses the
repository-pinned firmware at `vendor/firmware/ovmf/OVMF.fd`; set `OVMF_PATH`
only for a deliberate replacement.

### Build and validate

```bash
cargo xtask check
cargo xtask build
cargo xtask selftest
cargo xtask fuzz-host --target all
```

`build` stages a signed UEFI boot disk at `build/rustos-boot.img`. With no
`RUSTOS_GRUB_*` configuration, xtask creates a local development key under
`build/dev-grub-gpg`. Release keys stay outside the repository.

### Xen lifecycle smoke

Run the following from an active Xen Dom0 with `xl` privileges:

```bash
cargo xtask build-dvm
cargo xtask xen-smoke --expect 'uiserver: wayland compositor ready'
```

`build-dvm` produces a pinned Buildroot Linux driver-domain appliance and
`verify-dvm` checks its artifact and pre-transport control-contract hashes.
`xen-smoke` creates a private HVM disk under `build/xen/`, submits Linux DVM
and RustOS HVM creation concurrently without replacing existing domains, then
stops on either non-running domain and leaves Xen state intact for inspection.

`cargo xtask run` is the production Xen entry point. It is intentionally
blocked while the DVM advertises `agent-v1-pretransport`: there is no
authenticated RustOS↔DVM transport or usable device data plane yet. Do not
interpret a lifecycle smoke as NIC, storage, `.ko`, or passthrough validation.

<a id="korean"></a>

## 한국어

### 빌드 준비

Ubuntu/Debian 기준:

```bash
sudo apt update
sudo apt install -y rustup gcc nasm mingw-w64 grub-efi-amd64-bin grub-common gnupg libelf-dev

rustup default nightly
rustup component add rust-src llvm-tools-preview
rustup target add x86_64-unknown-linux-gnu
```

`libelf-dev`는 Linux DVM 빌드에만 필요합니다. RustOS는
`vendor/firmware/ovmf/OVMF.fd`의 고정 firmware를 사용합니다. `OVMF_PATH`는
의도적으로 교체할 때만 지정하세요.

### 빌드와 검증

```bash
cargo xtask check
cargo xtask build
cargo xtask selftest
cargo xtask fuzz-host --target all
```

`build`는 서명된 UEFI boot disk `build/rustos-boot.img`를 만듭니다.
`RUSTOS_GRUB_*`가 없으면 xtask는 `build/dev-grub-gpg`에 개발용 키를 만듭니다.
release key는 repository 밖에 둡니다.

### Xen lifecycle smoke

활성 Xen Dom0에서 `xl` 권한으로 실행합니다.

```bash
cargo xtask build-dvm
cargo xtask xen-smoke --expect 'uiserver: wayland compositor ready'
```

`build-dvm`은 고정된 Buildroot Linux driver-domain appliance를 만들고,
`verify-dvm`은 artifact와 pre-transport control-contract hash를 검증합니다.
`xen-smoke`는 `build/xen/`에 private HVM disk를 만들고 기존 domain을 교체하지
않은 채 Linux DVM과 RustOS HVM 생성 요청을 병렬로 냅니다. 어느 domain이 실행
상태를 유지하지 못하면 즉시 실패하며, 분석을 위해 Xen state는 그대로 남깁니다.

`cargo xtask run`은 상용 Xen 진입점입니다. 현재 DVM이
`agent-v1-pretransport`만 알리므로 인증된 RustOS↔DVM transport와 device data
plane이 아직 없습니다. 따라서 기본 실행은 의도적으로 차단됩니다. lifecycle
smoke를 NIC, storage, `.ko`, passthrough 검증으로 해석하면 안 됩니다.
