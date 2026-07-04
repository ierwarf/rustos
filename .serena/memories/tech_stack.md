# RustOS Tech Stack

- Language/toolchain: Rust nightly pinned by `rust-toolchain.toml` to `nightly-2026-02-16`, profile `minimal`, components `rust-src`, `rustfmt`, `clippy`.
- Targets pinned in toolchain: `x86_64-unknown-uefi`, `x86_64-unknown-linux-gnu`.
- Cargo workspace resolver: `resolver = "3"`; dev/release profiles use `panic = "abort"`.
- Cargo alias: `.cargo/config.toml` defines `cargo xtask` as `cargo run -q -p xtask --`.
- Cargo build wrapper: `.cargo/config.toml` uses `rustc-wrapper = "sccache"`.
- Primary project CLI/build system: `tools/xtask` (`cargo xtask check/build/stage/run/debug/...`).
- Package/stage metadata uses per-package `RUSTOS.package.toml` manifests under services/apps/drivers/kernel/compat.
- Runtime/service boundary crates include `libs/runtime-control`, `libs/rustos-user-abi`, `libs/rustos-svc-runtime`, `drivers/libs/driver-abi`, and storage/observability/fault-injection helper crates.
- Project-scoped Codex config enables hooks and MCP servers for `ripgrep` via `npx -y mcp-ripgrep` and `serena` via `uvx --from serena-agent serena start-mcp-server`. Local PATH has `uvx`, `npx`, `gh`, `cargo`, and `rg`.
- GitHub capabilities are available in this session through the GitHub plugin/app tools; project `.codex/config.toml` comments mention a github MCP server, but only `ripgrep` and `serena` are actually declared there.