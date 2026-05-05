# AI Task Router

Pick the smallest context set. Do not load all docs.

Mandatory first step: read `token-policy.md`.

| User task | Read first | Then read only if needed |
| --- | --- | --- |
| Build/run/debug issue | `commands.md`, `repo-map.md` | `tools/xtask/src/qemu.rs`, `tools/xtask/src/build.rs`, `logs/*` |
| Package/stage/registry issue | `contracts.md` | `tools/xtask/src/package_manifest.rs`, `tools/xtask/src/stage.rs`, affected `RUSTOS.package.toml` |
| Kernel API/change | `kernel-api-map.md`, `contracts.md` | relevant `kernel/*/src/api.rs`, backing module, `kernel/src/main.rs` for boot order |
| Logging change | `contracts.md` | `config/logging.toml`, `tools/build_log_cfg.rs`, `libs/rustos-observability/src/lib.rs` |
| Runtime launch/session issue | `contracts.md` | `libs/runtime-control/src/lib.rs`, `services/runtimed/src/main.rs`, `services/uiserver/src/app/runtime.rs` |
| UI/rendering issue | `repo-map.md` | `services/uiserver/src/render.rs`, `services/uiserver/src/app/input.rs`, `services/uiserver/src/app/runtime.rs` |
| Add service/app/driver | `workflows.md` | one closest existing manifest + target source dir |
| Docs update | `docs/SUMMARY.md` | target doc only; AI docs only if agent context changes |

Stop rules:

- If task can be answered from one AI doc and one source file, stop searching.
- If a human doc duplicates an AI contract, use the AI contract unless writing prose.
- If source contradicts AI docs, source wins; update AI docs if task includes docs.
- If a contract change is discovered, update the focused AI doc before finishing.

Context budget defaults:

- Simple answer: 1 AI doc + 1 source file.
- Small code change: 1 AI doc + 2-4 source files.
- Cross-subsystem change: task-router + contracts + relevant API maps + exact source files.
- Avoid opening files over ~500 lines unless using `rg`/line ranges first.
- Avoid generated/vendor paths by default; see `token-policy.md`.
