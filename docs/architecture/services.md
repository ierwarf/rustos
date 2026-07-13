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
| Linux DVM | Linux driver lifecycle, DRM/KMS, evdev, and virtio-net. | Fixed RDI2/ivshmem transports. |
| `devmgrd` | RustOS device namespace and hotplug policy. | `IPC_SERVICE_DEVMGRD`. |
| `inputd` | Authenticated DVM input routing into Wayland and console clients. | Wayland seat + console focus. |
| `storaged` | Block device policy and partition mapping over AHCI/NVMe. | Block API. |
| `netd` | Network stack policy over DVM Ethernet transport. | netprobe socket. |
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

### Linux Driver Domain

Linux modules execute only in the isolated DVM. RustOS receives keyboard and
pointer records through authenticated RDI2/COM2, and display/network through
fixed validated ivshmem regions. `inputd`, `uiserver`, and `netd` own RustOS
policy above those transports. A missing DVM transport disables its device;
there is no direct hardware, firmware-display, or RustOS module fallback.

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
| Linux DVM | Linux driver lifecycle, DRM/KMS, evdev, virtio-net. | 고정 RDI2/ivshmem transport. |
| `devmgrd` | RustOS device namespace와 hotplug policy. | `IPC_SERVICE_DEVMGRD`. |
| `inputd` | 인증된 DVM input을 Wayland와 console client로 route. | Wayland seat + console focus. |
| `storaged` | AHCI/NVMe 위 block device policy와 partition mapping. | Block API. |
| `netd` | DVM Ethernet transport 위 network stack policy. | netprobe socket. |
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

### Linux Driver Domain

Linux module은 격리된 DVM에서만 실행합니다. RustOS는 keyboard/pointer를
인증된 RDI2/COM2으로 받고, display/network은 고정 크기와 버전이 검증된
ivshmem transport로 받습니다. 그 위의 정책은 `inputd`, `uiserver`, `netd`가
소유합니다. DVM transport가 없거나 검증에 실패하면 해당 장치는 사용할 수
없으며, direct hardware·firmware display·RustOS module fallback은 없습니다.
