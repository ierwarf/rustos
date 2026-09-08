---
name: rustos-code-editing
description: Plan, inspect, edit, and verify RustOS source with tiered semantic, structural, and impact tooling. Use for kernel, service, driver, compatibility, library, or tooling source edits; skip documentation-only changes.
metadata:
  short-description: Tiered Serena-first RustOS source editing
---

# RustOS Code Editing

Choose the cheapest evidence set that can establish correctness. **Do not
preflight an unused MCP server.**

## Tool tiers

### Local

Use for a change contained in one owner/module with no public ABI, privilege,
ownership, lifecycle, concurrency, or cross-module effect. Search the exact
symbol/range first. Serena is the primary editor when semantic context or a
reference-aware edit helps; scoped `rg` is acceptable for exact textual lookup.
ast-grep and CodeGraph are not required.

### Structural

Use when signatures, multiple files/crates, syntax structure, callers, or
dependencies change. Serena remains the primary semantic navigator/editor. Add
ast-grep only when syntax-aware patterns answer a real question, and CodeGraph
only when call/dependency/blast-radius evidence is needed.

### Critical

Use for scheduler, MM, IPC, security/capability, ABI, SMP/TLB, privilege,
lifetime/concurrency, ring0/ring3, and hardware-critical work. Select every tool
needed to prove the affected boundary; broad critical edits normally use Serena,
ast-grep, and CodeGraph. If a selected required tool cannot complete its focused
probe, stop the source edit and report that exact blocker rather than replacing
it with an unverified guess.

## Serena is the primary editor

1. Search symbols/references before opening bodies.
2. Read only the exact symbol/range needed.
3. Prefer reference-aware rename/delete operations for refactors.
4. Use symbol-body or narrow content replacement for edits.
5. Re-query changed symbols/references only when that can catch a regression.

ast-grep expresses syntax rather than formatting. CodeGraph is for call,
dependency, ownership, and blast-radius questions. Detailed argument shapes live
in `references/three-tool-contract.md`; read that file only when exact tool
syntax is actually needed.

## OS correctness pass

For non-local changes, identify execution context and trust boundary. Check the
applicable subset of lock order, IRQ/preemption, blocking/allocation, user copy
and faults, publication/teardown, capability rights/generation, ABI/wire format,
cancellation/timeouts, and partial initialization/hot-unplug. Keep policy in the
owning user service rather than moving it into ring0.

## Validation

After product-source edits run `cargo xtask dev-plan`; execute the selected
`now` checks during the loop and relevant `stable-batch` checks once when the
change set settles. For DVM work run `make -C driver-domains/linux build-plan`
before integration. Never bypass hooks/signing or use clean builds as routine
recovery.

Update an owner Markdown contract only when behavior, interface, invariant,
ownership/lifecycle, registry, validation, or routing changed. Report changed
paths, selected tools, validation, and remaining unverified boundaries briefly.
