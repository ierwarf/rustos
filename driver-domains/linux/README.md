# RustOS Linux driver-domain appliance

[English](#english) | [한국어](#korean)

## English

This is a Buildroot-based, immutable Linux appliance for RustOS driver
domains. `cargo xtask build-dvm` and `verify-dvm` use this exact wrapper and
verify its manifest hashes. The image carries a hashed, fail-closed
`agent-v1-control` contract. Its agent permits only an L0-host-authenticated
KVM-vsock health and PCI-inventory probe; it is still not a usable RustOS
driver domain because RustOS has no vsock endpoint or device-consumption path.

The pinned inputs are in `sources.lock`:

- Buildroot 2026.05
- Linux 6.12.94
- x86_64 BusyBox initramfs with standard virtio and PCI/USB/NVMe baseline

Build it from this directory:

```sh
make fetch
make build
make verify
make print-artifacts
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
- `rustos-linux-dvm-x86_64.manifest`

The wrapper verifies pinned source hashes before extraction, caches downloads
in `out/dl`, and keeps the Buildroot output tree in `out/buildroot-output`.
A second `make build` reuses that cache and only rebuilds invalidated inputs.
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
있습니다. 이 agent는 L0 host가 인증한 KVM vsock health/PCI inventory probe만
허용합니다. RustOS에는 vsock endpoint와 device-consumption 경로가 아직 없으므로
이 이미지는 아직 사용 가능한 driver domain은 아닙니다.

고정 입력은 `sources.lock`에 있습니다.

- Buildroot 2026.05
- Linux 6.12.94
- x86_64 BusyBox initramfs와 표준 virtio, PCI/USB/NVMe 최소 기반

이 디렉터리에서 빌드합니다.

```sh
make fetch
make build
make verify
make print-artifacts
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
- `rustos-linux-dvm-x86_64.manifest`

wrapper는 source hash를 확인한 뒤 `out/dl`에 다운로드를 cache하고,
`out/buildroot-output`에 Buildroot 산출물을 유지합니다. 따라서 동일한
입력으로 다시 `make build`하면 cache를 재사용하고 무효화된 항목만 다시
빌드합니다. `make clean`은 빌드 산출물만, `make distclean`은 이 디렉터리의
생성 `out/`만 지웁니다.

바이너리는 manifest와 source lock hash가 일치할 때만 재사용할 수 있습니다.
이는 integrity check일 뿐 release qualification은 아닙니다. hardware 배포 전에는
kernel/module ABI, firmware bundle, protocol version, signing policy, 보안 지원
기간을 포함한 명시적 release manifest가 필요합니다. kernel·module·firmware·
protocol·보안 업데이트가 바뀌면 다시 빌드하고 서명해야 합니다.

소유권과 전송 경계는 [MODEL.md](MODEL.md)를 보세요.
