# AI Repo Map

Goal: minimize repo scan tokens.

Read `token-policy.md` and `task-router.md` before using this map.

Core entrypoints:

- Workspace: `Cargo.toml`
- Build/run CLI: `tools/xtask/src/cli.rs`
- Build orchestration: `tools/xtask/src/build.rs`
- Stage/registries: `tools/xtask/src/stage.rs`
- QEMU runner: `tools/xtask/src/qemu.rs`
- Host config/env: `tools/xtask/src/config.rs`
- Package schema: `tools/xtask/src/package_manifest.rs`
- Runtime client/protocol: `libs/runtime-control/src/lib.rs`
- Logging config parser/cfg: `tools/build_log_cfg.rs`
- Logging policy: `config/logging.toml`
- Kernel API surfaces: `kernel/*/src/api.rs`
- Kernel boot entry: `kernel/src/main.rs`
- Kernel orchestration: `kernel/executive/src/lib.rs`, `kernel/executive/src/boot.rs`

Ownership map:

- `boot/`: boot protocol crate shared by the GRUB-loaded nucleus.
- `kernel/`: kernel entry and subsystem crates.
- `services/`: userspace services (`initd`, `runtimed`, `uiserver`, etc.).
- `apps/`: user/demo applications.
- `drivers/bridges/`: kernel bridge drivers and `.ko` modules.
- `drivers/libs/`: driver ABI/runtime/helper crates.
- `libs/`: general shared crates.
- `compat/`: compatibility layer and Windows userspace support.
- `assets/image/`: static files copied into boot image.
- `vendor/`: external binary inputs.
- `build/`: generated output; do not edit as source.
- `logs/`: run/debug output; do not edit as source.

Docs:

- Human landing: `docs/index.md`
- mdBook nav: `docs/SUMMARY.md`
- Structure rules: `docs/structure.md`
- Logging: `docs/logging.md`
- OS dev APIs: `docs/api/*.md`
- Task guides: `docs/guides/*.md`
- Stable paths/env: `docs/reference/*.md`

Before editing:

- For package/stage behavior, inspect `tools/xtask/src/package_manifest.rs` and `tools/xtask/src/stage.rs`.
- For QEMU behavior, inspect `tools/xtask/src/qemu.rs`.
- For runtime launch behavior, inspect `services/runtimed/src/main.rs` and `libs/runtime-control/src/lib.rs`.
- For UI behavior, inspect `services/uiserver/src/app/*` and `services/uiserver/src/render.rs`.
- For kernel subsystem integration, inspect the relevant `kernel/*/src/api.rs` first, then the backing module.
- For documentation navigation, inspect `docs/SUMMARY.md` first.

Avoid by default:

- `target/`, `build/`, `logs/`: generated or run output.
- `vendor/`: external binary inputs.
- `Cargo.lock`: only inspect if dependency resolution changed.
- Full `docs/logging.md` or `docs/api/kernel.md`: use AI contracts first.

Allowed generated-path exceptions are defined in `token-policy.md`.
