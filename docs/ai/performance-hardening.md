# AI Performance & Hardening Runbook

Use this for boot/runtime performance work, UI stalls, and cleanup requests that
span docs plus code. Keep this file short; put detailed behavior in the owning
contract file.

## Evidence Order

1. Run or reuse one bounded Xen-smoke debugcon capture. Prefer focused `rg`;
   never read full logs.
2. Rank by elapsed evidence: service startup, driver autoload, UI boot stages,
   update ticks, input errors/slow reads, watchdog/stall lines.
3. Patch only the owner boundary that the log names. Do not harden unrelated
   helpers after the measured bottleneck is fixed.
4. Re-run `cargo xtask check`; run Xen smoke only after code and docs agree.

## Driver Boot

- `driverd` owns provider policy. Kernel brokers only probe aliases and load
  explicit module images.
- Keep hardware-specific `.ko` packages out of `default` unless the default
  Xen hardware profile actually needs them. Use explicit profiles such as `hardware-dev`,
  `storage-dev`, `network-dev`, or `input-dev`.
- `provider_group` is mutually exclusive for normal and fallback records. Once
  a provider in the group loads, later records in the same group must skip
  before alias probing.
- `fallback_only` means "use only if no primary provider loaded", not "load
  after primary".

## UI Runtime

- Default Xen-smoke runs keep coarse `uiserver: update tick` logs only.
- Detailed `uiserver profile: ...` and cursor/render pipeline diagnostics stay
  behind `RUSTOS_UI_PROFILE=1`.
- Under pointer stress, expected healthy markers are `input_errors=0`,
  `input_slow=0`, recurring `update tick`, and no watchdog/stall lines.

## Cleanup Rule

- `cargo xtask ring3-inventory` decides migration cleanup scope. If
  `migration_candidate_loc=0` and `cleanup_debt_loc=0`, do not delete marked
  substrate just because it looks old.
