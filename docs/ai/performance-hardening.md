# AI Performance & Hardening Runbook

Use this for boot/runtime performance work, UI stalls, and cleanup requests that
span docs plus code. Keep this file short; put detailed behavior in the owning
contract file.

## Evidence Order

1. Run or reuse one bounded KVM-smoke debugcon capture. Prefer focused `rg`;
   never read full logs.
2. Rank by elapsed evidence: service startup, DVM transport readiness, UI boot stages,
   update ticks, input errors/slow reads, watchdog/stall lines.
3. Patch only the owner boundary that the log names. Do not harden unrelated
   helpers after the measured bottleneck is fixed.
4. Re-run `cargo xtask check`; run KVM smoke only after code and docs agree.

## Driver Boot

- Linux DVM owns device drivers. RustOS validates only the fixed DVM transport.
- Missing or invalid DVM input, display, or network transport must leave that
  device unavailable; do not install a native, firmware, or direct-virtio
  fallback.

## UI Runtime

- Default KVM-smoke runs keep coarse `uiserver: update tick` logs only.
- Detailed `uiserver profile: ...` and cursor/render pipeline diagnostics stay
  behind `RUSTOS_UI_PROFILE=1`.
- Profile summaries use their own bounded asynchronous observability channel;
  profile delivery must never synchronously block the render loop. A dropped
  sample is a conservative KVM-gate failure, never a fabricated FPS success.
- Under pointer stress, expected healthy markers are `input_errors=0`,
  `input_slow=0`, recurring `update tick`, and no watchdog/stall lines.

## Cleanup Rule

- `cargo xtask ring3-inventory` decides migration cleanup scope. If
  `migration_candidate_loc=0` and `cleanup_debt_loc=0`, do not delete marked
  substrate just because it looks old.
