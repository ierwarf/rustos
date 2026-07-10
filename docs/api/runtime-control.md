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
| `notify_ui_ready()` | Send a one-way UI-ready notification to `runtimed`. |

### Data Flow

```text
client -> UnixStream(/run/runtimed.sock) -> RuntimeRequest -> runtimed
      <- RuntimeResponse + optional RuntimeRunningProgram payload
```

`notify_ui_ready()` is intentionally one-way and has no response payload.

The protocol is fixed-size and C-layout oriented. Keep path-like request text
under `MAX_REQUEST_PATH_BYTES`.

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
| `notify_ui_ready()` | UI-ready를 `runtimed`에 one-way로 알립니다. |

### Data Flow

```text
client -> UnixStream(/run/runtimed.sock) -> RuntimeRequest -> runtimed
      <- RuntimeResponse + optional RuntimeRunningProgram payload
```

`notify_ui_ready()`는 의도적으로 one-way이며 response payload가 없습니다.

protocol은 fixed-size C-layout 중심입니다. path-like request text는
`MAX_REQUEST_PATH_BYTES`보다 짧게 유지해야 합니다.

### Registry Loading

이 crate는 generated desktop/startup/runtime registry도 parsing합니다. 각 default
registry는 별도 immutable cache를 사용하며, cached desktop metadata를
runtime-launch policy 대신 사용하지 않습니다. service가 launch policy를 알아야 할
때 staged file을 직접 scan하지 말고 이 helper를
사용하세요.
