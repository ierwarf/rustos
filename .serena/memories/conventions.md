# RustOS Conventions

- Route before reading: `docs/ai/token-policy.md` then `docs/ai/task-router.md`; choose the smallest focused AI doc and 1-3 source ranges.
- Use symbol-aware Serena search or ripgrep MCP/`rg` before source opens. For files over about 500 lines, search first and open only focused ranges.
- Kernel subsystem changes start at the relevant `kernel/*/src/api.rs`; do not open backing modules until the API boundary identifies the needed owner.
- Hardening order is OS-risk weighted: app-visible ABI/compat, privilege/capability/broker boundaries, memory/user-copy/handle lifetime, scheduler/locks/timeouts, boot/launch/provider ordering, filesystem/network/input/display/block mutation paths.
- Prefer manifest fields, generated registries, protocol state, and existing subsystem APIs over ad hoc path/name/order policy.
- Fail closed with bounded waits and direct diagnostics; no fabricated success paths.
- Preserve native Linux ELF and Windows PE compatibility while migrating policy out of ring0.
- Package/stage behavior belongs in `RUSTOS.package.toml`, `tools/xtask/src/package_manifest.rs`, and `tools/xtask/src/stage/mod.rs` contracts.
- Runtime launch/session issues route through `libs/runtime-control/src/lib.rs` and `services/runtimed/src/main.rs`; add ABI docs only if IPC routing is involved.
- UI/rendering routes through `services/uiserver/src/app/*` and `services/uiserver/src/render.rs`; validate display work against black frames/provider-order regressions.
- Generated/runtime paths are not source: avoid broad reads of `target/`, `build/`, `logs/`, `vendor/`, `Cargo.lock`, and `perf.data` unless the token policy names the exception.
- Sub-agents, when used, default to GPT-5.4 mini and narrow read-only scopes; main agent owns reasoning and integration.