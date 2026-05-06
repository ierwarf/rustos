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

## 5. Update AI Contracts When Behavior Changes

If a change modifies any of these, update `docs/ai/contracts.md` or the focused
AI map in the same change:

- package manifest schema
- xtask command behavior
- generated registry path or field contract
- logging category/level behavior
- kernel `api.rs` boundary
- runtime socket/protocol behavior
- docs navigation or AI routing

## 6. Avoid Ad Hoc And Hardcoded Policy

Prefer manifest fields, registries, protocol state, and existing subsystem APIs
over ad hoc branches or hardcoded names, paths, priorities, and ordering. If a
temporary hardcoded fallback is unavoidable, keep it narrow, document the source
of truth it is standing in for, and route future behavior through the stable
contract instead of expanding the special case.

## 7. Avoid Generated And Vendor Paths By Default

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

## 8. Logs And Large Files

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

## 9. Prompt Cache Hygiene

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
