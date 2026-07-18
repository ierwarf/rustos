# RustOS Linux driver-domain appliance

[English](#english) | [한국어](#korean)

## English

This is a Buildroot-based, immutable Linux appliance for RustOS driver
domains. `cargo xtask build-dvm` and `verify-dvm` use this exact wrapper and
verify its manifest hashes. The image carries a hashed, fail-closed
`agent-v1-control` contract. Its agent permits only L0-host-authenticated
KVM-vsock health, device/driver inventory, versioned physical-display evidence,
and the bounded input stream that
feeds RustOS `inputd`. Display pixels use a separate read-only-DMA triple-slot
KMS transport; the virtual Ethernet test path uses its own fixed ring and is
not physical-device authority. The synthetic input smoke is test-only and is
not physical host-keyboard capture or arbitrary-key forwarding. Physical
display release remains blocked until the supervised IOMMUFD/VFIO lifecycle,
connector, DMA-fault/revoke/reset, and sustained page-flip evidence passes on
target hardware.
The enabled physical release policy is AMDGPU-only: it binds `1002:1900` and
requires five fresh authenticated samples of direct zero-copy page flips,
nominal-60-Hz throughput, and bounded page-flip/atomic-commit latency. The
current active boot GPU cannot be used to collect that evidence safely.

The pinned inputs are in `sources.lock`:

- Buildroot 2026.05
- Linux 6.12.94
- NVIDIA-open 580.173.02 modules and matching GSP firmware for the Blackwell
  display-DVM profile
- x86_64 BusyBox initramfs with standard virtio and PCI/USB/NVMe baseline

Build it from this directory:

```sh
make fetch
make build
make verify
make print-artifacts
# Choose a fresh path below an owner-only, non-symlinked parent.
make stage-release DEST=/run/rustos/releases/linux-dvm-2026.05-1
```

The host needs the usual Buildroot toolchain prerequisites and ELF development
headers. On Debian/Ubuntu, install `libelf-dev` before the first build. A
non-root CI host may instead set `RUSTOS_DVM_LIBELF_SYSROOT` to an immutable,
extracted `libelf-dev` package containing `usr/include/{libelf,gelf}.h` and an
unversioned `libelf.so`; the wrapper hash-binds those inputs before building.
It fails before downloading/building sources when neither source is valid.

Outputs remain untracked below `out/artifacts/`:

- `rustos-linux-dvm-x86_64.bzImage`
- `rustos-linux-dvm-x86_64.rootfs.cpio.xz`
- `rustos-linux-dvm-x86_64.config`
- `rustos-linux-dvm-x86_64.kernel.config`
- `rustos-linux-dvm-x86_64.module-signing.x509`
- `rustos-linux-dvm-x86_64.sources.lock`
- `rustos-linux-dvm-x86_64.control.env`
- `rustos-linux-dvm-x86_64.manifest`

The wrapper verifies pinned source hashes before extraction, caches downloads
in `out/dl`, and keeps the Buildroot output tree in `out/buildroot-output`.
Buildroot compiler caching is enabled with a base-directory-independent key,
so object reuse survives output-tree cleanup and workspace relocation when the
effective compiler inputs are unchanged. A second `make build` reuses both
caches and only rebuilds invalidated inputs.
Every installed module must carry a PKCS#7/SHA-256 signature that verifies
against the exported per-image X.509 certificate; the private key stays a
build-user-owned 0600 file and is never exported. Schema 8 binds both kernel
and Buildroot configurations, the certificate, source lock, NVIDIA release,
boot artifacts, and control contract as eight co-located files in one
self-contained bundle. `make verify` repeats these checks without rebuilding
the appliance. `make stage-release` refuses an existing destination, symlinked
path components, and group/world-writable ancestors, then copies and verifies
the bundle before atomically publishing it. Run it as the same account that
will own the trusted release tree (normally root in production).
Use `make clean` for build outputs or `make distclean` to remove only this
directory's generated `out/` tree.

An artifact may be reused only when its manifest and source lock hashes match.
That is an integrity check, not a release qualification: kernel/module ABI,
firmware bundle, protocol version, signing policy, and security support window
still need an explicit release manifest before hardware deployment. Rebuild and
re-sign on any kernel, module, firmware, protocol, or security-update change.

See [MODEL.md](MODEL.md) for the ownership and transport boundary.

## Korean

RustOS Linux driver domain용 Buildroot 기반 불변 appliance입니다.
`cargo xtask build-dvm`, `verify-dvm`가 이 wrapper를 사용하고 manifest hash를
검증합니다. 해시로 검증되는 fail-closed `agent-v1-control` contract가 들어
있습니다. 이 agent는 L0 host가 인증한 KVM vsock health, 장치/driver inventory,
versioned 물리 display evidence,
그리고 RustOS `inputd`로 전달되는 제한된 input stream만 허용합니다. 화면은 별도
read-only DMA triple-slot KMS transport를 사용하고, 가상 Ethernet 시험 경로는
독립된 고정 ring을 사용하며 물리 장치 권한을 주지 않습니다. 합성 input smoke는
시험 전용이고 물리 host keyboard capture나 임의 key forwarding이 아닙니다.
물리 display release는 target hardware에서 IOMMUFD/VFIO supervisor, connector,
DMA fault/revoke/reset, 지속 page-flip 증거가 통과할 때까지 차단됩니다.
활성화된 물리 release policy는 AMDGPU 전용이며 `1002:1900`, direct zero-copy,
연속 다섯 개의 신선한 nominal-60-Hz page-flip 표본과 bounded latency를 요구합니다.
현재 활성 boot GPU는 이 증거 수집에 안전하게 사용할 수 없습니다.

고정 입력은 `sources.lock`에 있습니다.

- Buildroot 2026.05
- Linux 6.12.94
- Blackwell display-DVM profile용 NVIDIA-open 580.173.02 module과 일치하는 GSP
  firmware
- x86_64 BusyBox initramfs와 표준 virtio, PCI/USB/NVMe 최소 기반

이 디렉터리에서 빌드합니다.

```sh
make fetch
make build
make verify
make print-artifacts
# owner 전용이며 symlink가 없는 상위 경로 아래의 새 경로만 사용합니다.
make stage-release DEST=/run/rustos/releases/linux-dvm-2026.05-1
```

호스트에는 일반 Buildroot toolchain 의존성과 ELF 개발 헤더가 필요합니다.
Debian/Ubuntu에서는 첫 빌드 전에 `libelf-dev`를 설치해야 합니다. root 권한이
없는 CI는 `usr/include/{libelf,gelf}.h`와 unversioned `libelf.so`가 들어 있는
불변 추출 `libelf-dev` package를 `RUSTOS_DVM_LIBELF_SYSROOT`으로 지정할 수
있고, wrapper는 그 입력 hash를 빌드 전에 결박합니다. 둘 다 없으면 source
다운로드·빌드 전에 바로 실패합니다.

생성물은 추적하지 않는 `out/artifacts/` 아래에 남습니다.

- `rustos-linux-dvm-x86_64.bzImage`
- `rustos-linux-dvm-x86_64.rootfs.cpio.xz`
- `rustos-linux-dvm-x86_64.config`
- `rustos-linux-dvm-x86_64.kernel.config`
- `rustos-linux-dvm-x86_64.module-signing.x509`
- `rustos-linux-dvm-x86_64.sources.lock`
- `rustos-linux-dvm-x86_64.control.env`
- `rustos-linux-dvm-x86_64.manifest`

wrapper는 source hash를 확인한 뒤 `out/dl`에 다운로드를 cache하고,
`out/buildroot-output`에 Buildroot 산출물을 유지합니다. Buildroot compiler
cache도 base-directory 독립 key로 활성화되어, 실질적인 compiler 입력이 같으면
output tree 정리나 workspace 이동 뒤에도 object를 재사용합니다. 따라서 동일한
입력으로 다시 `make build`하면 두 cache를 재사용하고 무효화된 항목만 다시
빌드합니다. `make clean`은 빌드 산출물만, `make distclean`은 이 디렉터리의 생성
`out/`만 지웁니다.

wrapper는 `.cpio.xz` 릴리스 ABI를 유지하면서 고정 4 MiB block의 병렬 `xz -1`을
사용합니다. 현재 454 MiB rootfs 실측은 기존 Buildroot 재현 빌드 기본값
`xz -9`의 약 79초/144 MiB에서 약 14초/182 MiB로 바뀝니다. 고정 block, 잠긴
host XZ, 정규화된 timestamp와 최종 manifest hash가 재현 증거를 유지합니다.
Buildroot를 직접 호출하면 이 계약을 우회하고 단일 스레드 병목을 되살리므로
반드시 이 디렉터리의 `make` target을 사용합니다.

설치되는 모든 module은 export된 image별 X.509 인증서로 검증되는
PKCS#7/SHA-256 서명을 가져야 합니다. private key는 build 사용자 소유의 0600
파일로만 남고 export되지 않습니다. Schema 8은 kernel/Buildroot 설정, 인증서,
source lock, NVIDIA release, boot artifact, control contract를 같은 디렉터리의
자기완결 8개 파일로 결속합니다. `make verify`는 appliance를 다시 빌드하지 않고
이 조건을 재검사합니다. `make stage-release`는 기존 목적지, symlink 경로 구성요소,
group/world-writable 상위 경로를 거부하고 복사 후 재검증한 뒤 원자적으로 공개합니다.
운영 환경에서는 신뢰 릴리스 트리의 소유자(보통 root)로 실행합니다.

바이너리는 manifest와 source lock hash가 일치할 때만 재사용할 수 있습니다.
이는 integrity check일 뿐 release qualification은 아닙니다. hardware 배포 전에는
kernel/module ABI, firmware bundle, protocol version, signing policy, 보안 지원
기간을 포함한 명시적 release manifest가 필요합니다. kernel·module·firmware·
protocol·보안 업데이트가 바뀌면 다시 빌드하고 서명해야 합니다.

소유권과 전송 경계는 [MODEL.md](MODEL.md)를 보세요.
