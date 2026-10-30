# 코어 외 경로: 정적 검토로 확정한 미구현·부실 목록

측정 인터페이스는 [벤치 묶음](../../benchmarks/core-path-bundle.md), 수치는
[진행표](../../benchmarks/core-paths-progress.md). 이 문서는 **2026-10-22 당시의 소스 정적 감사 기록**입니다.
아래 '미구현'·'부실' 판정은 최신 master의 보증된 현황표가 아닙니다.
현재 소스와 [Ring3 현황](../../architecture/ring3-process-model.md)을
먼저 비교하세요. 각 항목은 수정 후 벤치/회귀 witness로 닫고,
측정 없이 추정한 원인은 "가설"로 표시합니다.

## A. 미구현 (구조 설계 후 구현, 임시 우회 금지)

| # | 영역 | 정적 증거 | 설계 방향 |
|---|------|-----------|-----------|
| A1 | Linux syscall 커버리지 | `linux.rs` 디스패치에 `pipe/pipe2, eventfd2, timerfd, signalfd, inotify, readv, pwrite64, pwritev, rename*, link/symlink, chmod/chown, utimensat, fsync, truncate, fallocate, getrusage, gettimeofday/time/clock_getres, sysinfo, mremap, msync, flock, select/pselect6, prctl, sched_set*/getparam, getpriority, getgroups, sendmmsg, sendfile, fstatfs, times, getrlimit, waitid` 항목이 없어 기본 `ENOSYS` | ring0는 fd 종류(핸들)와 대기 기전만, 정책·버퍼는 ring-3 서비스. pipe/eventfd는 공유 메모리 ring + waitset 알림 |
| A2 | 쓰기 가능한 파일시스템 | `vfsd/src/lib.rs` `persistent_mutation_status`=EROFS, `mkdir_policy`는 `/run`만, `unlink_policy` EROFS. 주석: journal/recovery 프로토콜 부재 | 메모리 기반 쓰기 계층(tmpfs류)을 vfsd 소유로 먼저, 영속 쓰기는 journal 설계 후 |
| A3 | blocking 소켓 | `netd` `handle_accept`: `None if nonblocking => EAGAIN, None => EAGAIN` (두 분기 동일). `recv_connected_bytes`는 빈 큐에서 항상 `EAGAIN`; blocking 플래그를 읽는 경로 없음 | netd가 요청(reply cap)을 보관했다가 데이터/EOF 도착 시 응답하는 parked-request 방식, 또는 호출측 waitset 대기 후 재시도. 데드라인·취소 포함 |
| A4 | exec 실패 | **해결**: (1) resident lane에 주차된 대기 task가 stack pin을 쥐고 있어 이미지 writer가 거부 → `exec_transaction::evict_parked_target`(owner 전이와 같은 방식으로 cell 해제). (2) exec 뒤 새 이미지가 같은 주소를 다시 mmap하면, 이전 incarnation의 pager VMA 잔재가 범위 재작성에 끼어들어 `Stale`(EFAULT) → `pager_vma::rewrite_range`가 다른 incarnation의 영역을 무시하고 `publish`와 같이 회수. 회귀 witness `rewrite_reclaims_previous_exec_incarnation_instead_of_mutating_its_range`. 거부 단계는 `ExecRefusal`+`proc-exec-refused`, `pager-vma-unmap-refused`, `mm-broker-refused` 마일스톤으로 남김 | 완료 |

## B. 부실 (정확성/성능 위험)

| # | 위치 | 문제 | 상태 |
|---|------|------|------|
| B1 | `current.rs complete_retirement_side_effects` | 사용자 task 종료가 executor를 기다리며 `cond_resched()+spin_loop`로 대기. `cond_resched`는 이미 요청된 reschedule만 소비하므로 단일 CPU에서 한 quantum(~4 ms)을 태움 → thread join/exit p50이 정확히 1 quantum | **수정함**: `kick_housekeeping()+yield_now()`. thread_clone_exit_join p50 16.06M → 1.8M ticks (재현 3회) |
| B2 | `current/group_exit.rs:94`, `current/exec.rs` 두 곳 | 같은 spin-until-quantum 패턴 | **수정함**(yield). group_exit는 한 번 되돌렸다가 B6 수정 후 재적용 |
| B3 | `current.rs service_deferred_work` | housekeeping 한 턴에 task 은퇴/reap 각 1건만 처리 | **수정함**: 턴당 최대 16건 배치 |
| B4 | housekeeping 깨우기 | 1 tick `rtc::sleep(1)`에만 의존 | **수정함**: `kick_housekeeping`(원자 플래그+`wake_task`), `rtc::sleep_unless` |
| B5 | `address_space.rs` `clone_user_space_cow` | fork 반복 시 `InvalidFrameOwnership`(errno 14)이 간헐 발생(exec 수정 전 1회 관찰) | 수정 후 1·8 vCPU 반복 실행 7회에서 재현되지 않음. 원인은 미확정이므로 재발 시 `td-fork-err` 계열 계측부터 |
| B6 | fork 프로세스 슬롯 | `PROCESS_TABLE` 32개 중 서비스 17개 → fork 15회 후 `reserve_spawn` 실패(EAGAIN). 원인: housekeeping이 fair 대역이고 spawner가 yield만 해서 굶음 | **수정함**: spawner(`reserve_spawn_after_reclaim`)가 housekeeping을 kick하고 1 ms 단위로 sleep(최대 100 ms)해 CPU를 양보. housekeeping을 System 대역으로 올리는 안은 fork는 풀었으나 thread join p50이 1.5M→127M ticks로 악화돼 폐기 |
| B9 | `syscalld_ops.rs wait4` | busy-poll 및 상태 소비/신호 경합 | 블로킹 대기에 더해, 이미 소비한 상태가 EINTR에 버려지던 문제·물리 회수 전 종료 상태 중복 반환·WNOHANG의 잘못된 ECHILD 처리를 수정. 마지막 자식 소비 뒤 다른 부모 waiter도 깨움. pinned-child/경합/신호 우선순위 witness 추가 |
| B10 | fork+wait p50 | 약 9 ms를 tick 대기로 추정했음 | **원인 분리**: held fork transaction p50 0.411 ms, COW clone 0.400 ms(진단 137회). 반복 fork/정상 exit의 동기 debugcon frame을 opt-in lifecycle trace로 제한하자 1 vCPU fork+wait p50 35.02M→2.56M ticks. 원래 출력을 복원하면 35.49M으로 재상승(CPUID p50 모두 4480). 비정상 종료·exec 거부 계측 유지. 최종 반복/8 vCPU 검증 진행 중 |
| B7 | `reclaim.rs complete` | task 하나 reap마다 debug log | 기본 `logging.categories.sched="info"`에서 `rustos_log_sched_debug`가 생성되지 않아 formatting/output 자체가 컴파일 제외됨. B10의 무조건 milestone과는 다름 |
| B8 | 크기(과거 상태) | 당시 process table 32개 고정 | **2a에서 청크 디렉터리로 교체.** 현재 기본 한도 32와 내부 슬롯 상한 4096은 별도이며, 확장 부하 검증은 별도 과제 |

## C. SMP 배치

완료(idle 인지): `runqueue_policy::affinity_target_cpu`는 선호 CPU(마지막 CPU 또는
`slot % online`)가 idle이 아니면 큐와 mailbox가 비고 current 슬롯이 idle인 온라인 CPU를
`idle_eligible_cpu`/`first_idle_after`로 먼저 찾고, 없을 때만 큐 길이 2 이상 차이로
least-loaded를 고른다. 단위 witness:
`idle_cpu_search_wraps_skips_ineligible_and_never_returns_the_preferred_cpu`.
남은 것(후속, 정확성 위험 아님): SMT/LLC 토폴로지(CPUID 0xB/0x1F) 계층, wake_affine, CPU 용량.
설계 참고: Zircon CPU 선택 사다리, Linux `select_idle_sibling`/`wake_affine`.

## D. 정리 대상

완료(2026-10-22): 두 실제 구성(부팅 이미지 `--cfg rustos_boot_image`, `cargo test` 단위)에서
kernel-ps/compat/executive/hal/mm/nucleus-core/io-manager 경고 0. 호스트 비테스트 빌드는
실제 구성이 아니므로 기준에서 제외한다. 양 구성에서 죽은 항목은 삭제했고, 한쪽에서만
쓰이는 항목은 `#[cfg(test)]`/`#[cfg(not(test))]`로 구성을 명시했다. 구성별로 생성 지점이
다른 진단 코드표(`LockPhase`, `ImageRefusal`)만 `DIAGNOSTIC:` 근거로 `allow(dead_code)`를 둔다.
삭제로 필요 없어진 SMP 소스 핀, 변이 행(`run_authority` 구 비교 3행, 앵커 개수 조정)은 함께
정리했다. 테스트 전용으로 남은 local-dispatch 정책 함수(`choose_local_next` 등)는
독립 dispatch 소스 계약의 witness이며 운영 경로가 아니다.
- `formal/rust-large-files.tsv`의 숫자는 실측 행수가 아니라 허용 부채 상한이다.
  선행 변경에서 상한을 넘은 파일은 경고로 남으며, 검증을 조용하게 하려고 상한을 올리지 않는다.

## E. 현재 수집 상태 (2026-10-22 안정화 진행 중)

`c64a918f`에서 이미 core_lifecycle 1·8 vCPU 전 프로브 수집에 성공했다.
15회 뒤 EAGAIN/exec 미수집은 과거 실패이며 현재 상태가 아니다. 이번 후보의
1 vCPU 수집도 전 프로브 성공했으나, collection COMPLETE는 꼬리 목표 통과가 아니다.
같은 세션 ABAB 및 최종 8 vCPU 검증은 [진행표](../../benchmarks/core-paths-progress.md)에 기록한다.

## F. 안정화 검증 경계

- core_state 8 vCPU의 null rail 실패는 thread에만 국한되지 않는다. 이번 control과
  후보에서 fork/exit/exec 뒤에도 1.5x 초과가 관찰됐다. IPC 뒤 null 재측정은
  진단용으로만 추가했으며 원래 rail을 완화하거나 실패를 통과로 바꾸지 않는다.
- activation이 Cohort/frame-reader 해제 전에 원격 reschedule 요청을 공개하여,
  너무 이른 수신이 실패한 뒤 재시도가 타이머까지 늦어질 수 있음을 독립 검토로 확인했다.
  현재 후보는 외부 grant까지 해제한 뒤 모든 성공한 원격 wake(`notify:false` 포함)에
  새 요청을 보낸다. 같은 production engine의 single/batch witness를 추가했다.
  이 경합이 과거 5 ms 중앙값의 원인이었다고 주장하지 않는다(B10과 구분).
- 과거 8 vCPU fork_exit_wait 최대 17 s 이상치는 5회 재실행에서 재현되지 않았다.
  이번 wait4의 확정적 상태 소비 오류를 수정했지만 해당 이상치의 원인으로 확정하지 않는다.
- 이번 ABAB 마지막 후보 부팅 한 번은 uiserver wayland 초기화 중 pager VMA unmap
  Stale/EFAULT 뒤 errno5로 종료하여 수집 실패했다. 실패 archive를 보존하고 원인 조사 중이다.
- 죽은 코드의 경고 수는 삭제 가능 함수 수가 아니다. uncalled budget-refusal wrapper
  두 개와 전용 obsolete test를 삭제하고, 혼합 witness/소스 핀은 실제 사용하는
  exact-generation materializer로 옮겼다. 살아 있는 reply-wake/claim 경로의
  `sync_handoff`/`locality` 의존성을 경고 수만 보고 통째로 지우지 않는다.
- pipe/pipe2, eventfd, 쓰기 가능 FS, SEQPACKET, `/proc`, blocking 소켓은 사용자 지시로 제외.

## 참고한 외부 계약

- [Linux waitpid](https://man7.org/linux/man-pages/man2/waitpid.2.html): WNOHANG의 0과
  ECHILD 구분, 상태의 일회성 소비 및 EINTR 경계.
- [Linux wait queues](https://www.kernel.org/doc/html/latest/driver-api/basics.html):
  상태 공개 뒤 wake, prepare→조건 재확인→schedule의 lost-wakeup 방지 순서.
- [Linux printk](https://www.kernel.org/doc/html/latest/core-api/printk-basics.html):
  반복 콘솔 출력의 stall 위험, 비필수 진단의 제한/지연. RustOS 비용의 증거는 B10의
  같은 세션 측정이며 Linux 구현 자체를 RustOS 성능 증명으로 쓰지 않는다.
