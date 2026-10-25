# AI Contracts — Infrastructure

Package/stage schemas, runtime control, kernel API, build, fault injection, logging, docs. For service/IPC/broker ABI: `contracts-abi.md`.

## Repository Execution Infrastructure

- `tools/check-dev-environment.sh` is the read-only prerequisite diagnosis
  entrypoint. Its optional modes correspond to AI, documentation, formal,
  physical-GPU, and release work; it never installs tools or mutates the host.
- `.mcp.json` owns the project MCP launcher (`serena start-mcp-server`).
  `agent-tools-bootstrap.yml` pins and verifies the installed Serena version;
  the launcher itself does not select a package version. Version changes are
  reviewed inputs, not an implicit update at agent startup.
  Serena is the project semantic source-navigation MCP. Raw text search uses
  local `rg`; neither source-navigation path is OS acceptance evidence.
- `session-handoff.md` owns volatile continuation state. It is kept out of the
  stable prompt prefix and must not duplicate durable contracts or gate output.
- `tools/agent/check-ai-context-contract.sh` is the executable low-context
  contract. It caps stable/startup context, rejects redundant MCP/tool-policy
  drift, and prevents read/output ceilings from silently increasing. Claude Code
  commit gating, the remote-agent commit stage, and lightweight CI all run it;
  relaxing a ceiling or adding a stable-prefix file/MCP is an explicit contract
  change rather than incidental infrastructure cleanup.
- GitHub Actions use a fixed Ubuntu image, commit-pinned actions, bounded job
  timeouts, and pull-request concurrency cancellation. The check job validates
  shell syntax and the hook contract before compiling RustOS.
- The Rust post-edit hook may reuse success only for an identical hash of all
  staged, unstaged, and untracked content in the same canonical worktree.
  Elapsed time alone is never evidence that a later edit passed.

## Package Manifest

- File: `RUSTOS.package.toml`. Parser: `tools/xtask/src/package_manifest.rs`.
- Package ids = stable dependency keys. `runtime_deps` references package `id`, not path or desktop id.
- Manifest parsing is fail-closed: unknown top-level or nested fields fail the
  build. Retired `[boot]` metadata is not accepted; rootd/initd and generated
  registries are the boot-policy source of truth.

### Valid Enum Values

| Field | Values |
|-------|--------|
| `kind` | `kernel`, `service`, `app`, `compat` |
| `execution_domain` | `kernel`, `user` |
| `startup` | `none`, `init`, `session`, `desktop` |
| `install.layout` | `file`, `directory` |
| `desktop.entries.launch` | `none`, `new-session`, `all-sessions` |

`desktop.entries.no_display=true` is staged into both desktop registries and
the `.desktop` file; it hides discovery without disabling an explicit startup
or launch policy.

### Driver-domain policy

- RustOS package manifests do not describe Linux modules or direct hardware
  drivers. `bridge-driver` and `module-image` are invalid manifest values.
- Linux DVM build inputs live under `driver-domains/linux/` and are verified by
  `cargo xtask build-dvm` / `verify-dvm`, outside the RustOS package registry.
- RustOS accepts fixed DVM input, display, and Ethernet transports only. If a
  transport is absent or invalid, that device is unavailable; no native or
  firmware fallback is selected.

## Stage Outputs

| Path | Purpose |
|------|---------|
| `build/image` | Boot image root |
| `build/artifacts` | Artifact root |
| `assets/image` | Static overlay |
| `build/image/EFI/BOOT/BOOTX64.EFI` | UEFI entry (GRUB-generated) |
| `build/image/nucleus.elf.sig` | Kernel payload signature |

### Boot entropy

- The GRUB Multiboot2 handoff must populate `BootInfo.rng_seed` with 256 bits
  obtained from CPU `RDSEED`, falling back to bounded `RDRAND` retries only
  when necessary. An all-zero seed or a CPU exposing neither instruction is a
  terminal boot-admission failure; deterministic PID/time/counter mixing is
  not a fallback.
- `BootInfo::validate_staged`, the Multiboot2 adapter, and `boot-random::init`
  independently reject an unavailable seed before userspace or any
  capability-minting path becomes live.

### Registries

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

### Formal verification infrastructure

- `formal/models.tsv` is the only TLA model inventory. Run
  `bash formal/selftest.sh`; never hand-maintain a second model list. The
  selftest requires both primary `.tla` sources and `.cfg` files to match the
  registry exactly; only the separately registered Apalache pilot directory is
  excluded from that primary-source comparison. Every registered model must
  also have a direct entry in `formal/README.md`, `formal/COVERAGE.md`, and
  `formal/CONFORMANCE.md` so execution, failed-gate ownership, and source
  correspondence cannot drift independently.
- PR evidence is `bash formal/verify-all.sh --profile pr`: exhaustive finite
  TLC, non-vacuous Kani, Verus, concrete runtime trace replay, native
  Linux/Windows ABI reference comparison, a bounded restart/crash-consistency
  matrix, and source-level implementation mutation sensitivity.
- `formal/system-flows.tsv` is the executable cross-subsystem requirement and
  hazard graph. `formal/check-system-flows.sh` rejects duplicate identities,
  missing terminal paths, unbounded timeout edges, unregistered models,
  missing source witnesses, and any direct RustOS `.ko` lifecycle exception.
  `formal/selftest.sh` runs it before model execution.
- `formal/check-performance-contracts.sh` binds the shared source limits to
  typed compat IPC, single-attempt service publication, one-turn VFS recovery,
  and the temporary absence of an independent KVM boot deadline during SMP
  qualification. It runs from `selftest.sh`;
  widening a deadline, restoring an unclassified 30-second helper, or wrapping
  endpoint publication in a retry loop is a contract failure before TLC.
- Nightly evidence is `bash formal/verify-all.sh --profile nightly`: the PR
  tier plus selected fixed-seed simulation, Miri, Loom, Apalache, TLAPS, and
  bounded Rust/C libFuzzer lanes, plus pinned address/thread instrumentation
  over the registered host-testable critical/high boundaries. Simulation,
  instrumentation, and fuzzing are bug finding, not exhaustive proof.
- `formal/{sanitizer-targets,recovery-scenarios,implementation-mutations}.tsv`
  are executable inventories, not coverage prose. Each runner rejects missing
  source witnesses, unsupported classes, zero-test filters, unbounded
  deadlines, compile-only mutant failures, and stale registry anchors.
- Proof claims are configuration-scoped. `formal/proof-assumptions.tsv` names
  the assembly, hardware, boot, DMA, toolchain, external-kernel, tracing,
  hypervisor, physical-hardware, and side-channel assumptions below the current
  evidence. `formal/verified-configurations.tsv` binds positive and negative
  QEMU evidence to their exact topology and explicitly excludes physical
  hardware. A model pass cannot be reported as covering an absent assumption
  or configuration.
- Every `intentional-terminal` model must retain its reason in the registry.
  Every `temporal` model must configure `SPECIFICATION Spec`; direct
  `INIT`/`NEXT` configuration bypasses the fairness formula and is rejected by
  `formal/selftest.sh`.
  TLC's `-deadlock` option disables deadlock checking; never describe it as
  enabling the check. TLC 1.7.4 action coverage is `distinct-successors:evaluations`, so
  `0:N` is exercised convergence and only an evaluation count of zero fails.
- Stable summaries, SARIF, normalized counterexamples, runtime traces, and
  solver output belong under `build/formal/`. Solver caches and `_apalache-out`
  directories are ignored and must not be committed.
- A pilot flag in `models.tsv` must resolve to an executable proof/trace file.
  `formal/COVERAGE.md` owns the explicit gap between these pilots and a
  certification-grade assurance case.

## Runtime Control

- Client crate: `libs/runtime-control`.
- Wire protocol: `libs/runtime-control/src/protocol.rs` is its **only**
  definition. Runtimed consumes that module; it must not redeclare an opcode,
  a target kind, a capacity, or either frame struct. The two used to be
  separate private copies with nothing comparing them, so a divergence would
  have surfaced as a runtime `EPROTO` rather than a build failure.
- Default socket: `/run/runtimed.sock`.
- Main methods: `snapshot_running_programs`, `watch_running_programs`,
  `request_launch_program_new_session`, `request_terminate_session`,
  `request_terminate_pid`, `notify_ui_ready`.
- `notify_ui_ready` is one-way: runtimed records readiness without replying,
  so compositor bootstrap never waits on a closed readiness stream.
- `watch_running_programs` is `snapshot_running_programs` with the reply
  withheld until the running set stops matching the digest the caller passes
  back. It is how uiserver's taskbar and sessiond's restart policy observe
  launches and exits; neither polls a snapshot on a timer. Callers pass
  `RUNNING_PROGRAMS_DIGEST_UNKNOWN` on the first call to be answered
  immediately, and recompute the next digest from the array they receive - the
  digest itself never crosses the socket. An unchanged digest means the server
  re-armed, which is liveness, not an error.
- For request/reply operations, a successful `RuntimeResponse` must echo the
  exact request opcode. Only ops for which `op_carries_program_payload` holds -
  `OP_SNAPSHOT_RUNNING_PROGRAMS` and `OP_WATCH_RUNNING_PROGRAMS` - may carry a
  count, which is capped at `MAX_RUNTIME_PROGRAMS`; command replies have zero
  count. Parking changes when a watch is answered, never what it may answer
  with. After a current-version check, negative status is the server-error
  envelope (whose opcode may be zero), but positive or `i32::MIN` status is
  malformed and fails closed as `EPROTO`.
- Request text max: `MAX_REQUEST_PATH_BYTES`.
- `runtimed` loads the runtime launch catalog on its main loop after UI ready.
  Desktop metadata and runtime-launch policy have separate immutable caches;
  one registry must never satisfy a request for the other. Initial session
  autostart must not depend on a background thread completing before the first
  policy drain. Accepted nonblocking clients are retained in a bounded partial
  request set and serviced incrementally; a lower-class client preempted after
  `connect()` cannot make the supervisor busy-yield or delay catalog-policy
  convergence.
- `runtimed` spawns uiserver suspended, admits its rootd lease, activates it,
  and waits for the exact PID's display-policy endpoint before committing the
  tracked process.
- `initd` admits `runtimed` as display-critical interactive work. Synchronous
  IPC donation therefore carries the UI bootstrap priority through
  `loaderd`, `syscalld`, and `procd`; background service traffic must not leave
  a valid DVM scanout stuck indefinitely on its local bootstrap frame.

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

Scheduler-aware wait users should use `kernel_ps::api::{current_task_id, arm_block_current_task, commit_block_current_task_and_yield, wake_task}`. The legacy direct `block_current_task`/`block_current_user_task` entry points do not exist; do not reintroduce them.

## Kernel Build

- Kernel-target Cargo invocations route through `tools/xtask/src/build/cargo.rs::kernel_rustflags_env`.
- The boot nucleus deliberately compiles through Cargo's
  `x86_64-unknown-linux-gnu` target to preserve the required object/link shape.
  Bare-metal-only code must therefore use `cfg(rustos_boot_image)`, never
  `cfg(target_os = "none")`; `formal/run-source-conformance.sh` rejects that
  dead-branch pattern under `kernel/`.
- Operational config: `config/rustos.toml`. Build-shape defaults:
  `[kernel.build]`. Set `RUSTOS_CONFIG` to test an alternate complete config.
- Lock telemetry policy: `[lock_telemetry]`. `enabled=true` emits
  `rustos_lock_telemetry_enabled` for kernel crates and configures cycle
  thresholds through `RUSTOS_LOCK_TELEMETRY_WARN_WAIT_CYCLES` /
  `RUSTOS_LOCK_TELEMETRY_WARN_HOLD_CYCLES`.
- Raw critical sections use the shared lock-class implementation in
  `kernel/nucleus-core/src/util/lockdep.rs`; process state, MM page tables and
  physical allocation, service registries, futex/input/RTC waiters, and IPC
  slabs have distinct classes. Scheduler-aware blocking sections use
  `kernel/io-manager/src/sync.rs::KernelWaitLock`. Both paths report bounded
  wait/hold diagnostics without allocating from the diagnostic path. IDT
  leaves explicitly enter IRQ context, and lockdep rejects a class used as both
  IRQ-safe and ordinary interrupt-enabled plus every safe-to-unsafe dependency
  path. A `KernelWaitLock` acquisition is rejected in IRQ context or while any
  tracked raw-spin class is held. Every `KernelWaitLock` instance has a stable
  class in the same allocation-free dependency graph. Its held-class stack is
  keyed by scheduler task identity across a blocking handoff; inverse
  sleepable ordering is rejected. Raw-to-blocking nesting is rejected;
  sleepable-to-raw leaf acquisition is dependency-tracked and cycle-checked,
  including successful nonblocking external try-locks.
- Every boot-image `TrackedSpinLock` guard increments the acquisition CPU's
  task-preemption depth without masking unrelated device interrupts. It
  captures the dense CPU index and architectural APIC identity; release
  validates both plus positive nesting depth before unlocking. The software
  scheduler rejects every handoff while that CPU's depth is non-zero. Code
  under such a guard remains bounded, non-blocking, and allocation-free; a lock
  shared with an IRQ leaf must also wrap process-context access in
  `without_interrupts`. Per-CPU current-task publication and raw-spin stacks
  are mandatory SMP authority, while task-owned sleepable stacks remain
  identity-scoped. `scheduler-cpu-ownership` is the executable refinement.
- `boot-random` sits below `nucleus-core`, so its one master-seed lock cannot
  use `TrackedSpinLock`. Its only critical section derives one 32-byte child
  seed under local IRQ exclusion; no caller may hold the raw master lock across
  a preemptible kernel frame.

### Default Kernel `RUSTFLAGS`

`--cfg rustos_boot_image`, `-C no-redzone`, `-C codegen-units=1`, `-C opt-level=2`, `-C overflow-checks=true`, `-C debug-assertions=false`, `-C debuginfo=0`, `-C panic=abort`, `-C relocation-model=pic`.

Kernel KASLR requires `"pic"` or `"pie"` for every kernel crate; other
`relocation_model` values are rejected. The nucleus links as static PIE without
an interpreter, at the fixed physical 2 MiB bootstrap address. Its final image
alias is independently randomized, not the fixed direct map. The AP trampoline
retains its hand-written physical relocation. See [Kernel KASLR](kernel-kaslr.md)
for relocation, alias-permission, entropy, and debug-witness contracts.

### Build-Shape Knobs

- `RUSTOS_KERNEL_CODEGEN_UNITS` overrides kernel codegen unit count for experiments without changing userspace builds. Deprecated alias: `KERNEL_CODEGEN_UNITS`. Sweep range: `1..=256`.
- Other knobs: `lto`, `force_frame_pointers`, `incremental` (applied as `CARGO_INCREMENTAL` on kernel Cargo invocations), `debuginfo`, `embed_bitcode`, `panic`, `relocation_model`, `strip`, `extra_rustflags`.
- `embed_bitcode=true` required when `lto` ∈ {`thin`, `fat`}.
- The repository does not mandate a Cargo `rustc-wrapper`: an optional cache
  cannot be an F5 or xtask availability dependency. Developers may opt in
  through `RUSTC_WRAPPER=sccache`; kernel invocations then disable it because
  kernel build-std/LTO flag probes are not accepted by sccache.

### Config & Module Loader

- `cargo xtask config check` validates effective config.
- `cargo xtask config show` prints effective kernel build config.
- RustOS has no loadable kernel-module code-generation matrix. Linux DVM
  modules are built and signed only by the DVM build plan; RustOS release
  validation instead proves the retired module syscalls remain `ENOSYS` across
  supported optimization profiles.
- Linux compat load failures: write first disallowed/unresolved external symbol to debugcon directly. **Do not** rely only on category-filtered logs for module ABI diagnostics.

## KVM Launch

See [KVM launch and device ownership contract](infra/kvm.md#kvm-launch).

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

### Current Fault Points

The closed registry is `formal/fault-scenarios.tsv` and currently contains
`alloc.frame`, `block.read`, `block.write`, `block.flush`,
`display.present`, `display.provider.register`, `handle.commit`,
`handle.reserve`, `ipc.endpoint.enqueue`, `ipc.endpoint.reply`,
`process.spawn`, and `waitset.register`.
Configuration and guest admission reject unknown, retired, and duplicate
points. Production `source` and executable `witness_source` are separate,
mandatory registry fields and are both validated. Normal product configuration
keeps injection disabled; an
`RUSTOS_FAULTS` override replaces the matching `off` rule instead of appending
a shadowed duplicate. Only `block.flush` currently claims a negative KVM
acceptance profile; the registry labels the remaining runtime gaps explicitly.

Add new points only at realistic failure boundaries: allocation, block IO,
device registration, queue submit, or process spawn. Ring3-owned input/network
policy and DVM-owned hardware drivers need an owner-local fault channel and
must not be advertised as kernel points before that channel exists. **Do not
scatter fault checks through arbitrary helper functions.**

`config/rustos.toml` may use normal TOML formatting for fault rules (including multiline arrays); logging extraction must ignore non-logging sections.

## Network and driver boundary

- `netd` owns socket policy. `kernel/io-manager/src/network/mod.rs` is only a
  DVM fixed-frame transport facade and must never enumerate or initialize a
  virtio NIC directly.
- The DVM owns Linux driver lifecycle, DRM/KMS, evdev, and virtio-net. RustOS
  validates transport headers and bounds, then fails closed on transport loss.
- Deleted direct-driver sources are not a source of truth and must not be
  restored as a recovery path.

## Logging

- Policy: `config/rustos.toml` `[logging]`.
- Parser/cfg emitter: `tools/build_log_cfg.rs`.
- Canonical categories: `libs/rustos-observability/src/lib.rs`.
- Config is mostly build-time cfg — rebuild after changes.
- Kernel macros: `crate::debug::{trace,debug,info,warn,error}`.
- Userspace macros: `observability_client::{trace,debug,info,warn,error}`.
- Synchronous `SYS_RUSTOS_DEBUG_PRINT` is a console sink, not a free tracing
  buffer. Success-path phase logs inside loader/service loops must use a
  `RUSTOS_LOGGING_BOOT_TRACE_ENABLED` gate; commercial defaults compile them
  out. Terminal failure diagnostics, stable acceptance milestones, and one
  bounded completion marker remain unconditional.
- Repeated recoverable warnings use one-time or rate-limited publication with
  an aggregate counter. Never let console traffic extend an IPC deadline or
  become a boot scheduling dependency.

## Docs

- Human docs bilingual; English first; mandatory language jump links.
- AI docs English-only and compact.
- mdBook nav source: `docs/SUMMARY.md`. mdBook config: `book.toml`. Output: `build/mdbook`.
- Mandatory token policy: `docs/ai/token-policy.md`.
