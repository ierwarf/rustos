# AI Token Policy

Detailed limits for RustOS agents. `AGENTS.md` is the root authority and the
only stable repository prefix; read this file only when context/tool/output
budget details matter.

## Minimal context

Do not reread unchanged bootstrap documents after compaction. Recover only the
current owner/contract, dirty paths, validation state, hypothesis, and next
decision. Human docs are not bootstrap context.

## Search and read

Search before opening large files. Only binary artifacts and whole-file reads
of log/build/vendor output or text above 1 MiB are blocked; symbol navigation
and ranged reads are never limited. Read as much source as the task needs,
including whole files and several subsystems at once; the aim is to keep logs
and generated output out of context, not to limit what may be explored. Avoid
reopening unchanged ranges.

Batch known independent probes when that removes model/tool round trips.

Use `rg` for exact text. Serena is the preferred semantic navigator/editor when
symbols or references matter. Serena answer sizes are not capped by policy. Once Serena is useful, prefer continuing
with focused Serena reads/references/edits instead of reverting to broad raw
reads by habit. This is a preference, not a hard requirement.

Never emit full `ALL_TOOLS` objects or bulk tool descriptions. Serena is the
only project source-navigation MCP. If discovery is truly necessary, inspect
only the exact tool needed and keep its description bounded.

## Shell, logs, and diffs

Exploratory shell reads should stay small (guidance, not enforced). Capture larger output to a file and return a
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

No hook runs a check after an edit or at commit time except the agent-policy
selftest when agent policy files are committed. Validation is chosen by the task
and run after a coherent batch of changes: `cargo xtask dev-plan` suggests the
checks for the changed files, `cargo xtask check` is the quick compile gate, and
formal/KVM runs happen when the user asks for them or a task needs them. The
hooks that remain only stop irreversible mistakes: destructive shell commands,
force-push, hook bypass, secrets in prompts, and reading binaries or log
dumps.

Hooks are silent on success and emit one primary failure diagnostic plus a tiny
tail.

Project Claude Code config intentionally does not override the model context window or
auto-compaction threshold. Follow the selected model/runtime defaults so this
tracks Claude Code model metadata rather than pinning an early repo-specific limit.
At milestones update the short handoff and discard unnecessary history.
