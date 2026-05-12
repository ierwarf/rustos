# AI Token Policy

This file is mandatory operating policy for AI agents working in this repo.

## 1. Start With Task Router

Always read `docs/ai/task-router.md` before broad repo exploration.

Default context set:

- `docs/ai/task-router.md`
- one focused AI doc selected by the router
- one to three source files named by that focused doc

Do not preload all AI docs or all human docs.

## 2. Keep Human Docs And AI Docs Separate

Human docs are bilingual and explanatory. AI docs are English-only contracts.

Use human docs only when:

- writing or revising prose docs
- checking user-facing wording
- AI contracts are missing the needed behavior

Use AI docs for implementation routing, source ownership, stable contracts, and
verification commands.

## 3. Search Before Reading Large Files

Use `rg` before opening broad files.

Preferred pattern:

```bash
rg -n "symbol_or_contract" kernel services tools libs drivers apps docs
sed -n 'START,ENDp' path/to/file
```

Avoid opening files over roughly 500 lines from the top unless the task is a
full-file review.

## 4. AI Docs Store Source-Of-Truth Paths, Not Long Explanations

AI docs should point to canonical source files and stable contracts.

Do:

- list exact source paths
- list stable enum/value names
- list generated output paths
- list verification commands

Do not:

- duplicate long bilingual human docs
- paste large source excerpts
- explain background architecture unless it changes routing decisions

## 5. Prefer Fast Implementation Over Extended Reasoning

Default to a short reasoning pass, then make the smallest source change that
can satisfy the task. Do not spend time producing broad theory, long option
lists, or exhaustive subsystem analysis when the requested scope is already
clear.

Do:

- identify the narrow owner file or contract
- state the concrete edit target if needed
- implement promptly
- validate with the smallest relevant command

Spend extended reasoning time only when the user asks for debugging, failure
analysis, structural review, security review, or a design decision. For
debugging, reason from symptoms, command output, logs, or probes before editing.

## 6. OS Debugging Stop Rule

RustOS debugging must not drift into speculative patches. If execution is
blocked by a structural inconsistency, missing ownership boundary, missing
probe, unavailable runtime evidence, or a fix that would only guess at the
cause, stop changing code and report:

- observed symptom
- last trustworthy evidence
- structural blocker
- exact next evidence or owner needed

Do not fabricate a success path, add broad fallbacks, or keep hardening nearby
code just because the original path is unclear.

## 7. Risk-Weighted Hardening

Harden the highest-risk OS surfaces first:

- app-visible ABI and Linux ELF / Windows PE compatibility
- privilege, capability, broker, and namespace boundaries
- memory mapping, user-copy, handle-transfer, and lifetime checks
- scheduler, lock ordering, IRQ-off, wait, and timeout behavior
- boot, launch, service ownership, provider ordering, and driver loading
- filesystem, network, input, display, and block-device mutation paths

Avoid hardening low-risk helpers, cosmetic paths, or unrelated code unless the
user explicitly asks. Every hardening change should name the risk it reduces
and use the narrowest source boundary that can enforce it.

## 8. Update AI Contracts When Behavior Changes

If a change modifies any of these, update `docs/ai/contracts.md` or the focused
AI map in the same change:

- package manifest schema
- xtask command behavior
- generated registry path or field contract
- logging category/level behavior
- kernel `api.rs` boundary
- runtime socket/protocol behavior
- docs navigation or AI routing

## 9. Avoid Ad Hoc And Hardcoded Policy

Prefer manifest fields, registries, protocol state, and existing subsystem APIs
over ad hoc branches or hardcoded names, paths, priorities, and ordering. If a
temporary hardcoded fallback is unavoidable, keep it narrow, document the source
of truth it is standing in for, and route future behavior through the stable
contract instead of expanding the special case.

## 10. Avoid Generated And Vendor Paths By Default

Do not inspect these paths unless the task explicitly involves generated output
or external binary inputs:

- `build/`
- `target/`
- `logs/`
- `vendor/`
- `perf.data`
- `Cargo.lock`

Allowed exceptions:

- QEMU/debug failure investigation may inspect `logs/`.
- Stage verification may inspect `build/image/system/registry/`.
- Firmware/module packaging may inspect specific `vendor/` paths.
- Dependency resolution work may inspect focused `Cargo.lock` snippets.

When using an exception, inspect the narrowest file/path possible.

## 11. Logs And Large Files

Never read whole log files by default.

Preferred patterns:

```bash
tail -n 120 logs/debugcon.log
rg -n "panic|error|failed|DisplayUnavailable" logs/debugcon.log
sed -n 'START,ENDp' path/to/large.rs
```

For files over roughly 500 lines:

- search with `rg -n` first
- open one focused range
- summarize findings before opening another range

Avoid opening `Cargo.lock` unless dependency resolution changed. Use
`rg -n "crate-name" Cargo.lock` before reading a range.

## 12. Prompt Cache Hygiene

Keep stable context stable across requests:

- put durable repo instructions first
- keep task-specific details and pasted output at the end
- avoid rewriting stable instruction text mid-session
- avoid adding logs or generated output to the reusable prefix

OpenAI-style prompt caching works best when the beginning of the prompt is an
exact reusable prefix. Treat this sequence as the stable prefix:

1. `AGENTS.md`
2. `docs/ai-map.md`
3. `docs/ai/token-policy.md`
4. `docs/ai/task-router.md`
5. one focused `docs/ai/*` file selected by the router

Put user-specific task text, command output, logs, and file snippets after that
prefix.

For Gemini-style explicit context caching, cache the same stable prefix for
repeated repository analysis or bug-fixing sessions. Do not cache generated
output, logs, or broad source dumps; attach those as short task-specific suffixes
only when needed.
