# AI Task Router

Pick the smallest context set. This file and root `AGENTS.md` are the stable
prefix. Do not read `token-policy.md`, `ai-map.md`, or all AI docs by default.

## Source-work tier

Classify source work before navigation:

- **Local:** one owner/module; no public ABI, privilege, ownership, lifecycle,
  concurrency, or cross-module impact. Serena alone when semantic navigation is
  useful; scoped text search is fine for exact lookup.
- **Structural:** signatures/multiple files/crates/call structure. Serena plus
  ast-grep for syntax patterns and/or CodeGraph for dependency impact as needed.
- **Critical:** scheduler/MM/IPC/security/ABI/SMP/TLB, privilege, lifecycle,
  concurrency, ring boundary, or hardware-critical behavior. Use the full
  evidence set needed for correctness; broad critical changes normally warrant
  Serena + ast-grep + CodeGraph.

Do not preflight unused MCP servers. If a tool selected as required for the
current tier/fact fails, stop that edit and report the blocker.

## Task routes

| User task | Read first | Then only if unresolved |
| --- | --- | --- |
| Resume prior work / prepare session handoff | `session-handoff.md` current checkout snapshot | live goal state + `git status --short`, then one owner contract |
| Build/check issue | `commands.md` | failing command diagnostic, exact xtask owner range |
| Development-speed/tooling change | `commands.md`, then relevant part of `token-policy.md` | exact wrapper/hook/validation range |
| Run/KVM/debug issue | `commands.md` | exact KVM owner range; bounded `build/kvm/` evidence only after symptom is known |
| Physical GPU/VFIO continuation | `physical-gpu-status.md` | `contracts-abi.md`, then exact hostd/xtask/DVM owner |
| Package/stage/registry | `contracts-infra.md` | affected manifest and exact parser/stage range |
| Kernel API/change | `kernel-api-map.md` | relevant `kernel/*/src/api.rs`, then backing symbol |
| Kernel boot order | `kernel-api-map.md` | `contracts-infra.md`, then exact boot symbol |
| SMP/AP/IPI/per-CPU/TLB | `smp-contract.md` | relevant API/infra contract and measured owner |
| Paging/page fault/pagerd/VMA | `pager-protocol-contract.md` | exact pager/MM/compat/service symbol |
| Logging/fault injection | `contracts-infra.md` | exact config/source owner; `commands.md` only if command routing matters |
| Runtime launch/session | `contracts-infra.md` | runtime protocol/service symbol; add `contracts-abi.md` only for IPC routing |
| UI/rendering | scoped search in `services/uiserver/src` | `repo-map.md` only if ownership is unclear |
| Performance optimization/profiling | `.agents/skills/rustos-performance-optimization/SKILL.md` | `performance-hardening.md`, measured benchmark owner, exact hot symbol |
| IPC/syscall/scheduler cost | `performance-hardening.md` synchronous IPC section + `docs/benchmarks/README.md` | measured owner range; use counters/bench, not broad logs |
| Hardening/security review | `core-engineering-contract.md`, then highest-risk owner contract | add ABI/API/system-flow docs only for crossed boundaries |
| Ring0/ring3 ownership | `contracts-abi.md` | exact broker/service/API owner |
| Add service/app/driver | `workflows.md` | one closest manifest/source and target files |
| Docs update | target doc | `docs/SUMMARY.md` only when navigation changes |
| Agent/token policy | `token-policy.md` | exact active skill/hook/config only |

## Stop rules

- If one focused contract and one source range answer the question, stop.
- If search gives an exact symbol, open only that body/range.
- Before opening a fourth source file or second large range, name the exact
  missing fact; do not continue broad exploration by inertia.
- For implementation with a clear owner, patch the smallest viable slice.
- For debugging without runtime evidence, stop rather than patch speculatively.
- Add cross-subsystem contracts only when the boundary is actually crossed.
- Source overrides stale docs; update the owning contract only when behavior or
  its declared invariant/routing changed.
- `session-handoff.md` stores live state, not closed debugging narrative.
- `contracts-abi.md` is searched by owning surface heading; never read it from
  the top merely because IPC appears somewhere in the task.
