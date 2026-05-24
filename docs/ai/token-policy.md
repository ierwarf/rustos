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

Use Serena MCP symbol tools for code structure before broad text search. Use
the ripgrep MCP server before opening broad files. Do not use shell `rg` as a
fallback for normal repo exploration.

Preferred pattern:

- Search with ripgrep MCP, scoped to the smallest relevant path.
- Read only exact line ranges or focused files after MCP search identifies them.

Avoid opening files over roughly 500 lines from the top unless the task is a
full-file review.

## 3.1. Use MCP Servers And Hooks

Use available project MCP servers actively:

- Serena for project activation, symbol overview, declarations, references, and
  focused source reads.
- ripgrep MCP for raw text search, file listing, and match counts.
- GitHub MCP/plugin for GitHub repository, PR, issue, and Actions workflows;
  use `gh` only when the MCP tool lacks a specific required operation after MCP
  availability has been confirmed.

Let configured Codex hooks run and use their output as primary evidence. Do not
bypass hooks. If a required project-scoped MCP server is broken or unavailable,
stop and report the MCP failure instead of falling back to shell search tools.
Fix the local configuration or hook script only when the task explicitly asks
for MCP/hook repair or the failure is clearly inside this repo's config.

## 3.2. MCP Explorer Sub-Agent

For exploration, the main agent may spawn a read-only `gpt-5.4-mini` explorer
sub-agent when it would avoid repeated search/read churn. Use
`reasoning_effort = high` by default.

Rules:

- Use the mini agent only for MCP-backed exploration.
- Do not spawn the mini agent for trivial one-file lookups or when one direct
  MCP call is enough.
- Use multiple explorer sub-agents in parallel only when the questions are
  independent and the results can reduce repeated search/read churn.
- Do not use sub-agents for a single focused read or a single focused edit.
- The mini agent must use Serena MCP for symbols, declarations, references, and
  focused reads.
- The mini agent must use ripgrep MCP for raw text search, file listing, and
  match counts.
- The mini agent must not use shell `rg`, `grep`, broad `find`, direct log
  reads, or edit tools.
- If required MCP is unavailable, the mini agent must stop and report MCP
  failure.
- The mini agent returns compact evidence only: files, line numbers,
  symbols/contracts, short summaries, and confidence/uncertainty. Omit empty
  fields.
- The main agent owns reasoning, edits, validation, and final decisions.
- `reasoning_effort = medium` is allowed only for simple read-only location
  tasks: file listing, exact symbol lookup, literal text search, and doc-policy
  checks.
- Do not use `gpt-5.4-mini` at `xhigh` or `low` for explorer work.

## 3.3. Sub-Agent Roles

The main agent owns reasoning, edits, validation, and final decisions.
Sub-agents are optional. Use them only when they reduce waste from repeated
searching, large log triage, or independent verification.

| Role | Model / effort | Allowed scope | Output |
| --- | --- | --- | --- |
| Explorer | `gpt-5.4-mini high`; `medium` only for simple location tasks | Read-only MCP-backed source/doc exploration | Evidence packet with files, lines, symbols, summaries, confidence, unknowns |
| Verifier | Main model or `gpt-5.4-mini high` | Read-only review of a completed patch, focused on likely regressions | Risks, missing tests, suspicious file/line references |
| Log summarizer | Dedicated log summarizer role when available, otherwise `gpt-5.4-mini high` | Approved QEMU/debug log snippets only; no broad source exploration | Last progress marker, panic/stall evidence, ordering summary |
| Worker | Main model preferred; use sub-agent only with disjoint write ownership | Narrow implementation slice with explicit file/module ownership | Changed paths, validation run, blockers |

Sub-agent rules:

- Use a fan-out/fan-in workflow:
  1. Main agent classifies the task and names the critical path.
  2. Main agent keeps the immediate blocking task local.
  3. Sub-agents handle only independent sidecar exploration, log summarization,
     verification, or disjoint write slices.
  4. Main agent integrates the returned evidence before deciding or editing.
- Do not duplicate the main agent's immediate blocking task.
- Spawn agents only for bounded sidecar work that can run in parallel or reduce
  main-context search cost. Do not spawn one just to satisfy process.
- Parallel sub-agents are allowed when their questions or write scopes are
  independent. Avoid parallelism when the next main step depends on one result
  or when coordination overhead would exceed the saved context.
- Track active sub-agents mentally as `agent / role / scope / status / output`.
  Do not create a process-heavy dashboard unless the task is long-running.
- Give every sub-agent a concrete question and a stop condition.
- For write-capable workers, assign disjoint file/module ownership and tell the
  worker not to revert unrelated changes.
- Treat sub-agent output as evidence, not a final decision.
- Close sub-agents when their result has been integrated.

Evidence packet format:

```text
Question:
MCP used:
Files/lines:
Symbols/contracts:
Relevant summary:
Confidence:
Unknowns:
```

Omit fields that would be empty or redundant.

Worker output format:

```text
Scope:
Changed paths:
Validation:
Blockers:
```

Use worker sub-agents sparingly. They are appropriate for docs-only,
tooling-only, service-only, or otherwise disjoint implementation slices. Keep
kernel/service ABI, broker boundaries, runtime protocols, and root-cause
debugging under the main agent unless the write scope is exceptionally clear.

## 3.4. Commercial Sub-Agent Controls

Sub-agent use must stay auditable, bounded, and cheap enough to justify its
coordination cost.

Commercial controls:

- Cost control: spawn only when the saved search/log context is likely larger
  than the coordination overhead. Prefer `gpt-5.4-mini medium` for simple
  location tasks and `high` for cross-file ownership or log evidence.
- Auditability: every sub-agent result must name the question, scope, tools
  used, and evidence paths. Do not accept ungrounded summaries.
- Data minimization: pass only the task, relevant paths, and constraints. Do
  not forward large logs, generated output, secrets, credentials, or unrelated
  repo context.
- Authority boundary: sub-agents do not approve architecture, compatibility,
  security, or ABI decisions. The main agent owns those decisions.
- Failure handling: if a sub-agent times out, loses MCP access, or returns
  vague evidence, close it and continue locally or report the blocker. Do not
  spawn replacement agents repeatedly for the same unclear question.
- Integration control: before using a worker result, the main agent reviews the
  changed paths, checks for overlap with local/user changes, and runs the
  narrowest relevant validation.
- Traceability: final summaries should mention material sub-agent use only when
  it affected the result, including the role and evidence category, not long
  internal transcripts.
- Security: never ask sub-agents to inspect secrets, tokens, private keys,
  signing material, or credential stores. If such files are implicated, stop
  and ask for explicit handling direction.

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

- `tail -n 120 logs/debugcon.log` only for approved log exceptions.
- ripgrep MCP search for `panic|error|failed|DisplayUnavailable` in the
  relevant log file.
- focused source reads for exact `START..END` ranges after MCP identifies them.

For files over roughly 500 lines:

- search with Serena or ripgrep MCP first
- open one focused range
- summarize findings before opening another range

Avoid opening `Cargo.lock` unless dependency resolution changed. Use
ripgrep MCP to search `crate-name` before reading a focused range.

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
