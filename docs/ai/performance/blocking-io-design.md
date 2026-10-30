# blocking 소켓 · pipe2 · eventfd 구조 설계

> **보류된 설계 기록:** 이 문서는 당시의 기능 공백을 대상으로 작성된
> 제안이며 현재 구현·개발 우선순위의 선언이 아닙니다. 같은 감사의
> [제외 범위](core-paths-gaps.md#f-안정화-검증-경계)에서 해당 기능은
> 사용자 지시로 제외되었습니다. 다시 착수할 때는 최신 `netd`/compat
> 소스와 실제 요청 범위를 먼저 확인하세요.

정적 검토 근거는 [미구현·부실 목록](core-paths-gaps.md)의 A1/A3. 구현 전에 확정한 구조이며,
ring0는 fd 종류와 대기 기전만 갖고 정책·버퍼·의미론은 ring-3 서비스(netd)가 소유한다.

## 현재 구조(소스로 확인)

- AF_UNIX 스트림 소켓의 모든 연산은 compat → netd 동기 IPC 한 번이다
  (`syscall_linux_net6_with_timeout` → `serve_request_loop`).
- netd는 이미 **지연 응답(deferred reply) 기전**을 가진다: `DeferredLocalPoll` 큐, 슬롯 상한
  (`PENDING_LOCAL_POLLS`), 요청 데드라인, 요청 소유자에게 정확히 한 번 응답,
  `service_local_poll_waiters`가 매 루프/매 요청 뒤 재평가, 유휴 시 `next_local_poll_budget_ms`로
  가장 이른 데드라인까지만 수신 대기.
- 그러나 빈 큐에서의 `recv`/`accept`는 blocking 여부와 무관하게 즉시 `EAGAIN`이다
  (`recv_connected_bytes`, `handle_accept`의 두 분기 동일). 즉 blocking read가 블록하지 않는다.
- pipe/pipe2/eventfd는 디스패치에 없다(ENOSYS).

## 설계

### 1. blocking recv/accept = 기존 지연 응답 기전의 재사용

- netd: blocking(소켓 `O_NONBLOCK` 없음, `MSG_DONTWAIT` 없음)인 unix 스트림 `recv/recvmsg/accept`가
  `EAGAIN`이면 `DeferredLocalIo`(요청+reply cap)로 큐잉하고 `continue`한다.
  재평가는 poll waiter와 같은 지점이며, 완료 조건은 데이터/EOF/peer close/에러/데드라인이다.
- 데드라인은 요청에 실린 값을 그대로 쓴다. 데드라인 만료는 `ETIMEDOUT`로 응답하고,
  compat가 "소켓이 blocking이고 `SO_RCVTIMEO`가 없으면 재시도, pending signal이 있으면 `EINTR`"
  로 변환한다. IPC 상한 규칙(무한 대기 금지)을 깨지 않고 Linux의 무기한 blocking을 얻는다.
- 효율: waiter를 `socket_token`별로 보관해 해당 토큰이 바뀐 요청(상대 write/close) 뒤에만
  재평가한다. 전체 순회는 데드라인 점검에만 쓴다.
- 불변식: 큐 항목마다 정확히 한 번의 응답, 슬롯 해제 동반, 클라이언트 사망·소켓 close 시 revoke.

### 2. pipe2 = netd 소유의 단방향 연결 쌍

- compat `syscall_linux_pipe2` → net broker의 새 op(`PIPE`)로 netd가 연결 쌍을 만들고,
  compat는 읽기 끝/쓰기 끝 두 `Socket` 핸들을 설치한다(핸들에 접근 모드 기록).
- 의미: 쓰기 끝에 read → `EBADF`, 읽기 끝에 write → `EBADF`, 모든 읽기 끝이 닫힌 뒤 write →
  `EPIPE`+`SIGPIPE`, 쓰기 끝이 모두 닫힌 뒤 read → EOF(0), `PIPE_BUF`(4096) 이하 write는
  원자적, `O_NONBLOCK`/`O_CLOEXEC`/`O_DIRECT` 처리, `fstat`은 `S_IFIFO`, `FIONREAD`.
- 참고: Linux 파이프는 head/tail 링과 rd_wait/wr_wait 큐이며 `PIPE_BUF` 원자성이 계약이다.
  여기서는 netd의 바이트 큐가 링 역할을 하고, 대기는 1번의 지연 응답이 맡는다.

### 3. eventfd = netd 소유 카운터 객체

- `eventfd2(initval, flags)` → netd가 64비트 카운터 객체와 토큰을 만들고 핸들을 설치한다.
- read: 카운터가 0이면 blocking은 지연 응답, `EFD_NONBLOCK`은 `EAGAIN`; `EFD_SEMAPHORE`면 1 감소.
  write: 8바이트 값을 더하고 오버플로 시 blocking은 지연, nonblock은 `EAGAIN`.
  readiness는 기존 waitset provider(netd)에 연결해 epoll/poll이 그대로 동작한다.

## 구현 순서

1. netd 지연 응답을 recv/accept에 일반화하고 compat의 `ETIMEDOUT` 재시도/`EINTR` 변환 추가.
2. 같은 경로 위에 pipe2, 그다음 eventfd.
3. 각 단계마다 netd 단위 테스트(상태기계)와 `core_data` 묶음의 socket/pipe 프로브로 검증.

## 열린 위험

- netd는 이미 3,500줄 이상이다. 지연 응답 일반화는 별도 모듈(`netd/src/deferred.rs`)로 분리해
  파일 크기를 늘리지 않는다.
- blocking `connect`(unix)와 `send`의 가득 찬 큐 대기는 같은 기전으로 후속 처리한다.
