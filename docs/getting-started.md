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

### KVM parallel boot smoke

Run the following on a host with `/dev/kvm` access and `qemu-system-x86_64`:

```bash
cargo xtask build-dvm
cargo xtask kvm-smoke --expect 'runtimed: bootstrap ui done'
```

`build-dvm` produces a pinned Buildroot Linux driver-domain appliance and
`verify-dvm` checks its artifact and pre-transport control-contract hashes.
`kvm-smoke` creates a private disk under `build/kvm/`, launches Linux DVM and
RustOS concurrently, requires both readiness markers, then stops only the
QEMU children it created. It preserves their debugcon, serial, and stderr
captures for inspection.

The DVM currently advertises `agent-v1-pretransport`: there is no authenticated
RustOS↔DVM transport or usable device data plane yet. Do not interpret a KVM
smoke pass as NIC, storage, `.ko`, or passthrough validation.

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

### KVM 병렬 부팅 smoke

`/dev/kvm` 접근과 `qemu-system-x86_64`가 있는 host에서 실행합니다.

```bash
cargo xtask build-dvm
cargo xtask kvm-smoke --expect 'runtimed: bootstrap ui done'
```

`build-dvm`은 고정된 Buildroot Linux driver-domain appliance를 만들고,
`verify-dvm`은 artifact와 pre-transport control-contract hash를 검증합니다.
`kvm-smoke`는 `build/kvm/`에 private disk를 만들고 Linux DVM과 RustOS를 병렬로
부팅합니다. 양쪽 readiness marker를 모두 요구하며 자신이 만든 QEMU child만
종료합니다. debugcon, serial, stderr capture는 분석용으로 유지합니다.

현재 DVM이 `agent-v1-pretransport`만 알리므로 인증된 RustOS↔DVM transport와
device data plane이 아직 없습니다. KVM smoke 통과를 NIC, storage, `.ko`,
passthrough 검증으로 해석하면 안 됩니다.
