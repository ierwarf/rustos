# AI Repo Map

**Role:** deeper source ownership map. Use after `docs/ai-map.md` when the
one-screen entry index isn't enough — typically when an area needs an
annotated entrypoint or a pre-edit checklist.

Read `token-policy.md` and `task-router.md` first.

## Core entrypoints (annotated)

- Workspace: `Cargo.toml`
- Build/run CLI: `tools/xtask/src/cli.rs`
- Build orchestration: `tools/xtask/src/build/` (`mod.rs`, `cargo.rs`)
- Stage/registries: `tools/xtask/src/stage/mod.rs`
- Xen runner/config generator: `tools/xtask/src/xen.rs`
- Host config/env: `tools/xtask/src/config/` (`mod.rs`, `project.rs`)
- Package schema: `tools/xtask/src/package_manifest.rs`
- Runtime client/protocol: `libs/runtime-control/src/lib.rs`
- Logging config parser/cfg: `tools/build_log_cfg.rs`
- Logging policy: `config/rustos.toml` `[logging]`
- Fault injection policy: `config/rustos.toml` `[fault_injection]`
- Fault injection runtime: `kernel/nucleus-core/src/util/fault_injection.rs`
- Kernel API surfaces: `kernel/*/src/api.rs`
- Kernel boot entry: `kernel/src/main.rs`
- Kernel orchestration: `kernel/executive/src/lib.rs`, `kernel/executive/src/boot.rs`

## Ownership

- `boot/` — boot protocol crate shared by the GRUB-loaded nucleus.
- `kernel/` — kernel entry and subsystem crates.
- `services/` — userspace services (`initd`, `runtimed`, `uiserver`, etc.).
- `apps/` — user/demo applications.
- `drivers/bridges/` — kernel bridge drivers and `.ko` modules.
- `drivers/libs/` — driver ABI/runtime/helper crates.
- `libs/` — general shared crates.
- `compat/` — compatibility layer and Windows userspace support.
- `assets/image/` — static files copied into boot image.
- `vendor/` — external binary inputs.
- `build/` — generated output; do not edit as source.
- `logs/` — run/debug output; do not edit as source.

## Before editing

- Package/stage behavior → `tools/xtask/src/package_manifest.rs` and
  `tools/xtask/src/stage/mod.rs`.
- Xen behavior → `tools/xtask/src/xen.rs`.
- Runtime launch behavior → `services/runtimed/src/main.rs` and
  `libs/runtime-control/src/lib.rs`.
- UI behavior → `services/uiserver/src/app/*` and `services/uiserver/src/render.rs`.
- Kernel subsystem integration → relevant `kernel/*/src/api.rs` first, then the
  backing module.
- Documentation navigation → `docs/SUMMARY.md` first.

## Docs

- Human landing: `docs/index.md`
- mdBook nav: `docs/SUMMARY.md`
- Structure rules: `docs/structure.md`
- Logging: `docs/logging.md`
- OS dev APIs: `docs/api/*.md`
- Task guides: `docs/guides/*.md`
- Stable paths/env: `docs/reference/*.md`

## Avoid by default

- `target/`, `build/`, `logs/` — generated or run output.
- `vendor/` — external binary inputs.
- `Cargo.lock` — only inspect if dependency resolution changed.
- `perf.data` — binary profiling output; never inspect as text.
- Full `docs/logging.md` or `docs/api/kernel.md` — use AI contracts first.

Allowed generated-path exceptions are defined in `token-policy.md`.
