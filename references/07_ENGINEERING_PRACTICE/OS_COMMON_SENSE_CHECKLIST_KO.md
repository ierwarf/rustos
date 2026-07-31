# OS 개발 상식·누락 방지 체크리스트

SPDX-License-Identifier: CC0-1.0

각 항목은 설계 문서, 코드 리뷰, 테스트 계획에서 “해당 없음”까지 명시적으로 판정한다.

## Architecture and invariants

- [ ] 각 subsystem의 authoritative state가 한 곳인지, cache/replica인지 표시한다.
- [ ] 객체 lifecycle을 Uninit, Private, Published, Quiescing, Dead 등으로 명시한다.
- [ ] 핸들에 type·rights·generation을 넣고 숫자 재사용만으로 stale access가 생기지 않게 한다.
- [ ] kernel/userspace/hypervisor/driver host 사이 신뢰 경계를 그림으로 유지한다.
- [ ] fast path가 policy를 우회하지 않는지 검토한다.
- [ ] global singleton을 만들기 전에 namespace·multi-instance·test isolation을 검토한다.
- [ ] 부팅 중 임시 권한이 정상 운영에서 회수되는지 검사한다.
- [ ] 복구를 위해 필요한 최소 persistent state와 재구성 가능한 state를 구분한다.

## Execution context

- [ ] 함수별 process/softirq/hardirq/NMI/early-boot context를 annotation한다.
- [ ] sleep, allocation, page fault, IPC 허용 여부를 API 계약에 쓴다.
- [ ] preemption disabled와 IRQ disabled를 다른 속성으로 취급한다.
- [ ] local IRQ disable이 다른 CPU와의 동시성을 막지 않음을 전제로 한다.
- [ ] panic path에서 lock, allocator, logging을 재진입하지 않는다.
- [ ] interrupt top half는 bounded work만 하고 나머지를 deferred path로 넘긴다.

## Memory ordering and SMP

- [ ] plain load/store, compiler atomic, CPU atomic, MMIO access를 구분한다.
- [ ] publish 전에 모든 필드를 초기화하고 release/acquire 또는 더 강한 protocol을 둔다.
- [ ] reference count와 object visibility/lifetime을 분리한다.
- [ ] RCU read-side가 끝난 뒤 free하는 grace period를 명시한다.
- [ ] per-CPU data의 remote access와 CPU hotplug를 함께 본다.
- [ ] TLB shootdown 완료와 page-table memory free 순서를 검증한다.
- [ ] IPI payload publish와 interrupt send 사이 barrier를 정의한다.
- [ ] spinlock 안에서 fault 가능한 user copy를 하지 않는다.
- [ ] lockless ring의 producer/consumer index wrap과 ABA를 테스트한다.
- [ ] cache-coherent CPU 메모리와 non-coherent DMA를 혼동하지 않는다.

## Locks and scheduling

- [ ] 전역 lock order graph를 생성하고 예외를 문서화한다.
- [ ] IRQ handler와 process context가 공유하는 lock의 irqsave 규칙을 통일한다.
- [ ] priority inheritance가 IPC chain과 lock chain을 모두 커버하는지 본다.
- [ ] wait queue 등록과 condition 재검사를 원자적으로 구성한다.
- [ ] wakeup은 상태 변경 뒤 수행하고 lost wakeup litmus test를 둔다.
- [ ] lock contention·hold time·owner를 trace할 수 있게 한다.
- [ ] blocking call을 holding lock 밖으로 이동할 때 object pinning을 보장한다.

## Virtual memory

- [ ] VMA와 page table entry의 authoritative relation을 정의한다.
- [ ] COW break와 concurrent unmap/fork/page fault를 모델링한다.
- [ ] page pinning이 migration, reclaim, truncate, device DMA에 미치는 영향을 추적한다.
- [ ] user pointer를 한 번 검증한 뒤 재사용할 때 TOCTOU를 고려한다.
- [ ] ASID/PCID 재사용 전에 shootdown·generation을 보장한다.
- [ ] huge page split/merge의 부분 실패와 accounting을 테스트한다.
- [ ] kernel mapping과 user mapping의 cache attribute alias를 금지하거나 관리한다.
- [ ] zeroing과 information disclosure 방지를 allocation/reuse 경로 전체에 적용한다.

## Processes and threads

- [ ] pid/tid 숫자와 task object lifetime을 분리한다.
- [ ] fork 중 어느 자원이 복제·공유·reset되는지 표로 만든다.
- [ ] exec가 signal, fd, credentials, robust futex, timers에 미치는 영향을 테스트한다.
- [ ] process exit와 last thread exit를 분리한다.
- [ ] parent reaping과 pid reuse 사이 stale wait를 막는다.
- [ ] cancellation이 kernel object를 반쯤 변경한 채 끝나지 않게 한다.

## IPC and capabilities

- [ ] message length·handle count·rights attenuation을 수신 전 검증한다.
- [ ] capability copy, mint, move, revoke, delete의 차이를 명확히 한다.
- [ ] endpoint close와 in-flight send/reply의 terminal state를 정의한다.
- [ ] call/reply token 재사용과 confused deputy를 막는다.
- [ ] shared memory와 notification channel의 generation을 묶는다.
- [ ] backpressure, quota, priority, cancellation을 protocol에 포함한다.
- [ ] service restart 뒤 old client session을 fail-closed로 처리한다.

## VFS and files

- [ ] fd table entry와 open file description의 수명을 구분한다.
- [ ] path lookup 중 rename/mount/symlink 경쟁을 다룬다.
- [ ] rename atomicity와 crash consistency를 filesystem별로 문서화한다.
- [ ] unlink 후 열린 파일의 동작을 보존한다.
- [ ] close와 async I/O completion 경쟁을 테스트한다.
- [ ] epoll/poll readiness와 event consumption 규칙을 파일 타입별로 정의한다.
- [ ] filesystem parser는 untrusted input으로 fuzz한다.
- [ ] writeback error를 fsync/close/next write 중 어디에 보고할지 정한다.

## Drivers, DMA and devices

- [ ] 장치 초기화는 각 단계별 rollback을 갖는다.
- [ ] DMA map 전 CPU write flush, completion 후 invalidate 요구를 아키텍처별로 정의한다.
- [ ] IOMMU 권한은 request lifetime보다 길게 남지 않게 한다.
- [ ] IRQ handler가 ring entry를 읽기 전 device-to-CPU ordering을 보장한다.
- [ ] hot-unplug에서 새 요청 차단, IRQ mask, DMA quiesce, waiters wake, object free 순서를 지킨다.
- [ ] reset이 다른 function/queue/VM에 미치는 영향을 확인한다.
- [ ] firmware가 신뢰 경계 안인지, signature와 rollback 보호가 있는지 본다.
- [ ] userspace driver crash 시 장치와 shared pages를 회수한다.

## Time and timers

- [ ] monotonic, boottime, realtime, raw clock을 구분한다.
- [ ] counter wrap, frequency change, suspend, migration을 테스트한다.
- [ ] deadline 계산에서 overflow와 unit conversion을 확인한다.
- [ ] cancel과 callback 실행의 경쟁에 명확한 반환 계약을 둔다.
- [ ] periodic timer drift와 missed tick 처리 정책을 정의한다.

## ABI and compatibility

- [ ] 공개 구조체 padding을 0으로 요구하거나 명시적 reserved field를 둔다.
- [ ] 32/64비트 pointer와 time32/time64 변환을 중앙화한다.
- [ ] errno를 host 내부 오류와 분리한다.
- [ ] 새 flag는 unknown bit 처리 규칙을 정한다.
- [ ] ioctl size·direction encoding을 검증하고 variable payload 최대값을 둔다.
- [ ] signal frame·ucontext·auxv를 golden binary로 테스트한다.
- [ ] unsupported는 silent success보다 명확한 오류를 반환한다.
- [ ] ABI regression corpus를 release gate로 둔다.

## Security

- [ ] 모든 외부 입력 parser에 크기·중첩·정수 overflow 제한을 둔다.
- [ ] 권한 검사는 lookup 전/후와 object replacement race를 고려한다.
- [ ] credential snapshot 시점과 use 시점을 일치시킨다.
- [ ] debug interface가 production security policy를 우회하지 못하게 한다.
- [ ] 로그에 secret, kernel pointer, cross-domain data를 노출하지 않는다.
- [ ] capability/handle delegation은 최소 rights만 전달한다.
- [ ] update key와 runtime service key를 분리한다.
- [ ] panic/crash dump 접근을 인증·암호화·감사한다.

## Error handling and recovery

- [ ] 각 allocation/registration 단계에 역순 unwind를 둔다.
- [ ] 에러가 실제 원인 코드를 보존하는지 검사한다.
- [ ] 부분 성공을 전부 실패로 롤백할지 caller에 노출할지 정한다.
- [ ] retry가 중복 side effect를 만들지 않게 request id/idempotency를 둔다.
- [ ] out-of-memory에서 logging·cleanup이 추가 OOM을 만들지 않게 한다.
- [ ] service crash recovery에서 stale shared state를 폐기한다.
- [ ] watchdog reset이 corruption을 숨기지 않도록 crash evidence를 먼저 보존한다.

## Observability

- [ ] event에 timestamp, CPU, task, object id, generation, correlation id를 포함한다.
- [ ] 동일 object의 create/publish/revoke/free를 추적할 수 있게 한다.
- [ ] 로그 rate limit이 최초 원인까지 버리지 않게 한다.
- [ ] lock contention, IRQ latency, scheduler delay, IPC queue depth를 측정한다.
- [ ] flight recorder는 crash 전 마지막 상태 전이를 남긴다.
- [ ] 사용자 데이터와 secret은 redaction한다.
- [ ] release build에서도 저비용 invariant counter를 남긴다.

## Testing

- [ ] 정상 경로와 동일한 수만큼 오류 경로 테스트를 계획한다.
- [ ] allocation 실패를 N번째 allocation마다 주입한다.
- [ ] IRQ, timeout, cancel, close, exit 순서를 무작위화한다.
- [ ] SMP core 수 1/2/3/4/비대칭 조합을 테스트한다.
- [ ] QEMU뿐 아니라 최소 한 종류의 실제 하드웨어에서 cache/DMA를 확인한다.
- [ ] golden image·filesystem·ELF·packet corpus를 버전 관리한다.
- [ ] bug fix마다 재현 테스트를 먼저 추가한다.
- [ ] fuzzer crash는 최소화하고 deterministic seed로 보존한다.

## Performance

- [ ] 평균뿐 아니라 p95/p99/max latency를 본다.
- [ ] 성능 변경 전 bottleneck을 trace로 증명한다.
- [ ] IPC batching이 latency, cancellation, priority에 미치는 비용을 측정한다.
- [ ] cache line bouncing과 false sharing을 per-CPU counter로 확인한다.
- [ ] zero-copy가 pinning·lifetime·security 비용을 늘리는지 계산한다.
- [ ] fast path와 slow path 결과가 의미상 같은지 differential test한다.

## Collaboration and review

- [ ] 한 패치에 한 논리적 변경을 둔다.
- [ ] 중간 커밋마다 빌드와 기본 테스트가 통과한다.
- [ ] API 변경과 첫 사용자 코드를 가능하면 분리한다.
- [ ] 코드 이동과 코드 수정은 같은 패치에 섞지 않는다.
- [ ] 커밋 메시지에 문제, 영향, 원인, 해결, 테스트를 쓴다.
- [ ] 리뷰 지적을 다음 버전 changelog에 반영한다.
- [ ] 대규모 설계는 RFC와 작은 prototype으로 먼저 검토한다.
- [ ] owner 없는 subsystem을 만들지 않는다.
- [ ] AI 생성 코드는 작성자 책임으로 검증하고 provenance를 남긴다.

## Release and operations

- [ ] reproducible build와 SBOM을 생성한다.
- [ ] 업데이트는 전원 손실 중에도 이전 슬롯으로 돌아갈 수 있게 한다.
- [ ] kernel/userspace protocol version mismatch를 감지한다.
- [ ] rollback 방지와 emergency rollback의 충돌을 정책으로 해결한다.
- [ ] crash loop에서 safe mode·recovery console을 제공한다.
- [ ] 보안 패치의 embargo, disclosure, backport 절차를 문서화한다.
- [ ] 지원 hardware/ABI/feature matrix를 release artifact로 고정한다.
