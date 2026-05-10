# AI Task Router

Pick the smallest context set. Do not load all docs.

Mandatory first step: read `token-policy.md`.

Second step: classify the user task into one row below. Read only the
`Read first` files, then use `rg -n` before opening any file listed under
`Then read only if needed`.

| User task | Read first | Then read only if needed |
| --- | --- | --- |
| Build/check issue | `commands.md` | exact failing command output, then `tools/xtask/src/build.rs` range found by `rg` |
| Run/QEMU/debug issue | `commands.md`, `repo-map.md` | exact `tools/xtask/src/qemu.rs` range found by `rg`; logs only via `tail -n 120` or focused `rg` |
| Package/stage/registry issue | `contracts.md` | affected `RUSTOS.package.toml`, then exact `package_manifest.rs` or `stage.rs` range |
| Kernel API/change | `kernel-api-map.md` | relevant `kernel/*/src/api.rs`, then backing module range found by symbol search |
| Kernel boot-order change | `kernel-api-map.md`, `contracts.md` | `kernel/src/main.rs`, then exact `kernel/executive/src/boot.rs` range |
| Logging change | `contracts.md` | `config/rustos.toml`; open `tools/build_log_cfg.rs` only after searching category/level name |
| Fault injection change | `contracts.md`, `commands.md` | `config/rustos.toml`; exact `libs/rustos-fault-injection`, `tools/xtask/src/qemu.rs`, or `kernel/nucleus-core/src/util/fault_injection.rs` range found by `rg` |
| Runtime launch/session issue | `contracts.md` | `libs/runtime-control/src/lib.rs`, then exact `services/runtimed/src/main.rs` range |
| UI/rendering issue | `repo-map.md` | search `services/uiserver/src`; open only the matching `render.rs` or `app/*` range |
| Add service/app/driver | `workflows.md` | one closest existing manifest, one closest source file, target manifest/source only |
| Docs update | `docs/SUMMARY.md` | target doc only; AI docs only if agent context changes |

Stop rules:

- If task can be answered from one AI doc and one source file, stop searching.
- If `rg` returns an exact symbol/function, open only a narrow range around it.
- Do not open a backing module until the relevant `api.rs` or manifest contract
  shows the needed boundary.
- Do not inspect `logs/` for build/check failures; use the failing command
  output first.
- Do not inspect `logs/` for run/debug failures until the QEMU command line and
  failing symptom are known.
- Do not open `Cargo.lock` unless the task is dependency resolution.
- Do not open `build/`, `target/`, or `vendor/` unless `token-policy.md`
  explicitly allows the exception.
- If a human doc duplicates an AI contract, use the AI contract unless writing prose.
- If source contradicts AI docs, source wins; update AI docs if task includes docs.
- If a contract change is discovered, update the focused AI doc before finishing.

Context budget defaults:

- Simple answer: `token-policy.md` + 1 focused AI doc + 0-1 source file.
- Small code change: 1 focused AI doc + 1-3 source ranges.
- Cross-subsystem change: task-router + contracts + relevant API map + exact source ranges.
- Debug from logs: 1 command doc + failing command output or `tail -n 120` from
  one log file.
- Avoid opening files over ~500 lines unless using `rg`/line ranges first.
- Avoid generated/vendor paths by default; see `token-policy.md`.

Escalation rule:

- Before opening a fourth source file or second large range, summarize the
  current hypothesis and name the exact missing fact.
