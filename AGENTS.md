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

- Start at `docs/ai/token-policy.md`.
- Pick the smallest context set with `docs/ai/task-router.md`.
- Consult `docs/ai-map.md` or `docs/ai/repo-map.md` before any broad source
  search.
- Do not preload all docs, all manifests, or whole subsystems.
- Prefer Serena MCP symbols, then ripgrep MCP, then focused reads — open large
  files only when a symbol/pattern hit demands it.
- Keep stable instructions near the top of prompts, task-specific details at
  the end. Prompt caching depends on exact reusable prefixes — do not mix logs
  or generated output into that prefix.

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

Allowed exceptions:

- Run/debug failures: only the relevant `logs/` file, prefer the last 100–200
  lines.
- Stage/registry bugs: only specific files under
  `build/image/system/registry/`.
- Firmware/module packaging: only the specific `vendor/` path involved.
- Dependency resolution changes: focused `Cargo.lock` snippets via `rg` first.

## Common Commands

- Fast validation: `cargo xtask check`
- Full image build: `cargo xtask build`
- Kernel only: `cargo xtask build-kernel`
- Userspace only: `cargo xtask build-user`
- Driver modules only: `cargo xtask build-driver-modules`
- Stage existing artifacts: `cargo xtask stage`

These are expected to be quiet on success. On failure, use the reported
command output as primary context instead of scanning logs.

## Hardening Direction

**Active refactor — ring0 evacuation.** The first user process launched
directly by the kernel is `rootd`, not the Linux `initd` runtime. `rootd` is
the bootstrap authority modeled after an seL4 initial task: it must stay
independent of the Linux dynamic runtime and starts the foundational services
(`syscalld`, `vfsd`, `loaderd`) before handing off to normal `initd`. Do not
add generic Linux syscall fallbacks to make `initd` boot earlier — push that
pressure into `rootd`, service manifests, or narrow bootstrap brokers.

The old line-commented Linux compatibility reference files have been consumed
into service-oriented syscall routing and removed from the kernel tree. Only
unfinished Linux thread policy and Windows PE/Win32 policy remain as migration
reference comments. Linux MM ABI policy now belongs to `syscalld`; PTE
mutation and backing lifetime enforcement go through the gated
`SYS_RUSTOS_MM_BROKER`. Extend VFS, network, USB, input, provider, signal, and
clock policy in `syscalld`, `vfsd`, `netd`, `loaderd`, `devmgrd`, `driverd`,
`storaged`, or `inputd` — not in the kernel.

Do not restore deleted or commented ring0 policy modules for quick
compatibility fixes; the kernel keeps only narrow privileged primitives.
During this phase, compile/QEMU validation may be intentionally deferred for
structural code removal tasks.

**Product goal.** Preserve native compatibility for both Linux ELF and Windows
PE executables. Microkernel migration moves policy and namespace ownership to
user services without casually breaking observable app ABI behavior; when
ring0 code is removed, keep compatibility through narrow, explicit brokers or
service-owned implementations.

**General principles.**

- Long-term hardening over symptom patches: make ownership, provider choice,
  timeouts, queue bounds, and ABI contracts explicit in source, manifests,
  registries, probes, or AI contracts.
- Avoid broad catch-alls and fabricated success paths. Fail closed with
  bounded waits and direct diagnostics when an implementation is incomplete.
- For display, input, driver loading, and compat work, keep fallback providers
  behind real hardware/virtio providers and add validation that catches black
  frames, stalls, stale surfaces, and provider-order regressions.

## Repo Entrypoints

- Workspace: `Cargo.toml`
- xtask CLI: `tools/xtask/src/cli.rs`
- Build orchestration: `tools/xtask/src/build/` (`mod.rs`, `cargo.rs`)
- Staging and registries: `tools/xtask/src/stage/mod.rs`
- QEMU runner: `tools/xtask/src/qemu/mod.rs`
- Host config: `tools/xtask/src/config/` (`mod.rs`, `project.rs`)
- Package schema: `tools/xtask/src/package_manifest.rs`
- Kernel boot entry: `kernel/src/main.rs`
- Kernel API boundaries: `kernel/*/src/api.rs`
- Runtime protocol: `libs/runtime-control/src/lib.rs`

## Reporting Discipline

- Ask or infer the narrow subsystem before searching.
- Summarize findings before opening more files.
- Do not paste long command output into responses.
- Keep chat sparse during implementation: start, completion, blockers, real
  decisions only — no streamed search/build noise.
- On completion, report briefly: what changed, validation run, remaining
  blocker or risk.
