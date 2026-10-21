# AI Workflows

Follow `AGENTS.md`; load token policy or the task router only when their specific budget/routing detail is needed.
After product-source edits, run `cargo xtask dev-plan`, execute its `now` lane
while iterating, and run relevant `stable-batch` checks once stable. The source
recipes below use that plan for check/build selection, not ad hoc substitutes.

## Add service

1. Read `docs/guides/add-service.md`.
2. Inspect a similar manifest under `services/*/RUSTOS.package.toml`.
3. Add crate under `services/<name>`.
4. Add workspace member.
5. Add manifest with `kind = "service"`, `execution_domain = "user"`.
6. Run the dev-plan `now` lane, then relevant `stable-batch` checks once stable.
7. Update docs only if manifest/runtime behavior changes.

## Add app

1. Read `docs/guides/add-app.md`.
2. Inspect `apps/wayclick/RUSTOS.package.toml` or `apps/shell/RUSTOS.package.toml`.
3. Choose Rust/C/Windows app path.
4. Add manifest and desktop entry.
5. Run the dev-plan `now` lane, then relevant `stable-batch` checks once stable.
6. Update docs only if launch policy or app workflow changes.

## Add a DVM driver capability

1. Add the Linux-side package or relay under `driver-domains/linux/`.
2. Define a fixed, versioned transport contract before RustOS-side code.
3. Keep RustOS changes to bounded transport validation and the owning service
   (`inputd`, `netd`, or `uiserver`); do not add a direct hardware fallback.
4. Run `make -C driver-domains/linux build-plan` first; use its cached `dev-*`
   lane while iterating and one matching `rebuild-*` lane once stable, then
   `cargo xtask verify-dvm` and a focused KVM smoke command. RustOS source
   edits also follow dev-plan. Use a full `build-dvm` only when the plan needs it.

## Modify kernel API

1. Read the relevant `kernel/*/src/api.rs`; search the exact API-map or infra
   heading only if ownership, boot order, or validation routing is unresolved.
2. Preserve `api.rs` as the cross-crate boundary.
3. Update `docs/api/kernel.md` only for public API surface or boot/order changes.
4. Run dev-plan and its `now` lane; use focused checks only when selected by
   that plan, then relevant `stable-batch` checks once stable.

## Modify logging

1. Search `contracts-infra.md` for the Logging heading and read that range.
   Human `docs/logging.md` only for prose updates.
2. Update `config/rustos.toml` `[logging]`.
3. If adding a category, update `libs/rustos-observability/src/lib.rs`,
   `tools/build_log_cfg.rs`, and AI contracts.
4. Follow dev-plan's `now` and relevant `stable-batch` lanes for affected crates.

## Update docs

1. Human docs: bilingual, English first, language anchors.
2. AI docs: English only, dense, no repeated bilingual prose.
3. Update `docs/SUMMARY.md` for new human or AI pages.
4. If behavior contracts changed, update `contracts-infra.md` or `contracts-abi.md`.
5. Run focused docs/agent selftests or link checks for the affected contract;
   use mdBook validation when the task changes the book/navigation.

## Debug KVM lifecycle

1. Ensure `cargo xtask build` completed.
2. Reuse a verified DVM or follow its `build-plan` when Linux-side inputs
   changed; run `cargo xtask verify-dvm` before smoke.
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
3. Check `kernel/nucleus-core/src/multiboot2_entry.S` when the Multiboot2
   framebuffer request or an early boot diagnostic is suspected.
4. Treat GRUB/firmware framebuffer data as early-console or diagnostic input,
   never as a normal `uiserver` presentation provider.
5. Require the validated DVM primary provider with DVM-scanout provenance;
   absent or invalid transport must fail closed.
6. For KVM virtual GPU profiles, require the Linux DVM DRM/KMS relay and the
   current-epoch GUI-DVM aperture. Direct RustOS GPU initialization or any
   kernel/firmware presentation fallback is a regression.
7. Keep virtual readiness distinct from physical atomic-KMS, DMA-BUF, fence,
   FPS, reset, and recovery evidence; qualify those gates separately.

## Reduce context mid-task

1. Summarize findings into the current response before opening more files.
2. Prefer one subsystem at a time.
3. Close questions by pointing at source path + line/symbol — do not paste
   long code.
