# AI Contracts — Infrastructure

Package/stage schemas, runtime control, kernel API, build, fault injection, logging, docs. For service/IPC/broker ABI: `contracts-abi.md`.

## Repository Execution Infrastructure

- `tools/check-dev-environment.sh` is the read-only prerequisite diagnosis
  entrypoint. Its optional modes correspond to AI, documentation, formal,
  physical-GPU, and release work; it never installs tools or mutates the host.
- Project MCP launchers in `.codex/config.toml` use exact package versions.
  Version changes are reviewed inputs, not an implicit update at agent startup.
  Serena and ripgrep are scoped discovery accelerators; their absence falls
  back to local `rg` and is never OS acceptance evidence.
- `session-handoff.md` owns volatile continuation state. It is kept out of the
  stable prompt prefix and must not duplicate durable contracts or gate output.
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
  and the independent KVM five-second UI gate. It runs from `selftest.sh`;
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
  enabling the check. TLC 1.7.4 action coverage is `new-states:evaluations`, so
  `0:N` is exercised convergence and only an evaluation count of zero fails.
- Stable summaries, SARIF, normalized counterexamples, runtime traces, and
  solver output belong under `build/formal/`. Solver caches and `_apalache-out`
  directories are ignored and must not be committed.
- A pilot flag in `models.tsv` must resolve to an executable proof/trace file.
  `formal/COVERAGE.md` owns the explicit gap between these pilots and a
  certification-grade assurance case.

## Runtime Control

- Client crate: `libs/runtime-control`.
- Default socket: `/run/runtimed.sock`.
- Main methods: `snapshot_running_programs`, `request_launch_program_new_session`, `request_terminate_session`, `request_terminate_pid`, `notify_ui_ready`.
- `notify_ui_ready` is one-way: runtimed records readiness without replying,
  so compositor bootstrap never waits on a closed readiness stream.
- For request/reply operations, a successful `RuntimeResponse` must echo the
  exact request opcode. Only `OP_SNAPSHOT_RUNNING_PROGRAMS` may carry a count,
  which is capped at `MAX_RUNTIME_PROGRAMS`; command replies have zero count.
  After a current-version check, negative status is the server-error envelope
  (whose opcode may be zero), but positive or `i32::MIN` status is malformed
  and fails closed as `EPROTO`.
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

Scheduler-aware wait users should use `kernel_ps::api::{current_task_id, block_current_task, wake_task}`. The `current_user_id`, `block_current_user_task`, `wake_user_task` wrappers are userspace-task helpers — **not** general kernel wait primitives.

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
- While the product scheduler is BSP-only, every boot-image
  `TrackedSpinLock` guard increments one task-preemption depth without masking
  unrelated device interrupts. The software scheduler rejects every handoff
  while that depth is non-zero. Code under such a guard must still be bounded,
  non-blocking, and allocation-free; a lock shared with an IRQ leaf must also
  wrap its process-context access in `without_interrupts`. SMP enablement is
  gated on per-CPU preemption accounting, current-task publication, and
  raw-spin stacks; task-owned sleepable stacks are already identity-scoped.
- `boot-random` sits below `nucleus-core`, so its one master-seed lock cannot
  use `TrackedSpinLock`. Its only critical section derives one 32-byte child
  seed under local IRQ exclusion; no caller may hold the raw master lock across
  a preemptible kernel frame.

### Default Kernel `RUSTFLAGS`

`--cfg rustos_boot_image`, `-C no-redzone`, `-C codegen-units=1`, `-C opt-level=2`, `-C overflow-checks=true`, `-C debug-assertions=false`, `-C debuginfo=0`, `-C panic=abort`.

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

- `cargo xtask build-dvm` invokes `driver-domains/linux/Makefile`; the build
  cryptographically verifies every installed module's detached PKCS#7 payload
  against the generated X.509 certificate. `verify-dvm` validates schema 9 plus
  kernel, rootfs, Buildroot and kernel configs, signing certificate, source
  lock, and immutable DVM control-contract SHA-256 values before a KVM guest
  starts.
- Each KVM launch creates a fresh 256-bit DVM control secret in the L0 runtime
  directory (directory `0700`, file `0600`) and injects it as QEMU fw_cfg.
  The Linux agent may read that value only through fw_cfg's root-only `raw`
  attribute. The static contract and vsock CID select the expected DVM, but
  `dvm-agent-hmac-sha256-v1` must prove the fresh challenge plus exact HELLO
  before L0 writes `WELCOME` or allows a probe/input relay. `rustos-hostd
  probe` and `relay-input` therefore require `--control-secret` from the same
  owner-private launch material. Do not put the secret on a kernel command
  line, image filesystem, manifest, log, or service environment.
- The DVM wrapper exposes its next mutation through read-only `build-plan`.
  Buildroot/toolchain identity or an unsafe complete `BR2_*` transition is the
  full-output lane. Linux source/config and host kernel-build headers select
  Linux plus every enabled out-of-tree signed module and the rootfs. A local
  relay edit removes only that relay's Buildroot directory. Overlay, post-build,
  and AMD firmware-policy changes prune retired owned files and regenerate the
  CPIO image. This matches Buildroot's explicit warning that it does not infer
  all configuration dependencies and Linux kbuild's requirement that external
  modules use the exact prepared/built kernel configuration and symbol data.
  `rebuild-{agent,display,net}` remain explicit package-only integration
  entrypoints.
- Configuration identity stamps are written after successful Kconfig
  reconciliation so an interrupted build can resume. Kernel, relay, overlay,
  and release-input stamps are committed only after configuration, module
  signature, manifest, and artifact verification succeeds. After an
  interruption or ordinary compile failure, rerun the same wrapper target;
  never erase the partial output as a first response.
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
  `health,device-inventory,driver-inventory,display-evidence-v2,block-evidence-v1,input-stream` handshake using launch-assigned KVM
  vsock CID `4`. L0 then attaches as the fixed ivshmem peer 1 only after
  RustOS has claimed peer 0, writes only fixed RDI3 session/key/pointer frames
  into the host-owned 128 KiB input ring, and signals the one RustOS MSI-X
  eventfd at most once per inputd-published consumer wake generation; QMP is
  not launched and the DVM never maps that aperture. The DVM
  discovers one keyboard and one relative or absolute pointer by evdev capabilities, not
  by a QEMU product name. A guest that exits or merely starts cannot pass. The
  smoke proves endpoint setup only: it does not fabricate an input event. A
  live event needs a real input source assigned to the DVM, after which L0
  range-checks each key/pointer field and RustOS feeds it to ring-3 `inputd`.
- `cargo xtask kvm-smoke --exercise-input` is a separate bounded integration
  mode. It passes only the DVM boot flag `rustos.dvm.input-selftest=1`; the
  agent creates a local `uinput` evdev device and reconsumes it through the
  same relay before RustOS requires the one-shot `inputd` keyboard and pointer
  ingress markers. The composite device advertises pointer selection capability,
  emits one non-printable F12 keyboard proof, then emits pointer-only absolute positions;
  it never emits printable keys or clicks. It traces a 192-pixel axis-aligned
  square for 6,000 cycles (a bounded 90-second source window that covers the
  public 60-second gate plus guest admission), with source polling no faster
  than 15 ms. After L0 authenticates and requests the input stream, the agent
  admits only that live streaming interval to guest `SCHED_RR` priority 10 and
  first installs a 50 ms soft/100 ms hard `RLIMIT_RTTIME` continuous-CPU
  ceiling. It reads the policy and priority back, fails the stream closed if
  admission fails, and restores the prior policy/limit on exit; a runaway
  relay therefore cannot indefinitely starve DVM KMS or recovery. Both the
  KVM runner and physical supervisor boot this verified
  `CONFIG_PREEMPT_DYNAMIC` kernel with `preempt=full`; artifact verification
  also requires high-resolution timers and `CONFIG_HZ_1000`. The separate
  GPU/KMS relay stays under the normal policy through device setup and enters
  `SCHED_RR` priority 9 only after the host invitation is confirmed. It uses
  the same 50/100 ms continuous-CPU ceiling, verifies the installed policy,
  and restores it on every relay exit; display therefore cannot outrank the
  priority-10 input relay or retain realtime authority while retrying. A
  `--min-ui-fps` proof requires three consecutive active one-second uiserver
  and DVM windows at the requested rate. WayClick must reach the requested
  exact aggregate callback and commit rate across the same contiguous window
  count, every constituent window must reach at least 80% of that rate, and
  every commit/callback/release count must balance. Across the same contiguous
  uiserver input windows, exact aggregate rates must reach 55 accepted
  events/s and 50 presented cursor moves/s, and every constituent window must
  retain at least 80% of each rate. It also requires zero
  loss/slow/error/backlog, at most 50 ms input
  gap/age, exact logical/presented cursor agreement, and at least 96 pixels of
  travel on both axes. An absent or dropped sample fails the gate. No QMP
  socket, host-to-DVM input RPC, or production default path is added.
- `agent-v1-control` remains DVM-to-L0 only. The RDI3 input ring is a bounded
  L0-to-RustOS relay, not a general vsock endpoint or a NIC/block/GPU data
  plane. L0 limits event rate, emits held-key/button releases on disconnect,
  and sends a session end so reconnects cannot inherit input state. RustOS
  validates the header and arms exactly one MSI-X leaf before L0 requests the
  input stream. The leaf only wakes waiters; the capability-gated `inputd`
  ingest broker drains at most 256 complete frames per normal turn, so decoding
  and policy never run in IRQ context. The DVM coalesces relative pointer
  samples at a 5 ms interval; an absolute device publishes only complete,
  changed `SYN_REPORT` positions and keeps button edges report-atomic. L0 caps the relay at
  256 frames/s to preserve cleanup reserve. Additional
  `--expect` markers tighten RustOS proof; none prove PCI assignment, physical
  input capture, or a network route.
- `cargo xtask kvm-smoke --min-ui-fps N` modifies only the runner's fresh
  private FAT disk copy: the staged `uiserver.desktop` carries the
  equal-length disabled anchor `RUSTOS_UI_PROFILE=0`, which is changed to `1`
  without changing its extent length. `uiserver` emits one-second profile
  windows with integer `frame_hz_milli`; the runner requires `N * 1000`.
  WayClick publishes exact counts and elapsed milliseconds, so its aggregate
  acceptance is calculated from totals instead of rounded, phase-sensitive
  one-second sample rates; the 80% per-window floor and 50 ms maximum callback
  gap prevent bursts from masking a stall.
  Release images retain the disabled value.
- Every KVM smoke now attaches `virtio-gpu-gl-pci` to the Linux DVM and uses
  virgl through the exact AMD `/dev/dri/renderD128` host render node. The
  launcher rejects a symlink, non-character device, non-`0x1002` vendor, or
  non-`amdgpu` driver before QEMU starts. Headless runs use
  `egl-headless,rendernode=/dev/dri/renderD128`; interactive runs use GTK with
  GL enabled. The DVM fixed-command probe accepts only the `virtio_gpu` or
  `amdgpu` DRM driver, rejects llvmpipe/softpipe/swrast, executes clear,
  solid-quad, and transformed textured-quad through built-in GLES shaders,
  fences every completion, verifies pixels plus a nonzero frame hash, and
  keeps a one-second health submission alive. Before frame admission it runs
  one built-in textured draw behind a separate GPU fence, rejects context
  setup over 500,000 us, and reports the measured prime duration; this keeps
  one-time shader/pipeline translation outside the per-frame SLA without
  making it unbounded. The runner requires its virgl
  marker to report at least 120 frames and 60,000 mHz with maximum GPU fence
  completion at most 16,667 us. This is an execution-engine proof only: its
  required `public-abi=0 ui-connected=0 scanout=0` fields keep the private
  submit ABI, RustOS scene connection, KMS output handoff, foreign DMA-BUF
  import, and physical AMD evidence as explicit failed gates.
- `cargo xtask kvm-smoke --gui-dvm-surfaces` exercises the production V3
  `RSGUI002` three-slot GUI-DVM pool. The launch-local, owner-private
  `ivshmem-doorbell` broker gives exactly the RustOS compositor and GUI DVM one
  host-to-DVM wake vector and exactly two host receive vector meanings
  (`control`, `offline`) plus the fixed validated UC control plane; it rejects
  other UIDs and peers. A separate 128 MiB `virtio-pmem` pixel device is writable
  in RustOS QEMU and read-only/ROM in the DVM. The KVM runner creates its pixel
  backing in a private tmpfs directory and preallocates it from the RustOS QEMU
  before attaching VFIO in the DVM. Commercial hostd likewise requires an exact
  128 MiB owner-private tmpfs/hugetlbfs file and preallocates every page before
  reset or launch. This is a real IOMMUFD prerequisite: QEMU 10.2.1 on the
  measured host rejected the former repository-local ext4 device-memory mapping
  at IOVA `0x100000000` with `EINVAL`. After moving that backing to tmpfs, the
  next physical run advanced to a distinct `0xf0000000`/128 MiB failure: QEMU
  tried to admit an mmap-able AMD PCI BAR as an IOMMUFD peer-to-peer DMA region,
  which that interface does not support. The explicitly non-commercial runner
  disables VFIO BAR mmap and the ROM BAR to get past this diagnostic boot
  blocker, but that slower-MMIO configuration cannot prove the commercial
  performance property. A source import cannot exist unless the whole backing
  is DMA-pinnable and mapped into the VFIO IOAS. RustOS publishes a complete
  immutable frame in each slot. It may patch only the declared damage when a
  released slot retains the exact immediately preceding content generation and
  the compositor source mapping is unchanged; otherwise it copies the complete
  frame. Retained content generation never restores the cleared release token.
  RustOS fences the completed snapshot before publishing its exact even
  generation and rings only the fixed DVM peer. The DVM-only
  `rustos_dvm_ivshmem_uio` module validates the pool header, reconstructs an
  invitation that predates module load, allocates one local MSI-X UIO receive
  vector, requires the control and pixel headers to match, exposes only the
  pixel pages WB read-only to `rustos-dvm-display`, and accepts only an exact
  ready word or a module-validated 64-byte RELEASE record through private sysfs
  attributes. It rejects writable VMAs. It permits one outstanding release
  until host ACK; a later host event retries bounded flow control rather than
  terminating the relay.
  The adapter also reserves and owns the complete 128 MiB pixel aperture as a
  `ZONE_DEVICE` page-map. DMA-BUF export pins that owner and rejects any PFN
  outside it; a CPU-only `memremap()` alias cannot be mistaken for GPU-DMA
  backing. Kernel configuration verification therefore requires memory
  hotplug, hot-remove, and `CONFIG_ZONE_DEVICE`.
  Offline clears the confirmation, while a full pool re-invites its newest
  READY slot after restart. The relay rejects generic-UIO/INTx binding, releases
  superseded READY slots through the same validated path, and requires atomic
  KMS with one primary plane plus DMA-BUF import support. Each page-aligned
  immutable slot is exported as an EGL source image; it is never made the KMS
  front buffer. DRM PRIME's generic bidirectional import request is backed by a
  deliberately `DMA_TO_DEVICE` mapping, so the display device receives
  read-only DMA authority and there is no relay CPU copy or guest-writable
  source mapping. Since the producer is in another VM, import is additionally
  rejected unless the certified physical attachment reports coherent DMA; guest-side cache
  maintenance is not misrepresented as cross-VM producer synchronization.
  After an MSI-X publication, the root-only exporter validates
  the exact live slot, generation, sequence, acquire value, and bounded batch,
  performs the device-to-CPU acquire barrier, and materializes that completed
  CPU-producer release as a one-use `sync_file`. EGL imports it and inserts a
  server-side wait before GLES samples the source. GLES composes into a separate
  three-buffer GBM pool; its possibly-unsignalled native completion fence feeds
  KMS `IN_FENCE_FD` immediately, without a relay CPU pre-wait. One bounded poll
  observes render completion, the page-flip event, and the CRTC out-fence; only
  the latter completed chain retires the prior output. Offline
  revokes the entire pool. An accepted import, acquire ioctl, or atomic ioctl
  alone is not scanout completion.
  A freshly initialized passthrough GPU may expose a connected eDP connector
  with no current `encoder_id`. The relay therefore chooses only from that
  connector's kernel-advertised encoder/CRTC compatibility set; it does not
  require or trust stale firmware modeset state.
  The exporter device is explicitly root-only and may open before a host
  invitation so the relay can import all three read-only slots and complete an
  initial modeset using only its private, visibly non-black lifecycle texture.
  That frame communicates boot progress but grants no readiness authority and
  is atomically replaced by the first admitted RustOS PRESENT. The relay never
  samples a RustOS source before the first exact acquire `sync_file`. Host
  control/readiness authority is still granted
  only after those steps and an exact published invitation; requiring
  readiness at exporter open would create an impossible lifecycle cycle.
  V2, polling, and native-GPU fallback are not accepted as test or release
  topology.
  Both smoke and interactive DVM launches disable QEMU's default VGA so the
  service opens the sole virtio-GPU DRM device rather than a competing VGA DRM
  node. The interactive GTK frontend uses `zoom-to-fit=off`, and the private
  DVM virtio-GPU disables resize-aware EDID: otherwise GTK's bootstrap window
  can request 640×480 before Linux DRM starts and force unnecessary scaling.
  QEMU's explicit 1600×900 mode is therefore authoritative. The 60 FPS proof
  requires an active GTK consumer and atomic three-buffer page-flip
  completion. The same consecutive uiserver profile windows must satisfy the
  render-rate, input-gap/loss, backlog, and logical/presented-cursor predicates;
  evidence from disjoint time ranges cannot be combined. Linux 6.12 virtio-gpu
  cannot import the foreign SG-table
  DMA-BUF, so the standard virtual GPU cannot satisfy this gate; current QEMU
  vmware-svga also lacks the pitchlock capability required by vmwgfx. The DVM
  relay
  nevertheless preserves the fixed shared ABI and can scale only when a
  display backend genuinely requires a different mode. KVM still proves the
  bounded control apertures. Prime-completion v2 binds the backend-selected
  staged-copy or direct-DMA-BUF mode before host submission, and the direct
  contract requires explicit one-plane linear ARGB8888 modifier import. The
  sealed registry currently enables only virtio staged-copy and AMD direct
  DMA-BUF; later physical drivers require their own registry/evidence entry.
  The enabled AMD path has a source-level DMA-BUF
  import, GPU-composition, explicit-fence, and atomic-page-flip consumer, but
  its hardware gate remains failed until the assigned device supplies that
  evidence. The Linux appliance selects a display-class
  device only through its kernel-generated PCI modalias. NVIDIA initialization
  then requires `nvidia`, `nvidia_modeset`, and `nvidia_drm modeset=1 fbdev=0`
  from one pinned 580.173.02 release plus the matching GSP images; a partial or
  mixed-version stack never starts the display relay.
  Artifact-manifest schema 9 binds an exact 25-key set: Buildroot and Linux
  versions, the NVIDIA-open release and source hash, the non-redistributable
  release posture, the permitted display-module set, every boot artifact, the
  kernel-enforced module-signing certificate and exact kernel configuration,
  the source lock, and the authenticated control contract. Both hostd and xtask
  reject missing, duplicate, or additional keys instead of treating unknown
  supply metadata as advisory. The manifest and all seven named payloads must
  remain co-located in one self-contained release directory; no verifier or
  supervisor accepts an external config, source lock, certificate, or control
  contract. `make stage-release DEST=<fresh-absolute-path>` verifies before and
  after copy and atomically publishes only below an owner-controlled path with
  no symlink or group/world-writable ancestor.
  The per-image private key is retained only in the build tree as an exact 0600
  non-symlinked file owned by the build user. It is never exported; the signed
  release authorization instead binds the public certificate and immutable
  kernel/rootfs hashes.
- The default KVM image contains no RustOS module artifact. RustOS has no
  direct GPU, network, USB, or PS/2 provider; the UI and network fail closed
  until their respective DVM transports validate.
- `cargo xtask kvm-smoke --gui-dvm-surfaces --dvm-network-shmem` adds a
  host-created fixed 64-slot
  Ethernet aperture. RustOS may map only the validated header and fixed slots;
  it never follows DVM descriptors or allocations. Linux's `rustos-dvm-net`
  relay owns the DVM virtio-net NIC, while RustOS `netd` keeps socket namespace,
  TCP, and route policy. `--exercise-network` requires both topology flags,
  uses the private KVM copy of
  `netprobe` against the QEMU gateway and requires both ring directions to
  advance within their 64-slot invariants. This proves the KVM data transport,
  not physical NIC passthrough, reset, DMA, or revocation policy.
- The DVM `S48rustos-dvm-net` init service owns the display and network relay
  PIDs. Start is idempotent; stop releases the display relay first, waits at
  most 20 seconds for each process, then escalates only that recorded PID.
  Each relay may retry a transient device-readiness error internally, but a
  restart must never leave a second framebuffer consumer or Ethernet producer.
- Physical-GPU launch is selected through a sealed profile registry. A profile
  binds PCI vendor/device identity, expected DRM driver, DVM backend class,
  guest address, and firmware-table kind. The common QEMU IOMMUFD/VFIO,
  DMA-BUF, fence, KMS, and readiness path contains no vendor fallback. The DVM
  proof and relay consume one shared backend registry and publish
  `backend-class=<virtual-staged|physical-direct> certification=registered`;
  unknown profiles, unknown DRM drivers, and multiple eligible render nodes
  fail closed. `--physical-gpu` and `--gpu-firmware` are the generic lab CLI;
  the old AMD option names are compatibility aliases only. The reset-disabled
  lab writes one atomic launch claim keyed by the host boot ID before starting
  either guest and refuses every second attempt in that boot. A failed or hard
  timed-out assignment therefore requires a cold boot; it cannot be retried as
  if driver exit had reset the hardware.
  GPU prime and steady-state evidence are independently bounded to 1024 and
  2048 bytes. Any serialization overflow withdraws evidence and is surfaced by
  KVM as an immediate DVM GPU publication failure, never a readiness timeout.
- The currently certified physical AMD display profile is PCI `1002:1900` (Phoenix/HawkPoint GC
  11.0.1). DVM image verification requires the signed `amdgpu.ko` plus its
  exact GC 11.0.1, PSP 13.0.4, SDMA 6.0.1, and VCN 4.0.2 firmware files; a
  broad Buildroot `linux-firmware` selection alone is not supply evidence.
  Every required firmware file is a regular non-symlink whose SHA-256 is pinned
  in `driver-domains/linux/sources.lock` and checked before release hashing.
- `rustos-hostd discover` and `rustos-hostd preflight --plan ...` are L0
  read-only ownership gates. `launch-plan-v1.env` must explicitly enumerate
  every function in one actual IOMMU group and reject host-protected BDFs;
  hostd independently reads each assigned function's live `boot_vga` state and
  DRM connector status and rejects the L0 boot display or any connected host
  display even when the plan omits it from `HOST_PROTECTED_PCI_BDFS`;
  activated acquisition repeats this live check after the durable prepared
  record is written and immediately before the first driver mutation, removing
  that record if the display became active;
  neither command performs a driver unbind, VFIO bind, device reset, guest
  launch, or PCI assignment. Do not turn a successful preflight into a
  passthrough claim.
- `rustos-hostd preflight-physical` adds the complete reversible runtime gate:
  exact display-only policy and QEMU digest, the self-contained schema-9 DVM
  bundle, a writable `/dev/iommu`, successful empty-IOAS allocate/destroy
  ioctls, a soft `RLIMIT_MEMLOCK` of at least 4 GiB, the same live host-display
  safety check, and schema-3 proof that the
  sole display-class function is the signed `amdgpu` `1002:1900` target. Every
  AMD admission additionally reads the kernel-owned ACPI VFCT source with a
  4 MiB bound and no symlink following, verifies the ACPI checksum, selects one
  exact bus/device/function/vendor/device image, permits the subsystem pair
  only when exact or both firmware fields are zero, and requires its 0x55aa and
  ATOM headers. Supervision repeats that check before reset, rewrites only the
  validated image BDF to fixed guest slot `0000:00:08.0`, recomputes and
  revalidates the ACPI checksum, proves the VBIOS payload unchanged, and fsyncs
  the complete 0600 VFCT table in the owner-private runtime directory. QEMU
  receives only that table through `-acpitable` and pins the VFIO function to
  the same slot. Missing, duplicate, truncated, mutable, mismatched, or
  incorrectly relocated VBIOS state fails before launch.
  Every
  enabled reset method must have an impact scope contained by the complete
  lease. `bus`/`cxl_bus` is admitted only when every function on the affected
  bus belongs to that lease; missing, empty, unknown, or escaping reset scope
  fails closed. Physical binding also requires `vfio-pci.disable_idle_d3=Y`
  before probe, then clears and rereads PCI bus-master after bind and before
  and after every reset. This prevents an idle-D3 state restore from enabling
  DMA while the group still has an identity host mapping. `acquire --activate`
  repeats this gate
  after signed-release verification and before it creates durable prepared
  state or changes any driver binding. A host lacking IOMMUFD therefore fails
  without detaching the assigned GPU.
- `rustos-hostd relay-input` requires a matching strict device-policy file.
  Schema 2 remains valid for non-physical input-only domains. The enabled
  physical AMD display topology requires `driver-domain-policy-v3`. The signed policy binds the production QEMU
  digest and one transport per device class. `input-ring-msix` and
  `display-dmabuf-kms` are the admitted transports. The current commercial
  physical-device slice requires network and block to remain `disabled`; their
  virtual KVM test transports do not authorize physical assignment. The normal
  input command is a reconnecting L0 service; `--once` is diagnostics-only.
- `rustos-hostd supervise` accepts physical readiness only after five distinct
  authenticated `display-evidence-v2` samples meet the signed nominal-60-Hz
  floor and page-flip/atomic-commit latency bounds. The relay sample is derived
  from completed DRM page-flip events and requires a read-only DMA-BUF source,
  GPU composition, explicit fence, atomic KMS, a three-buffer scanout pool,
  zero relay CPU copy, and no staged damage copy. It carries an advancing
  sequence plus DVM-monotonic age.
  Wrong AMD identity, stale/restarted evidence, a CPU-copy path, low throughput,
  or excessive latency fails setup or ongoing health and triggers bounded recovery.
  The physical supervisor uses a fixed 2 GiB guest-memory profile. This leaves
  headroom for the approximately 453 MiB uncompressed release initramfs; the
  former 1 GiB profile was physically observed to fail during unpacking before
  the control and display services were available. Preflight and supervision
  require a 4 GiB soft memlock limit before device mutation/reset so IOMMUFD
  page pinning cannot fail for the first time after the GPU is detached.
- `rustos-hostd acquire` remains read-only unless `--activate` is supplied
  together with a detached OpenPGP signature, an explicit pinned release
  keyring, a strict `release-authorization-v1` payload, the bound DVM artifact
  manifest, the bound driver-domain policy, and a bound fleet policy. The
  signed authorization binds the complete validated IOMMU group, domain CID,
  all three file hashes, and a bounded validity window. The fleet policy must
  contain an exact matching member and rejects cross-domain CID, IOMMU-group,
  and PCI-BDF reuse; unsigned activation is unavailable. Only after all of
  those checks does hostd persist an owner-private `prepared` lease with each
  original PCI driver and `driver_override`. Durable leases use only
  `VFIO_LEASE_SCHEMA=3` and always bind release, artifact, device-policy, and
  fleet-policy digests; older schemas are rejected instead of being restored
  into the active authority graph. Hostd then binds the whole preflighted group
  to `vfio-pci`, and atomically mark it active. Reverse-order rollback is
  mandatory on failure; failed acquisition retains the prepared record, and
  `release --activate` restores either prepared or active records and deletes
  them only after success.
- `rustos-hostd supervise` is the only admitted physical display-DVM launch
  path. It revalidates the active lease and authorization window, exact signed
  artifact/policy hashes, QEMU hash, complete group, display-only policy, owner-
  private control/pixel files, and `/dev/iommu`. It resets the whole group,
  creates one QEMU IOMMUFD object, attaches every `vfio-pci` function to that
  non-identity address space, and permits no physical network or block device.
  A pre-exec gate prevents QEMU from opening VFIO until the private schema-2
  runtime record containing exact PID plus `/proc` start time has been fsync'd.
  Authenticated readiness is bounded to 30 seconds and additionally requires
  the agent to observe a supported DRM driver plus the direct relay's kernel-
  released lifetime lock after a real page flip. A fresh authenticated health
  exchange rechecks both every five seconds with a three-second response bound.
  Host-requested shutdown uses an owner-private QMP Unix socket: hostd validates
  the server greeting, negotiates `qmp_capabilities`, sends
  `system_powerdown`, and then waits at most ten seconds for the exact QEMU
  child to exit. The DVM runs Buildroot `acpid` with the fixed power-button to
  `/sbin/poweroff` action. QMP command acceptance is not shutdown evidence. A
  lost health/display exchange enters the same path. All release inputs, the signature verifier, QEMU, artifacts,
  and policy files are canonical regular files beneath root/service-owned,
  non-group/world-writable directory chains; hashing is in-process, so neither
  `PATH` substitution nor an untrusted symlink/parent can change the checked
  object. Only if QMP/ACPI fails does TERM get a five-second bound before KILL
  and reap; a forced stop cannot be accepted as a successful run. Original drivers are restored only after child
  exit and a second complete-group reset. A signaled or nonzero QEMU exit is a
  failed supervision result even when reset and restoration succeed; the CLI
  cannot report a crashed DVM as a completed run. A reset failure keeps the active
  durable lease and `vfio-pci` quarantine instead of rebinding a possibly dirty
  device.
- `rustos-hostd recover` signals a process only when the runtime record matches
  the lease and the live PID has the exact recorded start time. Recovery opens
  a pidfd, rechecks the start token after the open, attempts the private
  QMP/ACPI shutdown, and sends fallback TERM/KILL only via that descriptor. A missing kernel pidfd facility fails the commercial gate;
  there is no numeric-PID fallback. A reused numeric PID is never signaled. It then applies
  the same bounded stop, post-stop reset, restore, and durable-record deletion
  order. Missing, unsafe, stale, or foreign runtime state fails closed.

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
`display.present`, `display.provider.register`, and `process.spawn`.
Configuration and guest admission reject unknown, retired, and duplicate
points. Normal product configuration keeps injection disabled; an
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

## Docs

- Human docs bilingual; English first; mandatory language jump links.
- AI docs English-only and compact.
- mdBook nav source: `docs/SUMMARY.md`. mdBook config: `book.toml`. Output: `build/mdbook`.
- Mandatory token policy: `docs/ai/token-policy.md`.
