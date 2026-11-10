# Runtime Control API

[English](#english) | [한국어](#korean)

<a id="english"></a>

## English

`libs/runtime-control` is the Ring3 client for `runtimed`. The only wire
definition is `libs/runtime-control/src/protocol.rs`; protocol **version 3**
uses a 192-byte request and 12-byte response. Mixed versions fail closed.

### Constants

| Constant | Value |
| --- | --- |
| `DEFAULT_RUNTIME_SOCKET_PATH` | `/run/runtimed.sock` |
| `DEFAULT_APPLICATIONS_DIR` | `/usr/share/applications` |
| `DEFAULT_AUTOSTART_DIR` | `/etc/xdg/autostart` |
| `DEFAULT_STARTUP_REGISTRY_PATH` | `/system/registry/system/startup-programs.tsv` |
| `DEFAULT_DESKTOP_REGISTRY_PATH` | `/system/registry/system/desktop-programs.tsv` |
| `DEFAULT_RUNTIME_LAUNCH_REGISTRY_PATH` | `/system/registry/system/runtime-launch-programs.tsv` |

### RuntimeClient

| Method | Purpose |
| --- | --- |
| `open_default()` / `open(path)` | Select the runtime socket. |
| `snapshot_running_programs()` | Read the current bounded running set. |
| `watch_running_programs(digest, wait)` | Wait for a running-set change or a bounded re-arm. |
| `launch_transaction(target)` | Create a retained launch identity for a registered desktop id or executable. |
| `terminate_session_transaction(handle)` | Create a retained session-termination identity. |
| `terminate_pid_transaction(pid)` | Create a retained process-termination identity. |
| `submit_transaction(&transaction)` | Submit once; returns `MutationOutcome`. |
| `query_transaction(&transaction)` | Query the same operation without admitting another one. |
| `notify_ui_ready()` | Idempotent acknowledged UI-ready notification. |

```rust
let client = runtime_control::RuntimeClient::open_default()?;
let transaction = client.launch_transaction("shell.desktop")?;
let outcome = client.submit_transaction(&transaction);
// Retain `transaction` for reconciliation; do not replace it after transport loss.
```

`MutationOutcome::Committed` confirms completion, `Pending` means queued or
admitted, and `Rejected(errno)` is a validated terminal rejection.
`Transport(errno)` means the response was lost/malformed and `Unknown(errno)`
means the result cannot be established. After `Pending` or `Transport`, query
**the same identity**, with backoff bounded by `transaction.query_remaining()`.
Do not interpret `ENOENT`, timeout or `ESTALE` after server restart as proof
that the mutation did not happen. Unknown outcomes need reconciliation or an
explicit new user intent, never an automatic duplicate launch. The former
one-shot launch/terminate methods were removed because they lost that identity.

Each attempt has one kernel-wide `CLOCK_MONOTONIC` deadline across connect,
write, response header and body. Queries have a new attempt deadline, not a
new operation identity or admission lifetime. The server binds replay to the
authenticated peer and full request. Admission and bounded replay retention
are anchored to the immutable creation timestamp in the identity; changing a
request deadline cannot resurrect an evicted identity. Outcomes are not durable
across runtimed restart: old identities are rejected rather than re-executed.
See the [owner contract](../ai/runtime-rpc-deadlines.md) for retention and worker
commit/cancellation semantics.

`notify_ui_ready()` records the edge and returns success only after an
acknowledgement. Its idempotence permits retry after a lost reply. Path text
must be bounded, nonempty printable ASCII without backslashes. Only snapshot
and watch operations carry bounded `RuntimeRunningProgram` payloads.

### Registry loading

Each default registry has its own immutable cache. Desktop metadata cannot
substitute for runtime-launch policy; use the crate's loading helpers rather
than scanning staged files independently.

<a id="korean"></a>

## 한국어

`runtime-control`은 `runtimed`의 Ring3 클라이언트입니다. 유일한 wire 정의는
`libs/runtime-control/src/protocol.rs`이며 현재 **버전 3**, 요청 192바이트,
응답 12바이트입니다. 기본 경로와 메서드는 위 표를 따릅니다.

Launch/terminate는 먼저 `RuntimeTransaction`을 만들고 `submit_transaction`으로
한 번 제출합니다. 응답을 잃거나 `Pending`을 받으면 **같은 transaction을 보존**해
`query_transaction`으로 조회해야 합니다. 타임아웃 뒤 새 transaction을 자동으로
만들면 중복 실행될 수 있습니다. 이 때문에 기존 일회성 mutation 메서드는 제거됐습니다.

- `Committed`: 작업 완료가 확인됐습니다.
- `Pending`: 접수됐거나 실행 중입니다.
- `Rejected(errno)`: 서버가 확인한 최종 거절입니다.
- `Transport(errno)`: 응답 유실/손상으로 결과를 모릅니다.
- `Unknown(errno)`: 결과를 확정할 수 없습니다.

조회는 `query_remaining()`의 남은 시간 안에서 backoff하며 수행합니다. 타임아웃,
`ENOENT`, 서버 재시작 뒤의 `ESTALE`은 작업 미실행을 증명하지 않습니다.
재시작 전 결과는 복원되지 않지만 이전 ID를 새 작업으로 재실행하지도 않습니다.
세부 계약은 [runtime RPC deadline 계약](../ai/runtime-rpc-deadlines.md)을 참고하세요.

한 통신 시도의 연결·송신·헤더·본문은 같은 `CLOCK_MONOTONIC` 마감시간을 공유합니다.
조회용 마감시간을 새로 만들더라도 원래 작업의 수명은 늘어나지 않습니다.
`notify_ui_ready()`는 별도로 멱등적이며, 서버가 edge를 기록하고 응답한 뒤에만
성공합니다. registry별 immutable cache는 서로 대체할 수 없습니다.
