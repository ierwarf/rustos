# AI Map

**Role:** one-screen entry index loaded as part of the stable prompt-cache
prefix. For deeper source ownership and per-area editing guidance, use
`docs/ai/repo-map.md`. For task routing, use `docs/ai/task-router.md`.

## Stable cache unit

Cache these four files in order, then append exactly one focused `docs/ai/*`
file selected by the task router. Keep logs, generated output, and command
output outside the cached unit.

1. `AGENTS.md`
2. `docs/ai-map.md`
3. `docs/ai/token-policy.md`
4. `docs/ai/task-router.md`

## First files

- `AGENTS.md` — root operating instructions.
- `docs/ai/token-policy.md` — context budget and forbidden paths.
- `docs/ai/task-router.md` — smallest context set by task type.
- `docs/ai/session-handoff.md` — volatile checkout state for session
  continuation only; not part of the stable prefix.
- `docs/ai/repo-map.md` — source ownership and canonical entrypoints (deeper).
- `docs/ai/commands.md` — quiet build/check commands and focused debug commands.
- `docs/ai/contracts-infra.md` — manifest/stage/build/logging/fault contracts.
- `docs/ai/contracts-abi.md` — IPC service IDs, broker syscalls, service routing.
- `docs/ai/commercial-quality-gates.md` — non-negotiable completion and
  retirement gates for enabled product topologies.
- `docs/ai/core-engineering-contract.md` — product intent plus mandatory
  lifecycle, concurrency, ABI, comment, refactoring, and review rules for all
  Rust source.
- `docs/ai/physical-gpu-status.md` — current physical GPU evidence boundary,
  remaining generic userspace readiness ABI, and safe continuation rules.
- `docs/ai/performance-hardening.md` — boot/runtime bottleneck and cleanup runbook.
- `docs/benchmarks/README.md` — the IPC/syscall cost lane: measured phase
  decomposition, the anchor and noise-floor rules, and the ceilings that were
  closed by measurement rather than by a change. Outranks any plan file.
- `docs/ai/smp-contract.md` — normative x86_64 CPU topology, AP startup,
  per-CPU state, IPI, scheduler, TLB, lifetime, panic, and release gates.
- `docs/ai/structural-ownership-design.md` — source-verified status of the
  audit v5 items and the target ownership structure for the ones still open:
  per-CPU scheduler, vfsd lanes, uiserver owner split, frame identity, one
  absolute deadline, receiver-set epoch, TLB targeting.
- Ring0/ring3 ownership decisions come from the exact broker call path,
  owning service contract, and local `RING3-MIGRATION-*` annotation. LOC
  inventory is not an architecture gate.

## Source entrypoints (minimal)

- Workspace: `Cargo.toml`
- Build/run CLI: `tools/xtask/src/cli.rs`
- Stage/registries: `tools/xtask/src/stage/mod.rs`
- KVM parallel-boot runner: `tools/xtask/src/kvm.rs`
- Kernel boot entry: `kernel/src/main.rs`
- Kernel API surfaces: `kernel/*/src/api.rs`
- Runtime protocol: `libs/runtime-control/src/lib.rs`
- Logging policy: `config/rustos.toml` `[logging]`

For the full annotated list (config parser paths, fault-injection runtime,
boot orchestration, etc.) see `docs/ai/repo-map.md`.

## Ownership at a glance

- `kernel/` — kernel entry and subsystem crates.
- `services/` — userspace services.
- `apps/` — demo/user applications.
- `driver-domains/linux/` — isolated Linux DVM image, relays, and contracts.
- `drivers/libs/` — driver ABI/runtime/helper crates.
- `libs/` — shared Rust crates.
- `boot/` — boot protocol.
- `compat/` — Windows/Linux compatibility support.
- `assets/image/` — static boot-image overlay.

## Fast commands

- `cargo xtask check` — fast validation.
- `cargo xtask build` — full image.
- `cargo xtask build-kernel` / `build-user` / `build-dvm` — scoped.
- `cargo xtask stage` — restage existing artifacts.

Quiet on success. On failure, treat the command output as primary context.

## Path policy

Generated paths, logs, vendor inputs, `Cargo.lock`, and large-file exception
rules live in `docs/ai/token-policy.md`. This map carries entrypoints, not
exceptions.
