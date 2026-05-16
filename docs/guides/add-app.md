# Add an App

[English](#english) | [한국어](#korean)

<a id="english"></a>

## English

### Workflow

1. Choose app type:
   - Rust app: create `apps/<name>/` with `Cargo.toml`.
   - C smoke/demo app: create source under `apps/<name>/`.
   - Windows demo: place under `apps/windows/<name>/`.
2. Add workspace membership for Rust apps.
3. Add `RUSTOS.package.toml` at the app root.
4. Use `kind = "app"` and `execution_domain = "user"`.
5. Use `startup = "none"` unless the app should auto-launch.
6. Add `[[desktop.entries]]` with a clear `display_name`.
7. Set `console_hosted = true` for terminal/console apps.
8. Set `console_hosted = false` for UI/Wayland-style apps.
9. Run `cargo xtask check`, then `cargo xtask build`.

### Rust App Manifest

```toml
id = "example-app"
kind = "app"
execution_domain = "user"
profiles = ["default"]
startup = "none"

[build]
builder = "cargo-kernel-binary"
package = "example-app"

[install]
path = "apps/example-app/example-app.elf"

[[desktop.entries]]
display_name = "Example App"
weight_micros = 100
console_hosted = false
```

### Acceptance Criteria

- Desktop registry contains the app entry.
- Runtime can launch the app by desktop id.
- UI launcher/taskbar displays the expected title.

<a id="korean"></a>

## 한국어

### Workflow

1. app type을 선택합니다.
   - Rust app: `apps/<name>/`에 `Cargo.toml` 생성
   - C smoke/demo app: `apps/<name>/` 아래 source 생성
   - Windows demo: `apps/windows/<name>/` 아래 배치
2. Rust app은 workspace member에 추가합니다.
3. app root에 `RUSTOS.package.toml`을 추가합니다.
4. `kind = "app"`, `execution_domain = "user"`를 사용합니다.
5. 자동 실행이 필요하지 않으면 `startup = "none"`을 사용합니다.
6. 명확한 `display_name`으로 `[[desktop.entries]]`를 추가합니다.
7. terminal/console app은 `console_hosted = true`를 설정합니다.
8. UI/Wayland-style app은 `console_hosted = false`를 설정합니다.
9. `cargo xtask check`, 이후 `cargo xtask build`를 실행합니다.

### Rust App Manifest

```toml
id = "example-app"
kind = "app"
execution_domain = "user"
profiles = ["default"]
startup = "none"

[build]
builder = "cargo-kernel-binary"
package = "example-app"

[install]
path = "apps/example-app/example-app.elf"

[[desktop.entries]]
display_name = "Example App"
weight_micros = 100
console_hosted = false
```

### Acceptance Criteria

- desktop registry에 app entry가 포함됩니다.
- runtime이 desktop id로 app을 launch할 수 있습니다.
- UI launcher/taskbar에 예상 title이 표시됩니다.
