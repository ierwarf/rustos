---
name: rustos-kvm
description: Prepare, run, and diagnose bounded RustOS KVM and Linux DVM boots with independent acceptance evidence. Use for boot, run, KVM, DVM, or SMP requests.
---

# RustOS KVM

If source changes are needed, use `rustos-code-editing`; use `rustos-build` for
build routing. Do not preload either when no source/build decision needs it.

## Run

1. Run `cargo xtask dev-plan` and its selected fast checks after edits.
2. `kvm-smoke` refreshes the signed RustOS image by default; do not build it
   separately unless preparing a custom probe image to boot with `--no-build`.
   `kvm-run` uses the existing image unless passed `--build`. Reuse a verified
   DVM artifact for RustOS-only changes.
3. For DVM relay changes, run `make -C driver-domains/linux build-plan`, iterate
   with its cached lane, then one matching stable rebuild.
4. Run one bounded `cargo xtask kvm-smoke` with an explicit timeout.
5. Report the acceptance line, exit code, and only focused log extracts.

Acceptance requires the expected final line and exit code 0. Base smoke
proves input relay setup, not a live event; `--exercise-input` requires keyboard
and pointer ingress, and interactive `kvm-run` requires real pointer ingress.
UI markers alone do not prove DVM, input, GPU, or cross-service ABI readiness.

Ordinary multi-vCPU smoke and interactive runs only warn on an unsealed tree;
they do not run the formal gate. Multi-vCPU `--smp-iteration` smoke auto-seals
its narrower profile; `--no-auto-verify` skips that repair, not the qualification
seal checks. Run the PR gate explicitly for push/release evidence and report
each requested vCPU cohort separately. See `docs/ai/commands.md` for proof flags.

Timing profile: an idle-host boot proves readiness only for an idle host. For a
readiness or latency claim also run `--host-cpu-pressure <percent>` (e.g. 50 and
100), which keeps that share of the host cores the run may use pinned-busy for the
whole run and prints `timing profile host-cpu-pressure=...`. Quote the profile
with the result. It cannot combine with `--smp-iteration` (sealed evidence does
not record it). See `docs/ai/formal-scope-policy.md`.

For physical GPU/VFIO work read `docs/ai/physical-gpu-status.md` first. A stable
panel proves visual behavior only, not FPS, reset, revoke, latency, or recovery.
Do not repeat a failure-sticky physical launch in the same boot.

Never inspect all generated KVM output. Search the exact failure/acceptance
marker first and expose only the bounded surrounding lines needed for the next
decision; follow `docs/ai/token-policy.md` limits when more detail is required.
