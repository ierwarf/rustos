# Add a Driver

[English](#english) | [한국어](#korean)

<a id="english"></a>

## English

### Workflow

1. Choose driver ownership:
   - First-party kernel bridge: `drivers/bridges/<class>/<name>/`.
   - Shared driver API/helper: `drivers/libs/<name>/`.
   - Prebuilt/vendor module: `vendor/...` plus manifest.
2. Add workspace membership for source-based Rust drivers.
3. Add `RUSTOS.package.toml`.
4. Use `kind = "bridge-driver"` and `execution_domain = "kernel"` for bridge
   modules.
5. Use `[build] builder = "module-image"` for Rust bridge modules.
6. Use `[install] path = "system/drivers/<class>/<name>.ko"`.
7. Add `[autoload]` metadata when the driver should be loaded by device policy.
8. Keep hardware-specific providers out of `profiles = ["default"]` unless the
   default KVM hardware profile exposes that device.
9. Put firmware or prebuilt external inputs under `vendor/`.
10. Run `cargo xtask check`, then `cargo xtask build-driver-modules`, then
   `cargo xtask stage` or full `cargo xtask build`.

### Autoload Example

```toml
[autoload]
name = "example"
class = "display"
bus = "pci"
priority = 10
when = "vfs-ready"
aliases = ["pci:vendor=0x1234,class=0x03"]
provider_group = "display-primary"
fallback_only = false
```

Fallback providers in the same `provider_group` are loaded only when no primary
provider in that group has loaded.

### Acceptance Criteria

- Driver artifact is staged under `build/image/system/drivers/...`.
- `system/registry/kernel/loadable-drivers.tsv` contains the expected metadata.
- Driver logs use category `driver`, or a more specific category such as
  `display`, `usb`, `input`, or `storage`.

<a id="korean"></a>

## 한국어

### Workflow

1. driver ownership을 선택합니다.
   - first-party kernel bridge: `drivers/bridges/<class>/<name>/`
   - shared driver API/helper: `drivers/libs/<name>/`
   - prebuilt/vendor module: `vendor/...`와 manifest
2. source 기반 Rust driver는 workspace member에 추가합니다.
3. `RUSTOS.package.toml`을 추가합니다.
4. bridge module에는 `kind = "bridge-driver"`, `execution_domain = "kernel"`을 사용합니다.
5. Rust bridge module에는 `[build] builder = "module-image"`를 사용합니다.
6. `[install] path = "system/drivers/<class>/<name>.ko"`를 사용합니다.
7. device policy로 load되어야 하면 `[autoload]` metadata를 추가합니다.
8. hardware-specific provider는 default KVM hardware profile에 그 device가 있을
   때만 `profiles = ["default"]`에 넣습니다.
9. firmware나 prebuilt external input은 `vendor/` 아래에 둡니다.
10. `cargo xtask check`, `cargo xtask build-driver-modules`, 그 다음
   `cargo xtask stage` 또는 full `cargo xtask build`를 실행합니다.

### Autoload Example

```toml
[autoload]
name = "example"
class = "display"
bus = "pci"
priority = 10
when = "vfs-ready"
aliases = ["pci:vendor=0x1234,class=0x03"]
provider_group = "display-primary"
fallback_only = false
```

같은 `provider_group`의 fallback provider는 primary provider가 없을 때만
load됩니다.

### Acceptance Criteria

- driver artifact가 `build/image/system/drivers/...` 아래에 stage됩니다.
- `system/registry/kernel/loadable-drivers.tsv`에 예상 metadata가 포함됩니다.
- driver log는 `driver` category 또는 `display`, `usb`, `input`, `storage`
  같은 더 구체적인 category를 사용합니다.
