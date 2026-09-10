# AI Agent Reference

Optimized for AI agents, not human onboarding. English-only, dense, contract-shaped.

`core-engineering-contract.md` is authoritative when a routed source-writing or
review task needs its product, ownership, lifecycle, concurrency, comment,
refactoring, recovery, or evidence rules. It is not bootstrap context and must
not be preloaded for unrelated work.

## What lives here

| File | Role |
| --- | --- |
| `token-policy.md` | On-demand context/tool/output budgets and stop limits. |
| `task-router.md` | On-demand task → smallest context route when ownership or validation is unclear. |
| `session-handoff.md` | Volatile checkout state and safe new-session resume sequence. |
| `repo-map.md` | Source ownership and canonical entrypoints. Deeper than `docs/ai-map.md`. |
| `commands.md` | Quiet build/check/debug commands and their failure meanings. Search before opening. |
| `contracts.md` | Index into `contracts-infra.md`/`contracts-abi.md`; edit the section owner, not this file. |
| `contracts-infra.md` | Manifest/stage/build/logging/fault contracts. Search the owning heading first. |
| `contracts-abi.md` | IPC service IDs, broker syscalls, service routing contracts. Search the owning heading first. |
| `core-engineering-contract.md` | Routed source-writing/review contract: ownership, lifecycle, concurrency, comments, refactoring, evidence. |
| `smp-contract.md` | Multi-CPU release gate: AP startup, per-CPU scheduler/IPI/TLB/futex ownership, qualification matrix. |
| `system-flows.md` | Machine-linked end-to-end exception, IPC, wait-set, VFS, endpoint, and restart lifecycles. |
| `pager-protocol-contract.md` | Demand-paging protocol across ring0 and pagerd. Search the owning heading first. |
| `address-space-lifecycle-contract.md` | Fork/unmap/pressure/teardown conformance and reservation/reclamation rules. |
| `page-table-reclamation-contract.md` | Dynamic page-table descriptor lifetime and live empty-table reclamation order. |
| `fork-cow-contract.md` | Admission and teardown gate before private anonymous mappings may be shared across fork. |
| `commercial-quality-gates.md` | Non-negotiable definition of done and risk-ordered release acceptance scope. |
| `physical-gpu-status.md` | Current physical GPU evidence boundary, userspace wait-set release gates, and continuation rules. |
| `performance-hardening.md` | Boot/runtime bottleneck triage and provider policy. Search the measured section first. |
| `kernel-api-map.md` | Cross-crate kernel API surfaces (`kernel_*::api`) and boot order. |
| `workflows.md` | Step recipes: add service/app/driver, modify kernel API, debug KVM lifecycle. |

## Operating rules

- Follow root `AGENTS.md`; read `token-policy.md` only when detailed budget/tool limits matter.
- Search before opening large files. For a document above 32 KiB, open only the focused <=120-line range found by that search.
- Treat human docs (`docs/*.md` outside `docs/ai/`) as explanatory; AI docs as routed contracts, not a preload set.
- Verify code truth before editing when a contract references a source path.
- For OS hardening, prioritize high-risk boundaries over broad cleanup.
- For debugging, classify every decisive property as `implemented and evidenced`,
  `implemented but unverified`, or `absent`. If the cause is absent or structurally
  unsupported, report that gap, its owner, and the acceptance consequence before
  proposing a patch. Do not disguise an unimplemented path with a fallback,
  synthetic success signal, or symptom-only tuning.
- For blocked debugging, stop and report the structural blocker — no speculative patches.

## Stable cache prefix

Cache root `AGENTS.md` only. Load the task router, AI map, token policy, and focused contracts on demand. Keep task text and volatile evidence after the stable prefix; never cache logs or generated output.

## Human docs (use only when AI contracts are missing the needed behavior)

`docs/index.md`, `docs/ai-map.md`, `docs/getting-started.md`, `docs/execution-flow.md`, `docs/structure.md`, `docs/logging.md`.
