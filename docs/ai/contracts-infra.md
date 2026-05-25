# AI Contracts — Infrastructure

Package/stage schemas, runtime control, kernel API, build, fault injection, logging, docs. For service/IPC/broker ABI: `contracts-abi.md`.

## Package Manifest

- File: `RUSTOS.package.toml`. Parser: `tools/xtask/src/package_manifest.rs`.
- Package ids = stable dependency keys. `runtime_deps` references package `id`, not path or desktop id.

### Valid Enum Values

| Field | Values |
|-------|--------|
| `kind` | `boot`, `kernel`, `bridge-driver`, `user-driver`, `service`, `app`, `compat` |
| `execution_domain` | `kernel`, `user` |
| `startup` | `none`, `init`, `session`, `desktop` |
| `install.layout` | `file`, `directory` |
| `desktop.entries.launch` | `none`, `new-session`, `all-sessions` |

### Driver Autoload

- `deps`: required module preloads (Linux `modules.dep` role).
- `softdeps`: best-effort preloads (Linux `modprobe.d softdep pre:` role).
- List fields stage as comma-separated registry values; items must not contain tab, newline, carriage return, or comma.
- `linux_driver_names`: Linux in-module driver names allowed to register from that package. If omitted, defaults to autoload `name`. Linux driver compat registration must use this registry field — not hardcoded driver names.
- `provider_group`: mutually exclusive provider contract. Once a loadable/native provider marks the group active, later candidates are skipped. `fallback_only` candidates ordered after normal candidates as lower-priority substitutes.
- `display-primary` group: real hardware/virtio providers ordered ahead of firmware framebuffer fallbacks. `bootfb` is last-resort, **never** default primary for QEMU or hardware GPUs.
- `driverd` owns autoload policy. Kernel driver brokers may expose narrow hardware-presence primitives for staged aliases (`platform:bootfb`, `pci:*`, `virtio:*`) but **must not** pick provider order or bypass registry `provider_group` policy.

## Stage Outputs

| Path | Purpose |
|------|---------|
| `build/image` | Boot image root |
| `build/artifacts` | Artifact root |
| `assets/image` | Static overlay |
| `build/image/EFI/BOOT/BOOTX64.EFI` | UEFI entry (GRUB-generated) |
| `build/image/nucleus.elf.sig` | Kernel payload signature |

### Registries

- `system/registry/kernel/loadable-drivers.tsv`
- `system/registry/system/desktop-programs.tsv`
- `system/registry/system/runtime-launch-programs.tsv`
- `system/registry/system/startup-programs.tsv`
- `system/registry/system/linux-runtime-access.tsv` — `kind=dir|file`, `path=/absolute/path`. Kernel VFS consumes this instead of hardcoded default runtime path allowlists.
- `system/registry/system/runtime-env.tsv` — `scope=init|runtime`, `key=NAME`, `value=VALUE`. `initd` and `runtimed` consume this instead of hardcoded default `PATH`, `HOME`, `XDG_*`, display env values.
- `system/registry/compat/windows-system-dlls.txt`

## Runtime Control

- Client crate: `libs/runtime-control`.
- Default socket: `/run/runtimed.sock`.
- Main methods: `snapshot_running_programs`, `request_launch_program_new_session`, `request_terminate_session`, `request_terminate_pid`, `notify_ui_ready`.
- Request text max: `MAX_REQUEST_PATH_BYTES`.

## Kernel API

- Prefer `kernel/*/src/api.rs` public wrappers over private subsystem modules.
- Cross-crate rule: `use kernel_x::api as x_api;` — **never** reach into another crate's private modules when `api.rs` exposes a wrapper.
- User-memory IO APIs belong in syscall/process-context-aware paths only.
- Human reference: `docs/api/kernel.md`.

### API Surfaces

- `kernel/hal/src/api.rs`
- `kernel/mm/src/api.rs`
- `kernel/object/src/api.rs`
- `kernel/ipc-runtime/src/api.rs`
- `kernel/ps/src/api.rs`
- `kernel/io-manager/src/api.rs`
- `kernel/compat/src/api.rs`

### Boot Order

Kernel entry boot order lives in `kernel/src/main.rs`:

`disable interrupts → boot trace init → GDT → IDT → paging → higher-half jump → stack switch → executive bootstrap`

### Wait / Scheduler API

Scheduler-aware wait users should use `kernel_ps::api::{current_task_id, block_current_task, wake_task}`. The `current_user_id`, `block_current_user_task`, `wake_user_task` wrappers are userspace-task helpers — **not** general kernel wait primitives.

## Kernel Build

- Kernel-target Cargo invocations route through `tools/xtask/src/build/cargo.rs::kernel_rustflags_env`.
- Operational config: `config/rustos.toml`. Build-shape defaults: `[kernel.build]`. Set `KERNEL_BUILD_CONFIG` to test an alternate TOML file.

### Default Kernel `RUSTFLAGS`

`--cfg rustos_boot_image`, `-C no-redzone`, `-C codegen-units=1`, `-C opt-level=2`, `-C overflow-checks=true`, `-C debug-assertions=false`, `-C debuginfo=0`, `-C panic=abort`.

### Build-Shape Knobs

- `RUSTOS_KERNEL_CODEGEN_UNITS` overrides kernel codegen unit count for experiments without changing userspace builds. Deprecated alias: `KERNEL_CODEGEN_UNITS`. Sweep range: `1..=256`.
- Other knobs: `lto`, `force_frame_pointers`, `incremental` (applied as `CARGO_INCREMENTAL` on kernel Cargo invocations), `debuginfo`, `embed_bitcode`, `panic`, `relocation_model`, `strip`, `extra_rustflags`.
- `embed_bitcode=true` required when `lto` ∈ {`thin`, `fat`}.
- Kernel invocations disable any configured `sccache` rustc wrapper — kernel build-std/LTO flag probes are not accepted by sccache.

### Config & Module Loader

- `cargo xtask config check` validates effective config.
- `cargo xtask config show` prints effective kernel build config.
- Driver module loading must be stable across `codegen_units=1..=256` × `opt_level=0..=3` sweep. Loader policy may ignore relocations targeting non-loaded or non-ALLOC debug sections; loaded text/data relocations must still resolve through explicit ABI surfaces.
- Linux compat load failures: write first disallowed/unresolved external symbol to debugcon directly. **Do not** rely only on category-filtered logs for module ABI diagnostics.

## Fault Injection

- Human guide: `docs/fault-injection.md`.
- Shared parser: `libs/rustos-fault-injection`.
- Host config: `tools/xtask/src/config/project.rs` `[fault_injection]`.
- QEMU handoff: `tools/xtask/src/qemu/mod.rs` → fw_cfg `opt/rustos/fault-injection`.
- Kernel runtime: `kernel/nucleus-core/src/util/fault_injection.rs`.
- Kernel init: `kernel/executive/src/boot.rs::fault_injection::init_from_qemu_fw_cfg()` (after heap init).

### Rule Format

`location=action`.

| Action | Effect |
|--------|--------|
| `off` | Disabled |
| `fail` | Fail on every hit |
| `drop-every:N` | Drop every Nth |
| `fail-after:N` | Fail after N hits |
| `rate:N` | Fail at rate N |
| `delay-ms:N` | Parsed but not wired to sleep/delay yet |

### Current Fault Points

`alloc.frame`, `block.read`, `block.write`, `display.present`, `display.provider.register`, `driver.module.load`, `input.event.enqueue`, `pci.config.read`, `process.spawn`, `socket.recv`, `socket.send`, `virtio-gpu.control.submit`.

Add new points only at realistic failure boundaries: allocation, block IO, device registration, queue submit, process spawn, IPC/socket send/recv, driver probe/load. **Do not scatter fault checks through arbitrary helper functions.**

`config/rustos.toml` may use normal TOML formatting for fault rules (including multiline arrays); logging extraction must ignore non-logging sections.

## Linux Network & Driver Compat

- `netd` owns socket namespace/policy; `driverd` owns autoload/provider policy. `kernel/io-manager/src/driver/mod.rs` is limited to privileged DMA/IRQ/IOMMU + explicit module-load substrate.
- Kernel broker validates `.ko` images, relocates them, exposes `DriverKernelApiV1`, maps MMIO, executes module init. **Do not move `.ko` execution to ring3.**
- Deleted io-manager policy files are not source of truth — do not restore.
- Linux compat symbols must be explicitly implemented; no broad no-op fallbacks.
- Optional net features (XDP, BPF, AF_XDP, ethtool offloads, DIM) may use per-symbol disabled shims; must fail closed — **never** fabricate packets or carrier state.

## Logging

- Policy: `config/rustos.toml` `[logging]`.
- Parser/cfg emitter: `tools/build_log_cfg.rs`.
- Canonical categories: `libs/rustos-observability/src/lib.rs`.
- Config is mostly build-time cfg — rebuild after changes.
- Kernel macros: `crate::debug::{trace,debug,info,warn,error}`.
- Userspace macros: `observability_client::{trace,debug,info,warn,error}`.

## Docs

- Human docs bilingual; English first; mandatory language jump links.
- AI docs English-only and compact.
- mdBook nav source: `docs/SUMMARY.md`. mdBook config: `book.toml`. Output: `build/mdbook`.
- Mandatory token policy: `docs/ai/token-policy.md`.
