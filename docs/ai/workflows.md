# AI Workflows

Before any workflow: read `token-policy.md`, then `task-router.md`.

Add service:

1. Read `docs/guides/add-service.md`.
2. Inspect similar service manifest under `services/*/RUSTOS.package.toml`.
3. Add crate under `services/<name>`.
4. Add workspace member.
5. Add manifest with `kind = "service"`, `execution_domain = "user"`.
6. Run `cargo xtask check`.
7. Update docs only if manifest/runtime behavior changes.

Add app:

1. Read `docs/guides/add-app.md`.
2. Inspect `apps/wayclick/RUSTOS.package.toml` or `apps/shell/RUSTOS.package.toml`.
3. Choose Rust/C/Windows app path.
4. Add manifest and desktop entry.
5. Run `cargo xtask check`.
6. Update docs only if launch policy or app workflow changes.

Add bridge driver:

1. Read `docs/guides/add-driver.md`.
2. Inspect `drivers/bridges/display/bootfb/RUSTOS.package.toml`.
3. Add source under `drivers/bridges/<class>/<name>`.
4. Add manifest with `kind = "bridge-driver"`.
5. Add `[autoload]` only if policy-loaded.
6. Run `cargo xtask build-driver-modules`.
7. If autoload policy changes, inspect generated driver registry after stage.

Modify kernel API:

1. Read `docs/ai/task-router.md`, `docs/ai/contracts.md`, and relevant `kernel/*/src/api.rs`.
2. Preserve `api.rs` as the cross-crate boundary.
3. Update `docs/api/kernel.md` only for public API surface or boot/order changes.
4. Run focused `cargo check` where possible; otherwise `cargo xtask check`.

Modify logging:

1. Read `docs/logging.md` and `docs/ai/contracts.md`.
2. Update `config/logging.toml`.
3. If adding category, update `libs/rustos-observability/src/lib.rs`, `tools/build_log_cfg.rs`, human logging docs, and AI contracts.
4. Rebuild affected crates; prefer `cargo xtask build` for kernel logging.

Update docs:

1. Human docs: bilingual, English first, language anchors.
2. AI docs: English only, dense, no repeated bilingual prose.
3. Update `docs/SUMMARY.md` for new human or AI pages.
4. If behavior contracts changed, update `docs/ai/contracts.md` or the focused AI map.
5. Run mdBook/link sanity checks.

Debug QEMU boot:

1. Ensure `cargo xtask build` completed.
2. Run `cargo xtask run --debugcon stdio` for immediate logs.
3. Use `cargo xtask debug` for GDB attach.
4. Inspect `logs/debugcon.log`, `logs/qemu_interrupt.log` if interrupt trace enabled.
5. If display issue, run `cargo xtask probe-display`.

Reduce context mid-task:

1. Summarize findings into the current response before opening more files.
2. Prefer one subsystem at a time.
3. Close questions by pointing at source path + line/symbol, not by pasting long code.
