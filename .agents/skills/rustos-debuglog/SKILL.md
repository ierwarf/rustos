---
name: rustos-debuglog
description: Filter and summarize RustOS runtime logs (debugcon, serial) without flooding context. Use whenever the user references a boot log, asks why something stalled, or wants to find a panic / service failure. Use INSTEAD of reading log files whole.
---

# RustOS Debug Log Skill

## Files

- `logs/debugcon.log` — primary kernel + service debug stream
- `logs/serial.log` — UART serial output (sometimes duplicated)
- `logs/*.log` — per-run snapshots, may persist across boots

These files are excluded from the default inspection allow-list in the
repo root `AGENTS.md`. Always treat them as bounded extracts, never as
whole-file reads.

## Standard Filters

| Goal | Command |
|---|---|
| Find first panic | `rg -n --max-count=1 'panic\|BUG\|fatal' logs/debugcon.log` |
| Service startup failures | `rg -n 'service.*(failed\|crashed\|exit)' logs/debugcon.log \| tail -n 30` |
| Stalls / watchdogs | `rg -n 'stall\|timeout\|deadline' logs/debugcon.log \| tail -n 30` |
| UI / surface issues | `rg -n 'surface\|ConsoleWindow\|vsync\|black frame' logs/debugcon.log \| tail -n 30` |
| IPC / broker round-trips | `rg -n 'BROKER\|ipc\|fast.?path' logs/debugcon.log \| tail -n 30` |
| Show last N events | `tail -n 200 logs/debugcon.log` |

## Reporting

When summarizing a log for the user:

1. Report the **first** failure line (file, message, timestamp if shown).
2. Report the **last 5-10 events before the failure** (what was the system
   doing right before it broke).
3. Cite line ranges, not the full output: e.g. "panic at
   `logs/debugcon.log:4821`, preceded by uiserver surface rebuild loop
   from `:4790-:4820`".

## Do Not

- Do not `cat`, `head -n 9999`, or `Read` a log without a line range.
- Do not paste more than ~30 lines of log into chat. Summarize instead.
- Do not re-grep the same pattern multiple times — cache the result in
  your reply.
