# Add a Service

[English](#english) | [한국어](#korean)

<a id="english"></a>

## English

### Workflow

1. Create the service crate under `services/<name>/`.
2. Add the crate to the workspace in `Cargo.toml`.
3. Add `services/<name>/RUSTOS.package.toml`.
4. Choose `kind = "service"` and `execution_domain = "user"`.
5. Choose `startup`:
   - `init` for early system services.
   - `session` for session-owned services.
   - `desktop` for UI/desktop services.
   - `none` for manually launched services.
6. Use `[build] builder = "cargo-kernel-binary"` for Rust userspace services.
7. Use `[install] path = "services/<name>/<name>.elf"`.
8. Add at least one `[[desktop.entries]]` entry for registry generation.
9. Add `runtime_deps` only when the service requires another package first.
10. Run `cargo xtask check`, then `cargo xtask build`.

### Minimal Manifest

```toml
id = "exampled"
kind = "service"
execution_domain = "user"
startup = "init"

[build]
builder = "cargo-kernel-binary"
package = "exampled"

[install]
path = "services/exampled/exampled.elf"

[[desktop.entries]]
display_name = "exampled"
weight_micros = 100
logical_admin = true
console_hosted = false
```

### Acceptance Criteria

- `cargo xtask check` passes.
- `cargo xtask build` stages the service under `build/image/services/<name>/`.
- Generated registries under `build/image/system/registry/` include expected
  startup/desktop metadata.
- Service logs use `observability_client` with category `service`.

<a id="korean"></a>

## 한국어

### Workflow

1. `services/<name>/` 아래 service crate를 만듭니다.
2. root `Cargo.toml` workspace에 crate를 추가합니다.
3. `services/<name>/RUSTOS.package.toml`을 추가합니다.
4. `kind = "service"`, `execution_domain = "user"`를 선택합니다.
5. `startup`을 선택합니다.
   - early system service는 `init`
   - session-owned service는 `session`
   - UI/desktop service는 `desktop`
   - 수동 실행 service는 `none`
6. Rust userspace service는 `[build] builder = "cargo-kernel-binary"`를 사용합니다.
7. `[install] path = "services/<name>/<name>.elf"`를 사용합니다.
8. registry 생성을 위해 `[[desktop.entries]]`를 최소 하나 추가합니다.
9. 다른 package가 먼저 필요할 때만 `runtime_deps`를 추가합니다.
10. `cargo xtask check`, 이후 `cargo xtask build`를 실행합니다.

### 최소 Manifest

```toml
id = "exampled"
kind = "service"
execution_domain = "user"
startup = "init"

[build]
builder = "cargo-kernel-binary"
package = "exampled"

[install]
path = "services/exampled/exampled.elf"

[[desktop.entries]]
display_name = "exampled"
weight_micros = 100
logical_admin = true
console_hosted = false
```

### Acceptance Criteria

- `cargo xtask check`가 통과합니다.
- `cargo xtask build` 후 service가 `build/image/services/<name>/` 아래에 stage됩니다.
- `build/image/system/registry/`의 generated registry에 예상 startup/desktop
  metadata가 포함됩니다.
- service log는 `observability_client`와 `service` category를 사용합니다.
