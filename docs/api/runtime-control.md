# Runtime Control API

[English](#english) | [한국어](#korean)

<a id="english"></a>

## English

Runtime control is the userspace client API used to talk to `runtimed`. The
public API lives in `libs/runtime-control/src/lib.rs`.

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

```rust
let client = runtime_control::RuntimeClient::open_default()?;
let programs = client.snapshot_running_programs()?;
client.request_launch_program_new_session("shell.desktop")?;
client.request_terminate_session(session_handle)?;
```

| Method | Purpose |
| --- | --- |
| `open_default()` | Connect through `/run/runtimed.sock`. |
| `open(path)` | Connect through a custom socket path. |
| `snapshot_running_programs()` | Return current `RuntimeRunningProgram` records. |
| `request_launch_program_new_session(desktop_file_id)` | Launch by desktop id into a new session. |
| `request_launch_path_new_session(exec_path)` | Compatibility wrapper for path launch. |
| `request_terminate_session(session_handle)` | Terminate a session. |
| `request_terminate_pid(pid)` | Terminate a process id. |
| `notify_ui_ready()` | Report UI readiness; returns `Ok` only after `runtimed` acknowledged it. |

### Data Flow

```text
client -> UnixStream(/run/runtimed.sock) -> RuntimeRequest -> runtimed
      <- RuntimeResponse + optional RuntimeRunningProgram payload
```

`notify_ui_ready()` is request/response like every other mutation (wire version
2): `runtimed` records the edge, then replies `status = 0, count = 0`. The reply,
not the write, is delivery. The operation is idempotent, so a caller that lost
the reply retries. Version 1 was a one-way write; a version mismatch is refused
with `EPROTO` instead of stalling.

The protocol is fixed-size and C-layout oriented. Path-like request text must
be nonempty, at most `MAX_REQUEST_PATH_BYTES` bytes, and printable ASCII
without backslashes.

### Registry Loading

The same crate also parses generated desktop/startup/runtime registries. Each
default registry has its own immutable cache; cached desktop metadata is never
substituted for runtime-launch policy. Use these helpers instead of scanning
staged files manually.

<a id="korean"></a>

## 한국어

Runtime control은 `runtimed`와 통신하는 userspace client API입니다. public API는
`libs/runtime-control/src/lib.rs`에 있습니다.

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

```rust
let client = runtime_control::RuntimeClient::open_default()?;
let programs = client.snapshot_running_programs()?;
client.request_launch_program_new_session("shell.desktop")?;
client.request_terminate_session(session_handle)?;
```

| Method | Purpose |
| --- | --- |
| `open_default()` | `/run/runtimed.sock`으로 연결합니다. |
| `open(path)` | custom socket path로 연결합니다. |
| `snapshot_running_programs()` | 현재 `RuntimeRunningProgram` record를 반환합니다. |
| `request_launch_program_new_session(desktop_file_id)` | desktop id로 새 session에 launch합니다. |
| `request_launch_path_new_session(exec_path)` | path launch용 compatibility wrapper입니다. |
| `request_terminate_session(session_handle)` | session을 종료합니다. |
| `request_terminate_pid(pid)` | process id를 종료합니다. |
| `notify_ui_ready()` | UI-ready를 알리며 `runtimed`가 acknowledge한 뒤에만 `Ok`를 반환합니다. |

### Data Flow

```text
client -> UnixStream(/run/runtimed.sock) -> RuntimeRequest -> runtimed
      <- RuntimeResponse + optional RuntimeRunningProgram payload
```

`notify_ui_ready()`는 다른 mutation과 같은 request/response입니다(wire version 2).
`runtimed`가 edge를 기록한 뒤 `status = 0, count = 0`으로 응답하며, write가 아니라 이 응답이
전달 완료입니다. 연산은 idempotent이므로 응답을 잃은 호출자는 재시도합니다. version 1은
one-way write였고, version 불일치는 stall 대신 `EPROTO`로 거부됩니다.

protocol은 fixed-size C-layout 중심입니다. path-like request text는 비어 있지
않은 printable ASCII여야 하고 backslash를 포함할 수 없으며,
`MAX_REQUEST_PATH_BYTES` bytes 이하여야 합니다.

### Registry Loading

이 crate는 generated desktop/startup/runtime registry도 parsing합니다. 각 default
registry는 별도 immutable cache를 사용하며, cached desktop metadata를
runtime-launch policy 대신 사용하지 않습니다. service가 launch policy를 알아야 할
때 staged file을 직접 scan하지 말고 이 helper를
사용하세요.
