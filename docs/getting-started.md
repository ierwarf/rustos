# Getting Started

[English](#english) | [한국어](#korean)

<a id="english"></a>

## English

### Prerequisites

Ubuntu/Debian baseline:

```bash
sudo apt update
sudo apt install -y rustup gcc nasm qemu-system-x86 ovmf mingw-w64 grub-efi-amd64-bin grub-common gnupg

rustup default nightly
rustup component add rust-src llvm-tools-preview
rustup target add x86_64-unknown-linux-gnu
```

Default firmware path:

```text
vendor/firmware/ovmf/OVMF.fd
```

Use `OVMF_PATH=/path/to/OVMF.fd` to override it.

### Build

```bash
export RUSTOS_GRUB_PUBKEY=/path/to/grub-public-key.gpg # from: gpg --export <key>
export RUSTOS_GRUB_SIGNING_KEY=<gpg-key-id-or-fingerprint>
cargo xtask check
cargo xtask build
```

`cargo xtask build` validates layering, builds boot/user/kernel artifacts,
builds modules and compatibility assets, stages the boot image, and writes
runtime registries. The default boot manager build signs `nucleus.elf` with GPG
and embeds `RUSTOS_GRUB_PUBKEY` into the generated GRUB EFI binary; the private
signing key must stay outside the repository.

### Run

```bash
cargo xtask run
```

`cargo xtask run` expects an existing `build/image`. Re-run `cargo xtask build`
or `cargo xtask stage` after changing image contents.

### Useful Run Modes

| Command | Purpose |
| --- | --- |
| `cargo xtask run -- --no-reboot` | Pass raw QEMU args after `--`. |
| `cargo xtask run --profile nvme` | Boot using the NVMe QEMU profile. |
| `cargo xtask run --accel-profile kvm` | Use KVM acceleration and host CPU profile. |
| `cargo xtask run --usb-input` | Attach USB keyboard/tablet devices for HID testing. |
| `cargo xtask run --qemu-log int` | Write QEMU interrupt trace to `logs/qemu_interrupt.log`. |
| `cargo xtask run --debugcon stdio` | Route debugcon to the terminal. |

### Debug

```bash
cargo xtask debug
```

This starts QEMU with `-s -S` and writes `logs/rustos-debug.gdb`.

### Probe Display

```bash
cargo xtask probe-display
```

This uses the headless display probe path and is useful for framebuffer,
surface, and dirty-rect regressions.

<a id="korean"></a>

## 한국어

### 준비

Ubuntu/Debian 기준:

```bash
sudo apt update
sudo apt install -y rustup gcc nasm qemu-system-x86 ovmf mingw-w64 grub-efi-amd64-bin grub-common gnupg

rustup default nightly
rustup component add rust-src llvm-tools-preview
rustup target add x86_64-unknown-linux-gnu
```

기본 firmware 경로:

```text
vendor/firmware/ovmf/OVMF.fd
```

다른 firmware를 쓰려면 `OVMF_PATH=/path/to/OVMF.fd`를 지정합니다.

### 빌드

```bash
export RUSTOS_GRUB_PUBKEY=/path/to/grub-public-key.gpg # gpg --export <key> 산출물
export RUSTOS_GRUB_SIGNING_KEY=<gpg-key-id-or-fingerprint>
cargo xtask check
cargo xtask build
```

`cargo xtask build`는 layering 검사, boot/user/kernel artifact 빌드, module과
compatibility asset 빌드, boot image staging, runtime registry 생성을 수행합니다.
기본 boot manager build는 GPG로 `nucleus.elf`를 서명하고
`RUSTOS_GRUB_PUBKEY`를 generated GRUB EFI binary에 embed합니다. private signing
key는 repository 밖에 둡니다.

### 실행

```bash
cargo xtask run
```

`cargo xtask run`은 기존 `build/image`를 사용합니다. image 내용이 바뀌면
`cargo xtask build` 또는 `cargo xtask stage`를 다시 실행하세요.

### 유용한 실행 모드

| Command | Purpose |
| --- | --- |
| `cargo xtask run -- --no-reboot` | `--` 뒤의 값을 raw QEMU arg로 전달합니다. |
| `cargo xtask run --profile nvme` | NVMe QEMU profile로 부팅합니다. |
| `cargo xtask run --accel-profile kvm` | KVM acceleration과 host CPU profile을 사용합니다. |
| `cargo xtask run --usb-input` | HID test용 USB keyboard/tablet device를 붙입니다. |
| `cargo xtask run --qemu-log int` | QEMU interrupt trace를 `logs/qemu_interrupt.log`에 기록합니다. |
| `cargo xtask run --debugcon stdio` | debugcon을 terminal로 보냅니다. |

### 디버그

```bash
cargo xtask debug
```

QEMU를 `-s -S`로 시작하고 `logs/rustos-debug.gdb`를 생성합니다.

### Display Probe

```bash
cargo xtask probe-display
```

headless display probe path를 사용합니다. framebuffer, surface, dirty-rect
regression을 확인할 때 유용합니다.
