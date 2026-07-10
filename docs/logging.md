# Logging Guide

[English](#english) | [한국어](#korean)

<a id="english"></a>

## English

This guide documents RustOS logging configuration, where logs go, and how to
add new log lines without making the boot path noisy.

### Configuration File

The shared logging policy lives in `config/rustos.toml` under `[logging]`. Most settings are
consumed by crate `build.rs` scripts through `tools/build_log_cfg.rs`, so
changing the file requires rebuilding the affected crates.

```toml
[logging]
enabled = true
boot_trace_enabled = true
serial_mirror = true
ring_buffer_bytes = 16384
min_level = "info"

[logging.categories]
boot = "info"
panic = "fatal"
memory = "info"
sched = "info"
syscall = "off"
process = "info"
driver = "info"
storage = "info"
usb = "info"
input = "info"
display = "info"
vfs = "info"
console = "info"
service = "info"
compat = "info"
debug = "info"
heartbeat = "info"
```

### Fields

- `enabled`: enables structured category logging. When this is `false`, the
  category log cfgs are not emitted.
- `boot_trace_enabled`: emits `rustos_boot_trace_enabled` for early boot trace
  paths. This is intentionally separate from `enabled`.
- `serial_mirror`: mirrors kernel structured log lines to debugcon port `0xe9`
  in addition to keeping them in the kernel text ring. Xen HVM smoke captures
  that port through its device model.
- `ring_buffer_bytes`: byte capacity of the kernel structured log ring.
- `min_level`: default threshold for every category before per-category
  overrides are applied.
- `[logging.categories]`: per-category thresholds.

Supported levels are `off`, `trace`, `debug`, `info`, `warn`, `error`, and
`fatal`. The threshold is inclusive. For example, `storage = "warn"` enables
`warn`, `error`, and `fatal` storage logs, but filters out `info`, `debug`,
and `trace`.

### Categories

The canonical category list is defined in `libs/rustos-observability/src/lib.rs`
and duplicated by the build helper for cfg generation.

- `boot`: boot manager and kernel boot sequencing
- `panic`: panic and fatal crash reporting
- `memory`: physical memory, paging, heap, VM
- `sched`: scheduler and task switching
- `syscall`: Linux/compat syscall entry and exit paths
- `process`: process and thread lifecycle
- `driver`: driver loading and driver runtime
- `storage`: block devices, boot volume, filesystem paths
- `usb`: USB host/controller/device work
- `input`: keyboard, pointer, HID input
- `display`: framebuffer, DRM, GPU, presentation paths
- `vfs`: VFS lookup/open/read/write paths
- `console`: console host and terminal sessions
- `service`: userspace system services
- `compat`: Linux/Windows compatibility layer
- `debug`: breadcrumbs, diagnostics, internal debug helpers
- `heartbeat`: low-volume liveness markers

### Build-Time Behavior

The logging config is compiled into cfg flags such as:

- `rustos_debug_print_enabled`
- `rustos_boot_trace_enabled`
- `rustos_log_storage_debug`
- `rustos_log_service_info`

Kernel crates use generated macros from
`kernel/nucleus-core/src/debug/kdiag_macros.rs`. Userspace services use
generated helpers from `libs/observability-client`.

After changing `config/rustos.toml`, rebuild:

```bash
cargo xtask build
```

For a quicker userspace-only feedback loop:

```bash
cargo xtask build-user
```

For kernel logging changes, prefer the full build because several kernel crates
emit the same cfgs from their `build.rs` files.

### Kernel Logging

Kernel-side logging is exposed through `crate::debug` in kernel crates:

```rust
crate::debug::info!(storage, "mounted boot volume sectors={}", sector_count);
crate::debug::warn!(driver, "driver probe failed name={} err={}", name, err);
crate::debug::error_ratelimited!(usb, "xhci event ring stalled");
```

Useful APIs:

- `crate::debug::enabled!(category, level)`: check whether a log site is enabled
  before doing expensive formatting or probing.
- `crate::debug::warn_ratelimited!` and `crate::debug::error_ratelimited!`:
  one log per site per default interval.
- `crate::debug::snapshot_structured_log_bytes()`: snapshot the in-kernel text
  ring for diagnostics.
- `crate::debug::report_panic(info)`: panic reporting path.

Kernel structured log lines are rendered like:

```text
seq=17 ts_us=102399 tick=409596 lvl=info cat=storage mod=... line=42 pid=- tid=- msg="..."
```

When `serial_mirror = true`, those structured lines are also written to
debugcon. When the ring wraps, snapshots include a synthetic warning line:

```text
msg="oldest logs dropped"
```

### Userspace Service Logging

Userspace Rust services use `observability-client`:

```rust
observability_client::info!("runtimed", service, "ui ready received");
observability_client::warn!("sessiond", service, "session restart requested");
observability_client::error!("uiserver", display, "present failed errno={}", err);
```

The first argument is the service name string. The second argument is a category
identifier, not a string. It must be one of the configured categories.

Userspace log output is written to stderr using `os_observatory`. In Xen HVM
smoke, kernel debugcon is captured under `build/xen/`; userspace stderr needs
an explicit guest console route before it is considered a test signal.

### Boot Trace And Ad-Hoc Lines

There are two related but different paths:

- Structured logging: category/level controlled by `enabled`, `min_level`, and
  `[categories]`.
- Boot trace: early bring-up messages controlled by `boot_trace_enabled` in
  kernel code, plus a few service-local boot trace helpers.

Kernel boot trace uses `rustos_boot_trace_enabled`. Some userspace services
still have local `boot_line` helpers for early bootstrap debugging. `uiserver`
also supports:

```bash
RUSTOS_UI_BOOT_TRACE=1
RUSTOS_UI_PROFILE=1
```

These service-local helpers are useful during bring-up, but normal diagnostics
should use structured logging so category filtering works.

### Reading Logs

Xen smoke output is written under `build/xen/`.

- `build/xen/rustos-debugcon.log`: HVM debugcon capture
- `build/xen/rustos-serial.log`: HVM serial capture
- `build/xen/linux-dvm.cfg`, `build/xen/rustos-hvm.cfg`: generated `xl` inputs

Common commands:

```bash
cargo xtask xen-smoke --expect 'uiserver: wayland compositor ready'
```

Use focused `rg` or `tail` against `build/xen/rustos-debugcon.log`; do not
interpret an empty HVM debugcon file as successful boot.

### Recommended Profiles

Quiet default:

```toml
enabled = true
boot_trace_enabled = true
serial_mirror = true
ring_buffer_bytes = 16384
min_level = "info"

[categories]
syscall = "off"
heartbeat = "info"
```

Storage debugging:

```toml
min_level = "info"

[categories]
storage = "debug"
vfs = "debug"
driver = "info"
```

Syscall debugging:

```toml
min_level = "info"

[categories]
syscall = "trace"
compat = "debug"
process = "debug"
```

Scheduler debugging:

```toml
min_level = "warn"

[categories]
sched = "debug"
process = "debug"
heartbeat = "off"
```

For very noisy categories such as `syscall`, `sched`, and `vfs`, increase
`ring_buffer_bytes` if you need useful history after a failure.

### Adding A New Category

Add a category only when existing categories cannot describe the log source
cleanly. A new category must be added in all of these places:

1. `libs/rustos-observability/src/lib.rs`
2. `tools/build_log_cfg.rs`
3. `config/rustos.toml`
4. this guide

Then run:

```bash
cargo fmt --check
cargo test -p module-tests
cargo xtask check
```

### Logging Hygiene

- Before adding new log sites, first check whether the existing category and
  level controls can expose the needed signal. Prefer temporarily raising a
  category such as `sched = "debug"` over adding one-off output.
- When new kernel log output is needed, make it a reusable diagnostic path with
  a stable category, level, and structured fields. Avoid local raw debugcon
  writes unless the code runs before structured logging is available.
- Prefer `debug` or `trace` for loops, interrupts, syscalls, scheduler ticks,
  or packet/block paths.
- Use `info` for lifecycle events: service start, device ready, mount complete,
  UI ready.
- Use `warn` for recoverable failures or degraded fallback.
- Use `error` for failed operations that likely break a user-visible path.
- Use `fatal` only for panic/crash class events.
- Use ratelimited macros for repeated hardware, scheduler, or IO errors.
- Guard expensive log payloads with `enabled!`.
- Do not log raw untrusted buffers, large payloads, secrets, or full memory
  dumps through normal structured logs.

<a id="korean"></a>

## 한국어

이 가이드는 RustOS logging 설정, log가 기록되는 위치, boot path를 과도하게
시끄럽게 만들지 않고 새 log line을 추가하는 방법을 설명합니다.

### 설정 파일

공유 logging 정책은 `config/rustos.toml`의 `[logging]`에 있습니다. 대부분의 설정은 crate
`build.rs`가 `tools/build_log_cfg.rs`를 통해 읽습니다. 따라서 이 파일을
바꾸면 영향을 받는 crate를 다시 빌드해야 합니다.

```toml
[logging]
enabled = true
boot_trace_enabled = true
serial_mirror = true
ring_buffer_bytes = 16384
min_level = "info"

[logging.categories]
boot = "info"
panic = "fatal"
memory = "info"
sched = "info"
syscall = "off"
process = "info"
driver = "info"
storage = "info"
usb = "info"
input = "info"
display = "info"
vfs = "info"
console = "info"
service = "info"
compat = "info"
debug = "info"
heartbeat = "info"
```

### 필드

- `enabled`: structured category logging을 켭니다. `false`이면 category log
  cfg가 생성되지 않습니다.
- `boot_trace_enabled`: early boot trace path에 쓰이는
  `rustos_boot_trace_enabled`를 생성합니다. `enabled`와 의도적으로 분리되어
  있습니다.
- `serial_mirror`: kernel structured log line을 kernel text ring에 저장하는
  것과 별도로 debugcon port `0xe9`에도 미러링합니다. Xen HVM smoke는 이 port를
  device model로 capture합니다.
- `ring_buffer_bytes`: kernel structured log ring의 byte capacity입니다.
- `min_level`: category override가 적용되기 전 모든 category의 기본 threshold입니다.
- `[logging.categories]`: category별 threshold입니다.

지원 level은 `off`, `trace`, `debug`, `info`, `warn`, `error`, `fatal`입니다.
threshold는 inclusive입니다. 예를 들어 `storage = "warn"`은 storage의
`warn`, `error`, `fatal` log를 켜고 `info`, `debug`, `trace`는 걸러냅니다.

### 카테고리

canonical category list는 `libs/rustos-observability/src/lib.rs`에 정의되어
있고, cfg 생성을 위해 build helper에도 같은 목록이 있습니다.

- `boot`: boot manager and kernel boot sequencing
- `panic`: panic과 fatal crash reporting
- `memory`: physical memory, paging, heap, VM
- `sched`: scheduler와 task switching
- `syscall`: Linux/compat syscall entry/exit path
- `process`: process/thread lifecycle
- `driver`: driver loading과 driver runtime
- `storage`: block device, boot volume, filesystem path
- `usb`: USB host/controller/device 작업
- `input`: keyboard, pointer, HID input
- `display`: framebuffer, DRM, GPU, presentation path
- `vfs`: VFS lookup/open/read/write path
- `console`: console host와 terminal session
- `service`: userspace system service
- `compat`: Linux/Windows compatibility layer
- `debug`: breadcrumb, diagnostic, internal debug helper
- `heartbeat`: low-volume liveness marker

### 빌드 타임 동작

logging config는 다음과 같은 cfg flag로 컴파일됩니다.

- `rustos_debug_print_enabled`
- `rustos_boot_trace_enabled`
- `rustos_log_storage_debug`
- `rustos_log_service_info`

Kernel crate는 `kernel/nucleus-core/src/debug/kdiag_macros.rs`에서 생성된
macro를 사용합니다. Userspace service는 `libs/observability-client`의 생성
helper를 사용합니다.

`config/rustos.toml`을 바꾼 뒤에는 다시 빌드하세요.

```bash
cargo xtask build
```

userspace만 빠르게 확인하려면:

```bash
cargo xtask build-user
```

kernel logging 변경은 여러 kernel crate의 `build.rs`가 같은 cfg를 내보내므로
full build를 권장합니다.

### Kernel Logging

Kernel-side logging은 kernel crate 안에서 `crate::debug`로 노출됩니다.

```rust
crate::debug::info!(storage, "mounted boot volume sectors={}", sector_count);
crate::debug::warn!(driver, "driver probe failed name={} err={}", name, err);
crate::debug::error_ratelimited!(usb, "xhci event ring stalled");
```

유용한 API:

- `crate::debug::enabled!(category, level)`: 비싼 formatting/probing 전에 log
  site가 켜져 있는지 확인합니다.
- `crate::debug::warn_ratelimited!`, `crate::debug::error_ratelimited!`: site별
  default interval당 한 번만 기록합니다.
- `crate::debug::snapshot_structured_log_bytes()`: in-kernel text ring을
  diagnostic 용도로 snapshot합니다.
- `crate::debug::report_panic(info)`: panic reporting path입니다.

Kernel structured log line은 다음 형태로 출력됩니다.

```text
seq=17 ts_us=102399 tick=409596 lvl=info cat=storage mod=... line=42 pid=- tid=- msg="..."
```

`serial_mirror = true`이면 이 structured line이 debugcon에도 기록됩니다.
ring이 wrap되면 snapshot에 synthetic warning line이 포함됩니다.

```text
msg="oldest logs dropped"
```

### Userspace Service Logging

Rust userspace service는 `observability-client`를 사용합니다.

```rust
observability_client::info!("runtimed", service, "ui ready received");
observability_client::warn!("sessiond", service, "session restart requested");
observability_client::error!("uiserver", display, "present failed errno={}", err);
```

첫 번째 인자는 service name string입니다. 두 번째 인자는 string이 아니라
category identifier이며, 설정된 category 중 하나여야 합니다.

Userspace log output은 `os_observatory`를 통해 stderr에 기록됩니다. Xen HVM
smoke에서는 kernel debugcon이 `build/xen/`에 capture되며, userspace stderr는
명시적인 guest console route가 생기기 전에는 test signal로 사용하면 안 됩니다.

### Boot Trace와 임시 Line

관련 있지만 다른 path가 두 개 있습니다.

- Structured logging: `enabled`, `min_level`, `[categories]`가 제어하는
  category/level log
- Boot trace: kernel code의 `boot_trace_enabled`와 일부 service-local boot
  trace helper가 제어하는 early bring-up message

Kernel boot trace는 `rustos_boot_trace_enabled`를 사용합니다. 일부 userspace
service에는 early bootstrap debugging용 local `boot_line` helper가 아직
있습니다. `uiserver`는 다음 환경변수도 지원합니다.

```bash
RUSTOS_UI_BOOT_TRACE=1
RUSTOS_UI_PROFILE=1
```

이 service-local helper들은 bring-up 때 유용하지만, 일반 diagnostic은
category filtering이 동작하도록 structured logging을 사용하세요.

### Log 읽기

Xen smoke output은 `build/xen/` 아래에 기록됩니다.

- `build/xen/rustos-debugcon.log`: HVM debugcon capture
- `build/xen/rustos-serial.log`: HVM serial capture
- `build/xen/linux-dvm.cfg`, `build/xen/rustos-hvm.cfg`: generated `xl` input

자주 쓰는 명령:

```bash
cargo xtask xen-smoke --expect 'uiserver: wayland compositor ready'
```

`build/xen/rustos-debugcon.log`에는 focused `rg` 또는 `tail`을 사용하세요.
HVM debugcon이 비어 있다고 boot 성공으로 판단하면 안 됩니다.

### 추천 Profile

조용한 기본 설정:

```toml
enabled = true
boot_trace_enabled = true
serial_mirror = true
ring_buffer_bytes = 16384
min_level = "info"

[categories]
syscall = "off"
heartbeat = "info"
```

Storage debugging:

```toml
min_level = "info"

[categories]
storage = "debug"
vfs = "debug"
driver = "info"
```

Syscall debugging:

```toml
min_level = "info"

[categories]
syscall = "trace"
compat = "debug"
process = "debug"
```

Scheduler debugging:

```toml
min_level = "warn"

[categories]
sched = "debug"
process = "debug"
heartbeat = "off"
```

`syscall`, `sched`, `vfs`처럼 매우 시끄러운 category를 켠다면 failure 이후
쓸 만한 history가 남도록 `ring_buffer_bytes`를 늘리세요.

### 새 Category 추가

기존 category로 log source를 명확히 설명할 수 없을 때만 category를 추가하세요.
새 category는 다음 위치에 모두 추가해야 합니다.

1. `libs/rustos-observability/src/lib.rs`
2. `tools/build_log_cfg.rs`
3. `config/rustos.toml`
4. 이 가이드

그 뒤 실행합니다.

```bash
cargo fmt --check
cargo test -p module-tests
cargo xtask check
```

### Logging Hygiene

- 새 log site를 추가하기 전에 기존 category와 level 조정만으로 필요한 signal을
  볼 수 있는지 먼저 확인하세요. 일회성 출력을 추가하기보다 `sched = "debug"`처럼
  category level을 임시로 올리는 방식을 우선합니다.
- kernel log output을 새로 넣어야 한다면 안정적인 category, level, structured
  field를 가진 재사용 가능한 diagnostic path로 만드세요. structured logging이
  준비되기 전 경로가 아니라면 local raw debugcon write는 피하세요.
- loop, interrupt, syscall, scheduler tick, packet/block path에는 `debug` 또는
  `trace`를 선호하세요.
- service start, device ready, mount complete, UI ready 같은 lifecycle
  event에는 `info`를 사용하세요.
- recoverable failure나 degraded fallback에는 `warn`을 사용하세요.
- user-visible path를 깨뜨릴 가능성이 높은 실패에는 `error`를 사용하세요.
- panic/crash class event에만 `fatal`을 사용하세요.
- 반복되는 hardware, scheduler, IO error에는 ratelimited macro를 사용하세요.
- 비싼 log payload는 `enabled!`로 guard하세요.
- raw untrusted buffer, 큰 payload, secret, full memory dump는 일반 structured
  log로 기록하지 마세요.
