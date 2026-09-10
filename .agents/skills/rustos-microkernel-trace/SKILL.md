---
name: rustos-microkernel-trace
description: Trace RustOS IPC, broker, scheduler, and service handoff paths to explain stalls or cost. Use for IPC latency, routing, fast-path, or service-handoff diagnosis.
---

# RustOS Microkernel Trace

Start from the syscall/API and trace only the path needed to answer the current
cost or stall question: ring0 entry/broker, owning service, shared-memory or
capability handoff, reply/wakeup, and relevant cancel/timeout/revoke path.

Use `rg` for markers/exact text and Serena for symbols/references. If diagnosis
leads to a source change, use `rustos-code-editing`. There is no multi-MCP
preflight gate.

Count syscalls, IPC rounds, copies, rendezvous transitions, lock acquisitions,
queue depth, and tail latency separately. Do not call a scheduler handoff a data
fast path. Treat broker bypass as a hypothesis until capability/ownership policy
is proven equivalent, and do not move policy back into ring0 to hide cost.

Report the first trustworthy evidence, exact path, remaining unknown, and next
bounded probe. Do not infer performance from a single minimum or visual/model
output.
