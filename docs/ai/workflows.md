# AI Workflows

Before any workflow: read `token-policy.md`, then `task-router.md`.

## Add service

1. Read `docs/guides/add-service.md`.
2. Inspect a similar manifest under `services/*/RUSTOS.package.toml`.
3. Add crate under `services/<name>`.
4. Add workspace member.
5. Add manifest with `kind = "service"`, `execution_domain = "user"`.
6. Run `cargo xtask check`.
7. Update docs only if manifest/runtime behavior changes.

## Add app

1. Read `docs/guides/add-app.md`.
2. Inspect `apps/wayclick/RUSTOS.package.toml` or `apps/shell/RUSTOS.package.toml`.
3. Choose Rust/C/Windows app path.
4. Add manifest and desktop entry.
5. Run `cargo xtask check`.
6. Update docs only if launch policy or app workflow changes.

## Add bridge driver

1. Read `docs/guides/add-driver.md`.
2. Inspect `drivers/bridges/display/bootfb/RUSTOS.package.toml`.
3. Add source under `drivers/bridges/<class>/<name>`.
4. Add manifest with `kind = "bridge-driver"`.
5. Add `[autoload]` only if policy-loaded.
6. Run `cargo xtask build-driver-modules`.
7. If autoload policy changes, inspect the generated driver registry after stage.

## Modify kernel API

1. Read `task-router.md`, `contracts-infra.md`, and the relevant `kernel/*/src/api.rs`.
2. Preserve `api.rs` as the cross-crate boundary.
3. Update `docs/api/kernel.md` only for public API surface or boot/order changes.
4. Run focused `cargo check` where possible; otherwise `cargo xtask check`.

## Modify logging

1. Read `contracts.md` (Logging section). Human `docs/logging.md` only for prose updates.
2. Update `config/rustos.toml` `[logging]`.
3. If adding a category, update `libs/rustos-observability/src/lib.rs`,
   `tools/build_log_cfg.rs`, and AI contracts.
4. Rebuild affected crates; prefer `cargo xtask build` for kernel logging.

## Update docs

1. Human docs: bilingual, English first, language anchors.
2. AI docs: English only, dense, no repeated bilingual prose.
3. Update `docs/SUMMARY.md` for new human or AI pages.
4. If behavior contracts changed, update `contracts-infra.md` or `contracts-abi.md`.
5. Run mdBook / link sanity checks.

## Debug KVM lifecycle

1. Ensure `cargo xtask build` completed.
2. Build and verify the DVM: `cargo xtask build-dvm` then `verify-dvm`.
3. Run `cargo xtask kvm-smoke --expect '<milestone>'`. It boots RustOS and the
   DVM concurrently, then checks both readiness signals.
4. Inspect focused lines in `build/kvm/rustos-debugcon.log` and
   `build/kvm/linux-dvm-serial.log`.
5. Treat a lifecycle pass as a lifecycle pass; do not infer device transport.

## Debug GRUB display boot

1. Check `tools/xtask/src/build/mod.rs` embedded GRUB config before changing
   KVM firmware or disk inputs.
2. Keep GRUB on serial/firmware text consoles; RustOS owns graphical output
   after the nucleus starts.
3. Check `kernel/nucleus-core/src/multiboot2_entry.S` for the Multiboot2
   framebuffer request tag.
4. Confirm the kernel log prints a nonzero `boot framebuffer addr`.
5. Confirm `platform:bootfb` can match from
   `storage::boot_volume::boot_framebuffer_info()`, not from an
   already-installed GUI backend.
6. Confirm `display-primary` fallback decisions use active provider-group
   state and skip fallback alias probes after a primary provider is loaded.
7. For KVM virtual GPU profiles, prefer the Linux `.ko` display path. If
   `virtio-gpu native: display registered` appears, the removed native fallback
   has regressed back into the kernel.

## Reduce context mid-task

1. Summarize findings into the current response before opening more files.
2. Prefer one subsystem at a time.
3. Close questions by pointing at source path + line/symbol — do not paste
   long code.
