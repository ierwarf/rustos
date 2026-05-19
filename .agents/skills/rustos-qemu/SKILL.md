---
name: rustos-qemu
description: Boot RustOS in QEMU and capture serial/debugcon logs for inspection. Use when the user asks to run, boot, test, or reproduce a bug in RustOS — and when triaging boot failures, stalls, or black-frame regressions. Skip for non-RustOS projects.
---

# RustOS QEMU Skill

## Entry Points

- QEMU runner source: `tools/xtask/src/qemu.rs`
- Boot artifacts come from `cargo xtask build` (or `build-kernel` + `stage`)
- Serial / debugcon output is captured to `logs/debugcon.log` and
  `logs/serial.log` (paths may vary; check `qemu.rs` for current names)

## Standard Run

```sh
cargo xtask build && cargo xtask qemu
```

For a quicker iteration loop when only the kernel changed:

```sh
cargo xtask build-kernel && cargo xtask stage && cargo xtask qemu
```

## Log Hygiene (Critical)

`logs/debugcon.log` can grow to several MB on a normal boot. **Never**
`cat` it or open it whole — that destroys context budget.

Correct way to inspect:

```sh
# tail of the most recent run
tail -n 200 logs/debugcon.log

# search for a specific panic / module
rg -n "panic|BUG|service.*failed" logs/debugcon.log | tail -n 50

# narrow to a service
rg -n "uiserver|vfsd" logs/debugcon.log | tail -n 100
```

## Triage Order for Boot Failures

1. Last 100-200 lines of `logs/debugcon.log` — get the immediate symptom.
2. `rg "panic|BUG|unwrap|expected"` — find the first hard fault.
3. If the failure is in early bootstrap (before `runtimed`), check
   `services/runtimed/` and recall the bootstrap-ordering trap in
   `services/AGENTS.md`.
4. If it's a black frame / stall, check `uiserver`'s
   `apply_runtime_state` per the same notes.

## Do Not

- Do not paste full log output into chat replies.
- Do not re-run QEMU after every tiny edit. Use `cargo xtask check` first.
- Do not enable extra serial channels or verbose tracing without first
  checking if `rg` on the existing log already answers the question.
