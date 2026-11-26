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
| `syscalld` | Linux MM/credential and selected signal policy via gated brokers; routine clocks use the local compat timer path. | `IPC_SERVICE_SYSCALLD`. |
| `pagerd` | Backing and fault policy, boot prewarm and immutable library sources. Ring0 installs leaves; warmed private file mappings alias cached pages at mmap time, not through a general lazy file-fault path. | `IPC_SERVICE_PAGERD`. |
| `vfsd` | Linux VFS policy (mount table, openat resolution, FAT boot volume), plus leased, owner-bound executable snapshots streamed into a sealed memfd without a whole-file buffer. | `IPC_SERVICE_VFSD`. |
| `loaderd` | Process spawn: ELF dynamic main+interpreter, PE32+ main + System32 imports, Windows runtime broker registration. Large images are staged asynchronously (loader ABI v4 begin/poll/cancel work, explicit launch lease) before the serial image build. | `IPC_SERVICE_LOADERD`. |
| Linux DVM | Linux driver lifecycle, DRM/KMS, evdev, and virtio-net. | Fixed ivshmem transports; input uses RDI1 version 3. |
| `devmgrd` | RustOS device namespace and hotplug policy. | `IPC_SERVICE_DEVMGRD`. |
| `inputd` | Authenticated DVM input routing into Wayland and console clients. | Wayland seat + console focus. |
| `storaged` | Generation-bound block policy over the authenticated Linux-DVM transport. | DVM-backed block API. |
| `netd` | Network stack policy over DVM Ethernet transport. | netprobe socket. |
| `procd` | Process accounting, signal delivery, kernel ↔ runtime bridges for `ps`-style listings. | Pairs with `runtimed`. |
| `uiserver` | Display surface, compositor, Wayland server, console renderer. Profiling stays opt-in via `RUSTOS_UI_PROFILE`. | `/run/user/1000/wayland-0`, runtime client. |

### Bootstrap-launched vs Catalog-launched

Before full catalog discovery, `runtimed` runs **bootstrap_ui_server**. It
reads the signed Init environment registry and merges its sealed service-local
UI defaults; catalog admission later checks those defaults against generated
metadata. It uses the same suspended-create, lease-admit, activate, and
exact-endpoint transaction as initd.

Remaining catalog-managed services and apps are launched from the catalog;
rootd/initd-managed bootstrap services are not delayed until UI readiness. The catalog is
materialized on runtimed's main loop after UI readiness. Desktop metadata and
runtime-launch policy are cached separately so one registry can never be
mistaken for the other. In rootd's recovery path, waiting for loaderd or
another required service does not consume the dependent service's restart
budget; actual failed restart attempts still count.

### Linux Driver Domain

Linux modules execute only in the isolated DVM. RustOS receives keyboard and
pointer events as authenticated RDI1 version-3 records over an L0-owned MSI-X
input ring;
display, network, and block traffic use separately validated fixed
transports. `inputd`, `uiserver`, `netd`, and `storaged` own policy above
those transports. A missing DVM transport disables its device;
there is no direct hardware, firmware-display, or RustOS module fallback.

### Service allocation and traversal discipline

The non-display services retain bounded storage across repeated work and
move owned data when ownership already transfers. These paths preserve the
existing capability, generation, deadline and publication contracts:

| Service | Repeated work removed |
| --- | --- |
| `rootd` | Duplicate checkpoint SCAN traversal, post-journal COMPACT replanning, survivor-index reconstruction, and oldest-receipt vector shifting. |
| `initd` | Rebuilding package String sets on every idle turn; activation scratch is retained. |
| `runtimed` | Per-message D-Bus prefix shifting, per-byte TTY queue movement, and repeated watch/connection scratch allocations. |
| `sessiond` | Rebuilding membership sets when exact running desktop IDs have not changed. |
| `syscalld` | Cached MM lease index-node removal/reinsertion and fully covered VMA metadata removal/reinsertion. |
| `pagerd` | Repeated movement of the complete region-table tail for a multi-region edit. |
| `vfsd` | Ordinary path-component allocations, per-page sparse-read lookups, no-watch event metadata, whole-ledger close scans, and extra lock/coalescing buffers. |
| `loaderd` | Per-argument owned PE strings, temporary environment blocks, per-argument quoting strings, and duplicate FD-owner vectors. |
| `devmgrd` | Recreating the receive buffer for every exactly admitted request. |
| `inputd` | Intermediate native-event vectors and per-event dequeue work before native/evdev copyout. |
| `storaged` | Re-zeroing initialized recycled windows, repeated pump worklists, and common one/two-window pin allocations. |
| `netd` | Copying owned loopback packets, temporary netlink token sets/lists, and rebuilding the deferred-poll deque. |
| `procd` | Creating persistent policy entries for pure signal/alternate-stack queries. |

Checkpoint frames use the additive shared `Builder::append_to` path with exact
prefix rollback. D-Bus consumes coalesced messages by offset and compacts the
incomplete suffix once; strings require the specified NUL terminator and may
not contain embedded NUL. `bootstrap_ui_server` remains synchronous.

The 2026-11-26 batch was reviewed statically by request. Regression sources
were added, but no build, test, formal run, benchmark or live boot was performed;
these are structural cost reductions, not measured throughput claims. The
service-specific contracts remain in the [ABI index](../ai/contracts-abi.md),
[component model](ring3-component-model.md), and
[pager ownership](../ai/pager/ownership.md). Primary references include Rust's
[VecDeque slice contract](https://doc.rust-lang.org/alloc/collections/vec_deque/struct.VecDeque.html#method.as_slices),
[B-tree range iteration](https://doc.rust-lang.org/alloc/collections/btree_map/struct.BTreeMap.html#method.range),
[overlapping slice copies](https://doc.rust-lang.org/std/primitive.slice.html#method.copy_within),
the [D-Bus wire specification](https://dbus.freedesktop.org/doc/dbus-specification.html),
and the [Windows environment contract](https://learn.microsoft.com/en-us/windows/win32/api/processthreadsapi/nf-processthreadsapi-createprocessw).

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
| `syscalld` | Linux MM/credential 및 일부 signal 정책. 일반 clock 호출은 compat 내부 timer 경로. | `IPC_SERVICE_SYSCALLD`. |
| `pagerd` | backing·fault 정책, 부팅 시 사전 로드, 불변 라이브러리 페이지 공급. Ring0가 매핑을 설치하며, 사전 로드된 파일은 일반 지연 폴트가 아니라 mmap 시 캐시 페이지를 공유합니다. | `IPC_SERVICE_PAGERD`. |
| `vfsd` | Linux VFS policy (mount table, openat resolution, FAT boot volume)와 whole-file buffer 없이 sealed memfd로 스트리밍하는 lease·owner 결합 executable snapshot. | `IPC_SERVICE_VFSD`. |
| `loaderd` | process spawn: ELF dynamic main+interpreter, PE32+ main + System32 import, Windows runtime broker 등록. 큰 image는 직렬 image build 전에 비동기로 staging합니다(loader ABI v4 begin/poll/cancel work, 명시적 launch lease). | `IPC_SERVICE_LOADERD`. |
| Linux DVM | Linux driver lifecycle, DRM/KMS, evdev, virtio-net. | 고정 ivshmem 전송. 입력은 RDI1 버전 3. |
| `devmgrd` | RustOS device namespace와 hotplug policy. | `IPC_SERVICE_DEVMGRD`. |
| `inputd` | 인증된 DVM input을 Wayland와 console client로 route. | Wayland seat + console focus. |
| `storaged` | 인증된 Linux DVM transport 위 generation-bound block policy. | DVM 기반 block API. |
| `netd` | DVM Ethernet transport 위 network stack policy. | netprobe socket. |
| `procd` | process accounting, signal 전달, ps-style listing용 kernel ↔ runtime bridge. | `runtimed`와 쌍. |
| `uiserver` | display surface, compositor, Wayland server, console renderer. profiling은 `RUSTOS_UI_PROFILE`로 필요할 때만 켭니다. | `/run/user/1000/wayland-0`, runtime client. |

### 부트스트랩 launch vs catalog launch

`runtimed`는 전체 catalog 검색 전에 **bootstrap_ui_server**를 실행합니다.
서명된 Init environment registry에 봉인된 service-local UI 기본값을 합치고,
catalog admission 때 생성 metadata와 이 기본값이 일치하는지 확인합니다.
initd와 같은 suspended-create, lease-admit, activate, exact-endpoint
transaction을 사용합니다.

나머지 catalog-managed service와 app은 catalog에서 launch 됩니다.
rootd/initd가 관리하는 bootstrap service는 UI readiness를 기다리지 않습니다. UI ready 뒤 runtimed의
main loop가 catalog를 구성하며 desktop metadata와 runtime-launch policy는 별도
cache를 사용하므로 서로 다른 registry가 뒤섞이지 않습니다. rootd가 복구 중
loaderd 등 의존 서비스의 준비를 기다릴 때는 재시작 예산을 차감하지 않고,
실제 재시작 시도가 실패할 때만 차감합니다.

### Linux Driver Domain

Linux module은 격리된 DVM에서만 실행합니다. RustOS는 keyboard/pointer를
L0 소유 MSI-X 입력 링의 인증된 RDI1 버전 3 레코드로 받고, 화면·네트워크·블록은 각각
검증된 고정 transport로 받습니다. 그 위의 정책은 `inputd`, `uiserver`, `netd`,
`storaged`가 소유합니다. DVM transport가 없거나 검증에 실패하면 해당 장치는 사용할 수
없으며, direct hardware·firmware display·RustOS module fallback은 없습니다.

### 반복 할당과 순회 비용

`uiserver`를 제외한 13개 서비스는 반복 작업용 저장 공간을 재사용하고,
이미 소유권이 넘어가는 데이터는 복사 대신 이동합니다. 각 경로의 capability,
generation, deadline 및 공개 순서는 기존 계약을 유지합니다.

| Service | 줄인 반복 작업 |
| --- | --- |
| `rootd` | 체크포인트 SCAN 중복 순회, 저널 이후 COMPACT 계획 재생성, 인덱스 재구축, 오래된 환불 영수증 제거 시 전체 이동. |
| `initd` | 매 idle 루프의 package 문자열 집합 재생성 및 activation scratch 재할당. |
| `runtimed` | D-Bus 메시지별 앞부분 이동, TTY 바이트별 큐 처리, watch/connection scratch 재할당. |
| `sessiond` | running desktop ID가 그대로인 경우 membership 집합 재생성. |
| `syscalld` | 캐시된 MM lease의 인덱스 삭제·재삽입과 완전히 포함된 VMA 메타데이터의 삭제·재삽입. |
| `pagerd` | 여러 region을 수정할 때마다 벡터 뒷부분 전체 이동. |
| `vfsd` | 일반 경로 component 할당, sparse read의 페이지별 조회, watch 없는 event 메타데이터, close 시 전체 checkpoint 검색 및 lock 병합 임시 버퍼. |
| `loaderd` | PE 인자별 문자열 복사, 임시 환경 블록, 인자별 quoting 문자열, FD 소유 목록 복사. |
| `devmgrd` | 정확한 길이의 요청마다 receive 버퍼 재생성. |
| `inputd` | native event 중간 벡터와 native/evdev copyout 전 이벤트별 dequeue. |
| `storaged` | 초기화된 재활용 window의 중복 zeroing, pump 작업 목록 할당, 일반적인 1~2 window pin 할당. |
| `netd` | 소유 중인 loopback packet 복사, 임시 netlink token 집합·목록, deferred-poll deque 재생성. |
| `procd` | 순수 signal/alternate-stack 조회 시 영구 policy entry 생성. |

공유 체크포인트 인코딩은 오류 시 원래 prefix를 복원하는 `Builder::append_to`를
사용합니다. D-Bus는 offset으로 메시지를 처리한 뒤 미완성 suffix만 한 번 이동하고,
문자열의 NUL 종료와 내부 NUL 금지를 검증합니다. `bootstrap_ui_server`의 동기
부트스트랩은 유지합니다.

2026-11-26 변경은 요청에 따라 정적으로 검토했습니다. 회귀 소스는 추가했지만
빌드·테스트·formal·벤치마크·부팅을 실행하지 않았으며, 처리량 향상을 실측한
결과가 아닙니다. 상세 계약과 공식 참고 문서는 위 영문 절의 링크를 따릅니다.
