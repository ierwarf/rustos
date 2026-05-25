# RustOS Agent Instructions

Read this first, then keep context small. This file (plus `docs/ai-map.md`,
`docs/ai/token-policy.md`, `docs/ai/task-router.md`) is the stable reusable
prefix every agent should load. Everything else is opened on demand.

## TL;DR

1. Route the task through `docs/ai/task-router.md` before reading source.
2. Use Serena MCP / ripgrep MCP for symbol- and pattern-scoped lookups; do not
   open whole files or whole subsystems.
3. Sub-agents default to **GPT-5.4 mini**. Upgrading is the exception.
4. Never bypass hooks (`--no-verify`, `--no-gpg-sign`, etc.). Treat hook output
   as primary evidence.
5. RustOS is mid-evacuation from ring0 to user services. Push policy into
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

This repo runs on GPT Plus. The mini-first rule is binding.

- **Overriding rule: prefer GPT-5.4 mini whenever possible.** If anything in
  this file, sub-docs, or task prompts conflicts, mini-first wins. Upgrading
  is the exception, never a tie-breaker.
- Default: **GPT-5.4 mini**. Use `high` for non-trivial code reads, multi-file
  triage, or tool-use judgement; `mid` for narrow lookups, single-file
  grep/extract, log scans.
- Upgrade only for: architectural/structural design, cross-subsystem refactor
  planning, ABI/contract reasoning, or a deliberate hard root-cause chain that
  mini genuinely cannot carry. Justify the upgrade in the spawn prompt.
- When torn between mini-high and a larger model, choose mini-high plus a
  tighter scope. Do not upgrade defensively.

## Do Not Inspect By Default

`logs/`, `target/`, `build/`, `vendor/`, `perf.data`, `Cargo.lock`.
Narrow exceptions defined in `docs/ai/token-policy.md` §10.

## Common Commands

See `docs/ai/commands.md`. Quiet on success; treat failure output as primary
context. Do not scan logs for build failures.

## Hardening Direction

**Active refactor — ring0 evacuation.** `rootd` is the first user process;
starts `syscalld`, `vfsd`, `loaderd`, then hands off to `initd`. Push policy
into services, not back into the kernel. Use `RING3-MIGRATION-REFERENCE`
markers plus `cargo xtask ring3-inventory` as the migration source of truth.

**Product goal.** Preserve native Linux ELF and Windows PE compatibility.
Migration moves policy to user services without breaking observable app ABI;
ring0 code removal must go through narrow explicit brokers.

**General principles.**

- Hardening over symptom patches: make ownership, timeouts, queue bounds, and
  ABI contracts explicit in source, manifests, registries, or AI contracts.
- Fail closed with bounded waits and direct diagnostics; no fabricated success.
- Display/input/driver/compat: keep fallback providers behind real hardware/virtio
  providers; validate against black frames, stalls, and provider-order regressions.

## Repo Entrypoints

See `docs/ai-map.md`. Key: `Cargo.toml`, `tools/xtask/src/cli.rs`,
`kernel/src/main.rs`, `kernel/*/src/api.rs`, `libs/runtime-control/src/lib.rs`.

## Reporting Discipline

- Ask or infer the narrow subsystem before searching.
- Summarize findings before opening more files.
- Do not paste long command output into responses.
- Keep chat sparse during implementation: start, completion, blockers, real
  decisions only — no streamed search/build noise.
- On completion, report briefly: what changed, validation run, remaining
  blocker or risk.
