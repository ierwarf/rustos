# AI Token Policy

Detailed limits for RustOS agents. `AGENTS.md` is the root authority and the
only stable repository prefix; read this file only when context/tool/output
budget details matter.

## Minimal context

Do not reread unchanged bootstrap documents after compaction. Recover only the
current owner/contract, dirty paths, validation state, hypothesis, and next
decision. Human docs are not bootstrap context.

## Search and read

Search before opening large files. Default source read is <=120 lines; expand
only after a focused search identifies the missing range. Start ordinary
searches at 6–12 results and avoid reopening unchanged ranges.

Batch 3–10 known independent probes when that removes model/tool round trips.
Keep an ordinary combined model-visible batch <=8 KiB; a justified critical
batch may reach 12 KiB. Narrow a query before splitting a large batch into many
model turns.

Use `rg` for exact text. Serena is the preferred semantic navigator/editor when
symbols or references matter. Default Serena answer budgets are 3,000–4,000
characters and must not exceed 8,000. Once Serena is useful, prefer continuing
with focused Serena reads/references/edits instead of reverting to broad raw
reads by habit. This is a preference, not a hard requirement.

Never emit full `ALL_TOOLS` objects or bulk tool descriptions. Serena is the
only project source-navigation MCP. If discovery is truly necessary, inspect
only the exact tool needed and keep its description bounded.

## Shell, logs, and diffs

Exploratory shell reads should request `max_output_tokens <=1500`; the hard
interactive ceiling is 3000. Capture larger output to a file and return a
bounded summary. Successful commands expose exit status plus <=1 KiB of useful
summary.

Never read whole logs. On failure return the first useful diagnostic plus at
most 8 trailing lines / 3 KiB, whichever is smaller. Full build, KVM, serial,
and debug logs stay outside model context. For diffs inspect names/stat first,
then relevant hunks only.

Avoid `build/`, `target/`, `logs/`, `vendor/`, `perf.data`, and `Cargo.lock` by
default. Use a bounded matching range only when the task specifically needs it.
Never patch generated/vendor output as a shortcut.

## Validation and long sessions

After product-source edits run `cargo xtask dev-plan`; execute `now` lanes while
iterating and relevant `stable-batch` lanes once after the change settles.
Agent/docs-only changes use focused selftests.

Hooks are silent on success and should emit one primary failure diagnostic plus
a tiny tail. Do not increase output limits/timeouts as routine recovery.

Project Codex config intentionally does not override the model context window or
auto-compaction threshold. Follow the selected model/runtime defaults so this
tracks Codex model metadata rather than pinning an early repo-specific limit.
At milestones update the short handoff and discard unnecessary history.
