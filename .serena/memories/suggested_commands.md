# RustOS Suggested Commands

- `cargo xtask check` — fast validation for layering/manifests/workspace; primary low-cost check.
- `cargo xtask build` — full OS build plus staging into `build/`.
- `cargo xtask build-kernel` / `cargo xtask build-user` / `cargo xtask build-driver-modules` — scoped builds.
- `cargo xtask stage` — restage already-built artifacts into `build/image`.
- `cargo xtask kvm-smoke --timeout 30` — boot RustOS and Linux DVM with KVM; writes `build/kvm/`.
- `cargo xtask debug` — QEMU with GDB stub.
- `cargo xtask probe-display` — headless display probe with non-black-frame validation.
- `cargo xtask qemu-scenarios --list` and `cargo xtask qemu-scenarios --scenario display-probe` — QEMU regression scenario discovery/run.
- `cargo xtask selftest` — host selftests for fault parsing, ABI/layout, runtime contracts, module tests.
- `cargo xtask fuzz-host --target all` — deterministic host fuzz smoke.
- `cargo xtask ring3-inventory` — authoritative ring0/ring3 migration marker inventory; use `migration_candidate_loc` for real migration work and `cleanup_debt_loc` for legacy delete/retire work.
- `cargo test -p module-tests` — module tests.
- `git diff --check` — whitespace/conflict-marker sanity.
- QEMU-focused runs should prefer `--summarize-log`, `--expect`, and focused log searches instead of reading whole log files.
- Do not bypass hooks (`--no-verify`, `--no-gpg-sign`, etc.); hook output is primary evidence.
