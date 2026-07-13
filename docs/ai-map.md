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
- `docs/ai/repo-map.md` — source ownership and canonical entrypoints (deeper).
- `docs/ai/commands.md` — quiet build/check commands and focused debug commands.
- `docs/ai/contracts-infra.md` — manifest/stage/build/logging/fault contracts.
- `docs/ai/contracts-abi.md` — IPC service IDs, broker syscalls, service routing.
- `docs/ai/performance-hardening.md` — boot/runtime bottleneck and cleanup runbook.
- `cargo xtask ring3-inventory` — current `RING3-MIGRATION-REFERENCE` and
  `RING3-MIGRATION-COMMENTED-OUT` LOC/owner/action snapshot. Use
  `migration_candidate_loc` for true ring3 work and `cleanup_debt_loc` for
  legacy native code that should be retired rather than migrated.

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
- `cargo xtask build-kernel` / `build-user` / `build-driver-modules` — scoped.
- `cargo xtask stage` — restage existing artifacts.

Quiet on success. On failure, treat the command output as primary context.

## Path policy

Generated paths, logs, vendor inputs, `Cargo.lock`, and large-file exception
rules live in `docs/ai/token-policy.md`. This map carries entrypoints, not
exceptions.
