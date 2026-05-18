# Userspace Services

[English](#english) | [한국어](#korean)

<a id="english"></a>

## English

RustOS keeps the kernel small by pushing policy into userspace services.
Every service ships with a `RUSTOS.package.toml` manifest, an ELF in
`build/image/services/<id>/...`, and a row in the staged registries. The
table below names the live services, what they own, and the kernel
surfaces or sockets they expose.

| Service | Owns | Key surface |
| --- | --- | --- |
| `rootd` | Userspace privilege root. Brings up foundational service brokers and hands off to `initd`. | Direct spawn from kernel finalize. |
| `initd` | Linux-style init. Starts `runtimed`, sets per-session env, and reaps strays. | `/proc/1`. |
| `runtimed` | Service manager + launch broker. Bootstraps `uiserver` (synchronously with manifest env), then dispatches autostart and on-demand launches. | `/run/runtimed.sock` (`RuntimeClient`). |
| `sessiond` | Session policy: focus, console session lifecycle, per-user resources. | Pairs with `runtimed`. |
| `syscalld` | Linux MM/clock/signal policy. Calls into the gated `SYS_RUSTOS_MM_BROKER`. | `IPC_SERVICE_SYSCALLD`. |
| `vfsd` | Linux VFS policy (mount table, openat resolution, FAT boot volume). | `IPC_SERVICE_VFSD`. |
| `loaderd` | Process spawn: ELF dynamic main+interpreter, PE32+ main + System32 imports, Windows runtime broker registration. | `IPC_SERVICE_LOADERD`. |
| `driverd` | `.ko` autoload, alias probe, provider-group arbitration. Single owner of `SYS_RUSTOS_DRIVER_*` brokers. | Driver registry tsv + brokers. |
| `devmgrd` | Device manager / hotplug. Talks to driverd and consumers (`inputd`, `storaged`). | `IPC_SERVICE_DEVMGRD`. |
| `inputd` | Input event routing from kernel HID/serio into Wayland and console clients. | Wayland seat + console focus. |
| `storaged` | Block device policy and partition mapping over AHCI/NVMe. | Block API + driverd. |
| `netd` | Network stack policy (virtio-net today, more to follow). | netprobe socket. |
| `procd` | Process accounting, signal delivery, kernel ↔ runtime bridges for `ps`-style listings. | Pairs with `runtimed`. |
| `uiserver` | Display surface, compositor, Wayland server, console renderer. Bootstrap-launched with `RUSTOS_UI_PROFILE` applied from the manifest. | `/run/user/1000/wayland-0`, runtime client. |

### Bootstrap-launched vs Catalog-launched

The first thing `runtimed` does is **bootstrap_ui_server**: it spawns
`uiserver` immediately, before the desktop/runtime-launch registries are
finished loading. That bootstrap path now reads the uiserver desktop entry
synchronously so manifest env (e.g. `RUSTOS_UI_PROFILE=1`) is honored on the
very first run, not just after the catalog loader finishes on a worker
thread.

All other services and apps are launched from the catalog. The catalog is
loaded asynchronously to keep boot wall-clock short; the OnceLock cache in
`runtime-control` makes the bootstrap read and the worker read share the
same parse.

### Driver Modules

Driver modules (`.ko`) are owned by `driverd`, but the actual link + init
happens inside the kernel module loader because they need ring0 access.
`driverd` reads `system/registry/kernel/loadable-drivers.tsv`, calls
`SYS_RUSTOS_DRIVER_LOAD_MODULE_BROKER` per record, and walks dependency
edges. The on-boot probe loads:

- `hid` (HID core) and `hid-generic` (generic HID input driver) for USB
  pointers and keyboards.
- `virtio-gpu` when present (otherwise `bootfb` is the active display
  provider).
- `virtio-net` for the default emulated NIC.

A skipped driver shows up as `driverd: skipped name=... reason=...` and is
expected when the alias doesn't match the live hardware or when a higher
priority provider in the same `provider_group` is already active.

<a id="korean"></a>

## 한국어

RustOS는 정책을 userspace service로 옮겨 커널을 작게 유지합니다. 모든
service는 `RUSTOS.package.toml` manifest, `build/image/services/<id>/...`
ELF, 그리고 staged registry에 한 줄을 함께 가집니다. 아래 표는 현재 운영
중인 service와 그 소유 정책, 그리고 노출되는 kernel surface / socket을
요약합니다.

| Service | 소유 정책 | 주요 surface |
| --- | --- | --- |
| `rootd` | userspace privilege root. foundational broker를 띄우고 `initd`에 인계. | kernel finalize에서 직접 spawn. |
| `initd` | Linux 스타일 init. `runtimed`를 띄우고 session 환경을 set, 좀비 회수. | `/proc/1`. |
| `runtimed` | service manager + launch broker. manifest env를 적용한 채 `uiserver`를 동기 부트스트랩한 뒤, autostart와 on-demand launch를 dispatch. | `/run/runtimed.sock` (`RuntimeClient`). |
| `sessiond` | session policy: focus, console session lifecycle, per-user resource. | `runtimed`와 쌍. |
| `syscalld` | Linux MM/clock/signal policy. `SYS_RUSTOS_MM_BROKER` 호출. | `IPC_SERVICE_SYSCALLD`. |
| `vfsd` | Linux VFS policy (mount table, openat resolution, FAT boot volume). | `IPC_SERVICE_VFSD`. |
| `loaderd` | process spawn: ELF dynamic main+interpreter, PE32+ main + System32 import, Windows runtime broker 등록. | `IPC_SERVICE_LOADERD`. |
| `driverd` | `.ko` autoload, alias probe, provider-group 중재. `SYS_RUSTOS_DRIVER_*` broker 단일 소유자. | driver registry tsv + broker. |
| `devmgrd` | device manager / hotplug. driverd 및 consumer(`inputd`, `storaged`)와 통신. | `IPC_SERVICE_DEVMGRD`. |
| `inputd` | kernel HID/serio에서 들어온 input event를 Wayland와 console client로 route. | Wayland seat + console focus. |
| `storaged` | AHCI/NVMe 위 block device policy와 partition mapping. | Block API + driverd. |
| `netd` | network stack policy (현재 virtio-net). | netprobe socket. |
| `procd` | process accounting, signal 전달, ps-style listing용 kernel ↔ runtime bridge. | `runtimed`와 쌍. |
| `uiserver` | display surface, compositor, Wayland server, console renderer. manifest의 `RUSTOS_UI_PROFILE` 가 적용된 채 부트스트랩 launch. | `/run/user/1000/wayland-0`, runtime client. |

### 부트스트랩 launch vs catalog launch

`runtimed`가 가장 먼저 하는 일은 **bootstrap_ui_server** 입니다. desktop /
runtime-launch registry 로딩이 끝나기 전에 `uiserver`를 바로 spawn 합니다.
이제 이 bootstrap 경로는 uiserver desktop entry를 동기적으로 읽어서
manifest env (예: `RUSTOS_UI_PROFILE=1`)를 첫 실행부터 반영합니다. catalog
loader worker 완료를 기다리던 이전 동작에서 회귀입니다.

다른 모든 service와 app은 catalog에서 launch 됩니다. catalog는 boot
wall-clock을 줄이려고 비동기로 로드되며, `runtime-control`의 OnceLock
cache가 bootstrap 경로와 worker 경로의 parsing을 공유하게 만듭니다.

### Driver module

driver module (`.ko`)은 `driverd`가 소유 정책이지만, ring0 접근이 필요해
실제 link + init은 kernel module loader 안에서 일어납니다. `driverd`는
`system/registry/kernel/loadable-drivers.tsv`를 읽고 각 record에 대해
`SYS_RUSTOS_DRIVER_LOAD_MODULE_BROKER`를 호출하며 의존 edge를 따라
순서를 정합니다. boot 중 probe되는 module 예:

- USB pointer/keyboard용 `hid` (HID core)와 `hid-generic` (generic HID
  input driver).
- 가능하면 `virtio-gpu` (없으면 `bootfb`가 display provider).
- emulated NIC default인 `virtio-net`.

skip된 driver는 `driverd: skipped name=... reason=...`로 표시되며, alias가
실제 hardware와 매칭되지 않거나 같은 `provider_group` 안에 더 높은 우선
순위의 provider가 이미 활성일 때 정상적으로 발생합니다.
