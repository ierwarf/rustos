---
name: rustos-microkernel-trace
description: Debug RustOS microkernel IPC offload paths — kernel-to-service brokers, fast-path skip rules, and stall diagnosis. Use when investigating syscall latency, broker stalls, missing fast path, or service handoff bugs. Skip for unrelated debugging.
---

# RustOS Microkernel Trace Skill

## Architecture Recap

Offload syscalls follow this path:

```
ring0 entry  →  narrow privileged broker  →  user service
            (SYS_RUSTOS_*_BROKER)         (syscalld/vfsd/etc.)
```

Brokers exist to **arbitrate capabilities**. When a broker is a no-op
forwarder (just shuttles a request into a shared region and signals the
service), the broker IPC is pure overhead and should be skipped.

## Diagnosis Checklist

When the user reports a syscall stall, slowdown, or unexpected IPC churn:

1. **Identify the syscall family** — MM, VFS, signal, clock, net, etc.
2. **Locate the broker** — `rg "SYS_RUSTOS_.*_BROKER" kernel/`
3. **Inspect the broker body** — does it actually arbitrate, or just
   forward? If forward-only → fast-path candidate.
4. **Check shared region access** — is the kernel pointer to the shared
   region cached at the call site, or re-resolved each call? Re-resolve =
   stall.
5. **Check the service side** — `services/<svc>d/` — is it draining the
   request ring promptly, or blocked on its own dependency?

## Common Patterns

| Symptom | Likely Cause | Fix Location |
|---|---|---|
| Syscall takes 2× expected IPC count | No-op broker not bypassed | broker source in `kernel/<area>/` |
| Stall on first call after fork | Shared region pointer not cached per task | call site in `kernel/ps/` |
| Service spins but never replies | Service ring drain stuck on a dep | `services/<svc>d/src/main.rs` |
| Black frame after policy change | Surface re-prime in `apply_runtime_state` | `services/uiserver/` |

## Cross-Reference

- `kernel/AGENTS.md` — fast-path rule, broker naming
- `services/AGENTS.md` — service authority map, bootstrap traps
- `libs/runtime-control/src/lib.rs` — runtime protocol surface

## Do Not

- Do not "fix" a stall by reintroducing ring0 policy. Move the work into
  the owning service instead.
- Do not add a new broker unless the new syscall family genuinely needs
  capability arbitration. Otherwise the service can be called directly
  through an existing port.
