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

Gather independent evidence in batches. When three or more known searches,
symbol reads, diagnostics, or test commands do not depend on one another, issue
them in one orchestrated call and reason once over the combined result. Reducing
model/tool round trips has priority over saving a few hundred characters from a
single focused result.

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

After edits, use `cargo xtask dev-plan` instead of reconstructing the
changed-file validation matrix from prose. Run the listed `now` checks during
the edit loop. Defer `stable-batch` DVM rebuild and KVM preparation until the
related change set settles, then run that batch once. `dev-plan` only selects
commands; a printed command is never evidence that the command passed.

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

- KVM/debug failure investigation → `build/kvm/`.
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

Build and test commands must use a quiet-success wrapper when their normal
output is verbose. Capture complete output in a task-specific temporary file.
If the command passes, expose only its exit status and bounded `test result` or
gate summary lines. If it fails, expose the first relevant diagnostic and at
most 120 trailing lines. Searches over KVM, serial, and debug logs must set both
a match bound (for example `rg -m 30`) and a line/tail bound.

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

Documents already supplied in the live context count as read. In particular,
do not reopen environment-supplied `AGENTS.md`. After compaction or continuation,
use headings/search first and read only the relevant router, policy, or
`session-handoff.md` range unless the file changed.

## 13. Round-Trip And Discovery Budget

The dominant cost in a long repository task is repeatedly reprocessing a large
context, not the size of one small lookup. Apply these rules by default:

1. Plan an evidence set before calling tools; batch 3–10 independent operations.
2. Do not interleave one-symbol lookup and model reasoning when the next lookups
   are already known.
3. Never emit full `ALL_TOOLS` objects or bulk descriptions. Search names only,
   then inspect at most two exact descriptions capped at 2,000 characters each.
4. Call the known RustOS source tools by namespace:
   `mcp__serena__*`, `mcp__ast_grep__*`, and
   `mcp__codegraph__codegraph_*`.
5. Keep success-path command output to exit status and bounded summary lines;
   expand diagnostics only after failure.
6. Do not reread unchanged bootstrap documents already present in context.
7. At a completed architectural milestone, refresh the short session handoff
   and continue in a fresh context when available instead of carrying a task to
   repeated 200K-token windows.
8. A compaction is not permission to rediscover known tool schemas or replay
   earlier evidence. Resume from the compacted state and verify only volatile
   facts.

These constraints must not weaken the Serena/ast-grep/CodeGraph source-editing
gate, failure diagnostics, formal evidence, or required runtime acceptance.
