# AI Contracts

Package manifest:

- File name: `RUSTOS.package.toml`.
- Parser: `tools/xtask/src/package_manifest.rs`.
- Package ids are stable dependency keys.
- `runtime_deps` references package `id`, not path or desktop id.
- Valid `kind`: `boot`, `kernel`, `bridge-driver`, `user-driver`, `service`, `app`, `compat`.
- Valid `execution_domain`: `kernel`, `user`.
- Valid `startup`: `none`, `init`, `session`, `desktop`.
- Valid `install.layout`: `file`, `directory`.
- Valid `desktop.entries.launch`: `none`, `new-session`, `all-sessions`.
- Driver `[autoload]` supports `deps` for required module preloads and
  `softdeps` for best-effort preloads, matching Linux `modules.dep` and
  `modprobe.d softdep pre:` roles.
- Driver autoload list fields are staged as comma-separated registry values;
  individual list items must not contain tab, newline, carriage return, or
  comma.
- Driver `[autoload].linux_driver_names` lists Linux in-module driver names
  allowed to register from that package. If omitted, staging defaults it to the
  autoload `name`. Linux driver compat registration must use this registry
  field rather than hardcoded driver names.
- Driver `[autoload].provider_group` is a mutually exclusive provider contract.
  Once a loadable or native provider marks the group active, later candidates in
  the same group are skipped. `fallback_only` candidates are ordered after normal
  candidates and should be used only as lower-priority substitutes.
- In the `display-primary` provider group, real hardware/virtio display
  providers must be ordered ahead of firmware framebuffer fallbacks. `bootfb`
  is a last-resort fallback, not the default primary for QEMU or hardware GPUs.

Stage outputs:

- Boot image root: `build/image`.
- Artifact root: `build/artifacts`.
- Static overlay: `assets/image`.
- UEFI entry: GRUB-generated `build/image/EFI/BOOT/BOOTX64.EFI`.
- Kernel payload signature: `build/image/nucleus.elf.sig`.
- Registries:
  - `system/registry/kernel/loadable-drivers.tsv`
  - `system/registry/system/desktop-programs.tsv`
  - `system/registry/system/runtime-launch-programs.tsv`
  - `system/registry/system/startup-programs.tsv`
  - `system/registry/system/linux-runtime-access.tsv`
  - `system/registry/system/runtime-env.tsv`
  - `system/registry/compat/windows-system-dlls.txt`
- Linux runtime filesystem access is staged into
  `system/registry/system/linux-runtime-access.tsv` as `kind=dir|file` and
  `path=/absolute/path` fields. Kernel VFS should consume that registry instead
  of carrying hardcoded default runtime path allowlists.
- Userspace default environment policy is staged into
  `system/registry/system/runtime-env.tsv` as `scope=init|runtime`, `key=NAME`,
  and `value=VALUE` fields. `initd` and `runtimed` should consume this registry
  instead of hardcoding default `PATH`, `HOME`, `XDG_*`, or display env values.

Runtime control:

- Client crate: `libs/runtime-control`.
- Default socket: `/run/runtimed.sock`.
- Main methods: `snapshot_running_programs`, `request_launch_program_new_session`, `request_terminate_session`, `request_terminate_pid`, `notify_ui_ready`.
- Request text max: `MAX_REQUEST_PATH_BYTES`.

Kernel/userspace ABI:

- Shared ABI crate: `libs/rustos-user-abi`.
- Kernel re-export surfaces: `kernel/ps/src/user/abi.rs` and
  `kernel/compat/src/user/abi.rs`.
- Device, console, and UI `repr(C)` structs and ioctl numbers must be defined
  in `rustos-user-abi`; services such as `uiserver` and `runtimed` should use
  that crate rather than duplicating request structs or ioctl encoding logic.
- `device::DisplayInfo.flags` distinguishes boot firmware framebuffers from a
  real primary display provider. Userspace display surfaces must require
  `DISPLAY_INFO_FLAG_PRIMARY_PROVIDER`; GRUB/firmware boot framebuffers are for
  early console and panic output by default. The `bootfb` driver is the only
  last-resort exception; if it exposes a firmware framebuffer as a primary
  provider, it must stay behind hardware/virtio providers and preserve
  `DISPLAY_INFO_FLAG_BOOT_FRAMEBUFFER`.
- Driver framebuffer registration also carries explicit source flags in
  `drivers/libs/driver-abi::DisplayFramebufferRegistration`; do not infer
  primary display ownership from framebuffer geometry or from `display_info()`
  being present.

Kernel API:

- Prefer `kernel/*/src/api.rs` public wrappers over private subsystem modules.
- Main API surfaces:
  - `kernel/hal/src/api.rs`
  - `kernel/mm/src/api.rs`
  - `kernel/object/src/api.rs`
  - `kernel/ipc-runtime/src/api.rs`
  - `kernel/ps/src/api.rs`
  - `kernel/io-manager/src/api.rs`
  - `kernel/compat/src/api.rs`
- Kernel entry boot ordering lives in `kernel/src/main.rs`.
- Human reference: `docs/api/kernel.md`.
- Boot order: disable interrupts -> boot trace init -> GDT -> IDT -> paging -> higher-half jump -> stack switch -> executive bootstrap.
- Cross-crate rule: import `kernel_x::api as x_api`; do not reach into another crate's private modules when `api.rs` exposes a wrapper.
- User-memory IO APIs belong in syscall/process-context-aware paths only.
- Scheduler-aware wait users should use `kernel_ps::api::{current_task_id,
  block_current_task, wake_task}`. The `current_user_id`,
  `block_current_user_task`, and `wake_user_task` wrappers remain userspace-task
  helpers, not general kernel wait primitives.

Kernel build:

- Kernel-target Cargo invocations route through
  `tools/xtask/src/build/cargo.rs::kernel_rustflags_env`.
- RustOS operational config lives in `config/rustos.toml`.
- Kernel build-shape defaults live under `[kernel.build]`; set
  `KERNEL_BUILD_CONFIG` to test an alternate TOML file.
- Default kernel `RUSTFLAGS`: `--cfg rustos_boot_image`, `-C no-redzone`,
  `-C codegen-units=1`, `-C opt-level=2`, `-C overflow-checks=true`,
  `-C debug-assertions=false`, `-C debuginfo=0`, `-C panic=abort`.
- `RUSTOS_KERNEL_CODEGEN_UNITS` overrides the kernel codegen unit count for
  experiments without changing userspace builds. `KERNEL_CODEGEN_UNITS` remains
  a deprecated alias. The supported sweep range is `1..=256`.
- Kernel build-shape knobs also include `lto`, `force_frame_pointers`,
  `incremental`, `debuginfo`, `embed_bitcode`, `panic`, `relocation_model`,
  `strip`, and `extra_rustflags`. `incremental` is applied as
  `CARGO_INCREMENTAL` on kernel Cargo invocations.
- Kernel Cargo invocations disable a configured `sccache` rustc wrapper because
  kernel build-std/LTO flag probes are not accepted by sccache.
- `embed_bitcode=true` is required whenever `lto` is `thin` or `fat`.
- `cargo xtask config check` validates effective config; `cargo xtask config
  show` prints the effective kernel build config.
- Driver module loading must be stable across the supported
  `codegen_units=1..=256` and `opt_level=0..=3` sweep. Loader policy may ignore
  relocation sections that target non-loaded or non-ALLOC debug sections, but
  loaded text/data relocations must still resolve through explicit ABI surfaces.
- Linux compat load failures should write the first disallowed or unresolved
  external symbol to debugcon directly. Do not rely only on category-filtered
  logs for module ABI diagnostics.

Linux network driver compat:

- Linux `.ko` relocated calls into RustOS must go through the compat export ABI
  metadata in `kernel/io-manager/src/driver/linux/mod.rs`. Keep RustOS internal
  Rust/SysV call alignment intact; classify exported Linux compat symbols as
  either aligned Rust calls or stack-preserving tail calls instead of changing
  global Rust ABI rules.
- Common netdev lifecycle is routed through `kernel/io-manager/src/network/mod.rs`
  and `kernel/io-manager/src/driver/linux/netdev.rs`.
- Linux compat symbols must be explicitly implemented. Do not add broad
  prefix-based `0`, `NULL`, or no-op fallbacks to make a module load; unresolved
  required externals should fail module load.
- Optional Linux net features such as XDP, BPF, AF_XDP, failover, ethtool
  offloads, DIM, and trace/static-call hooks may be represented by explicit
  per-symbol disabled/no-op shims only when RustOS does not advertise that
  capability. These shims must fail closed or return disabled state; they must
  not fabricate packets, carrier, queue progress, or successful offload.
- Static-call, tracepoint, and netdev data anchors may resolve to inert compat
  storage only for symbols whose absence should disable instrumentation. Do not
  use data-anchor catch-alls for operational queues, DMA ownership, or packet
  memory.
- `register_netdev`/`register_netdevice` must not imply carrier/link-up.
  Link state follows `netif_carrier_on` and `netif_carrier_off`.
- PCI network modules, including future `e1000e.ko` packages, use the same
  netdev/skbuff/DMA compat surface as virtio network modules plus PCI probe
  binding in `kernel/io-manager/src/driver/pci.rs`.
- Virtio network data path is backed by the native modern PCI virtio-net backend
  in `kernel/io-manager/src/network/virtio_net.rs`; Linux compat module load
  should initialize or defer to that backend instead of fabricating link or TCP
  success.
- For native-backed virtio-net, the Linux module is an ownership/registration
  signal. Its compat shims should keep module init/exit and relocation safe,
  while packet IO remains in the native backend unless a real Linux netdev data
  path is implemented end to end.

Logging:

- Policy file: `config/rustos.toml` `[logging]`.
- Parser/cfg emitter: `tools/build_log_cfg.rs`.
- Canonical categories: `libs/rustos-observability/src/lib.rs`.
- Config is mostly build-time cfg; rebuild after changes.
- Kernel macros: `crate::debug::{trace,debug,info,warn,error}`.
- Userspace macros: `observability_client::{trace,debug,info,warn,error}`.

Docs:

- Human docs are bilingual; English first.
- Human docs must have language jump links.
- AI docs are English-only and compact.
- mdBook nav source: `docs/SUMMARY.md`.
- mdBook config: `book.toml`; output under `build/mdbook`.
- Mandatory token policy: `docs/ai/token-policy.md`.

Token-saving context rules:

- Mandatory policy first; see `docs/ai/token-policy.md`.
- Task classification second; see `docs/ai/task-router.md`.
- Prefer source files named in contracts over human docs.
- For broad docs updates, inspect `docs/SUMMARY.md` and the specific target doc only.
- For code changes, inspect docs only if behavior touches a documented contract.
- If behavior changes a stable contract, update the relevant AI doc in the same change.
