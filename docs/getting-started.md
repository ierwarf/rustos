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
`verify-dvm` checks its self-contained schema-9 artifact bundle, including the
co-located source lock and host-control contract. Stage a production bundle to
a fresh trusted path with `make -C driver-domains/linux stage-release
DEST=/trusted/new/path`; this does not rebuild the DVM.
`kvm-smoke` creates a private disk under `build/kvm/`, launches Linux DVM and
RustOS concurrently, requires the RustOS readiness marker plus the validated
host-to-DVM control probe, then stops only the QEMU children it created. It
preserves their debugcon, serial, and stderr captures for inspection.

The DVM advertises a hash-bound `agent-v1-control` handshake. The synthetic
keyboard smoke injects `A` into the DVM virtio keyboard and verifies Linux
evdev code `30` plus a newly consumed `inputd` event. Separate bounded
DVM input, display, Ethernet, and read-only block relays are exercised by
explicit KVM options (see [xtask API](api/xtask.md)). This virtual smoke
is not proof of physical host-keyboard capture, hardware passthrough,
production driver lifecycle, or data durability.

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
`verify-dvm`은 같은 디렉터리에 함께 있는 source lock과 host-control contract를
포함한 자기완결 schema-9 artifact bundle을 검증합니다. 운영 bundle은
`make -C driver-domains/linux stage-release DEST=/trusted/new/path`로 새 신뢰
경로에 배치하며, 이 명령은 DVM을 다시 빌드하지 않습니다.
`kvm-smoke`는 `build/kvm/`에 private disk를 만들고 Linux DVM과 RustOS를 병렬로
부팅합니다. RustOS readiness marker와 검증된 host-to-DVM control probe를
요구하며 자신이 만든 QEMU child만 종료합니다. debugcon, serial, stderr capture는
분석용으로 유지합니다.

DVM은 해시로 검증되는 `agent-v1-control` handshake를 제공합니다. 합성
keyboard smoke는 DVM virtio keyboard에 `A`를 주입하고 Linux evdev code
`30` 및 `inputd`의 새 이벤트 소비를 검증합니다. 입력·화면·Ethernet·읽기 전용
블록 relay는 별도 KVM 옵션으로 검증하며(참고: [xtask API](api/xtask.md)),
이 가상 smoke가 물리 host keyboard capture, 하드웨어 passthrough, 상용
드라이버 lifecycle 또는 데이터 내구성을 증명하지는 않습니다.
