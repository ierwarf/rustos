---
name: rustos-microkernel-trace
description: Trace RustOS microkernel IPC, broker, scheduler, and service handoff paths to explain stalls or cost. Use for IPC latency, broker routing, fast-path, or service-handoff diagnosis.
---

# RustOS Microkernel Trace

If the diagnosis leads to a source change, load `rustos-code-editing` and pass
the mandatory Serena, ast-grep MCP, and CodeGraph preflight before editing.

## Trace the ownership path

Start with the syscall family and authoritative API surface. Then identify:

- ring0 entry and the narrow broker, if any;
- the owning service (`syscalld`, `vfsd`, `loaderd`, `netd`, `inputd`, or other);
- shared-memory or capability mapping, queue bounds, and readiness generation;
- reply/wakeup, cancellation, timeout, close, restart, and revoke paths.

Use Serena for symbols/references, ast-grep for structural patterns such as
broker-forwarding shapes, and CodeGraph for callers, callees, dependencies,
and blast radius. Local text search is not a substitute for the source-editing
gate.

## Interpretation

Do not call a scheduler handoff a data fast path. Count syscalls, IPC rounds,
payload copies, rendezvous transitions, lock acquisitions, queue depth, and
tail latency separately. A fast path is valid only when capability checks,
ordering, ownership, cancellation, and observable results remain equivalent.

If a broker only forwards data, treat bypassing it as a hypothesis until the
contract proves that no capability arbitration occurs there. Move policy into
the owning user service; never restore ring0 policy to hide a stall.

Report the first trustworthy evidence, the exact path, the remaining unknown,
and the next bounded probe. Do not make performance claims from a single
minimum or from visual/model output.
