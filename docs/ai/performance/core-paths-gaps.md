# 코어 외 경로: 정적 검토로 확정한 미구현·부실 목록

측정 인터페이스는 [벤치 묶음](../../benchmarks/core-path-bundle.md), 수치는
[진행표](../../benchmarks/core-paths-progress.md). 이 문서는 **소스만 읽고 확정한**
사실이며, 각 항목은 수정 후 해당 벤치/회귀 witness로 닫는다. 측정 없이 추정한
원인은 "가설"로 표시한다.

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
| B9 | `syscalld_ops.rs wait4` | 자식이 아직 살아 있으면 `yield_now` busy-poll | **수정함**: 블로킹 대기(`child_wait.rs`, arm→register→recheck→commit). 효과(지연)는 아직 분리 측정 못 함 |
| B10 | fork+wait p50 | 1 vCPU 12~19 ms, 8 vCPU 18 ms. 최소는 1.2 ms. idle이 30%인데도 대기 → 어딘가 tick 단위 대기가 남음 | 미해결, 원인 미확정 |
| B7 | `reclaim.rs complete` | task 하나 reap마다 `debug::debug!("reaped exited task ...")` | shipping에서 비활성인지 확인 필요 |
| B8 | 크기 | process table 32개 고정 | 상용 수준에는 부족. 증설 시 reap queue/ledger 크기 동반 검토 |

## C. SMP 배치

`runqueue_policy::affinity_target_cpu`: 마지막 CPU 또는 `slot % online` 잔여로 선호 CPU를
정하고 큐 길이가 2 이상 차이날 때만 least-loaded로 바꾼다.
idle CPU 우선, SMT/LLC 토폴로지(CPUID 0xB/0x1F), wake_affine, CPU 용량 고려가 없다.
`least_loaded_cpu`도 큐 길이(`published_load`)만 본다. 설계 참고: Zircon CPU 선택 사다리,
Linux `select_idle_sibling`/`wake_affine`.

## D. 정리 대상

- kernel-ps/compat/executive/hal/mm 컴파일 경고 169건(미사용 함수 91, 메서드 약 30,
  enum 6, const 6, 미사용 import 약 20). 모델 고정용 코드와 실제 죽은 코드를 구분해 삭제.
- md 불일치: 이 문서와 진행표/세션 핸드오프, `formal/rust-large-files.tsv`의 stale 행.

## E. 이번에 닫지 못한 측정 결과

core_lifecycle 1 vCPU: thread join 개선 외에 `fork_exit_wait`는 여전히 15회 뒤 EAGAIN이며
exec/fork_cow/exit_retire_to_reap은 수집되지 않았다. 따라서 lifecycle 묶음은 미완료다.

## F. 이번 안정화에서 남긴 미해결

- fork+wait / exec의 절대 지연이 크다(1 vCPU p50 fork+wait 10~16 ms, fork+exec 약 46 ms; 8 vCPU 9~12 ms / 약 80 ms).
  최소값은 0.7~1.2 ms로 tick 단위 대기가 어딘가 남아 있다. 유휴 CPU 선호 배치(wake-idle)는 시험했으나
  변화가 없어 되돌렸다. 활성화→자식 첫 턴이 5 ms인 원인은 미확정.
- 8 vCPU 한 번의 실행에서 fork_exit_wait 최대 17 s가 관찰됐다(재현 2회 실패). 호스트 지연인지
  `wait4` 대기의 깨움 누락인지 미확정이며, 재발 시 우선 의심한다.
- 죽은 코드: 경고 164건 중 정의 외 참조가 없는 항목은 1건뿐이었고(삭제함) 나머지 112건은
  테스트·formal 핀·다른 죽은 항목이 참조하는 묶음이다(`sync_handoff`, `locality`, `ipc_handoff` 등 구 IPC 경로).
  묶음 삭제는 formal 핀/변이 앵커와 함께 해야 하므로 별도 작업으로 둔다.
- 아직 손대지 않은 미구현(pipe/pipe2, eventfd, 쓰기 가능 FS, SEQPACKET, `/proc`, blocking 소켓 등)은 사용자 지시로 제외.
