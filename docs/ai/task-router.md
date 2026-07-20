# AI Task Router

Pick the smallest context set. Do not load all docs.

**Mandatory first step:** read `token-policy.md`.

**Second step:** classify the user task into one row below. Read only the
`Read first` files, then use symbol-aware or scoped text search before
opening anything under `Then read only if needed`.

**After edits:** run `cargo xtask dev-plan`. Execute its `now` commands during
the edit loop and its `stable-batch` commands once after the related change
set settles. The printed plan is not validation evidence.

| User task | Read first | Then read only if needed |
| --- | --- | --- |
| Build/check issue | `commands.md` | exact failing command output, then `tools/xtask/src/build/` range found by search |
| Development-speed/tooling change | `commands.md`, `token-policy.md` | for DVM builds, read the `DVM build-speed contract` before the exact wrapper range; otherwise read the exact `tools/xtask/src/dev.rs` or validation-runner range found by search |
| Run/KVM/debug issue | `commands.md`, `repo-map.md` | exact `tools/xtask/src/kvm.rs` range found by search; inspect `build/kvm/` only through focused reads |
| Physical GPU/VFIO continuation | `physical-gpu-status.md`, `commands.md` | `contracts-abi.md` for the ownership boundary, then only the exact hostd/xtask/DVM relay range named by the failed gate; do not rerun hardware before classifying the existing evidence |
| Package/stage/registry issue | `contracts-infra.md` | affected `RUSTOS.package.toml`, then exact `package_manifest.rs` or `stage/mod.rs` range |
| Kernel API/change | `kernel-api-map.md` | relevant `kernel/*/src/api.rs`, then backing module range found by symbol search |
| Kernel boot-order change | `kernel-api-map.md`, `contracts-infra.md` | `kernel/src/main.rs`, then exact `kernel/executive/src/boot.rs` range |
| Logging change | `contracts-infra.md` | `config/rustos.toml`; open `tools/build_log_cfg.rs` only after searching category/level name |
| Fault injection change | `contracts-infra.md`, `commands.md` | `config/rustos.toml`; exact `libs/rustos-fault-injection` or `kernel/nucleus-core/src/util/fault_injection.rs` range found by search |
| Runtime launch/session issue | `contracts-infra.md` | `libs/runtime-control/src/lib.rs`, then exact `services/runtimed/src/main.rs` range; add `contracts-abi.md` only if IPC-level routing is involved |
| UI/rendering issue | `repo-map.md` | search `services/uiserver/src`; open only the matching `render.rs` or `app/*` range |
| Hardening request | `commercial-quality-gates.md`, `contracts-abi.md`, `kernel-api-map.md` | highest-risk boundary first; exact API, broker, service, lock, memory, or device path found by MCP search |
| Boot/runtime performance cleanup | `performance-hardening.md`, `commands.md` | one focused log extract; exact `services/*`, `kernel/*`, `drivers/*`, or `tools/xtask/*` owner range named by the log |
| Ring0/ring3 ownership or microkernel boundary | `contracts-abi.md`, `commands.md` | `kernel-api-map.md`, then exact broker, service, driver, input, storage, or compat path and its owning service contract |
| Add service/app/driver | `workflows.md` | one closest existing manifest, one closest source file, target manifest/source only |
| Docs update | `docs/SUMMARY.md` | target doc only; AI docs only if agent context changes |

## Stop rules

- If the task can be answered from one AI doc and one source file, stop searching.
- If search returns an exact symbol/function, open only a narrow range around it.
- If the user asks for implementation and the target owner is clear, stop
  reasoning and patch the smallest viable slice.
- Reserve extended reasoning for debugging, failure analysis, structural
  review, security review, or explicit design-choice requests.
- For hardening, rank OS risk first; do not harden unrelated low-risk helpers
  after the high-risk boundary is identified.
- If debugging hits a structural blocker or lacks runtime evidence, stop and
  report the blocker — no speculative patches.
- Do not open a backing module until the relevant `api.rs` or manifest
  contract shows the needed boundary.
- Do not inspect `logs/` for build/check failures; use the failing command
  output first.
- Do not inspect `build/kvm/` for run/debug failures until the KVM command line
  and failing symptom are known.
- Follow `token-policy.md` for generated paths, logs, and `Cargo.lock`.
- If a human doc duplicates an AI contract, use the AI contract unless writing
  prose.
- If source contradicts AI docs, source wins; update AI docs if the task
  includes docs.
- If a contract change is discovered, update the focused AI doc before
  finishing.

## Context budget defaults

- Simple answer: this file plus one focused AI doc.
- Small code change: one focused AI doc plus 1–3 source ranges.
- Debugging: one command doc plus failing output or one focused log snippet.
- Cross-subsystem work: add `contracts-abi.md` and the relevant API map only
  when the boundary crosses crates or services.

## Escalation rule

Before opening a fourth source file or second large range, summarize the
current hypothesis and name the exact missing fact.
