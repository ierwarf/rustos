# AI Token Policy

Detailed context/output limits for RustOS agents. Root authority is `AGENTS.md`;
this file is **not** part of the stable prefix and is read only when routing,
tooling, large-output, or context-budget details matter.

## 1. Minimal context

The reusable repository prefix is exactly `AGENTS.md`. Read
`docs/ai/task-router.md` only when owner/contract/validation routing is unclear;
otherwise go directly to the focused contract/source range. `docs/ai-map.md`
is an on-demand index, not bootstrap.
Documents already present in live context count as read. **Do not reread unchanged bootstrap documents** after compaction; recover only the focused
contract/handoff range and volatile facts needed next.

Human docs are for user-facing prose or missing behavior; AI contracts own
routing, ownership, stable values, and verification.

## 2. Search and read budget

Search before opening large files. Default source read is <=120 lines; expand
only after a focused search identifies the missing range. Start ordinary
searches at 6–12 results. Avoid reopening unchanged ranges.

Gather known independent evidence together: **batch 3–10 independent operations** when that avoids round trips, but do not collect speculative data.
Keep an ordinary combined model-visible tool batch <=8 KiB; a justified
cross-module/critical batch may reach 12 KiB. Narrow the query before splitting
a token-heavy batch into many model turns.

Serena answer budgets default to 3,000–4,000 characters and must not exceed **8000**. ast-grep discovery returns at most 24 matches; CodeGraph compact
discovery at most 16 entries. Expand only a selected symbol/module.

**Never emit full `ALL_TOOLS` objects** or bulk tool descriptions. Known source
namespaces are `mcp__serena__*`, `mcp__ast_grep__*`, and
`mcp__codegraph__codegraph_*`; call known tools directly. If discovery is truly
necessary, inspect at most two exact descriptions capped at 2,000 characters.

## 3. Shell, logs, and diffs

Exploratory shell reads should request `max_output_tokens <=1500`; the hard
interactive ceiling is **3000**. Capture larger output under `/tmp` and return a
bounded summary. Successful commands expose exit status plus <=1 KiB of useful
summary.

Never read whole logs. On failure expose the first useful diagnostic plus at
most 8 trailing lines / 3 KiB, whichever is smaller. Search the captured log
for the failing symbol/stage before expanding. Full build, KVM, serial, or debug
logs stay outside model context.

For diffs inspect names/stat first, then relevant hunks. A complete diff is read
only when it is itself the task.

## 4. Protected/generated paths

Do not inspect `build/`, `target/`, `logs/`, `vendor/`, `perf.data`, or
`Cargo.lock` by default. Narrow exceptions:

- KVM/debug failure: bounded matching `build/kvm/` evidence.
- Stage verification: focused generated registry/artifact metadata.
- Firmware/module packaging: the named vendor artifact.
- Dependency resolution: search `Cargo.lock`, then one matching range.

Never patch generated/vendor output as a shortcut.

## 5. Reasoning and tool selection

Use the Local/Structural/Critical tiers in `AGENTS.md`. Do not preflight an MCP
server the task does not need. Reserve extended reasoning for debugging,
security/structural review, explicit design choices, and critical boundaries.
When scope and owner are clear, implement the smallest correct slice and run the
narrow validation.

Do not patch speculative OS failures. If runtime evidence, ownership, or a
required focused tool is missing, report the observed symptom, trustworthy
evidence, blocker, and exact next probe.

## 6. Validation and hooks

After product-source edits run `cargo xtask dev-plan`; execute `now` lanes while
iterating and relevant `stable-batch` lanes once after the related set settles.
Agent/docs-only changes use their focused selftests and do not pay unrelated
product validation.

Hooks are part of the context budget: allow/success paths are silent; failures
should contain one primary diagnostic plus a tiny tail (target <=2 KiB). Cache
duplicate validation only by exact content fingerprint. Report timeouts
separately from compiler/test failures and do not respond by blindly increasing
timeouts.

## 7. Long sessions

Keep task text/output after the stable prefix so cached content remains stable.
At an architectural milestone update the short session handoff and prefer
compaction/fresh context over filling a huge window. Project Codex config uses
an early auto-compaction threshold; after compaction recover only current owner,
hypothesis, dirty paths, validation state, and next decision.
