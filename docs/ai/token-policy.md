# AI Token Policy

Mandatory context, discovery, output, and stop policy for RustOS agents. Hook and
sub-agent authority lives in root `AGENTS.md`.

## 1. Route Before Reading

Read `task-router.md` before broad exploration. Default task context is:

- `task-router.md`
- one focused AI doc selected by it
- 1–3 source files/ranges named by that doc or a search

Do not preload the AI/human documentation set.

## 2. Human Docs vs AI Docs

Use `docs/ai/` for routing, ownership, stable contracts, and verification. Open
human docs only for user-facing prose or when the AI contract lacks required
behavior.

## 3. Search Before Opening

Prefer Serena symbol search and scoped ripgrep over file dumps. For files over
~500 lines, search first and read one focused range at a time. Default focused
reads are <=200 lines.

Gather known independent evidence together: **batch 3–10 independent operations**
when possible, then reason once. Model/tool round trips usually cost more than a
few hundred extra characters in one focused result.

## 4. AI Docs Are Pointers

Keep AI docs contractual: exact owner paths, stable symbols/values, generated
paths, and verification commands. Do not duplicate explanatory architecture or
large source excerpts.

## 5. Fast Implementation Over Extended Reasoning

When scope is clear, do a short reasoning pass, make the smallest correct edit,
and run the narrow validation. Reserve extended reasoning for debugging,
security/structural review, or explicit design choices.

After edits run `cargo xtask dev-plan`; execute its `now` lanes during the edit
loop and batch `stable-batch` work once after the change set settles. A printed
plan is not validation evidence.

## 6. OS Debugging Stop Rule

Do not patch speculatively. If blocked by missing runtime evidence, ownership,
probe coverage, or a structural inconsistency, stop and report the observed
symptom, last trustworthy evidence, blocker, and exact next evidence/owner.
Never invent a success path or broad fallback.

## 7. Risk-Weighted Hardening

Prioritize app-visible ABI, privilege/capability boundaries, memory/user-copy,
handle lifetime, scheduler/locking/timeout behavior, boot/service ordering, and
mutable I/O paths. Avoid unrelated defensive churn. Name the risk reduced and
enforce it at the narrowest owner boundary.

## 8. Update AI Contracts When Behavior Changes

Update the owning AI contract in the same change when package schema, xtask
behavior, registry fields/paths, logging behavior, kernel `api.rs` boundaries,
runtime protocols, or AI routing changes. Prefer manifests/registries/protocol
state over ad-hoc hardcoded policy.

## 9. Avoid Ad Hoc And Hardcoded Policy

Use existing subsystem APIs and declared sources of truth. If a temporary
hardcoded fallback is unavoidable, keep it local and document what canonical
state will replace it.

## 10. Generated And Vendor Paths

Do not inspect `build/`, `target/`, `logs/`, `vendor/`, `perf.data`, or
`Cargo.lock` unless the task requires it. Narrow exceptions:

- KVM/debug failure -> focused `build/kvm/` or bounded matching log lines.
- Stage verification -> focused `build/image/system/registry/`.
- Firmware/module packaging -> the specific `vendor/` artifact.
- Dependency resolution -> search `Cargo.lock`, then read only the matching range.

## 11. Logs And Command Output

Never read whole logs. Search with both match and line bounds or use a short tail.
Verbose commands write full output to a task-local temporary file.

On success expose only exit status plus bounded gate/test summary lines. On
failure expose the first useful diagnostic and **at most 32 trailing lines or
6 KiB**, whichever is smaller. If that is insufficient, search the captured log
for the failing symbol/stage; only then expand up to the repository hard ceiling
of 120 trailing lines. Do not dump a full build, KVM, serial, or debug log into
model context.

## 12. Prompt Cache Hygiene

Keep this reusable prefix exact and ordered:

1. `AGENTS.md`
2. `docs/ai-map.md`
3. `docs/ai/token-policy.md`
4. `docs/ai/task-router.md`
5. one router-selected focused `docs/ai/*` file

Task text, command output, logs, and source snippets come after it. Documents
already supplied in live context count as read. **Do not reread unchanged bootstrap documents**. After compaction, recover only the needed router/handoff
range and volatile facts; do not replay prior evidence.

## 13. Round-Trip And Progressive-Disclosure Budget

1. Plan the evidence set before tool calls; batch known independent operations.
2. Keep the **combined expected model-visible payload of one tool batch <=24 KiB**.
   Narrow queries before splitting a large batch into many sequential model turns.
3. Exploratory shell reads should request `max_output_tokens <=3000`; the hard
   interactive ceiling is **6000**. Larger command output goes to a task-local
   file (normally under `/tmp`) and only a bounded summary returns to the model.
4. Serena search/read calls default to `max_answer_chars=4000..8000` and must not
   exceed **12000**. Broad substring discovery does not request full bodies;
   locate names first, then read the exact symbol body.
5. ast-grep discovery returns at most **40** matches per call. Tighten the
   structural pattern before asking for more.
6. CodeGraph discovery uses compact results, at most **30** entries, and expands
   one selected symbol/module only after the graph narrows the blast radius.
7. Start ordinary code/path searches at 8–20 results. Increase only when unresolved.
8. Read symbols/ranges, not whole large files. Avoid repeatedly reopening the
   same unchanged range.
9. **Never emit full `ALL_TOOLS` objects** or bulk tool descriptions. Search
   names only, then inspect at most two exact descriptions capped at 2,000
   characters each.
10. Known source namespaces are `mcp__serena__*`, `mcp__ast_grep__*`, and
    `mcp__codegraph__codegraph_*`; do not rediscover their schemas mid-session.
11. For diffs, inspect names/stat first; request only relevant hunks unless a
    complete diff is itself the task.
12. Do not restate the same evidence after every tool result. Keep a short delta
    summary and carry only facts that affect the next decision.
13. At an architectural milestone refresh the short session handoff and use a
    fresh context when available instead of repeatedly filling huge windows.

These limits never weaken the Serena/ast-grep/CodeGraph edit gate, formal
validation, failure diagnosis, or required runtime acceptance.

## 14. Hook And Timeout Budget

Hooks are part of the token/latency budget:

- allow/success paths are silent; only blocks/failures inject model-visible text;
- failure payloads target <=4 KiB and contain one primary diagnostic plus a tiny
  tail; the full log stays out of context;
- source-navigation hooks reject oversized MCP answer/result budgets before the
  tool can inject a large uncached tail into the conversation;
- exploratory shell hooks reject oversized direct-output budgets while allowing
  long work whose verbose output is captured outside model context;
- duplicate validation is cached by an exact content fingerprint, never by a
  time-only success stamp;
- expensive PostToolUse checks may coalesce a short edit burst only when the
  pre-commit gate verifies the final fingerprint before source/build commits;
- external hook helpers use explicit short deadlines;
- bounded commands receive TERM first and KILL after a short grace period;
- an inner command deadline must leave >=10 seconds of margin before its outer
  hook deadline when the hook may do additional work;
- report `timeout` separately from ordinary command failure so agents do not
  waste turns diagnosing a nonexistent compiler/test error.

Do not respond to a timeout by blindly increasing every deadline. First decide
whether the command is a fast interactive gate, a stable-batch gate, or a
runtime/benchmark lane and move long work out of synchronous hooks when needed.
