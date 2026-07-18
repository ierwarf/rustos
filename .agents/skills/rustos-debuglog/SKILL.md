---
name: rustos-debuglog
description: Filter and summarize RustOS runtime logs (debugcon, serial) without flooding context. Use whenever the user references a boot log, asks why something stalled, or wants to find a panic / service failure. Use INSTEAD of reading log files whole.
---

# RustOS Debug Log Skill

## Files

- `logs/debugcon.log` — primary kernel + service debug stream
- `logs/serial.log` — UART serial output (sometimes duplicated)
- `logs/*.log` — per-run snapshots, may persist across boots
- `build/kvm/rustos-debugcon.log` — current `xtask kvm-smoke` RustOS capture
- `build/kvm/linux-dvm-serial.log` — current Linux DVM relay capture

These files are excluded from the default inspection allow-list in the
repo root `AGENTS.md`. Always treat them as bounded extracts, never as
whole-file reads.

## Standard Filters

| Goal | Command |
|---|---|
| Find first panic | `rg -n --max-count=1 'panic\|BUG\|fatal' logs/debugcon.log` |
| Service startup failures | `rg -n 'service.*(failed\|crashed\|exit)' logs/debugcon.log \| tail -n 30` |
| Stalls / watchdogs | `rg -n 'stall\|timeout\|deadline' logs/debugcon.log \| tail -n 30` |
| UI / surface issues | `rg -n 'wayclick profile\|uiserver wayland callback profile\|uiserver profile\|display unavailable\|black frame' build/kvm/rustos-debugcon.log \| tail -n 30` |
| IPC / broker round-trips | `rg -n 'BROKER\|ipc\|fast.?path' logs/debugcon.log \| tail -n 30` |

For a slow Wayland client, compare three independent rates before editing:

1. WayClick redraw time and commit/callback/release rate.
2. uiserver callback wait and render/present rate.
3. DVM atomic-page-flip relay rate.

Low WayClick redraw time plus low callback/commit rate while uiserver and the
DVM stay near refresh rate points to OS transport or scheduling, not expensive
application drawing. A callback without a matching buffer release is a
different lifetime bug. Treat a `--min-ui-fps` failure as evidence; do not
average independent good windows into success.

## Reporting

When summarizing a log for the user:

1. Report the **first** failure line (file, message, timestamp if shown).
2. Report the **last 5-10 events before the failure** (what was the system
   doing right before it broke).
3. Cite line ranges, not the full output: e.g. "panic at
   `logs/debugcon.log:4821`, preceded by uiserver surface rebuild loop
   from `:4790-:4820`".

## Do Not

- Do not `cat`, broad `head`/`tail`, or read a log without a marker or line
  range. If no marker is known, inspect file size and the last 30 lines once.
- Do not paste more than ~30 lines of log into chat. Summarize instead.
- Do not re-grep the same pattern multiple times — cache the result in
  your reply.
