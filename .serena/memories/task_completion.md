# RustOS Task Completion

- Use the smallest relevant validation command for the touched area; treat failure output as primary context.
- General code change baseline: `cargo xtask check` plus `git diff --check`.
- Full OS/image-impacting changes: `cargo xtask build`; use scoped `build-kernel`, `build-user`, or `build-driver-modules` when the owner is narrow.
- Package/stage/registry changes: `cargo xtask stage` after the relevant build artifact exists; inspect only focused generated registry paths if needed.
- Runtime/ABI contracts: `cargo xtask selftest` and focused tests such as `cargo test -p module-tests` when module-level behavior changed.
- Ring0/ring3 ownership work: run `cargo xtask ring3-inventory` when migration markers or ownership boundaries change.
- KVM/display/input regressions: run the focused `cargo xtask kvm-smoke` command named by the task; use focused `rg`/`tail -n 120` against `build/kvm/`, not whole-log reads.
- Docs-only changes: `mdbook build` if available; check markdown links with a focused pattern. Human top-level docs should include `[English](#english) | [한국어](#korean)`.
- Never use bypass flags for hooks or signing. If hooks fail, fix the hook/config/command path or report the blocker.
- After memory updates, `serena memories check` from repo root can sanity-check memory references.
