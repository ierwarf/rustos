# AI Task Router

Pick the smallest context set. Do not load all docs.

Mandatory first step: read `token-policy.md`.

Second step: classify the user task into one row below. Read only the
`Read first` files, then use Serena or ripgrep MCP before opening any file
listed under `Then read only if needed`. Do not use shell `rg`.

| User task | Read first | Then read only if needed |
| --- | --- | --- |
| Build/check issue | `commands.md` | exact failing command output, then `tools/xtask/src/build.rs` range found by MCP search |
| Run/QEMU/debug issue | `commands.md`, `repo-map.md` | exact `tools/xtask/src/qemu.rs` range found by MCP search; logs only via `tail -n 120` or focused ripgrep MCP search |
| Package/stage/registry issue | `contracts.md` | affected `RUSTOS.package.toml`, then exact `package_manifest.rs` or `stage.rs` range |
| Kernel API/change | `kernel-api-map.md` | relevant `kernel/*/src/api.rs`, then backing module range found by symbol search |
| Kernel boot-order change | `kernel-api-map.md`, `contracts.md` | `kernel/src/main.rs`, then exact `kernel/executive/src/boot.rs` range |
| Logging change | `contracts.md` | `config/rustos.toml`; open `tools/build_log_cfg.rs` only after searching category/level name |
| Fault injection change | `contracts.md`, `commands.md` | `config/rustos.toml`; exact `libs/rustos-fault-injection`, `tools/xtask/src/qemu.rs`, or `kernel/nucleus-core/src/util/fault_injection.rs` range found by MCP search |
| Runtime launch/session issue | `contracts.md` | `libs/runtime-control/src/lib.rs`, then exact `services/runtimed/src/main.rs` range |
| UI/rendering issue | `repo-map.md` | search `services/uiserver/src`; open only the matching `render.rs` or `app/*` range |
| Hardening request | `contracts.md`, `kernel-api-map.md` | highest-risk boundary first; exact API, broker, service, lock, memory, or device path found by MCP search |
| Ring0/ring3 ownership or microkernel boundary | `contracts.md`, `ring3-evacuation.md` | `ring3-inventory.md` for current marker LOC/owner/action snapshots; `commercial-microkernel-closure.md` for final-shape/LOC questions; `kernel-api-map.md`, then exact broker, service, driver, input, storage, or compat path found by MCP search |
| Add service/app/driver | `workflows.md` | one closest existing manifest, one closest source file, target manifest/source only |
| Docs update | `docs/SUMMARY.md` | target doc only; AI docs only if agent context changes |

Stop rules:

- If task can be answered from one AI doc and one source file, stop searching.
- If MCP search returns an exact symbol/function, open only a narrow range around it.
- If a required MCP server is unavailable, stop and report the MCP failure
  instead of falling back to shell `rg`.
- If the user asks for implementation and the target owner is clear, stop
  reasoning and patch the smallest viable slice.
- Reserve extended reasoning for debugging, failure analysis, structural review,
  security review, or explicit design-choice requests.
- For hardening, rank the OS risk first; do not spend time hardening unrelated
  low-risk helpers after the high-risk boundary is identified.
- If debugging hits a structural blocker or lacks runtime evidence, stop and
  report the blocker instead of making speculative patches.
- Do not open a backing module until the relevant `api.rs` or manifest contract
  shows the needed boundary.
- Do not inspect `logs/` for build/check failures; use the failing command
  output first.
- Do not inspect `logs/` for run/debug failures until the QEMU command line and
  failing symptom are known.
- Follow `token-policy.md` for generated paths, logs, and `Cargo.lock`.
- If a human doc duplicates an AI contract, use the AI contract unless writing prose.
- If source contradicts AI docs, source wins; update AI docs if task includes docs.
- If a contract change is discovered, update the focused AI doc before finishing.

Context budget defaults:

- Simple answer: this file plus one focused AI doc.
- Small code change: one focused AI doc plus 1-3 source ranges.
- Debugging: one command doc plus failing output or one focused log snippet.
- Cross-subsystem work: add `contracts.md` and the relevant API map only when
  the boundary crosses crates or services.

Escalation rule:

- Before opening a fourth source file or second large range, summarize the
  current hypothesis and name the exact missing fact.
