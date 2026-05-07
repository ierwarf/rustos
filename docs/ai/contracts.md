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
- Driver `[autoload].provider_group` is a mutually exclusive provider contract.
  Once a loadable or native provider marks the group active, later candidates in
  the same group are skipped. `fallback_only` candidates are ordered after normal
  candidates and should be used only as lower-priority substitutes.

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
  - `system/registry/compat/windows-system-dlls.txt`

Runtime control:

- Client crate: `libs/runtime-control`.
- Default socket: `/run/runtimed.sock`.
- Main methods: `snapshot_running_programs`, `request_launch_program_new_session`, `request_terminate_session`, `request_terminate_pid`, `notify_ui_ready`.
- Request text max: `MAX_REQUEST_PATH_BYTES`.

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

Linux network driver compat:

- Linux `.ko` relocated calls into RustOS must go through the compat export ABI
  metadata in `kernel/io-manager/src/driver/linux/mod.rs`. Keep RustOS internal
  Rust/SysV call alignment intact; classify exported Linux compat symbols as
  either aligned Rust calls or stack-preserving tail calls instead of changing
  global Rust ABI rules.
- Common netdev lifecycle is routed through `kernel/io-manager/src/network/mod.rs`
  and `kernel/io-manager/src/driver/linux/netdev.rs`.
- `register_netdev`/`register_netdevice` must not imply carrier/link-up.
  Link state follows `netif_carrier_on` and `netif_carrier_off`.
- PCI network modules, including future `e1000e.ko` packages, use the same
  netdev/skbuff/DMA compat surface as virtio network modules plus PCI probe
  binding in `kernel/io-manager/src/driver/pci.rs`.
- Virtio network data path is backed by the native modern PCI virtio-net backend
  in `kernel/io-manager/src/network/virtio_net.rs`; Linux compat module load
  should initialize or defer to that backend instead of fabricating link or TCP
  success.

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
