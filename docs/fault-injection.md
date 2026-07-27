# Fault Injection

[English](#english) | [한국어](#korean)

<a id="english"></a>

## English

RustOS fault injection is a controlled way to make common OS boundaries fail on
purpose. It is for hardening recovery paths, not for random breakage.

The current path is:

1. Rules live in `config/rustos.toml` under `[fault_injection]`.
2. Normal boots keep injection disabled. When a hardening run enables it,
   `cargo xtask kvm-smoke` passes the rules through QEMU fw_cfg
   `opt/rustos/fault-injection`.
3. The kernel reads that fw_cfg file during boot after the heap is initialized.
4. Selected kernel boundaries call `should_fail("fault.point")`.
5. If a rule fires, that boundary returns the same kind of failure it would
   return for a real device, storage, allocation, or IPC problem.

### Configuration

```toml
[fault_injection]
enabled = true
rules = ["display.present=off"]
```

Rule format:

```text
fault.point=action
```

Actions:

| Action | Meaning |
| --- | --- |
| `off` | Register the point but do not fail. Good default. |
| `fail` | Fail every call. |
| `drop-every:N` | Fail every Nth call. |
| `fail-after:N` | Let N calls pass, then fail later calls. |
| `rate:N` | Fail about N out of 1000 calls. |

Normal TOML array formatting is accepted. Configuration admission parses the
TOML structure and rejects duplicate, unknown, or retired fault points before
the image can be staged.

### Fault Points

| Point | Simulated failure |
| --- | --- |
| `alloc.frame` | Physical frame allocation returns `None`. |
| `block.read` | Block device read returns `DeviceFault`. |
| `block.write` | Block device write returns `DeviceFault`. |
| `block.flush` | A real generation-bound DVM flush completion is observed, then the caller receives `DeviceFault` without a durability-success claim. |
| `display.present` | Display present is dropped. |
| `display.provider.register` | Driver framebuffer provider registration fails. |
| `handle.commit` | A reserved descriptor transfer fails before any slot is committed. |
| `handle.reserve` | Descriptor reservation fails before table ownership changes. |
| `ipc.endpoint.enqueue` | Endpoint call admission fails before allocating or queueing message/reply state. |
| `ipc.endpoint.reply` | Endpoint reply admission fails before consuming the one-shot reply or publishing response data. |
| `process.spawn` | User process spawn fails as if no task slot was available. |
| `waitset.register` | Wait-set registration fails before replacing the caller's existing provider observations. |

### Examples

Drop every tenth display present:

```toml
[fault_injection]
enabled = true
rules = ["display.present=drop-every:10"]
```

Fail storage reads after early boot has made some progress:

```toml
[fault_injection]
enabled = true
rules = ["block.read=fail-after:50"]
```

Inject a low-rate process-spawn failure:

```toml
[fault_injection]
enabled = true
rules = ["process.spawn=rate:5"]
```

After changing the config, rebuild and run the bounded KVM smoke:

```bash
cargo xtask build
cargo xtask kvm-smoke --timeout 30
```

`formal/fault-scenarios.tsv` is the closed registry. Unknown, duplicate, and
retired points fail both host configuration and guest admission instead of
silently doing nothing. The storage-DVM flush failure has a first-class
negative acceptance gate. The
gate admits exactly one unconditional flush rule, requires both peers, exact
geometry, and a real first completion, then rejects any fabricated flush
success:

```bash
RUSTOS_FAULTS='block.flush=fail' cargo xtask kvm-smoke --timeout 30 \
  --storage-dvm-only --storage-dvm-expect-flush-fault
```

### Adding New Points

Add fault points at real failure boundaries, not inside arbitrary helper
functions. Good places are allocator, block I/O, device registration, queue
submit, and process spawn. Ring3 input/network policy and DVM hardware drivers
need an owner-local channel before their points can enter this kernel registry.

Use the existing shared parser in `libs/rustos-fault-injection` and the kernel
runtime in `kernel/nucleus-core/src/util/fault_injection.rs`. Do not invent a
one-off config format for a single subsystem.

<a id="korean"></a>

## 한국어

RustOS fault injection은 OS의 중요한 경계가 일부러 실패한 것처럼 만드는
하드닝 장치입니다. 목적은 무작위로 망가뜨리는 것이 아니라, 실제 장애가 났을
때 복구 경로가 제대로 동작하는지 보는 것입니다.

현재 흐름은 이렇습니다.

1. 규칙은 `config/rustos.toml`의 `[fault_injection]`에 둡니다.
2. 일반 부팅에서는 injection을 비활성화합니다. 하드닝 실행에서 활성화하면
   `cargo xtask kvm-smoke`가 규칙을 QEMU fw_cfg
   `opt/rustos/fault-injection`으로 넘깁니다.
3. 커널은 heap 초기화 직후 부팅 중 fw_cfg 파일을 읽습니다.
4. 선택된 커널 경계가 `should_fail("fault.point")`를 호출합니다.
5. 규칙이 발동하면 실제 device, storage, allocation, IPC 문제가 난 것처럼
   해당 경계가 실패를 반환합니다.

### 설정

```toml
[fault_injection]
enabled = true
rules = ["display.present=off"]
```

규칙 형식:

```text
fault.point=action
```

지원 action:

| Action | 의미 |
| --- | --- |
| `off` | 지점은 등록하지만 실패시키지 않음. 기본값으로 적합합니다. |
| `fail` | 매번 실패 |
| `drop-every:N` | N번째 호출마다 실패 |
| `fail-after:N` | N번은 통과시키고 이후 호출 실패 |
| `rate:N` | 1000번 중 대략 N번 실패 |

일반 TOML 배열 형식을 사용할 수 있습니다. 설정 admission은 TOML 구조를
파싱하고, 중복되거나 등록되지 않았거나 폐기된 fault point를 이미지 staging
전에 거부합니다.

### Fault Point

| Point | 흉내 내는 실패 |
| --- | --- |
| `alloc.frame` | 물리 frame allocation이 `None` 반환 |
| `block.read` | block device read가 `DeviceFault` 반환 |
| `block.write` | block device write가 `DeviceFault` 반환 |
| `block.flush` | 실제 generation-bound DVM flush completion 뒤 caller에 `DeviceFault`를 반환하며 durability 성공은 게시하지 않음 |
| `display.present` | display present drop |
| `display.provider.register` | driver framebuffer provider 등록 실패 |
| `handle.commit` | 예약된 descriptor transfer가 slot commit 전에 실패 |
| `handle.reserve` | descriptor reservation이 table ownership 변경 전에 실패 |
| `ipc.endpoint.enqueue` | endpoint call admission이 message/reply 할당 또는 queue 전에 실패 |
| `ipc.endpoint.reply` | endpoint reply admission이 one-shot reply 소비 또는 response publish 전에 실패 |
| `process.spawn` | task slot 부족처럼 user process spawn 실패 |
| `waitset.register` | wait-set 등록이 기존 provider observation 교체 전에 실패 |

### 예시

화면 present를 10번마다 한 번 drop:

```toml
[fault_injection]
enabled = true
rules = ["display.present=drop-every:10"]
```

초기 부팅 이후 storage read 실패:

```toml
[fault_injection]
enabled = true
rules = ["block.read=fail-after:50"]
```

낮은 확률의 process spawn 실패:

```toml
[fault_injection]
enabled = true
rules = ["process.spawn=rate:5"]
```

config를 바꾼 뒤에는 다시 빌드하고 bounded KVM smoke를 실행합니다.

```bash
cargo xtask build
cargo xtask kvm-smoke --timeout 30
```

`formal/fault-scenarios.tsv`가 닫힌 registry이며, 알 수 없거나 중복되거나
폐기된 point는 조용히 무시되지 않고 host/guest admission에서 실패합니다.
storage-DVM flush 실패는 별도의 음성(negative) acceptance gate로 검증합니다.
정확히 하나의 무조건 flush 실패 규칙만 허용하고, 양쪽 peer, 정확한 geometry,
실제 첫 completion을 확인한 뒤 허위 flush 성공 표식이 나오면 실패합니다.

```bash
RUSTOS_FAULTS='block.flush=fail' cargo xtask kvm-smoke --timeout 30 \
  --storage-dvm-only --storage-dvm-expect-flush-fault
```

### 새 지점 추가

fault point는 아무 helper에나 넣지 말고 실제 실패 경계에 넣으세요. 좋은 위치는
allocator, block I/O, device registration, queue submit, process spawn
경계입니다. Ring3 input/network policy와 DVM hardware driver는 소유자 로컬
채널이 생기기 전에는 kernel registry에 넣지 않습니다.

규칙 파싱은 `libs/rustos-fault-injection`, 커널 런타임은
`kernel/nucleus-core/src/util/fault_injection.rs`를 사용하세요. 특정
subsystem 전용 임시 config 형식을 만들지 않습니다.
