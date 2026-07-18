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
6. **Count one logical operation** — look for query/wait/query sequences,
   fixed-capacity payload copies, a single receiver serializing callers, or a
   one-slot handoff that overwrites an earlier owner.
7. **Name the ABI boundary** — if every data byte still crosses synchronous
   broker IPC, scheduler hints can reduce wake delay but cannot provide a
   shared data fast path. Record the missing userspace ABI as a failed gate;
   do not add an application-specific ring or move socket policy into ring0.
   Before calling that ABI a small patch, account for bounded ring ownership,
   asymmetric mapping rights, readiness generations/lost-wake closure,
   short-I/O ordering, dup/fork/exec, peer close/shutdown, descriptor and
   credential transfer, revoke, and recovery. If these cross several owners,
   stop and report the interface/model scope before implementing it.

## Common Patterns

| Symptom | Likely Cause | Fix Location |
|---|---|---|
| Syscall takes 2× expected IPC count | No-op broker not bypassed | broker source in `kernel/<area>/` |
| Stall on first call after fork | Shared region pointer not cached per task | call site in `kernel/ps/` |
| Service spins but never replies | Service ring drain stuck on a dep | `services/<svc>d/src/main.rs` |
| Black frame after policy change | Surface re-prime in `apply_runtime_state` | `services/uiserver/` |
| Local socket client is slow while compositor is fast | Per-send/recv synchronous service IPC or redundant readiness queries | `kernel/compat/.../service_ops`, `services/netd/`, ABI crate |
| One caller delays unrelated clients | Single service receiver or unbounded blocking worker | service receiver and fixed worker admission |
| Wake optimization loses an earlier target | One-slot or cross-class handoff overwrite | bounded scheduler FIFO plus exact endpoint authorization |

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
- Do not call a scheduler handoff a data fast path. Validate payload copies,
  request count, service ownership, and end-to-end throughput separately.
