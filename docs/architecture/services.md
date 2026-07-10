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
| `uiserver` | Display surface, compositor, Wayland server, console renderer. Profiling stays opt-in via `RUSTOS_UI_PROFILE`. | `/run/user/1000/wayland-0`, runtime client. |

### Bootstrap-launched vs Catalog-launched

The first thing `runtimed` does is **bootstrap_ui_server**. That path reads the
uiserver desktop entry synchronously and honors manifest args/env on the very
first run. It then uses the same suspended-create, lease-admit, activate, and
exact-endpoint transaction as initd.

All other services and apps are launched from the catalog. The catalog is
materialized on runtimed's main loop after UI readiness. Desktop metadata and
runtime-launch policy are cached separately so one registry can never be
mistaken for the other.

### Driver Modules

Driver modules (`.ko`) are owned by `driverd`, but the actual link + init
happens inside the kernel module loader because they need ring0 access.
`driverd` reads `system/registry/kernel/loadable-drivers.tsv`, calls
`SYS_RUSTOS_DRIVER_LOAD_MODULE_BROKER` per record, and walks dependency
edges. The on-boot probe loads:

- Native RustOS xHCI for USB pointers and keyboards; raw HID reports are
  routed to `inputd` for policy/translation. Linux USB HID `.ko` artifacts stay
  staged only for compatibility work and are not the default boot input path.
- Linux `.ko` `virtio-gpu` when present (otherwise `bootfb` is the active
  display provider). Native virtio-gpu fallback must not be reintroduced.
- `virtio-net` for the default emulated NIC.

A skipped driver shows up as `driverd: skipped name=... reason=...` and is
expected when the alias doesn't match the live hardware or when a higher
priority provider in the same `provider_group` is already active. Active
provider groups skip later normal and fallback records before alias probing.

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
| `uiserver` | display surface, compositor, Wayland server, console renderer. profiling은 `RUSTOS_UI_PROFILE`로 필요할 때만 켭니다. | `/run/user/1000/wayland-0`, runtime client. |

### 부트스트랩 launch vs catalog launch

`runtimed`가 가장 먼저 하는 일은 **bootstrap_ui_server** 입니다. 이 경로는
uiserver desktop entry를 동기적으로 읽어 manifest args/env를 첫 실행부터
반영하고, initd와 같은 suspended-create, lease-admit, activate, exact-endpoint
transaction을 사용합니다.

다른 모든 service와 app은 catalog에서 launch 됩니다. UI ready 뒤 runtimed의
main loop가 catalog를 구성하며 desktop metadata와 runtime-launch policy는 별도
cache를 사용하므로 서로 다른 registry가 뒤섞이지 않습니다.

### Driver module

driver module (`.ko`)은 `driverd`가 소유 정책이지만, ring0 접근이 필요해
실제 link + init은 kernel module loader 안에서 일어납니다. `driverd`는
`system/registry/kernel/loadable-drivers.tsv`를 읽고 각 record에 대해
`SYS_RUSTOS_DRIVER_LOAD_MODULE_BROKER`를 호출하며 의존 edge를 따라
순서를 정합니다. active provider group은 같은 group의 이후 normal/fallback
record를 alias probe 전에 skip합니다. boot 중 probe되는 module 예:

- USB pointer/keyboard는 native RustOS xHCI가 잡고 raw HID report를
  `inputd`로 넘깁니다. Linux USB HID `.ko` artifact는 호환 작업용으로만
  stage되며 기본 부팅 입력 경로가 아닙니다.
- 가능하면 Linux `.ko` `virtio-gpu` (없으면 `bootfb`가 display provider).
  native virtio-gpu fallback은 다시 넣으면 안 됩니다.
- emulated NIC default인 `virtio-net`.

skip된 driver는 `driverd: skipped name=... reason=...`로 표시되며, alias가
실제 hardware와 매칭되지 않거나 같은 `provider_group` 안에 더 높은 우선
순위의 provider가 이미 활성일 때 정상적으로 발생합니다.
