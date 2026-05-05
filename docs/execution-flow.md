# Execution Flow

[English](#english) | [한국어](#korean)

<a id="english"></a>

## English

### Host Flow

```text
cargo xtask build
  -> validate workspace layering
  -> build UEFI bootloader
  -> build prekernel and nucleus
  -> build userspace services/apps
  -> build modules and compatibility artifacts
  -> stage build/image
  -> write generated registries
```

`cargo xtask run` does not rebuild the image. It launches QEMU against the
current `build/image`.

### Staged Image Flow

```text
build/artifacts/* + assets/image/* + vendor/*
  -> build/image/EFI/BOOT/BOOTX64.EFI
  -> build/image/services/*
  -> build/image/apps/*
  -> build/image/system/drivers/*
  -> build/image/system/registry/*
```

Stage uses `RUSTOS.package.toml` manifests as the deployment source of truth.

### Boot Flow

```text
UEFI firmware
  -> EFI/BOOT/BOOTX64.EFI
  -> prekernel.elf
  -> nucleus.elf
  -> kernel executive
  -> init services
  -> runtime service manager
  -> UI server
  -> desktop/session apps
```

The kernel and services rely on generated registries for startup policy,
desktop entries, runtime launch policy, driver autoload metadata, and Windows
system DLL inventory.

### Runtime Flow

```text
uiserver -> RuntimeClient -> /run/runtimed.sock -> runtimed
  -> launch desktop id or executable path
  -> create/track console session
  -> snapshot running programs
  -> update launcher/taskbar/window state
```

The UI server uses runtime snapshots to reconcile visible windows with running
programs. Console-hosted programs and Wayland-style windows are presented by
the UI server through framebuffer rendering.

### Diagnostic Flow

```text
config/logging.toml
  -> build.rs cfg generation
  -> kernel ring/debugcon and userspace stderr
  -> logs/debugcon.log or --debugcon stdio
```

Use [Logging Guide](logging.md) for logging categories, levels, and output
paths.

<a id="korean"></a>

## 한국어

### Host Flow

```text
cargo xtask build
  -> workspace layering 검사
  -> UEFI bootloader 빌드
  -> prekernel과 nucleus 빌드
  -> userspace services/apps 빌드
  -> modules와 compatibility artifacts 빌드
  -> build/image stage
  -> generated registries 작성
```

`cargo xtask run`은 image를 다시 빌드하지 않습니다. 현재 `build/image`를
QEMU로 실행합니다.

### Staged Image Flow

```text
build/artifacts/* + assets/image/* + vendor/*
  -> build/image/EFI/BOOT/BOOTX64.EFI
  -> build/image/services/*
  -> build/image/apps/*
  -> build/image/system/drivers/*
  -> build/image/system/registry/*
```

stage는 `RUSTOS.package.toml` manifest를 deployment source of truth로 사용합니다.

### Boot Flow

```text
UEFI firmware
  -> EFI/BOOT/BOOTX64.EFI
  -> prekernel.elf
  -> nucleus.elf
  -> kernel executive
  -> init services
  -> runtime service manager
  -> UI server
  -> desktop/session apps
```

kernel과 service는 startup policy, desktop entry, runtime launch policy,
driver autoload metadata, Windows system DLL inventory를 generated registry에서
읽습니다.

### Runtime Flow

```text
uiserver -> RuntimeClient -> /run/runtimed.sock -> runtimed
  -> desktop id 또는 executable path launch
  -> console session 생성/추적
  -> running program snapshot
  -> launcher/taskbar/window state 갱신
```

UI server는 runtime snapshot으로 visible window와 running program을 reconcile합니다.
console-hosted program과 Wayland-style window는 framebuffer rendering으로 표시됩니다.

### Diagnostic Flow

```text
config/logging.toml
  -> build.rs cfg generation
  -> kernel ring/debugcon and userspace stderr
  -> logs/debugcon.log or --debugcon stdio
```

logging category, level, output path는 [로깅 가이드](logging.md)를 참고하세요.
