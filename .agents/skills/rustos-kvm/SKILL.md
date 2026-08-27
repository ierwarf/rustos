---
name: rustos-kvm
description: Prepare, run, and diagnose bounded RustOS KVM and Linux DVM boots with independent acceptance evidence. Use for boot, run, KVM, DVM, or SMP requests.
---

# RustOS KVM

If source changes are needed, load `rustos-code-editing` first and pass the
Serena/ast-grep/CodeGraph gate. For build routing, use `rustos-build`.

## Order

1. Run `cargo xtask dev-plan` and its selected fast checks.
2. Run `cargo xtask build` for a RustOS disk when the requested gate needs a
   fresh signed image. RustOS-only changes reuse a verified DVM artifact.
3. For DVM relay changes, run `make -C driver-domains/linux build-plan`, then
   the matching stable `rebuild-*` before verification.
4. Run one bounded `cargo xtask kvm-smoke` command with an explicit timeout.
5. Use the command's acceptance output, exit code, and focused log extracts.

Acceptance requires the final expected acceptance line, exit code 0, and real
pointer ingress where the topology claims input. A relay reset after those
conditions is non-fatal only if the final acceptance evidence is already
complete. Do not accept a GUI or UI-server marker as proof of DVM, input, or
GPU readiness; a cross-service userspace ABI claim needs its own contract and
end-to-end evidence.

For UI/DVM work, use the repository's explicit `--gui-dvm-surfaces` and
`--min-ui-fps` gates only when requested; keep commit, frame-callback, release,
uiserver, and relay windows balanced. For SMP, run the requested vCPU cohort
and report each result separately.

## Physical hardware

Read `docs/ai/physical-gpu-status.md` before physical GPU/VFIO work. A stable
panel proves visual behavior only, not frame rate, reset, revoke, latency, or
recovery. Do not repeat a failed physical launch in the same boot when the
device is failure-sticky.

Never inspect all generated KVM output. Use `rustos-debuglog` for bounded
extracts and do not convert visual/model output into physical performance
evidence.
