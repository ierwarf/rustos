# Package Manifest API

[English](#english) | [한국어](#korean)

<a id="english"></a>

## English

`RUSTOS.package.toml` is the deployment source of truth. The parser and schema
types are in `tools/xtask/src/package_manifest.rs`.

### Minimal Shape

```toml
id = "example"
kind = "service"
execution_domain = "user"
profiles = ["default"]
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
| `kind` | `boot`, `kernel`, `bridge-driver`, `user-driver`, `service`, `app`, `compat` | Package taxonomy. |
| `execution_domain` | `kernel`, `user` | Optional explicit execution domain. |
| `profiles` | string list | Build/profile membership; defaults to `["default"]`. |
| `startup` | `none`, `init`, `session`, `desktop` | Startup policy for generated registries. |
| `runtime_deps` | package id list | Runtime ordering/exposure dependency metadata. |

### Build Section

| `builder` | Purpose |
| --- | --- |
| `bootloader-uefi` | Compatibility alias for GRUB EFI boot manager generation. |
| `kernel-rustc` | Kernel/nucleus artifact build. |
| `cargo-kernel-binary` | Rust userspace service/app ELF. |
| `mingw-c-exe` | Windows PE executable demo. |
| `c-demo` | Host C demo/smoke artifact. |
| `module-image` | Kernel bridge `.ko` module image. |
| `winsys-dll-bundle` | Windows system DLL bundle. |
| `external-copy` | Copy externally provided artifact. |

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
| `weight_micros` | Scheduling/task weight metadata. |
| `logical_admin` | Marks privileged/admin-style components. |
| `console_hosted` | Whether runtime should host it through console. |
| `launch` | `none`, `new-session`, or `all-sessions`. |
| `args` | Command argv metadata. |
| `env` | Environment entries. |

### Autoload Section

Bridge drivers can declare autoload metadata:

```toml
[autoload]
name = "amdgpu"
class = "display"
bus = "pci"
priority = 10
when = "vfs-ready"
aliases = ["pci:vendor=0x1002,class=0x03"]
provider_group = "display-primary"
fallback_only = false
```

Stage writes this into `system/registry/kernel/loadable-drivers.tsv`.

<a id="korean"></a>

## 한국어

`RUSTOS.package.toml`은 deployment source of truth입니다. parser와 schema type은
`tools/xtask/src/package_manifest.rs`에 있습니다.

### 최소 형태

```toml
id = "example"
kind = "service"
execution_domain = "user"
profiles = ["default"]
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
| `kind` | `boot`, `kernel`, `bridge-driver`, `user-driver`, `service`, `app`, `compat` | package taxonomy |
| `execution_domain` | `kernel`, `user` | optional explicit execution domain |
| `profiles` | string list | build/profile membership, 기본값 `["default"]` |
| `startup` | `none`, `init`, `session`, `desktop` | generated registry startup policy |
| `runtime_deps` | package id list | runtime ordering/exposure dependency metadata |

### Build Section

| `builder` | Purpose |
| --- | --- |
| `bootloader-uefi` | GRUB EFI boot manager 생성용 compatibility alias |
| `kernel-rustc` | kernel/nucleus artifact build |
| `cargo-kernel-binary` | Rust userspace service/app ELF |
| `mingw-c-exe` | Windows PE executable demo |
| `c-demo` | host C demo/smoke artifact |
| `module-image` | kernel bridge `.ko` module image |
| `winsys-dll-bundle` | Windows system DLL bundle |
| `external-copy` | externally provided artifact copy |

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
| `weight_micros` | scheduling/task weight metadata |
| `logical_admin` | privileged/admin-style component 표시 |
| `console_hosted` | runtime이 console로 host할지 여부 |
| `launch` | `none`, `new-session`, `all-sessions` |
| `args` | command argv metadata |
| `env` | environment entries |

### Autoload Section

Bridge driver는 autoload metadata를 선언할 수 있습니다.

```toml
[autoload]
name = "amdgpu"
class = "display"
bus = "pci"
priority = 10
when = "vfs-ready"
aliases = ["pci:vendor=0x1002,class=0x03"]
provider_group = "display-primary"
fallback_only = false
```

stage는 이 정보를 `system/registry/kernel/loadable-drivers.tsv`에 기록합니다.
