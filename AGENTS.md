# RustOS Agent Instructions

Read this first, then keep context small. This file (plus `docs/ai-map.md`,
`docs/ai/token-policy.md`, `docs/ai/task-router.md`) is the stable reusable
prefix every agent should load. Everything else is opened on demand.

## TL;DR

1. Route the task through `docs/ai/task-router.md` before reading source.
2. After edits, run `cargo xtask dev-plan` to separate fast checks from the
   one-time stable change-set gates; the plan is routing, not evidence.
   Before any Linux DVM integration build, also run
   `make -C driver-domains/linux build-plan`; if a build is interrupted, resume
   the same target without `clean` or `distclean`.
   For one cached DVM relay source package, its `dev-*` command is only the
   fast compile loop; batch the matching `rebuild-*` image/artifact refresh
   once after the change set is stable. Never clean or rebuild a toolchain for
   an ordinary relay edit.
3. Use Serena MCP / ripgrep MCP for symbol- and pattern-scoped lookups; do not
   open whole files or whole subsystems.
4. Sub-agents use **GPT-5.6 terra** with `xhigh` reasoning. Do not use GPT-5.5.
5. Never bypass hooks (`--no-verify`, `--no-gpg-sign`, etc.). Treat hook output
   as primary evidence.
6. RustOS is mid-evacuation from ring0 to user services. Push policy into
   `rootd` / `syscalld` / `vfsd` / `loaderd` / `netd` / `inputd` etc., not back
   into the kernel.

## Context Budget

See `docs/ai-map.md` (cache order) and `docs/ai/token-policy.md` (full rules).
Search before opening. Stable prefix first, task text last. No logs in prefix.

## Tool Usage

Symbol-aware search and scoped text search before file opens. When project MCP
servers (Serena, ripgrep, GitHub) are available, use them first; otherwise fall
back to equivalent shell tooling with tight scopes.

Let project hooks run. Do not bypass with `--no-verify`, `--no-gpg-sign`, or
equivalents. If a hook blocks or reports a tool/config failure, repair the hook
or command path before continuing — that output is the primary signal.

## Sub-Agent Use

Justified only when it reduces search/read churn that would otherwise pollute
the main context: parallel independent exploration, log triage, or a disjoint
write slice. Not for a single focused read or edit.

Constraints:

- Read-only by default. Workers write only with a disjoint file scope.
- Narrow context only: task, relevant paths, stop condition. No secrets,
  signing material, or unrelated repo state.
- Return evidence (files, line numbers, symbols, short summary, uncertainty),
  not final decisions. The main agent owns reasoning, integration, and
  validation.

### Model selection

User-directed repository policy: every sub-agent must use **GPT-5.6 terra**
with `xhigh` reasoning. Do not use GPT-5.5 for sub-agent work. This policy
overrides the previous mini-first guidance until the user revises it.

## Do Not Inspect By Default

`logs/`, `target/`, `build/`, `vendor/`, `perf.data`, `Cargo.lock`.
Narrow exceptions defined in `docs/ai/token-policy.md` §10.

## Common Commands

See `docs/ai/commands.md`. Quiet on success; treat failure output as primary
context. Do not scan logs for build failures.

## Hardening Direction

**Active refactor — ring0 evacuation.** `rootd` is the first user process;
starts `syscalld`, `vfsd`, `loaderd`, then hands off to `initd`. Push policy
into services, not back into the kernel. Use `RING3-MIGRATION-REFERENCE` /
`RING3-MIGRATION-COMMENTED-OUT` markers only as local annotations; source
ownership, broker call paths, and service contracts are the migration source
of truth.

**Product goal.** RustOS is a clean modern dual-ABI microkernel system:
preserve native Linux ELF and Windows PE64/EXE application compatibility,
isolate Linux driver stacks in DVMs, keep policy in named user services, and
keep ring0 small, fast, and capability-oriented. Do not preserve obsolete
`.ko`, kernel-extension, driver-private, or undocumented compatibility merely
because older systems exposed it. Migration must not break the explicitly
supported observable app ABI, and ring0 code removal goes through narrow
explicit brokers. Source-writing, lifecycle, concurrency, comment, and
refactoring rules live in `docs/ai/core-engineering-contract.md`.

## Commercial Completion Bar

No path, document, test plan, or review may excuse a missing required property
because the system is "early", experimental, transitional, or a prototype.
For every enabled product topology, completion requires explicit ownership and
least authority, bounded failure detection and recovery, authenticated and
versioned cross-domain contracts, observable latency/throughput limits, and
evidence that the implementation satisfies its stated invariants. A mandatory
capability that is absent, unverified, or retained only behind a fallback is a
failed acceptance gate, not a caveat. Retire replaced code, contracts, and
documentation after dependency proof; do not preserve a legacy route merely to
make an incomplete primary route appear successful.

**General principles.**

- Hardening over symptom patches: make ownership, timeouts, queue bounds, and
  ABI contracts explicit in source, manifests, registries, or AI contracts.
- Fail closed with bounded waits and direct diagnostics; no fabricated success.
- Display/input/driver/compat: keep fallback providers behind real hardware/virtio
  providers; validate against black frames, stalls, and provider-order regressions.

## Repo Entrypoints

See `docs/ai-map.md`. Key: `Cargo.toml`, `tools/xtask/src/cli.rs`,
`kernel/src/main.rs`, `kernel/*/src/api.rs`, `libs/runtime-control/src/lib.rs`.
For session continuation or handoff, read `docs/ai/session-handoff.md` after
the stable prefix and verify its volatile state against the live goal and Git
status.
For physical GPU continuation, read `docs/ai/physical-gpu-status.md` before
opening source or re-running hardware.

## Reporting Discipline

- Ask or infer the narrow subsystem before searching.
- Summarize findings before opening more files.
- Do not paste long command output into responses.
- Keep chat sparse during implementation: start, completion, blockers, real
  decisions only — no streamed search/build noise.
- On completion, report briefly: what changed, validation run, remaining
  blocker or risk.
