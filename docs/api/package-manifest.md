# Package Manifest API

[English](#english) | [한국어](#korean)

<a id="english"></a>

## English

`RUSTOS.package.toml` is the deployment source of truth. The parser and schema
types are in `tools/xtask/src/package_manifest.rs`. Unknown top-level and
nested fields are rejected; do not leave inactive compatibility metadata in a
manifest.

### Minimal Shape

```toml
id = "example"
kind = "service"
execution_domain = "user"
startup = "none"

[build]
builder = "cargo-kernel-binary"
package = "example"

[install]
path = "services/example/example.elf"

[[desktop.entries]]
display_name = "example"
weight_micros = 100
console_hosted = false
```

### Top-Level Fields

| Field | Values | Meaning |
| --- | --- | --- |
| `id` | string | Stable package id used by runtime deps and registries. |
| `kind` | `kernel`, `service`, `app`, `compat` | Package taxonomy. |
| `execution_domain` | `kernel`, `user` | Required execution domain. |
| `startup` | `none`, `init`, `session`, `desktop` | Startup policy for generated registries. |
| `runtime_deps` | package id list | Runtime ordering/exposure dependency metadata. |

### Build Section

| `builder` | Purpose |
| --- | --- |
| `kernel-rustc` | Kernel/nucleus artifact build. |
| `cargo-kernel-binary` | Rust userspace service/app ELF. |
| `mingw-c-exe` | Windows PE executable demo. |
| `c-demo` | Host C demo/smoke artifact. |
| `winsys-dll-bundle` | Windows system DLL bundle. |

### Install Section

| Field | Meaning |
| --- | --- |
| `path` | Relative path inside artifacts and staged image. |
| `layout` | `file` or `directory`; defaults to `file`. |

### Desktop Entries

`[[desktop.entries]]` generates desktop/runtime launch metadata.

| Field | Meaning |
| --- | --- |
| `display_name` | UI/runtime display name. |
| `image` | Optional staged image path override. |
| `exec` | Optional executable path override. |
| `no_display` | Hide this entry from application discovery while retaining its launch policy. |
| `weight_micros` | Scheduling/task weight metadata. |
| `logical_admin` | Marks privileged/admin-style components. |
| `console_hosted` | Whether runtime should host it through console. |
| `launch` | `none`, `new-session`, or `all-sessions`. |
| `args` | Command argv metadata. |
| `env` | Environment entries. |

<a id="korean"></a>

## 한국어

`RUSTOS.package.toml`은 deployment source of truth입니다. parser와 schema type은
`tools/xtask/src/package_manifest.rs`에 있습니다. 알 수 없는 top-level/nested field는
오류로 처리하므로, 효과 없는 compatibility metadata를 manifest에 남기지 마세요.

### 최소 형태

```toml
id = "example"
kind = "service"
execution_domain = "user"
startup = "none"

[build]
builder = "cargo-kernel-binary"
package = "example"

[install]
path = "services/example/example.elf"

[[desktop.entries]]
display_name = "example"
weight_micros = 100
console_hosted = false
```

### Top-Level Fields

| Field | Values | Meaning |
| --- | --- | --- |
| `id` | string | runtime deps와 registry에서 쓰는 stable package id |
| `kind` | `kernel`, `service`, `app`, `compat` | package taxonomy |
| `execution_domain` | `kernel`, `user` | 필수 execution domain |
| `startup` | `none`, `init`, `session`, `desktop` | generated registry startup policy |
| `runtime_deps` | package id list | runtime ordering/exposure dependency metadata |

### Build Section

| `builder` | Purpose |
| --- | --- |
| `kernel-rustc` | kernel/nucleus artifact build |
| `cargo-kernel-binary` | Rust userspace service/app ELF |
| `mingw-c-exe` | Windows PE executable demo |
| `c-demo` | host C demo/smoke artifact |
| `winsys-dll-bundle` | Windows system DLL bundle |

### Install Section

| Field | Meaning |
| --- | --- |
| `path` | artifact와 staged image 안의 relative path |
| `layout` | `file` 또는 `directory`, 기본값 `file` |

### Desktop Entries

`[[desktop.entries]]`는 desktop/runtime launch metadata를 생성합니다.

| Field | Meaning |
| --- | --- |
| `display_name` | UI/runtime display name |
| `image` | optional staged image path override |
| `exec` | optional executable path override |
| `no_display` | launch policy는 유지하고 application discovery에서는 숨깁니다. |
| `weight_micros` | scheduling/task weight metadata |
| `logical_admin` | privileged/admin-style component 표시 |
| `console_hosted` | runtime이 console로 host할지 여부 |
| `launch` | `none`, `new-session`, `all-sessions` |
| `args` | command argv metadata |
| `env` | environment entries |
