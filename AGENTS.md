# RustOS Agent Instructions

Read this document first, then keep the working context small. This file, together with `docs/ai-map.md`, `docs/ai/token-policy.md`, and `docs/ai/task-router.md`, forms the stable reusable prefix that every agent must load. Open everything else only when required.

## TL;DR

1. Route the task through `docs/ai/task-router.md` before reading source code.
2. After making edits, run `cargo xtask dev-plan` to separate fast checks from the one-time stable change-set gates. The plan is for routing only; it is not validation evidence.
   Before any Linux DVM integration build, also run `make -C driver-domains/linux build-plan`.
   If a build is interrupted, resume the same target without running `clean` or `distclean`.
   For a cached DVM relay source package, its `dev-*` command is only the fast compilation loop. Batch the corresponding `rebuild-*` image or artifact refresh once, after the change set is stable.
   Never clean or rebuild a toolchain for an ordinary relay edit.
3. For every source-code change, preflight all three project MCP servers: Serena, ast-grep, and CodeGraph. Use Serena as the primary semantic navigator and editor; use ast-grep for syntax-structure searches/rules and CodeGraph for call/dependency/impact context. If any one of the three cannot be listed or cannot complete its focused probe, stop source editing and report the exact failing server/tool. Do not continue a code edit with local `rg` or a text-only fallback. Documentation-only and agent-infrastructure edits may use the normal scoped-search fallback.
4. Sub-agents must use **GPT-5.6 terra** with `xhigh` reasoning. Do not use GPT-5.5.
5. Never bypass hooks with `--no-verify`, `--no-gpg-sign`, or equivalent options. Treat hook output as primary evidence.
6. RustOS is in the middle of evacuating functionality from ring0 into user services. Move policy into `rootd`, `syscalld`, `vfsd`, `loaderd`, `netd`, `inputd`, and other named services rather than moving it back into the kernel.
7. Every bug fix must harden the violated contract and add evidence that prevents the same failure class from returning. Every source-code commit must update the applicable Markdown owner/flow contract in the same commit; a source-only commit is incomplete.

## Context Budget

See `docs/ai-map.md` for cache order and `docs/ai/token-policy.md` for the complete rules.

Search before opening files. Load the stable prefix first and place the task text last. Do not include logs in the stable prefix.
An `AGENTS.md` supplied by the environment already counts as loaded. Do not read
it again unless its contents changed. After compaction, recover only the focused
router or handoff section needed for the next decision; do not replay the whole
bootstrap set.

## Tool Usage

Use Serena, ast-grep, and CodeGraph together for source work. Serena owns symbol discovery, references, focused reads, and edits; ast-grep owns syntax-aware pattern/rule matching; CodeGraph owns call graph, dependency, and blast-radius checks. Use ripgrep MCP or local `rg` only for narrowly scoped text/documentation lookup. The three source-editing MCPs are a hard gate: if one fails its preflight or focused query, do not modify source and report the failure.

Optimize model/tool round trips before shaving individual result bytes. Batch
three or more known independent reads, searches, diagnostics, or checks into one
orchestrated tool call, collect the evidence set, and then reason over it. Do not
return to the model after each symbol or file lookup.

Never print complete `ALL_TOOLS` entries or bulk tool descriptions. The known
project namespaces are `mcp__serena__*`, `mcp__ast_grep__*`, and
`mcp__codegraph__codegraph_*`. If discovery is unavoidable, print matching names
only; inspect at most two exact descriptions and cap each at 2,000 characters.

Allow project hooks to run. Do not bypass them with `--no-verify`, `--no-gpg-sign`, or equivalent options.

If a hook blocks execution or reports a tool or configuration failure, repair the hook or command path before continuing. Hook output is the primary signal.

## Sub-Agent Use

Use sub-agents only when doing so reduces search and read churn that would otherwise pollute the main context. Valid cases include parallel independent exploration, log triage, or a disjoint write slice.

Do not use a sub-agent for a single focused read or edit.

Constraints:

* Sub-agents are read-only by default. A worker may write only within an explicitly disjoint file scope.
* Provide only narrow context: the task, relevant paths, and a clear stopping condition.
* Do not provide secrets, signing material, or unrelated repository state.
* Workers must return evidence rather than final decisions: file paths, line numbers, symbols, a short summary, and explicit uncertainty.
* The main agent owns reasoning, integration, validation, and final decisions.

### Model Selection

Repository policy, as directed by the user, requires every sub-agent to use **GPT-5.6 terra** with `xhigh` reasoning.

Do not use GPT-5.5 for sub-agent work.

This policy supersedes the previous mini-first guidance until the user explicitly revises it.

## Do Not Inspect by Default

Do not inspect the following paths or files by default:

* `logs/`
* `target/`
* `build/`
* `vendor/`
* `perf.data`
* `Cargo.lock`

Narrow exceptions are defined in `docs/ai/token-policy.md` §10.

## Common Commands

See `docs/ai/commands.md`.

Commands should remain quiet on success. Treat failure output as primary context. Do not scan log directories to diagnose build failures.
Capture verbose test/build output outside the model context. On success, return
only the command, exit status, and bounded result summary; on failure, return the
first relevant error plus a bounded tail. Cap log searches with match and line
limits.

## Hardening Direction

### Active Refactor: Ring0 Evacuation

`rootd` is the first user process. It starts `syscalld`, `vfsd`, and `loaderd`, then hands control to `initd`.

Move policy into services rather than back into the kernel.

Use `RING3-MIGRATION-REFERENCE` and `RING3-MIGRATION-COMMENTED-OUT` markers only as local annotations. Source ownership, broker call paths, and service contracts are the authoritative migration sources.

### Product Goal

RustOS is a clean, modern, dual-ABI microkernel system with the following goals:

* Preserve native Linux ELF application compatibility.
* Preserve native Windows PE64 and EXE application compatibility.
* Isolate Linux driver stacks inside DVMs.
* Keep policy inside named user services.
* Keep ring0 small, fast, and capability-oriented.

Do not preserve obsolete `.ko`, kernel-extension, driver-private, or undocumented compatibility merely because an older system exposed it.

Migration must not break the explicitly supported observable application ABI.

Removal of ring0 code must proceed through narrow, explicit broker interfaces.

Source-writing, lifecycle, concurrency, commenting, and refactoring requirements are defined in `docs/ai/core-engineering-contract.md`.

## Commercial Completion Bar

No implementation path, document, test plan, or review may excuse a missing required property on the grounds that the system is early, experimental, transitional, or a prototype.

For every enabled product topology, completion requires:

* Explicit ownership and least authority.
* Bounded failure detection and recovery.
* Authenticated and versioned cross-domain contracts.
* Observable latency and throughput limits.
* Evidence that the implementation satisfies its stated invariants.

A mandatory capability that is absent, unverified, or retained only behind a fallback fails the acceptance gate. It must not be treated as a caveat.

After dependency proof is complete, retire replaced code, contracts, and documentation.

Do not preserve a legacy route merely to make an incomplete primary route appear successful.

### General Principles

* Prefer hardening over symptom patches. Make ownership, timeouts, queue bounds, and ABI contracts explicit in source code, manifests, registries, or AI-readable contracts.
* A bug fix is complete only when it names the violated invariant, repairs the root cause, and adds a regression witness for the whole failure class. Testing only the observed input or retaining an errno-only/boolean-only diagnostic does not satisfy this rule.
* Every commit that changes source code must update at least one applicable Markdown contract in the same commit. The Markdown change must record the changed invariant, lifecycle, failure classification, or evidence; release-note filler does not count. If no owner contract exists, create it before committing.
* Fail closed, use bounded waits, and provide direct diagnostics. Never fabricate success.
* For display, input, driver, and compatibility paths, keep fallback providers behind real hardware or virtio providers.
* Validate against black frames, stalls, and provider-order regressions.

## Repository Entrypoints

See `docs/ai-map.md`.

Key entrypoints include:

* `Cargo.toml`
* `tools/xtask/src/cli.rs`
* `kernel/src/main.rs`
* `kernel/*/src/api.rs`
* `libs/runtime-control/src/lib.rs`

For session continuation or handoff, read `docs/ai/session-handoff.md` after loading the stable prefix. Verify all volatile state in that document against the current goal and live Git status.

For physical GPU continuation, read `docs/ai/physical-gpu-status.md` before opening source code or rerunning hardware tests.

## Reporting Discipline

* Ask for or infer the narrow target subsystem before searching.
* Summarize findings before opening additional files.
* Do not paste long command output into responses.
* Keep chat output sparse during implementation. Report only the start, completion, blockers, and decisions that materially affect the work. Do not stream search or build noise.
* On completion, report briefly:

  * What changed.
  * Which validation commands ran.
  * Any remaining blocker or risk.

## Web and Reference Usage

* When designing a new subsystem architecture, external web research is mandatory.
* Periodically reassess whether the existing architecture remains sound, using external web research where appropriate.
* Before beginning any task, inspect the `references` directory and acquire the relevant operating-system engineering background.
* During the task, periodically consult narrowly selected materials from `references` when they are relevant.
* Do not consume excessive reference material when doing so would reduce productivity or use an unreasonable amount of context.
* When a debugging task is high-risk or technically difficult, consult relevant accepted and rejected commit examples from the `references` directory before deciding on a fix.
