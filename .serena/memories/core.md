# RustOS Core

- Rust workspace for an experimental OS: kernel crates, ring3 services, user apps, driver bridge modules, shared libraries, compat support, boot protocol, and xtask tooling in one workspace.
- Stable AI prefix: `AGENTS.md` only; task-router, AI map, token policy, and focused contracts are on-demand.
- For a continued checkout or session switch, read `docs/ai/session-handoff.md` after `AGENTS.md`. It is volatile routing state, not evidence and not a universal prefix file.
- Search first; read `docs/ai/task-router.md` only when owner/contract/validation routing remains unclear. Source wins over AI docs.
- Search discipline: Serena for semantic navigation and local `rg` for exact text before opening source; use focused ranges. Avoid generated/runtime paths unless a routed exception applies.
- Ring0 evacuation is an active invariant: push policy into `rootd`, `syscalld`, `vfsd`, `loaderd`, `netd`, `inputd`, etc.; do not move policy back into the kernel. Preserve Linux ELF and Windows PE observable ABI.
- Major ownership: `kernel/` kernel entry/subsystem crates; `services/` userspace daemons; `apps/` demo/user packages; `drivers/bridges/` kernel bridge modules; `drivers/libs/` driver ABI/runtime helpers; `libs/` shared crates; `compat/` Linux/Windows compatibility; `boot/` boot protocol; `tools/xtask` build/run/stage orchestration.
- Canonical entrypoints: workspace `Cargo.toml`; xtask CLI `tools/xtask/src/cli.rs`; stage `tools/xtask/src/stage/mod.rs`; KVM `tools/xtask/src/kvm.rs`; package schema `tools/xtask/src/package_manifest.rs`; kernel boot `kernel/src/main.rs`; kernel APIs `kernel/*/src/api.rs`; runtime protocol `libs/runtime-control/src/lib.rs`; logging policy `config/rustos.toml`.
- Read `mem:tech_stack` for pinned toolchain/build stack, `mem:suggested_commands` for commands, `mem:conventions` for coding and routing conventions, and `mem:task_completion` for done checks.
