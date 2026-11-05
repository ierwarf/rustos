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

1. Reuse a verified DVM or follow its `build-plan` when Linux-side inputs
   changed; run `cargo xtask verify-dvm` before smoke.
2. Run `cargo xtask kvm-smoke --timeout 30 --expect '<milestone>'`. It refreshes
   the RustOS image, boots both guests, and checks their readiness signals.
   Use `--no-build` only to boot a deliberately prebuilt image (for example,
   one built with custom probe variables); report that artifact choice.
3. For SMP seal behavior and proof modes, see [KVM smoke arguments](commands.md#kvm-smoke-arguments).
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
2. Work across as many subsystems as the goal needs; keep a short list of
   what is done and what is still disconnected.
3. Close questions by pointing at source path + line/symbol — do not paste
   long code.

## Verify fast, and work in parallel without breaking each other

Shorten the loop before the sealed run, not instead of it.

1. `tools/agent/formal-preflight.sh` runs the registered static registry and
   source-contract checks in parallel: sorted ids, unique REQ/HAZ numbers, matching
   flows, full documented paths, mutation anchors, the AI-context contract. Run it
   after formal/registry edits and before `formal/verify-all.sh`. Documentation-only
   changes use the focused checks in “Update docs”; preflight also invokes Cargo.
2. A new TLA+ model touches nine files. `tools/agent/formal-register-model.py` writes
   all of its registry rows from one command line and regenerates the contract
   document; the model's invariants, at least one spec mutant and its implementation
   mutants remain the author's work.
3. `cargo host-test` runs `rootd`, `procd`, and the `loaderd` library via the
   `.cargo/config.toml` alias. The first two require their package-qualified
   `host-test` features; loaderd's freestanding binary has `test=false`.
4. Compare a change against a control with `tools/agent/kvm-ab.py`: it builds the
   control ref in its own worktree and alternates one boot of each arm, so host drift
   hits both and the working tree stays usable. It reports a Fisher exact p-value;
   with a baseline failure rate near 4% a few dozen pairs only separate large effects.
5. Concurrent agents work in separate worktrees (`tools/agent/agent-worktree.sh new
   NAME`). Seals and builds are per worktree, so one agent's edit cannot invalidate
   another's sealed boot. Merge at a slice boundary, after preflight and a sealed run
   pass in that worktree; the script never merges for you.
6. `docs/ai/session-handoff.md` is the short resume head; dated evidence belongs in
   git history. `formal/binding-exempt-paths.txt` is the authoritative list of prose
   excluded from the source binding, including the handoff. Do not assume every
   document is exempt or that only the handoff is; see
   [binding exemptions](commands.md#kvm-smoke-arguments).
