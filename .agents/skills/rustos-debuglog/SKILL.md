---
name: rustos-debuglog
description: Filter and summarize RustOS debugcon or serial runtime logs with bounded evidence. Use for boot stalls, panics, service failures, and runtime symptoms; do not use for source edits.
---

# RustOS Debug Logs

Keep the main context small. Do not read a log whole. Search a marker first,
then extract only the relevant lines from `logs/` or the focused KVM captures
under `build/kvm/`.

Useful bounded filters:

```sh
rg -n -m 1 'panic|BUG|fatal' logs/debugcon.log
rg -n 'service.*(failed|crashed|exit)' logs/debugcon.log | tail -n 30
rg -n 'stall|timeout|deadline' logs/debugcon.log | tail -n 30
```

For KVM, use the command's failure output first, then inspect only matching
lines in `build/kvm/rustos-debugcon.log` and
`build/kvm/linux-dvm-serial.log`. Do not turn a visual observation, one good
window, or a provider-side FPS number into end-to-end evidence.

## Report

Return the earliest relevant failure with file and line, the five to ten
events immediately before it, the likely owning subsystem, and the next
focused source query. Distinguish fact from inference. If source must change,
stop log triage and load `rustos-code-editing`; its three-MCP preflight is a
hard gate.

Do not propose speculative fixes from a log alone. Preserve the first failure
and its causal context instead of averaging later healthy events into success.
