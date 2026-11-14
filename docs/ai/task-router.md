# AI Task Router

On-demand route table. Do not load it when `AGENTS.md` plus exact search already
identifies the owner, contract, and validation path. Do not preload token policy,
the AI map, or all AI docs.

## Source-work routing

Use `rg` for exact text and Serena for symbol/reference-aware navigation and
editing. Serena is preferred when it helps, but is not a mandatory gate. The
scope of a task is set by the requested goal and its real dependencies: read
and change every producer, consumer and call site that the goal touches, across
as many subsystems as that takes.

A route into a very large document means: search the owning heading first and
read the sections you need, rather than paging from line 1 by habit.

## Task routes

| User task | Read first | Then only if unresolved |
| --- | --- | --- |
| Resume prior work / prepare session handoff | `session-handoff.md` repository snapshot, never local authority | live goal + `git status --short` + `git log -1`, then one owner contract |
| Linux `pipe`/`eventfd` semantics, local fd kinds, `WAITSET_PROVIDER_KERNEL` | `abi/local-fd.md` | `kernel/ps/src/user/{pipe,eventfd,local_wait}.rs`, `service_ops/local_fd.rs`; probe `kvm-smoke --local-fd-probe` |
| Build/check issue | `.agents/skills/rustos-build/SKILL.md` or the failing diagnostic | search `commands.md` only if exact invocation remains unclear |
| Development-speed/tooling change | relevant `token-policy.md` section + exact active hook/config | search `commands.md` only if a build invocation is part of the issue |
| Run/KVM/debug issue | `.agents/skills/rustos-kvm/SKILL.md` | exact KVM owner; bounded `build/kvm/` evidence after symptom is known |
| Physical GPU/VFIO continuation | `physical-gpu-status.md` | search the owning `contracts-abi.md` heading, then exact hostd/xtask/DVM owner |
| Large-file I/O / future ext4 or NTFS support | `large-file-io-plan.md` (proposal, not implemented support) | owning sections of `abi/vfs-storage.md`, `abi/loader-scheduling.md`, and pager contract |
| Package/stage/registry | search the owning heading in `contracts-infra.md` | affected manifest and exact parser/stage range |
| Kernel API/change | `kernel-api-map.md` | relevant `kernel/*/src/api.rs`, then backing symbol |
| Kernel boot order | `kernel-api-map.md` | search the relevant `contracts-infra.md` heading, then exact boot symbol |
| SMP/AP/IPI/per-CPU/TLB | `smp-contract.md` | relevant API/infra contract and measured owner |
| Paging/page fault/pagerd/VMA | search the owning heading in `pager-protocol-contract.md` | exact pager/MM/compat/service symbol |
| Logging/fault injection | search the owning heading in `contracts-infra.md` | exact config/source owner; `commands.md` only if invocation detail matters |
| Runtime launch/session | search the owning heading in `contracts-infra.md` | runtime protocol/service symbol; search `contracts-abi.md` only for crossed IPC boundaries |
| UI/rendering | scoped search in `services/uiserver/src` | `repo-map.md` only if ownership is unclear |
| Performance optimization/profiling | `.agents/skills/rustos-performance-optimization/SKILL.md` | search `performance-hardening.md`, then measured owner and exact hot symbol |
| IPC/syscall/scheduler cost | performance skill + exact benchmark symptom | search the synchronous-IPC section of `performance-hardening.md` and matching `docs/benchmarks/README.md` range |
| Hardening/security review | relevant section of `core-engineering-contract.md`, then highest-risk owner contract | add ABI/API/system-flow ranges only for crossed boundaries |
| Add or change an ABI/syscall/handle/protocol identifier, or kernel scope question | `../architecture/evolution-rules.md` | owning `abi/*.md` section and `libs/rustos-user-abi` constants |
| Ring0/ring3 ownership | search the owning heading in `contracts-abi.md` | exact broker/service/API owner |
| Add service/app/driver | `workflows.md` | closest manifest/source and target files |
| Docs update | target doc | `docs/SUMMARY.md` only when navigation changes |
| Agent/token policy | `token-policy.md` | exact active skill/hook/config only |

## Working rules

- Explore until the owner, the contract and every dependent call site are
  known; there is no file-count or range limit.
- Do not patch runtime failures without evidence.
- `session-handoff.md` is a dated snapshot; verify its claims against the live
  goal, checkout, and `git status --short` before resuming.
- Search large contracts by owning heading instead of reading them from the top
  merely because the subsystem appears somewhere in the task.
