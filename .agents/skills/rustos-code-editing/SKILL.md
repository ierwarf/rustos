---
name: rustos-code-editing
description: Inspect, edit, and verify RustOS source with focused Serena-first navigation. Use for kernel, service, driver, compatibility, library, or tooling source edits; skip documentation-only changes.
metadata:
  short-description: Focused Serena-first RustOS editing
---

# RustOS Code Editing

Use the cheapest evidence that establishes correctness.

## Navigation and editing

- Use `rg` for exact text and Serena for symbols, references, callers, and
  semantic edits.
- Serena is preferred, not mandatory. If Serena is already useful for the task,
  keep using focused Serena operations instead of drifting back to broad
  `sed`/whole-file reads or manual editing by habit.
- Use `apply_patch` or narrow shell/text operations when they are smaller,
  clearer, or Serena is unavailable/ill-suited.
- Read only the exact symbol/range needed; after edits prefer the changed symbol
  or `git diff -U3` over rereading a large unchanged region.
- There is no ast-grep/CodeGraph preflight requirement.

## Correctness

For structural/critical work, identify execution context and trust boundary and
check only the applicable subset: lock order, IRQ/preemption,
blocking/allocation, user copy/faults, publication/teardown, capability rights
and generation, ABI/wire format, cancellation/timeouts, and partial
initialization/hot-unplug. Keep policy in the owning user service.

## Validation

After product-source edits run `cargo xtask dev-plan`; execute selected `now`
checks during iteration and relevant `stable-batch` checks once when stable. For
DVM work run `make -C driver-domains/linux build-plan` before integration.
Never bypass hooks/signing or use clean builds as routine recovery.

Update an owner Markdown contract only when behavior, interface, invariant,
ownership/lifecycle, registry, validation, or routing changed. Report changed
paths, validation, and remaining unverified boundaries briefly.
