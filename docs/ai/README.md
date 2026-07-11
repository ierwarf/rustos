# AI Agent Reference

Optimized for AI agents, not human onboarding. English-only, dense, contract-shaped.

## What lives here

| File | Role |
| --- | --- |
| `token-policy.md` | Mandatory operating policy: context budget, forbidden paths, when to stop. |
| `task-router.md` | Task → smallest context set. Read after `token-policy.md`. |
| `repo-map.md` | Source ownership and canonical entrypoints. Deeper than `docs/ai-map.md`. |
| `commands.md` | Quiet build/check/debug commands and their failure meanings. |
| `contracts-infra.md` | Manifest/stage/build/logging/fault contracts. |
| `contracts-abi.md` | IPC service IDs, broker syscalls, service routing contracts. |
| `performance-hardening.md` | Boot/runtime bottleneck triage, provider policy, cleanup rules. |
| `kernel-api-map.md` | Cross-crate kernel API surfaces (`kernel_*::api`) and boot order. |
| `workflows.md` | Step recipes: add service/app/driver, modify kernel API, debug KVM lifecycle. |

## Operating rules

- Follow `token-policy.md`.
- Read the smallest file/range needed before scanning the repo.
- Treat human docs (`docs/*.md` outside `docs/ai/`) as explanatory; AI docs as compact contracts.
- Verify code truth before editing when a contract references a source path.
- For OS hardening, prioritize high-risk boundaries over broad cleanup.
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
