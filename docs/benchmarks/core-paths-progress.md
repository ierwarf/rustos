# 코어 외 경로 최적화 — 진행 및 증거

목표는 코어 IPC 이외 **전체 경로**의 비용 분해, 불필요한 작업 제거,
p99/p50 ≤ 1.5, 동일 호스트 Linux 게스트 대비 목표 달성이다. 부분 벤치 추가나
네이티브 Linux 측정만으로 완료를 주장하지 않는다. ring-3 정책 및 Linux/Windows
호환 경계를 유지한다. 아래 초기 전체 프로그램의 목표/미착수 표는 역사적
스냅샷이다. 현재 요청은 기존·부분구현 경로 안정화이며, 새 pipe/eventfd/쓰기
FS/SEQPACKET/proc/blocking socket 구현은 제외한다. 현재 요청은 검증·문서 정리
후 에이전트가 커밋하는 것으로 이전의 사용자 커밋 조건을 대체한다.

## 2026-10-22 안정화 continuation (진행 중)

정적 목록은 [core-paths-gaps](../ai/performance/core-paths-gaps.md)가 소유한다.
wait4의 소비 후 EINTR 유실, pin이 남은 종료 상태 재소비, WNOHANG/ECHILD 및
경쟁 waiter 통지를 보완했다. remote activation의 reschedule 요청은 cohort,
frame readers/pins와 외부 authority 해제 후에만 발행한다. coalesced mailbox도
다시 통지한다. 진단 출력은 실제 수락 prefix만 반환하고 suffix만 bounded retry한다.
공유 writer와 직접 service writer도 같은 계약으로 수렴시켰다.

같은 호스트·1 vCPU·CPUID p50=4,480 ticks의 대조:

| 로그 정책 | fork+wait p50 | activation→first turn p50 | exit→reap p50 | archive suffix |
|---|---:|---:|---:|---|
| A1 정상 경로도 출력 | 35,020,040 | 19,858,000 | 14,999,480 | 1792653416508135497 |
| B1 trace opt-in | 2,560,680 | 2,248,920 | 320,680 | 1792654153036620740 |
| A2 정상 출력 복원 | 35,485,040 | 16,481,960 | 14,593,840 | 1792654454510144372 |

각 archive는 `build/bench-core/core_lifecycle-vcpu-1-<suffix>/`이다.
B2는 benchmark 전 uiserver 부팅 오류로 거부했으며 개선 반복 증거로 세지 않는다
(`1792654542580042857`). p99/tail 합격이나 Linux 대비 목표 달성을 주장하지 않는다.
정상 fork/exit 진단만 opt-in으로 옮겼고 비정상 종료/exec 거부 진단은 유지한다.

후속 sparse/mixed munmap 보완은 반복 hole 제거, 부분 초기화 cleanup, 전체
range ownership/할당 preflight, backing의 TLB barrier 수명 및 syscalld의 단일
broker transaction을 포함한다. native MM 80 / PS 977 / compat 184 / ipcbench 20은
해당 중간 tree에서 통과했다. 최종 now/PR/SMP/KVM 및 state rail은 아직 확인 중이다.
8CPU state의 과세 판정은 해제하지 않았고 checkpoint 뒤 진단 재측정만 추가했다.

## 초기 요구사항 / 당시 완료 증거 (역사)

| 영역 | 필요한 증거 | 현재 상태 |
|---|---|---|
| 0: 재현 가능한 벤치 | syscall, 프로세스/MM, 파일/파이프/소켓, 스케줄링, boot/exec/GUI, 1/8 vCPU, p50/p99/mean | portable core_* 및 실제 Linux 수집기, UI→DVM completion 1/8 첫 수집; boot/Linux GUI 및 런타임 실패 미해결 |
| Linux 기준 | 동일 호스트 게스트, 동일 워크로드/토폴로지, 비율표 | portable 6묶음 1/8 수집 성공; RustOS 실패·CPUID 드리프트로 유효 비율표 없음 |
| 호환/서비스 | 핫 syscall 홉/복사/락 분해, rootd/storaged/devmgrd/netd/uiserver 융합 | 미착수; procd/vfsd의 기존 융합은 새 검증 필요 |
| 프로세스/MM | fork/exec/mmap/munmap/mprotect/COW/fault 분해 및 재측정 | 미착수 |
| 꼬리 | shipping/계측 분리, debugcon·하우스키핑·IPI·타이머·락·호스트 노이즈 구분 | 미착수 |
| SMP | idle/cache/topology 배치, wake→run, 8 CPU 균형, 주기/steal | 미착수; 외부 설계 자료 확인 후 변경 |
| 보조/데이터 | yield/futex/epoll/signal/thread, 복사·공유·핸들·버퍼링 | 미착수 |
| boot/GUI/DVM | 서비스 부팅/exec/입력→프레임, Linux 동급 비교 | UI→observed DVM completion 첫 1/8 수집, 계측 수정 검증 중; boot/Linux GUI 미완료 |
| 영구 상태세 | fork/exec/exit/signal/mmap/thread 이후 null+IPC 상시 회귀 | 기본 벤치에 추가 중; 전체 실제 실행 증거는 아직 없음 |
| 검증 | 영역별 단위 테스트, dev-plan now, PR formal, SMP formal, 지정 8-vCPU GUI/input smoke | now95/native/SMP 실행; GUI 수정 후 재실행 및 PR/지정 smoke/전체 kernel tests 미완료 |
| 안전/핀 | CR0.WP/user 범위/fault 재검증, uaccess selftest, 깨진 앵커·변이 갱신 및 --only | 기존 상태에 대한 새 검증 필요 |
| 마무리 | dead code, docs/ai/**·formal/*.md·AGENTS 일치, large-file 목록 정리, ABAB 전후표 및 커밋 제안 | 미착수 |

## 변경 전 대조군

기준 소스: `f6f618c7`, 시작 시 worktree clean. 아래는 **원인이나 개선을
증명하지 않는** 신선한 기본 벤치 결과이다. 코어 외 전체 벤치를 먼저 갖춘 뒤
변경별 같은 시간대 ABAB를 별도로 수행한다. 원시 로그는
`/tmp/rustos-core-path-evidence/control-{1,8}.{log,txt}`에 보관한다.

| topology | probe | p50 ticks | p99 ticks | mean ticks | p99/p50 |
|---|---|---:|---:|---:|---:|
| 1 vCPU | null_syscall_getpid | 480 | 680 | 490 | 1.42 |
| 1 vCPU | vmexit_cpuid | 3,680 | 4,840 | 3,907 | 1.32 |
| 1 vCPU | sched_yield | 92,480 | 170,960 | 114,055 | 1.85 |
| 1 vCPU | fused reply_recv | 9,800 | 13,320 | 10,699 | 1.36 |
| 1 vCPU | getuid (syscalld) | 15,160 | 20,920 | 16,150 | 1.38 |
| 1 vCPU | fork_cow_private_write | 26,709,600 | 35,872,560 | 28,503,770 | 1.34 |
| 1 vCPU | mmap_split_survives_3_pages | 276,040 | 14,729,520 | 675,989 | 53.36 |
| 8 vCPU | null_syscall_getpid | 480 | 840 | 530 | 1.75 |
| 8 vCPU | vmexit_cpuid | 3,760 | 5,080 | 3,902 | 1.35 |
| 8 vCPU | sched_yield | 10,840 | 28,360 | 11,561 | 2.62 |
| 8 vCPU | fused reply_recv | 11,440 | 17,280 | 11,566 | 1.51 |
| 8 vCPU | getuid (syscalld) | 17,840 | 29,560 | 18,297 | 1.66 |
| 8 vCPU | fork_cow_private_write | 26,905,560 | 37,060,920 | 28,476,025 | 1.38 |
| 8 vCPU | mmap_split_survives_3_pages | 310,840 | 11,628,040 | 491,626 | 37.41 |

위 COW 행은 fork·검증·wait를 포함하므로 COW 첫 쓰기 단독 비용으로 인용하지 않는다.

## 0단계 구현 중 발견 (성능 개선/완료 주장 아님)

- 초기 Linux 1-vCPU syscall 묶음은 8개 연산 × 2,000 samples를 완료했다.
  RustOS에서는 `pipe2`가 ENOSYS로 실패하여 묶음 COMPLETE를 거부했다.
  구현 없는 syscall의 오류 시간을 빠른 pipe 성능으로 보고하지 않는다.
- 같은 초기 쌍의 CPUID p50은 Linux 3,200 / RustOS 3,680 ticks로 약 15%
  움직였다. 동일 하드웨어라도 **유효한 비율표나 개선 증거로 인정하지 않는다**.
  raw 진단: RustOS openat p50 237,600, stat 70,440, empty epoll 68,440,
  getuid 15,000 ticks. stat p99/p50=2.107로 꼬리 목표 미달이다.
- 처음 상태 rail에서 socketpair EOF gate를 사용했으나 child가 125로 종료했다.
  아직 errno를 분해하지 않았으므로 원인을 단정하지 않는다. 정상 상태세 비교에
  살아 있는 runnable child를 남기는 것도 부하 혼동이므로, rail은 fork+정상 reap,
  별도 nonzero exit+reap 후 비용을 비교하도록 정리했다. socket blocking/EOF는
  데이터 경로에서 독립적으로 검증할 미해결 항목이다.
- native `ipcbench` 19 tests, 전체 xtask 187 tests, Python initramfs 2 tests,
  dev-plan now(`git diff --check`, `cargo xtask check`)가 통과했다. 이후 상태 오류
  진단/순서 조정에 대한 재실행은 필요하며, 최종 영역 검증으로 확대 해석하지 않는다.
- 독립 read-only review에서 stale COW stamp, 실패 Linux archive 재수입,
  부분 state split 수용을 발견했다. 현재 iteration의 parent TSC window,
  runner/PID1/collection seal, 각 split 20,000 count 검사를 추가했고, 세 수정의
  재검토는 source-only OK였다. 테스트는 위 개수로 실제 재실행했다.
- source/ELF-bound `core_state` 1-vCPU 실행은 정상 fork/exit 이후 null p50
  600 ticks를 유지했지만, exec 단계에서 `loaderd: commit broker failed`로
  실패했다. 성공으로 처리하지 않았다. archive:
  `build/bench-core/core_state-vcpu-1-1792605162292922183/`.
  같은 실행 pristine null p50=840, CPUID p50=4,480으로 이전 세션의 host scale과
  다르다. 아직 exec errno/commit refusal 원인은 분해하지 않았다.
- GUI 기존 `input-to-present`는 `services/uiserver/src/main.rs`에서 느린
  rendered turn만 출력한다. 전체 p50/p99나 실제 DVM completion latency로
  인용할 수 없다. 0단계의 all-sample/fence-paired 측정은 여전히 미완료다.
- read-only service 조사: storaged와 uiserver policy worker는 단순 융합 후보.
  rootd/netd는 bounded maintenance receive, devmgrd는 worker 소유 reply 및
  handle-transfer reply가 있어 무조건적인 fused blocking receive로 바꾸면 안 된다.
  제품 최적화는 아직 없으며, 후속 UI 변경은 private benchmark 계측뿐이다.

## 첫 전체 matrix (고정 소스, 24개 실행)

목록: `/tmp/rustos-core-path-evidence/matrix.tsv`. 11개 수집 성공 / 13개 실패.
성능 목표 달성 수가 아니라 collection 결과이다.

- Linux syscall/MM/lifecycle/scheduler/data는 1·8 vCPU에서 수집 완료.
  state는 양쪽 모두 초기 pristine 대비 null p50 증가로 거부했다. 초기 snapshot과
  calibration sleep/host warmup 간 혼동을 줄이도록 Linux pristine을 실제 상태
  전이 직전으로 옮겼으며 재측정해야 한다. 실패 기준을 완화하지 않았다.
- RustOS 1-vCPU scheduler 수집 완료: yield p50/p99/mean
  176,440/306,960/198,394; wake→run 36,040/80,240/40,657;
  8-worker balance 35,280/576,960/85,361 ticks. CPU 0 관찰 수는 각각 2,000/2,048로
  정확히 합산되었다. 1-vCPU이므로 분산 균형 통과 증거가 아니며 꼬리 목표는 미달.
- RustOS lifecycle은 fork+wait의 15번째 완료 이후 fork EAGAIN을 보고했고,
  MM의 COW 첫 store도 수집하지 못했다. 후자는 구체적 실패 stage를 보강 중이다.
- 데이터 fixture는 `/tmp` 부재(ENOENT)에 걸렸다. published mkdir ABI로 setup에서
  디렉터리를 생성하도록 수정 중이다. socket worker 오류는 parent의 EPIPE 등만
  보였으므로 worker stage/errno 및 EOF와 stale errno 구분을 보강 중이다.
- 모든 RustOS 8-vCPU 실행은 **게스트 부팅 전** formal source-contract에서 차단됐다:
  새 shared-contract 모듈의 dead_code allow 주석에 안정된 rationale이 없었다.
  allow 자체를 없애고 실제 warmup 값을 archive metadata에 쓰도록 수정했다.
  8-vCPU 성능 데이터가 있다고 해석하지 않는다. 기존 실패 archive의
  `launch_ok=false`도 반드시 존중한다.

## 진단 재실행 (formal 차단 해소 후)

`/tmp/rustos-core-path-evidence/diagnostic.tsv`에 11개 실행을 기록했다. Linux
state 1·8 vCPU와 RustOS scheduler 8 vCPU는 수집 완료했다. 나머지 RustOS
8-vCPU 묶음은 이제 실제 게스트에서 실행되었으나 실패 항목 때문에 거부됐다.

- `ipcbench` 20 / xtask 187 / Python 2 native tests, dev-plan now,
  `bash formal/verify-smp-iteration.sh`가 통과했다(24 models, sealed).
- RustOS state 1 vCPU의 pristine/before/fork/exit/signal/mmap/thread null p50은
  모두 600 ticks였다. exec는 실제 loader commit broker 실패와 child status
  768(errno ESRCH=3 전달)로 거부됐다. 이후 exec 상태세 검증은 아직 없다.
- COW 첫 쓰기 누락은 이제 명확히 `stage=fork errno=11`이다. COW stamp/첫
  store 실패로 단정하지 않는다. 반복 lifecycle fork EAGAIN의 회수 경계를
  조사해야 한다(8 vCPU에서는 29회 완료 후 발생).
- `/tmp` mkdir은 EROFS로 거부됐다. VFS는 현재 O_CREAT/O_TRUNC 및 persistent
  write를 명시적으로 read-only 정책으로 거부한다. 파일 쓰기 벤치를 memfd나
  빠른 오류로 바꾸어 filesystem 성능을 주장하지 않는다.
- socket worker는 모든 크기에서 `stage=consumer-read errno=11`을 기록했다.
  blocking socket EAGAIN을 벤치의 무한 재시도로 숨기지 않는다.
- RustOS 8 vCPU scheduler p50/p99/mean ticks:
  yield 20,320/42,520/21,740; wake→run 178,760/669,360/249,338;
  balance 191,640/12,036,720/672,198. balance의 APIC 관찰 수는
  `[204,181,164,194,277,513,254,261]`, 합계 2,048이다. wake→run의 1,651/2,000은
  APIC 6에 관찰됐다. 관찰 시점은 TSC와 atomic하지 않으며 균형 목표 통과가 아니다.
  같은 실행 CPUID p50=10,360으로 host scale도 움직여 Linux 비율은 주장하지 않는다.

## GUI 첫 receipt 묶음 (검토 수정 전 진단값)

각각 16개 completed warmup 뒤 128개 input-bearing GPU frame을 수집했다.
전체 지연은 UI의 cursor-moving drain 시작→검증된 DVM completion 관찰 시각이다.
physical scanout/key echo/Linux 동급 또는 ABAB 개선 증거가 아니다.

| topology | probe | p50 µs | p99 µs | mean µs | p99/p50 |
|---|---|---:|---:|---:|---:|
| 1 vCPU | consume→observed completion | 4,055 | 38,172 | 6,418 | 9.414 |
| 8 vCPU | consume→observed completion | 13,214 | 23,859 | 11,127 | 1.806 |

archive: `build/bench-core/core_gui-vcpu-1-1792607892246862785/`,
`build/bench-core/core_gui-vcpu-8-1792607911305719612/`. raw logs 및 app/kernel/UI
ELF, DVM serial, source/profile/topology metadata를 보존한다. 두 실행 모두 raw
꼬리 목표에 실패했다. source inspection review는 cursor가 실제 compiled scene에
포함되고 두 GPU receipts 검증 뒤 completion이 기록됨을 확인했지만 다음 결함을
발견했다. 따라서 기존 분해값을 확정 비용표로 사용하지 않는다.

- idle 상태의 provider 복구가 이미 완료한 warmup을 보존할 수 있었다. pending
  ticket 유무와 무관하게 시작된 미동결 cohort 전체를 무효화하도록 수정하고,
  warmup 완료→idle reset→새 provider 입력 거부 witness를 추가했다.
- submit timestamp가 성공 ioctl 뒤의 slow-submit formatting/logging까지 포함했다.
  성공 ioctl 반환 직후 timestamp를 보존해 split 경계를 수정했다. 전체 지연과
  달리 기존 split에는 해당 진단 시간이 잘못 배분될 수 있었다.
- 수정 전 실제 검증: dev-plan now 95 commands, UI 76 / xtask 189 / ipcbench 20 /
  Python 2 tests, SMP formal seal. 두 수정은 focused input_completion 6 tests와
  독립 source-only 재검토(OK)를 통과했다. 전체 now/단위/GUI 재실행은 필요하다.

벤치 명령/계약은 [묶음 설명](core-path-bundle.md)에 있다. 외부 pipe ABI와
Linux/Zircon scheduler 자료는 `/tmp/rustos-core-path-evidence/references/`에
URL·수집시각·SHA-256과 함께 보관했다. 아직 적용 설계/ABAB 최적화는 없다.

## lifecycle 안정화 후 수집 (고정 소스, 2026-10-22)

`core_lifecycle` 1 vCPU의 모든 프로브가 수집된다(이전에는 fork 15회 뒤 EAGAIN, exec 실패).
원인과 수정은 [미구현·부실 목록](../ai/performance/core-paths-gaps.md)의 A4/B1~B10.
1 vCPU p50 ticks: fork_exit_wait 35.5M, fork_exec_exit_wait 141M, thread_clone_exit_join 1.58M,
exec_replace_single_thread 98.1M, spawn_activation_to_first_turn 17.7M, exit_retire_to_reap 14.9M,
fork_cow_private_write 34.7M. 수집 성공이며 지연 목표 달성이 아니다(fork+wait는 여전히 9 ms,
p99/p50 목표 미달).
8 vCPU p50 ticks: fork_exit_wait 35.2M, fork_exec_exit_wait 164M, thread_clone_exit_join 14.9M,
exec_replace_single_thread 162M, spawn_activation_to_first_turn 19.2M, exit_retire_to_reap 16.7M,
fork_cow_private_write 39.7M.
