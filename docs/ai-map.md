# AI Map

Compact repo map for AI agents. For detailed routing, read
`docs/ai/token-policy.md` and `docs/ai/task-router.md`.

## First Files

- `AGENTS.md`: root operating instructions.
- `docs/ai/token-policy.md`: context budget and forbidden paths.
- `docs/ai/task-router.md`: smallest context set by task type.
- `docs/ai/repo-map.md`: source ownership and canonical entrypoints.
- `docs/ai/commands.md`: quiet build/check commands and focused debug commands.
- `docs/ai/contracts.md`: stable schemas, generated paths, API boundaries.

Use these first files as the stable prefix for AI sessions. Keep changing task
details, logs, and command output after them so provider prompt/context caches
can reuse the prefix.

## Source Entrypoints

- Build/run CLI: `tools/xtask/src/cli.rs`
- Build orchestration: `tools/xtask/src/build.rs`
- Stage/image/registries: `tools/xtask/src/stage.rs`
- QEMU/debug runner: `tools/xtask/src/qemu.rs`
- Host env/config: `tools/xtask/src/config.rs`
- Package manifest schema: `tools/xtask/src/package_manifest.rs`
- Kernel boot order: `kernel/src/main.rs`
- Kernel public surfaces: `kernel/*/src/api.rs`
- Runtime protocol/client: `libs/runtime-control/src/lib.rs`
- Logging cfg generator: `tools/build_log_cfg.rs`
- Logging policy: `config/rustos.toml` `[logging]`

## Ownership

- `kernel/`: kernel entry and subsystem crates.
- `services/`: userspace services.
- `apps/`: demo/user applications.
- `drivers/bridges/`: kernel bridge drivers and modules.
- `drivers/libs/`: driver ABI/runtime/helper crates.
- `libs/`: shared Rust crates.
- `boot/`: boot protocol.
- `compat/`: Windows/Linux compatibility support.
- `assets/image/`: static boot-image overlay.

## Avoid

Do not inspect these unless the task explicitly requires them:

- `logs/`
- `target/`
- `build/`
- `vendor/`
- `perf.data`
- `Cargo.lock`

For logs, prefer `tail -n 100` to `tail -n 200`. For large source files, use
`rg -n` first and then open only the matching line range.

## Fast Commands

- `cargo xtask check`
- `cargo xtask build-kernel`
- `cargo xtask build-user`
- `cargo xtask build-driver-modules`
- `cargo xtask stage`
- `cargo xtask build`

These should be quiet on success. Treat failure output as the first debugging
context.

## Cache Unit

For explicit context caching systems, cache this minimal repo context:

1. `AGENTS.md`
2. `docs/ai-map.md`
3. `docs/ai/token-policy.md`
4. `docs/ai/task-router.md`

Then append exactly one focused AI doc from `docs/ai/` after classifying the
task. Do not include `logs/`, `build/`, `target/`, `vendor/`, or command output
in the cached unit.
