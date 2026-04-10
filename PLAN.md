# Nucleus Commercial Layering Completion Plan

## Summary
완료 목표는 `nucleus`를 실제로 thin kernel binary로 만들고, 남은 구현 소유권을 내부 manager crate로 전부 넘겨서 협업 단위를 crate 경계로 고정하는 것이다. 최종 상태에서 `nucleus`는 bin-only이고, manager crate는 더 이상 `pub use nucleus::*`를 하지 않는다. CI는 layering + god-module 재발 방지 규칙을 강제하고, 각 manager의 공식 API만 cross-crate에서 사용한다.

핵심 전제는 다음으로 고정한다.
- 마이그레이션은 manager 단위 단계 진행으로 한다.
- facade/API는 지금 고정하고 구현만 그 뒤로 이동한다.
- CI는 layering + size gates를 즉시 강제하고, warning 전체 실패는 이번 단계에서 하지 않는다.
- 실구현 이동을 위해 `nucleus` 중심 구조를 버리고 별도 기반 라이브러리 crate를 추가한다.

## Architecture Decisions
- 새 기반 crate `kernel/nucleus-core`를 추가한다.
  - 역할: `debug`, `util`, 공통 설정, 최소 공용 primitive, 테스트 공통 헬퍼.
  - 금지: scheduler, VFS, syscall, host bootstrap 같은 manager 정책 로직.
- `nucleus` package는 최종적으로 bin-only로 만든다.
  - 남기는 것: `_start`, bootstrap stack, `panic_handler`, `alloc_error_handler`, 초기 boot handoff.
  - 제거하는 것: 현재 `kernel/src/lib.rs` 기반의 구현 소유권.
- manager crate 의존 방향을 실제 package graph로 고정한다.
  - `kernel-hal` -> `nucleus-core`
  - `kernel-mm` -> `nucleus-core`, `kernel-hal`
  - `kernel-object` -> `nucleus-core`
  - `kernel-ipc-runtime` -> `nucleus-core`, `kernel-mm`, `kernel-object`
  - `kernel-ps` -> `nucleus-core`, `kernel-mm`, `kernel-object`, `kernel-ipc-runtime`
  - `kernel-io-manager` -> `nucleus-core`, `kernel-mm`, `kernel-object`, `kernel-ipc-runtime`, `kernel-ps`
  - `kernel-compat` -> `nucleus-core`, `kernel-mm`, `kernel-object`, `kernel-ipc-runtime`, `kernel-ps`, `kernel-io-manager`
  - `kernel-executive` -> `nucleus-core`, all lower managers
  - `nucleus` bin -> `nucleus-core`, `kernel-hal`, `kernel-mm`, `kernel-executive`
- 공식 cross-crate API는 아래로 고정한다.
  - `kernel_hal::api`
  - `kernel_mm::api`
  - `kernel_object::api`
  - `kernel_ipc_runtime::api`
  - `kernel_ps::api`
  - `kernel_io_manager::api`
  - `kernel_compat::{linux, windows}`
  - `kernel_executive::boot`

## Implementation Changes
1. Foundation split
- `kernel/nucleus-core`를 만들고 현재 `nucleus` lib가 들고 있는 공통 기반을 이동한다.
- 기존 manager crate들의 `nucleus = { path = ".." }` 의존을 모두 제거하고 `nucleus-core` 및 하위 manager 의존으로 바꾼다.
- `kernel/src/main.rs`는 `nucleus::...`가 아니라 manager crate와 `nucleus-core`를 직접 import하도록 바꾼다.
- 전환 중 1단계까지만 `kernel/src/lib.rs`를 compatibility umbrella로 유지하고, manager crate 전환이 끝나면 삭제한다.

2. Facade freeze
- 각 manager crate에 `api` 모듈을 실제 소유 API로 정의하고, 현재 `kernel/src/*_api.rs`는 1단계 호환 shim으로만 남긴다.
- `main.rs`와 `executive` 외부 호출부는 shim이 아니라 manager crate API를 직접 사용하도록 바꾼다.
- cross-crate에서 concrete internal struct import를 금지한다. 외부 노출 타입은 DTO, handle token, snapshot, error enum만 허용한다.

3. Manager ownership migration
- `kernel-hal`
  - `arch/*`와 부트 직후 CPU/interrupt/GDT/IDT/RTC/SIMD/asm helper를 이동한다.
- `kernel-mm`
  - `memory/*` 전체를 이동하고 주소공간, 물리 메모리, 힙, user mapping API를 소유한다.
- `kernel-ipc-runtime`
  - `ipc/mod.rs` 실구현과 IPC handle/type을 이동한다.
  - channel/port/region/event 테스트도 함께 이동한다.
- `kernel-ps`
  - `multitask/*`와 process/thread/wait/reap/current snapshot을 이동한다.
  - `UserProcessState`는 ABI-neutral core만 `ps`가 소유하게 정리한다.
- `kernel-io-manager`
  - `vfs/*`, `storage/*`, `io/device/*`, `io/session`, `input/*`, `usb/*`, `driver/*`의 IO/device 관련 구현을 이동한다.
  - `kernel_host/{vfs,disk_block,display,input,driver,pci,usb,serio}`의 서비스별 RPC client 책임도 여기로 옮긴다.
- `kernel-compat`
  - `user/syscall/*`, `user/sysops/*`, `user/process/*`, `user/linux.rs`, `user/socket.rs`, `user/epoll.rs`, `user/memfd.rs`를 이동한다.
  - Linux/Windows ABI decode, syscall dispatch, process loader는 여기서만 소유한다.
- `kernel-executive`
  - 현재 `executive.rs`와 `kernel_host`의 orchestration, staged host loading, init bootstrap, housekeeping을 소유한다.
  - 서비스별 세부 RPC는 직접 구현하지 않고 `io-manager`/`ipc-runtime` facade 호출만 한다.

4. Object/handle finalization
- `kernel-object`는 공통 handle table과 rights/lifetime만 소유한다.
- 최종적으로 heterogeneous `KernelHandle` enum은 제거한다.
- 대체 구조는 다음으로 고정한다.
  - `HandleToken { owner: HandleOwner, object_id: u64 }`
  - `HandleOwner = Ipc | Io | Compat | Ps`
  - `HandleTable`은 token + fd/status flags만 저장
  - 실제 payload 저장소는 owning manager 내부 per-process state가 관리
- `ps`는 process-local state를 `HandleTable + manager-owned process state` 조합으로 재구성한다.
- `io-manager`는 file/device/display 관련 handle payload를, `compat`는 socket/epoll/memfd payload를 소유한다.

5. Cleanup and enforcement
- `system.rs`는 삭제한다.
- `kernel_host/mod.rs`, `storage/block.rs`, `multitask/mod.rs`, `user/syscall/linux.rs` 같은 잔존 god-module은 소유 crate 안에서 하위 모듈로 추가 분해한다.
- `xtask layering`는 다음을 검사한다.
  - package dependency 방향
  - forbidden source import
  - `nucleus` bin이 manager internal module이 아니라 facade만 사용하는지
  - size gates: thin entry, thin shims, god-module 상한
  - manager crate가 `pub use nucleus::*`를 하면 실패

## Test Plan
- 단계별 공통 게이트
  - `cargo test --workspace`
  - `cargo test -p nucleus --bin nucleus -- --test-threads=1`
  - `cargo xtask build`
- manager 단위 게이트
  - `kernel-ipc-runtime`: 메시지 순서, handle attach, shared region/event semantics
  - `kernel-ps`: current snapshot, spawn/wait, reclaim, deferred work
  - `kernel-io-manager`: boot volume/block descriptor, mount resolution, device/display/input paths
  - `kernel-compat`: Linux/Windows syscall dispatch, loader/runtime blob, socket/memfd/epoll behavior
  - `kernel-executive`: staged host load, host barrier, init bootstrap, housekeeping
- 완료 정의
  - `nucleus` package는 bin-only
  - manager crate가 더 이상 `nucleus` package에 의존하지 않음
  - `KernelHandle` 제거 완료
  - `cargo xtask build`와 전체 테스트가 green

## Assumptions And Defaults
- 새 기반 crate `nucleus-core` 추가는 허용한다. 이 crate는 manager가 아니라 packaging prerequisite로 취급한다.
- warning 전체 실패는 이번 단계에 넣지 않는다. 다만 새 public facade는 `private_interfaces` 경고가 없도록 정리한다.
- 중간 단계에서도 항상 workspace/test/build green을 유지한다.
- 외부 유저 ABI는 유지하고, 내부 wire/API는 manager facade 뒤에서 재정리한다.
