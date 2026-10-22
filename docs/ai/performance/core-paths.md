# 코어 외 경로 최적화 작업 계약 (진행 중)

측정 인터페이스: [벤치 묶음](../../benchmarks/core-path-bundle.md).
현재 수치·미완료 항목: [진행표](../../benchmarks/core-paths-progress.md).
이 작업은 전체 목표를 유지하며, 부분 수집/테스트 통과를 최적화 완료로 간주하지 않는다.

- 먼저 0단계 경로별 RustOS/Linux 게스트 측정과 coverage gap을 확정한다.
  제품 최적화는 아직 없으며 벤치 구현 및 검증 중이다. UI에는 private KVM
  selector로만 켜지는 input→validated DVM completion 계측을 추가 중이다.
- 이후 한 번에 한 최적화, 동일 workload/profile/topology의 동시간대
  토글·재빌드·ABAB만 효과 판단에 사용한다. 무효/무효과 변경은 유지하지 않는다.
- ring-3 정책, Linux/Windows 호환, CR0.WP, user 주소 범위,
  fault 발생 시 검증 경로 재실행은 약화하지 않는다.
- source/ELF/profile/topology에 묶인 원시 p50/p99/mean/max와 비용 분해를 보존한다.
  debugcon 출력은 VM exit 비용이므로 shipping/계측을 별도로 측정한다.
  host noise나 실패 샘플을 사후 삭제하여 p99 목표를 통과시키지 않는다.
- 매 영역 종료 시 명시된 native 단위 테스트, dev-plan now, PR formal,
  SMP iteration formal 및 8-vCPU GUI/input KVM smoke를 실행한다.
  소스 핀/변이가 영향을 받으면 새 실제 구조에 맞추고 해당 --only 변이도 확인한다.
- rootd/netd의 maintenance/deadline, devmgrd worker/handle-transfer 소유권을
  유지한다. shared runtime의 현재 reply_then_recv에는 timeout/handle 인자가
  없으므로 plain receive를 무조건적인 fused blocking receive로 대체하지 않는다.
- 새 ABI 또는 서비스 구조 결정이 필요한 경우만 질문한다. 커밋은 만들지 않으며
  새 파일은 최종 보고에서 사용자가 `git add`해야 함을 알린다.

일반 `cargo xtask bench`의 persistent-state rail은 before와
fork/exec/exit/signal/mmap/thread 이후 null 및 실제 IPC의 완전한 분포를 요구하고,
null p50 > preflight 전 pristine×1.5 또는 IPC p50 > before×1.5를 거부한다.
모든 state IPC split은 정확히 20,000 samples여야 한다. `core_gui`는 CPU
submission을 completion으로 취급하지 않고 16개 completed warmup 이후 128개
GPU receipt sample을 고정 배열에 모아 동결 후 출력한다. UI-consumption 및
observed DVM-completion 경계이며 physical scanout이나 Linux GUI 동급 증거가 아니다. 실패한 Linux 수집은
비교 기준선으로 재사용할 수 없다. 전체 p99 목표 판정 및 같은 하드웨어
Linux 비율 목표는 별도 의무이며 아직 검증되지 않았다.
