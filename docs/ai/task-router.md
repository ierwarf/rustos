# AI Task Router

On-demand route table. Do not load it when `AGENTS.md` plus exact search already
identifies the owner, contract, and validation path. Do not preload token policy,
the AI map, or all AI docs.

## Source-work routing

Use `rg` for exact text and Serena for symbol/reference-aware navigation and
editing. Serena is preferred when it helps, but is not a mandatory gate. For
structural or critical work, widen Serena/reference/compiler/test evidence only
as far as needed to establish correctness; do not preflight extra source MCPs.

## Task routes

| User task | Read first | Then only if unresolved |
| --- | --- | --- |
| Resume prior work / prepare session handoff | `session-handoff.md` current checkout snapshot | live goal + `git status --short`, then one owner contract |
| Build/check issue | `.agents/skills/rustos-build/SKILL.md` or the failing diagnostic | search `commands.md` only if exact invocation remains unclear |
| Development-speed/tooling change | relevant `token-policy.md` section + exact active hook/config | `commands.md` only if a build invocation is part of the issue |
| Run/KVM/debug issue | `.agents/skills/rustos-kvm/SKILL.md` | exact KVM owner; bounded `build/kvm/` evidence after symptom is known |
| Physical GPU/VFIO continuation | `physical-gpu-status.md` | `contracts-abi.md`, then exact hostd/xtask/DVM owner |
| Package/stage/registry | `contracts-infra.md` | affected manifest and exact parser/stage range |
| Kernel API/change | `kernel-api-map.md` | relevant `kernel/*/src/api.rs`, then backing symbol |
| Kernel boot order | `kernel-api-map.md` | `contracts-infra.md`, then exact boot symbol |
| SMP/AP/IPI/per-CPU/TLB | `smp-contract.md` | relevant API/infra contract and measured owner |
| Paging/page fault/pagerd/VMA | `pager-protocol-contract.md` | exact pager/MM/compat/service symbol |
| Logging/fault injection | `contracts-infra.md` | exact config/source owner; `commands.md` only if routing matters |
| Runtime launch/session | `contracts-infra.md` | runtime protocol/service symbol; add `contracts-abi.md` only for crossed IPC boundaries |
| UI/rendering | scoped search in `services/uiserver/src` | `repo-map.md` only if ownership is unclear |
| Performance optimization/profiling | `.agents/skills/rustos-performance-optimization/SKILL.md` | `performance-hardening.md`, measured owner, exact hot symbol |
| IPC/syscall/scheduler cost | `performance-hardening.md` synchronous IPC section + `docs/benchmarks/README.md` | measured owner; counters/bench, not broad logs |
| Hardening/security review | `core-engineering-contract.md`, then highest-risk owner contract | add ABI/API/system-flow docs only for crossed boundaries |
| Ring0/ring3 ownership | `contracts-abi.md` | exact broker/service/API owner |
| Add service/app/driver | `workflows.md` | closest manifest/source and target files |
| Docs update | target doc | `docs/SUMMARY.md` only when navigation changes |
| Agent/token policy | `token-policy.md` | exact active skill/hook/config only |

## Stop rules

- If one focused contract and one source range answer the question, stop.
- If search gives an exact symbol, open only that body/range.
- Before a fourth source file or second large range, name the exact missing fact.
- For clear ownership, patch the smallest viable slice.
- Do not patch runtime failures without evidence.
- `session-handoff.md` stores live state, not closed narrative.
- Search `contracts-abi.md` by owning heading; do not read it from the top merely
  because IPC appears somewhere in the task.
