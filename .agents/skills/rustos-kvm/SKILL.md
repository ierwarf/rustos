---
name: rustos-kvm
description: Prepare, run, and diagnose bounded RustOS KVM and Linux DVM boots with independent acceptance evidence. Use for boot, run, KVM, DVM, or SMP requests.
---

# RustOS KVM

If source changes are needed, use `rustos-code-editing`; use `rustos-build` for
build routing. Do not preload either when no source/build decision needs it.

## Run

1. Run `cargo xtask dev-plan` and its selected fast checks after edits.
2. Build a fresh signed image only when the requested gate needs one. Reuse a
   verified DVM artifact for RustOS-only changes.
3. For DVM relay changes, run `make -C driver-domains/linux build-plan`, iterate
   with its cached lane, then one matching stable rebuild.
4. Run one bounded `cargo xtask kvm-smoke` with an explicit timeout.
5. Report the acceptance line, exit code, and only focused log extracts.

Acceptance requires the expected final line, exit code 0, and real pointer
ingress where the topology claims input. UI markers alone do not prove DVM,
input, GPU, or cross-service ABI readiness. For SMP, report each requested vCPU
cohort separately.

For physical GPU/VFIO work read `docs/ai/physical-gpu-status.md` first. A stable
panel proves visual behavior only, not FPS, reset, revoke, latency, or recovery.
Do not repeat a failure-sticky physical launch in the same boot.

Never inspect all generated KVM output. Search the exact failure/acceptance
marker first and expose only the bounded surrounding lines needed for the next
decision; follow `docs/ai/token-policy.md` limits when more detail is required.
