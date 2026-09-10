# RustOS Agent Instructions

Keep model context small. This `AGENTS.md` is the only stable repository prompt
prefix. If already supplied, do not reread it unless it changed. Everything else
is on demand. AI-infrastructure edits must keep
`tools/agent/check-ai-context-contract.sh` passing.

## Core workflow

1. Search first and open the smallest focused contract/source range. Read
   `docs/ai/task-router.md` only when ownership or validation routing is unclear.
2. Use local `rg` for exact text. Prefer Serena when symbol/reference context or
   semantic editing helps.
3. Serena is a preference, not a mandatory gate. Once it is useful for a
   symbol-aware task, prefer continuing with focused Serena bodies, references,
   and edits instead of drifting back to broad raw reads or whole-file manual
   editing from habit. Use `rg`, shell reads, or `apply_patch` when they are
   smaller/clearer or Serena is unavailable or ill-suited.
4. After product-source edits run `cargo xtask dev-plan`, execute its `now` lane
   while iterating, then relevant `stable-batch` checks once when stable.
   Agent/docs-only edits use focused selftests.
5. Never bypass hooks/signing with `--no-verify`, `--no-gpg-sign`, or equivalent
   flags. Do not use `clean`/`distclean` as routine build recovery.
6. Keep policy in named userspace services (`rootd`, `syscalld`, `vfsd`,
   `loaderd`, `netd`, `inputd`, etc.); do not move policy back into ring0 as a
   compatibility or performance shortcut.

## Source work

For local edits, search the exact symbol/range and change the smallest owner.
For structural or critical work (scheduler, MM, IPC, security/capability, ABI,
SMP/TLB, privilege, lifetime/concurrency, ring boundaries, hardware), widen
Serena/reference/compiler/test evidence only as far as correctness requires.
There is no multi-MCP preflight gate.

For non-local changes check the applicable subset of lock order,
IRQ/preemption, blocking/allocation, user copy/faults, publication/teardown,
capability generation/rights, ABI/wire format, cancellation/timeouts, and
partial initialization/hot-unplug.

## Context and output discipline

Use `docs/ai/token-policy.md` only when detailed limits matter. Keep reads
focused, batch independent probes, keep successful command output quiet, and
return only the first useful failure diagnostic plus a tiny tail. Avoid raw
`logs/`, `target/`, `build/`, `vendor/`, `perf.data`, and `Cargo.lock` unless a
routed task requires a bounded read there.

Never dump `ALL_TOOLS` or bulk tool descriptions. Do not gather evidence that
cannot change the next decision. At meaningful milestones refresh the short
handoff and let Codex use model-specific default compaction rather than a
repo-pinned early threshold.

## Contracts and validation

Fix the violated invariant and add an appropriate regression witness. Update an
owner/flow Markdown contract only when behavior, public interface, invariant,
ownership/lifecycle, registry, validation, or routing actually changes.
Mechanical/internal edits do not require documentation churn.

Before Linux DVM integration run `make -C driver-domains/linux build-plan`; use
cached `dev-*` lanes while iterating and one matching `rebuild-*` when stable.
Resume interrupted targets instead of cleaning them.

## Sub-agents

Use sub-agents only when independent parallel exploration or a disjoint slice
reduces main-context churn. Give them only the task, relevant paths, stop
condition, and required evidence. They are read-only unless a disjoint write
scope is explicit. Repository policy uses GPT-5.6 Terra with `xhigh` reasoning;
the main agent owns integration and validation.

## Routing pointers

- Resume: `docs/ai/session-handoff.md`, then live `git status --short`.
- Physical GPU/VFIO: `docs/ai/physical-gpu-status.md` before hardware tests.
- Source ownership: `docs/ai-map.md` only if the router does not name the owner.
- Commands/build: `docs/ai/commands.md`.
- Kernel APIs: `docs/ai/kernel-api-map.md`.
- ABI/service routing: `docs/ai/contracts-abi.md`.
- SMP: `docs/ai/smp-contract.md`.
- Performance: `.agents/skills/rustos-performance-optimization/SKILL.md` and the
  measured benchmark owner.

External research is mandatory for a new subsystem architecture or unfamiliar
external ABI/specification. Otherwise prefer local evidence. At completion,
briefly state what changed, validation actually run, and any unverified boundary.
