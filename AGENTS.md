# RustOS Agent Instructions

Keep model context small. The stable reusable prefix is only this file plus
`docs/ai/task-router.md`. An `AGENTS.md` supplied by the environment already
counts as loaded; do not reread it unless it changed. Everything else is
on-demand.

## Core workflow

1. Route the request through `docs/ai/task-router.md`; load the smallest focused
   contract it names. Do not preload `docs/ai-map.md`, `docs/ai/token-policy.md`,
   human docs, references, logs, or broad source trees.
2. Search before opening source. Prefer exact symbols/ranges and stop searching
   once the missing fact is known.
3. Select the source-tool tier below. **Do not preflight unused MCP servers.**
4. After product-source edits run `cargo xtask dev-plan`, execute its `now` lane
   while iterating, and run relevant `stable-batch` work once after the change
   set settles. The printed plan is routing, not evidence. Agent/docs-only edits
   use their focused selftests instead.
5. Never bypass hooks/signing with `--no-verify`, `--no-gpg-sign`, or equivalent
   options. Do not use `clean`/`distclean` as ordinary build recovery.
6. RustOS is evacuating policy from ring0 into named services (`rootd`,
   `syscalld`, `vfsd`, `loaderd`, `netd`, `inputd`, etc.). Do not move policy
   back into the kernel or retain obsolete compatibility merely as fallback.

## Source-tool tiers

**Local** — one owner/module, no public ABI, privilege, ownership, lifecycle,
concurrency, or cross-module change. Serena is the default semantic navigator
when symbol context helps; a scoped text search is sufficient for exact textual
lookup. ast-grep and CodeGraph are not required.

**Structural** — signatures, several files/crates, non-trivial call structure,
or dependency/ownership movement. Use Serena plus ast-grep only for structural
patterns and CodeGraph only for call/dependency/blast-radius evidence.

**Critical** — scheduler, MM, IPC, security/capability, ABI, SMP/TLB, privilege,
lifetime/concurrency, ring0/ring3 boundary, or hardware-critical behavior. Use
Serena and the structural/graph tools that materially establish correctness;
for broad critical changes this normally means all three. If a tool selected as
required for the task fails its focused probe, stop that source edit and report
the exact blocker rather than guessing.

Serena remains the primary semantic navigator/editor. ast-grep owns syntax-aware
patterns; CodeGraph owns call/dependency/impact analysis. Never dump complete
tool catalogs merely to discover a known tool.

## Context and output discipline

Follow `docs/ai/token-policy.md` when a task needs detailed limits. In ordinary
work, keep source reads to focused ranges, command output quiet on success, and
failures to the first useful diagnostic plus a tiny tail. Avoid `logs/`,
`target/`, `build/`, `vendor/`, `perf.data`, and `Cargo.lock` unless the routed
task specifically requires a bounded read there.

Batch independent evidence when known, but do not gather evidence that cannot
change the next decision. Do not restate the same findings after every tool
call. At a meaningful milestone, refresh the short handoff and compact/freshen
context rather than carrying a large history indefinitely.

## Contracts and validation

A bug fix must repair the violated invariant and add an appropriate regression
witness. Update a Markdown owner/flow contract in the same change **only when**
the behavior, public interface, invariant, ownership/lifecycle rule, registry,
validation command, or agent routing actually changes. Mechanical/internal
implementation edits do not require documentation churn.

Use existing subsystem APIs and declared sources of truth. Fail closed at
privilege/ABI boundaries, use bounded waits, and preserve observable application
ABI. For enabled product topologies, ownership, recovery, authenticated/versioned
cross-domain contracts, bounded performance, and evidence remain completion
requirements.

Before a Linux DVM integration build run
`make -C driver-domains/linux build-plan`; use cached `dev-*` loops while
iterating and one matching `rebuild-*` when stable. Resume interrupted targets.

## Sub-agents

Use sub-agents only when parallel independent exploration or a disjoint slice
reduces main-context churn. Give them only the task, relevant paths, stopping
condition, and required evidence. They are read-only unless a disjoint write
scope is explicit. Repository policy requires sub-agents to use **GPT-5.6
terra** with `xhigh` reasoning; the main agent owns integration and validation.

## Routing pointers

- Resume prior work: `docs/ai/session-handoff.md`, then live `git status --short`.
- Physical GPU/VFIO continuation: `docs/ai/physical-gpu-status.md` before new
  hardware tests.
- Source ownership/index: `docs/ai-map.md` only when the router does not already
  name the owner.
- Commands/build lanes: `docs/ai/commands.md`.
- Kernel APIs: `docs/ai/kernel-api-map.md`.
- ABI/service routing: `docs/ai/contracts-abi.md`.
- SMP: `docs/ai/smp-contract.md`.
- Performance: `.agents/skills/rustos-performance-optimization/SKILL.md` and the
  measured benchmark owner.

## External references

External web/reference research is mandatory for a new subsystem architecture
or when correctness depends on an unfamiliar external ABI/specification. Use
`references/` for high-risk debugging only when local evidence is insufficient
or a routed skill names a specific reference. Do **not** scan references before
routine implementation, tests, mechanical refactors, or well-scoped fixes.

## Reporting

Keep implementation chatter sparse. Report starts, material decisions/blockers,
and completion. At completion state what changed, the validation commands that
actually ran, and any remaining unverified boundary.
