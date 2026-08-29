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

## Add a DVM driver capability

1. Add the Linux-side package or relay under `driver-domains/linux/`.
2. Define a fixed, versioned transport contract before RustOS-side code.
3. Keep RustOS changes to bounded transport validation and the owning service
   (`inputd`, `netd`, or `uiserver`); do not add a direct hardware fallback.
4. Run `cargo xtask build-dvm`, `cargo xtask verify-dvm`, then the focused KVM
   smoke command.

## Modify kernel API

1. Read `task-router.md`, `contracts-infra.md`, and the relevant `kernel/*/src/api.rs`.
2. Preserve `api.rs` as the cross-crate boundary.
3. Update `docs/api/kernel.md` only for public API surface or boot/order changes.
4. Run focused `cargo check` where possible; otherwise `cargo xtask check`.

## Modify logging

1. Read `contracts-infra.md` (Logging section). Human `docs/logging.md` only for prose updates.
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
5. Confirm GUI bootstrap registers the validated firmware framebuffer before
   any optional provider; it must not be packaged as a loadable driver.
6. Confirm `display-primary` provider decisions use active provider-group
   state and do not replace an active DVM/hardware provider with a fallback.
7. For KVM virtual GPU profiles, require the Linux DVM DRM/KMS relay. Any
   RustOS direct-GPU initialization is a regression.

## Reduce context mid-task

1. Summarize findings into the current response before opening more files.
2. Prefer one subsystem at a time.
3. Close questions by pointing at source path + line/symbol — do not paste
   long code.
