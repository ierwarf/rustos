# AI Agent Reference

Optimized for AI agents, not human onboarding. English-only, dense, contract-shaped.

`core-engineering-contract.md` is the mandatory source-writing contract for
RustOS product intent, ownership, lifecycle, concurrency, ABI, comments,
refactoring, recovery, and review.

## What lives here

| File | Role |
| --- | --- |
| `token-policy.md` | Mandatory operating policy: context budget, forbidden paths, when to stop. |
| `task-router.md` | Task → smallest context set. Read after `token-policy.md`. |
| `session-handoff.md` | Volatile checkout state and safe new-session resume sequence. |
| `repo-map.md` | Source ownership and canonical entrypoints. Deeper than `docs/ai-map.md`. |
| `commands.md` | Quiet build/check/debug commands and their failure meanings. |
| `contracts.md` | Index into `contracts-infra.md`/`contracts-abi.md`; edit the section owner, not this file. |
| `contracts-infra.md` | Manifest/stage/build/logging/fault contracts. |
| `contracts-abi.md` | IPC service IDs, broker syscalls, service routing contracts. |
| `core-engineering-contract.md` | Mandatory source-writing contract: ownership, lifecycle, concurrency, comments, refactoring, review. |
| `smp-contract.md` | Multi-CPU release gate: AP startup, per-CPU scheduler/IPI/TLB/futex ownership, qualification matrix. |
| `system-flows.md` | Machine-linked end-to-end exception, IPC, wait-set, VFS, endpoint, and restart lifecycles. |
| `commercial-quality-gates.md` | Non-negotiable definition of done and risk-ordered release acceptance scope. |
| `physical-gpu-status.md` | Current physical GPU evidence boundary, userspace wait-set release gates, and continuation rules. |
| `performance-hardening.md` | Boot/runtime bottleneck triage, provider policy, cleanup rules. |
| `kernel-api-map.md` | Cross-crate kernel API surfaces (`kernel_*::api`) and boot order. |
| `workflows.md` | Step recipes: add service/app/driver, modify kernel API, debug KVM lifecycle. |

## Operating rules

- Follow `token-policy.md`.
- Read the smallest file/range needed before scanning the repo.
- Treat human docs (`docs/*.md` outside `docs/ai/`) as explanatory; AI docs as compact contracts.
- Verify code truth before editing when a contract references a source path.
- For OS hardening, prioritize high-risk boundaries over broad cleanup.
- For debugging, classify every decisive property as `implemented and evidenced`,
  `implemented but unverified`, or `absent`. If the cause is absent or structurally
  unsupported, report that gap, its owner, and the acceptance consequence before
  proposing a patch. Do not disguise an unimplemented path with a fallback,
  synthetic success signal, or symptom-only tuning.
- For blocked debugging, stop and report the structural blocker — no speculative patches.

## Stable cache prefix

Cache exactly these, in order, then append **one** focused AI doc selected by `task-router.md`:

1. Root `AGENTS.md`
2. `docs/ai-map.md`
3. `token-policy.md`
4. `task-router.md`

Keep task text, logs, command output, and source snippets *after* this prefix. Never cache logs or generated output.

## Human docs (use only when AI contracts are missing the needed behavior)

`docs/index.md`, `docs/ai-map.md`, `docs/getting-started.md`, `docs/execution-flow.md`, `docs/structure.md`, `docs/logging.md`.
