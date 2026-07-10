# RustOS Linux driver-domain appliance

[English](#english) | [한국어](#korean)

## English

This is a Buildroot-based, immutable Linux appliance for RustOS driver
domains. It is intentionally separate from `cargo xtask`: RustOS does not yet
provide the L0 hypervisor/backend required to consume this image, and the
existing xtask build surface remains untouched.

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
headers. On Debian/Ubuntu, install `libelf-dev` before the first build; the
wrapper fails before downloading/building packages if its `libelf.h` and
`gelf.h` headers are missing.

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

An artifact may be reused only when its manifest, source lock, kernel/module
ABI, firmware bundle, RustOS DVM protocol version, and security support window
all match the deployment. Rebuild and re-sign on any kernel, module, firmware,
protocol, or security-update change.

See [MODEL.md](MODEL.md) for the ownership and transport boundary.

## Korean

RustOS Linux driver domain용 Buildroot 기반 불변 appliance입니다. 현재
RustOS에는 이 이미지를 소비할 L0 hypervisor/backend가 없으므로 `cargo xtask`
빌드와 분리했습니다. 기존 xtask 표면은 건드리지 않습니다.

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
Debian/Ubuntu에서는 첫 빌드 전에 `libelf-dev`를 설치해야 합니다. wrapper는
`libelf.h`와 `gelf.h`가 없으면 package 다운로드·빌드 전에 바로 실패합니다.

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

바이너리는 manifest, source lock, kernel/module ABI, firmware bundle,
RustOS DVM protocol version, 보안 지원 기간이 모두 같은 경우에만 계속
재사용할 수 있습니다. kernel·module·firmware·protocol·보안 업데이트가
바뀌면 다시 빌드하고 서명해야 합니다.

소유권과 전송 경계는 [MODEL.md](MODEL.md)를 보세요.
