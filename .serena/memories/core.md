# RustOS Core

- Rust workspace for an experimental OS: kernel crates, ring3 services, user apps, driver bridge modules, shared libraries, compat support, boot protocol, and xtask tooling in one workspace.
- Stable AI prefix/order: `AGENTS.md` -> `docs/ai-map.md` -> `docs/ai/token-policy.md` -> `docs/ai/task-router.md` -> one focused `docs/ai/*` file selected by the router.
- For a continued checkout or session switch, read `docs/ai/session-handoff.md` after the stable prefix. It is volatile routing state, not evidence and not a universal prefix file.
- Always route tasks through `docs/ai/task-router.md` before source reads; source wins over AI docs, and contract behavior changes should update the focused AI doc.
- Search discipline: Serena/ripgrep before opening source; open focused ranges, not whole subsystems. Avoid `target/`, `build/`, `logs/`, `vendor/`, `perf.data`, and `Cargo.lock` unless the token policy exception applies.
- Serena and ripgrep MCP are conveniences, not product gates. Fall back to local `rg` when either server is unavailable.
- Ring0 evacuation is an active invariant: push policy into `rootd`, `syscalld`, `vfsd`, `loaderd`, `netd`, `inputd`, etc.; do not move policy back into the kernel. Preserve Linux ELF and Windows PE observable ABI.
- Major ownership: `kernel/` kernel entry/subsystem crates; `services/` userspace daemons; `apps/` demo/user packages; `drivers/bridges/` kernel bridge modules; `drivers/libs/` driver ABI/runtime helpers; `libs/` shared crates; `compat/` Linux/Windows compatibility; `boot/` boot protocol; `tools/xtask` build/run/stage orchestration.
- Canonical entrypoints: workspace `Cargo.toml`; xtask CLI `tools/xtask/src/cli.rs`; stage `tools/xtask/src/stage/mod.rs`; KVM `tools/xtask/src/kvm.rs`; package schema `tools/xtask/src/package_manifest.rs`; kernel boot `kernel/src/main.rs`; kernel APIs `kernel/*/src/api.rs`; runtime protocol `libs/runtime-control/src/lib.rs`; logging policy `config/rustos.toml`.
- Read `mem:tech_stack` for pinned toolchain/build stack, `mem:suggested_commands` for commands, `mem:conventions` for coding and routing conventions, and `mem:task_completion` for done checks.
