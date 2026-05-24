# AI Token Policy

Mandatory operating policy for AI agents in this repo. Sub-agent rules and hook
rules live in root `AGENTS.md`; this file owns context budget, search
discipline, and stop rules.

## 1. Route Before Reading

Always read `task-router.md` before broad repo exploration.

Default context set:

- `task-router.md`
- one focused AI doc selected by the router
- 1–3 source files named by that focused doc

Do not preload all AI docs or all human docs.

## 2. Human Docs vs AI Docs

Human docs (`docs/` outside `docs/ai/`) are bilingual and explanatory. AI docs
are English-only contracts.

Use human docs only when:

- writing or revising prose docs
- checking user-facing wording
- AI contracts are missing the needed behavior

For implementation routing, source ownership, stable contracts, and
verification commands, use AI docs.

## 3. Search Before Opening

Prefer symbol-aware search (Serena MCP) and scoped text search (ripgrep MCP)
over opening files. Read only exact line ranges or focused files after the
search identifies them. Avoid opening files over ~500 lines from the top
unless the task is a full-file review.

For files over ~500 lines: search first, open one focused range, summarize
findings before opening another range.

## 4. AI Docs Are Pointers, Not Essays

AI docs point to canonical source files and stable contracts.

Do: list exact source paths, stable enum/value names, generated output paths,
verification commands.

Do not: duplicate bilingual human docs, paste large source excerpts, explain
background architecture unless it changes routing decisions.

## 5. Fast Implementation Over Extended Reasoning

Default to a short reasoning pass, then make the smallest source change that
satisfies the task. Do not produce broad theory, long option lists, or
exhaustive subsystem analysis when the scope is already clear.

Do: identify the narrow owner file or contract, state the concrete edit target
if needed, implement, validate with the smallest relevant command.

Reserve extended reasoning for debugging, failure analysis, structural review,
security review, or explicit design decisions. For debugging, reason from
symptoms, command output, logs, or probes before editing.

## 6. OS Debugging Stop Rule

Do not drift into speculative patches. If execution is blocked by a structural
inconsistency, missing ownership boundary, missing probe, unavailable runtime
evidence, or a fix that would only guess at the cause — stop changing code and
report:

- observed symptom
- last trustworthy evidence
- structural blocker
- exact next evidence or owner needed

Do not fabricate a success path, add broad fallbacks, or harden nearby code
just because the original path is unclear.

## 7. Risk-Weighted Hardening

Harden highest-risk surfaces first:

- app-visible ABI and Linux ELF / Windows PE compatibility
- privilege, capability, broker, and namespace boundaries
- memory mapping, user-copy, handle-transfer, and lifetime checks
- scheduler, lock ordering, IRQ-off, wait, and timeout behavior
- boot, launch, service ownership, provider ordering, driver loading
- filesystem, network, input, display, block-device mutation paths

Avoid hardening low-risk helpers, cosmetic paths, or unrelated code unless
asked. Every hardening change should name the risk it reduces and use the
narrowest source boundary that can enforce it.

## 8. Update AI Contracts When Behavior Changes

If a change modifies any of the following, update `contracts-infra.md`,
`contracts-abi.md`, or the focused AI map in the same change:

- package manifest schema
- xtask command behavior
- generated registry path or field contract
- logging category/level behavior
- kernel `api.rs` boundary
- runtime socket/protocol behavior
- docs navigation or AI routing

## 9. Avoid Ad Hoc And Hardcoded Policy

Prefer manifest fields, registries, protocol state, and existing subsystem
APIs over ad hoc branches or hardcoded names, paths, priorities, ordering. If
a temporary hardcoded fallback is unavoidable, keep it narrow, document the
source of truth it stands in for, and route future behavior through the
stable contract.

## 10. Generated And Vendor Paths

Do not inspect these unless the task explicitly involves generated output or
external binary inputs: `build/`, `target/`, `logs/`, `vendor/`, `perf.data`,
`Cargo.lock`.

Allowed exceptions (inspect the narrowest file/path possible):

- QEMU/debug failure investigation → `logs/`.
- Stage verification → `build/image/system/registry/`.
- Firmware/module packaging → specific `vendor/` paths.
- Dependency resolution work → focused `Cargo.lock` snippets via `rg` first.

## 11. Logs

Never read whole log files. Preferred:

- `tail -n 120 logs/debugcon.log` for approved log exceptions.
- scoped search for `panic|error|failed|DisplayUnavailable` in the relevant log.
- focused source reads for exact `START..END` ranges after search.

Avoid opening `Cargo.lock` unless dependency resolution changed. Search for
`crate-name` before reading a focused range.

## 12. Prompt Cache Hygiene

Prompt caching depends on an exact reusable prefix. Treat this as the stable
prefix, in order:

1. `AGENTS.md`
2. `docs/ai-map.md`
3. `docs/ai/token-policy.md`
4. `docs/ai/task-router.md`
5. one focused `docs/ai/*` file selected by the router

Put user task text, command output, logs, and file snippets *after* that
prefix. Do not rewrite stable instruction text mid-session. Do not cache logs
or broad source dumps.
