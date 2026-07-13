# AI Contracts — Infrastructure

Package/stage schemas, runtime control, kernel API, build, fault injection, logging, docs. For service/IPC/broker ABI: `contracts-abi.md`.

## Package Manifest

- File: `RUSTOS.package.toml`. Parser: `tools/xtask/src/package_manifest.rs`.
- Package ids = stable dependency keys. `runtime_deps` references package `id`, not path or desktop id.
- Manifest parsing is fail-closed: unknown top-level or nested fields fail the
  build. Retired `[boot]` metadata is not accepted; rootd/initd and generated
  registries are the boot-policy source of truth.
- `external-copy` accepts plain files and `.zst` sources; `.zst` sources are
  decompressed into the configured artifact path during build.

### Valid Enum Values

| Field | Values |
|-------|--------|
| `kind` | `boot`, `kernel`, `bridge-driver`, `user-driver`, `service`, `app`, `compat` |
| `execution_domain` | `kernel`, `user` |
| `startup` | `none`, `init`, `session`, `desktop` |
| `install.layout` | `file`, `directory` |
| `desktop.entries.launch` | `none`, `new-session`, `all-sessions` |

`desktop.entries.no_display=true` is staged into both desktop registries and
the `.desktop` file; it hides discovery without disabling an explicit startup
or launch policy.

### Driver Autoload

- `deps`: required module preloads (Linux `modules.dep` role).
- Missing or skipped `deps` must skip the dependent driver; never autoload a
  hard-dependent `.ko` after its required provider is absent.
- `softdeps`: best-effort preloads (Linux `modprobe.d softdep pre:` role).
- List fields stage as comma-separated registry values; items must not contain tab, newline, carriage return, or comma.
- `linux_driver_names`: Linux in-module driver names allowed to register from that package. If omitted, defaults to autoload `name`. Linux driver compat registration must use this registry field — not hardcoded driver names.
- `provider_group`: mutually exclusive provider contract. Once a loadable/native provider marks the group active, later normal and fallback candidates are skipped before alias probing. `fallback_only` candidates are lower-priority substitutes used only when no provider in the group loaded.
- Retired display preferred-scanout policy flags/width/height are rejected by
  the ring0 module-load broker; driverd/provider state owns scanout selection.
- Driver `class` registry values: `display`, `input`, `network`, `usb`, `storage`. `usb` is reserved for explicit USB compat/dev bridge modules; native xHCI is the RustOS host-controller path and is not staged as a Linux `.ko`.
- `display-primary` group: real hardware/DVM providers are ordered ahead of the
  kernel-owned firmware framebuffer fallback. The fallback is registered during
  GUI bootstrap, never staged as a `.ko`, and is replaced only by a validated
  primary provider.
- `driverd` owns autoload policy. Kernel driver brokers may expose narrow
  hardware-presence primitives for staged aliases (`pci:*`, `virtio:*`) but
  **must not** pick provider order or bypass registry `provider_group` policy.

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

## Fuzzing

- Policy: `config/rustos.toml` `[fuzzing]`.
- `enabled=false` removes `abifuzz.desktop` autostart from the staged image
  and excludes the `abifuzz` package from desktop/runtime launch registries.
- When enabled, `abifuzz` launches after `wayclick` via `runtime_deps` so UI
  smoke gets a chance to connect before ABI fuzzing starts.
- `fd_transfer_stress=true` passes `--fd-transfer-stress` to `abifuzz`; keep it
  separate from default ABI smoke because SCM_RIGHTS stress can perturb UI
  launch diagnostics.

## Runtime Control

- Client crate: `libs/runtime-control`.
- Default socket: `/run/runtimed.sock`.
- Main methods: `snapshot_running_programs`, `request_launch_program_new_session`, `request_terminate_session`, `request_terminate_pid`, `notify_ui_ready`.
- `notify_ui_ready` is one-way: runtimed records readiness without replying,
  so compositor bootstrap never waits on a closed readiness stream.
- Request text max: `MAX_REQUEST_PATH_BYTES`.
- `runtimed` loads the runtime launch catalog on its main loop after UI ready.
  Desktop metadata and runtime-launch policy have separate immutable caches;
  one registry must never satisfy a request for the other. Initial session
  autostart must not depend on a background thread completing before the first
  policy drain.
- `runtimed` spawns uiserver suspended, admits its rootd lease, activates it,
  and waits for the exact PID's display-policy endpoint before committing the
  tracked process.

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
- Lock telemetry policy: `[lock_telemetry]`. `enabled=true` emits
  `rustos_lock_telemetry_enabled` for kernel crates and configures cycle
  thresholds through `RUSTOS_LOCK_TELEMETRY_WARN_WAIT_CYCLES` /
  `RUSTOS_LOCK_TELEMETRY_WARN_HOLD_CYCLES`.
- Initial lock telemetry owner: `kernel/io-manager/src/sync.rs`
  `KernelSpinLock` / `KernelWaitLock`. It warns on contended acquire latency and
  long guard hold time with `lock-telemetry:` debugcon records.

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

## KVM Launch

- `cargo xtask build-dvm` invokes `driver-domains/linux/Makefile`; `verify-dvm`
  validates the manifest schema plus kernel, rootfs, config, source-lock, and
  immutable DVM control-contract SHA-256 values before a KVM guest starts.
- The DVM wrapper fingerprints Buildroot configuration, each local relay
  (`rustos-dvm-agent`, `rustos-dvm-display`, `rustos-dvm-net`), and the rootfs
  overlay separately. Configuration or toolchain changes use `distclean`; a
  relay edit removes only that relay's Buildroot directory, while overlay-only
  changes regenerate only the CPIO image. `make -C driver-domains/linux
  rebuild-{agent,display,net}` are explicit package-only rebuild entrypoints.
- The DVM wrapper requires host `libelf` headers and an unversioned linker
  library. `RUSTOS_DVM_LIBELF_SYSROOT` may name an immutable extracted
  `libelf-dev` package for non-root CI; the wrapper validates and hashes those
  headers/library before it configures Buildroot.
- `cargo xtask kvm-smoke` requires `qemu-system-x86_64` and `/dev/kvm`, makes a
  private writable copy of `build/rustos-boot.img`, restricts `build/kvm/` to
  the launch user, and uses the repository-pinned `OVMF_PATH` with QEMU. It
  starts RustOS and Linux DVM as independent KVM guests and preserves their
  logs under `build/kvm/`.
- A successful smoke requires RustOS's default `rootd` readiness marker and
  the L0 host broker to complete the DVM
  `health,device-inventory,driver-inventory,input-stream` handshake using launch-assigned KVM
  vsock CID `4`. L0 then opens RustOS's dedicated QEMU COM2 socket and writes
  only fixed RDI2 session/key/pointer frames; QMP is not launched. The DVM
  discovers one keyboard and one relative pointer by evdev capabilities, not
  by a QEMU product name. A guest that exits or merely starts cannot pass. The
  smoke proves endpoint setup only: it does not fabricate an input event. A
  live event needs a real input source assigned to the DVM, after which L0
  range-checks each key/pointer field and RustOS feeds it to ring-3 `inputd`.
- `cargo xtask kvm-smoke --exercise-input` is a separate bounded integration
  mode. It passes only the DVM boot flag `rustos.dvm.input-selftest=1`; the
  agent creates a local `uinput` evdev device and reconsumes it through the
  same relay before RustOS requires the one-shot `inputd` keyboard and pointer
  ingress markers. No QMP socket, host-to-DVM input RPC, or production default
  path is added.
- `agent-v1-control` remains DVM-to-L0 only. The RDI2 COM2 channel is a
  bounded input relay, not a general vsock endpoint or a NIC/block/GPU data
  plane. L0 limits event rate, emits held-key/button releases on disconnect,
  and sends a session end so reconnects cannot inherit input state. Additional
  `--expect` markers tighten RustOS proof; none prove a
  `.ko`, PCI assignment, physical input capture, or a network route.
- `cargo xtask kvm-smoke --min-ui-fps N` modifies only the runner's fresh
  private FAT disk copy: the staged `uiserver.desktop` carries the
  equal-length disabled anchor `RUSTOS_UI_PROFILE=0`, which is changed to `1`
  without changing its extent length. `uiserver` emits one-second profile
  windows with integer `frame_hz_milli`; the runner requires `N * 1000`.
  Release images retain the disabled value.
- `cargo xtask kvm-smoke --dvm-display-shmem` replaces RustOS's direct test
  GPU with a private host-created `ivshmem-plain` aperture. RustOS maps only a
  fixed validated header and publishes it as a primary framebuffer; the
  separate Linux `rustos-dvm-display` service reads that aperture and uses its
  own DRM/KMS double-buffered page flips. The gate requires RustOS's observed
  display ABI to match the exact header and the DVM's active DRM relay marker.
  This KVM topology proves the display transport; it is not PCI passthrough or
  a physical-GPU assignment contract.
- The default KVM image does not stage any `system/drivers/*.ko` artifact; the
  manifest loader rejects one before image construction. RustOS virtio-GPU and
  virtio-net `.ko` paths are removed; physical input artifacts require
  `legacy-input-compat`. The kernel registers the bounded early-boot firmware
  framebuffer directly until the DVM display provider is available.
- `cargo xtask kvm-smoke --dvm-network-shmem` adds a host-created fixed 64-slot
  Ethernet aperture. RustOS may map only the validated header and fixed slots;
  it never follows DVM descriptors or allocations. Linux's `rustos-dvm-net`
  relay owns the DVM virtio-net NIC, while RustOS `netd` keeps socket namespace,
  TCP, and route policy. `--exercise-network` uses the private KVM copy of
  `netprobe` against the QEMU gateway and requires both ring directions to
  advance within their 64-slot invariants. This proves the KVM data transport,
  not physical NIC passthrough, reset, DMA, or revocation policy.
- The DVM `S48rustos-dvm-net` init service owns the display and network relay
  PIDs. Start is idempotent; stop releases the display relay first, waits at
  most 20 seconds for each process, then escalates only that recorded PID.
  Each relay may retry a transient device-readiness error internally, but a
  restart must never leave a second framebuffer consumer or Ethernet producer.
- `rustos-hostd discover` and `rustos-hostd preflight --plan ...` are L0
  read-only ownership gates. `launch-plan-v1.env` must explicitly enumerate
  every function in one actual IOMMU group and reject host-protected BDFs;
  neither command performs a driver unbind, VFIO bind, device reset, guest
  launch, or PCI assignment. Do not turn a successful preflight into a
  passthrough claim.
- `rustos-hostd relay-input` requires a matching `driver-domain-policy-v1`
  file. Each class has an explicit transport; only `INPUT_TRANSPORT=rdi2-com2`
  is implemented. Network, block, and display must remain `disabled` until
  their separate queue/DMA/reset/revocation contracts are implemented. The
  normal command is a reconnecting L0 service; `--once` is diagnostics-only.
- `rustos-hostd acquire` remains read-only unless both `--activate` and
  `--allow-unsigned-test-bind` are supplied. That laboratory-only path first
  persists an owner-private `prepared` lease with each original PCI driver and
  `driver_override`, then binds the whole preflighted group to `vfio-pci` and
  atomically marks it active. Reverse-order rollback is mandatory on failure;
  failed acquisition retains the prepared record, and `release --activate`
  restores either prepared or active records and deletes them only after
  success. Do not enable ordinary activation until a release
  manifest cryptographically binds the validated plan to the DVM artifacts.

## Fault Injection

- Human guide: `docs/fault-injection.md`.
- Shared parser: `libs/rustos-fault-injection`.
- Host config: `tools/xtask/src/config/project.rs` `[fault_injection]`.
- KVM smoke passes enabled fault rules through QEMU as
  fw_cfg `opt/rustos/fault-injection`. Production transport remains separate
  from this development-only fault channel.
- Kernel runtime: `kernel/nucleus-core/src/util/fault_injection.rs`.

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
- The legacy-only kernel broker validates `.ko` images, relocates them, exposes
  `DriverKernelApiV1`, maps MMIO, and executes module init. The default image
  cannot stage a bridge-driver `.ko`; new device support belongs in a bounded
  driver domain rather than this compatibility path.
- Deleted io-manager policy files are not source of truth — do not restore.
- Linux compat symbols must be explicitly implemented; no broad no-op fallbacks.
- Vendor NVMe host `.ko` packages stay out of the default profile until block-layer/auth/io_uring compat is explicit; native RustOS NVMe remains the default boot/storage provider.
- RustOS virtio-GPU `.ko` artifacts and their in-kernel scanout/DRM shim are
  removed. Default KVM uses the isolated Linux DVM DRM/KMS relay; do not
  reintroduce either path or count it as a ring3 service-driver migration
  target.
- RustOS's virtio-net `.ko` shim is removed. The KVM DVM route is a fixed
  Ethernet-frame transport and `netd` remains the default network policy
  owner; it does not authorize physical NIC passthrough.
- Vendor HID core `.ko` stays out of the default profile while USB HID leaf modules are disabled; native RustOS input remains the default boot input provider.
- Hardware-specific display drivers such as AMDGPU stay out of the default KVM
  profile unless the assigned hardware matches; use an explicit hardware
  profile instead.
- Native xHCI and HID interrupt polling are always on for USB input/display probes.
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
