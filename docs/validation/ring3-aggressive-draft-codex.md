# Codex handoff: aggressive Ring3 integration draft (2026-10-31)

**Not production-ready.** This source batch was intentionally authored without
running a compiler, test harness, boot smoke or CI in this session. The user
requested that Codex perform exhaustive reviews and fixes. Preserve existing
changes, and do not call this a complete eight-stage migration.

## Invariants that must survive the repair

- Kernel retains IPC transfer and reply, scheduler, futex, getpid/clock,
  page fault, pager and prepared TLB commit mechanics.
- Ring3 owns configuration, services, policy, supervisor and recovery.
- The kernel owns trusted Job identity, process membership, page accounting
  and exact resource-limit enforcement; raw PIDs and Job IDs grant nothing.
- Every funded session creates one and only one charge, and retires exactly
  one charge even after process exit or provider restart.
- Job slot generations, session generations and PID custody must never be
  reused to authorize old requests.
- No new ring3 IPC, allocator call, block or policy callback inside kernel
  frame charging, schedule, futex or IPC entry.

## Draft code graph

- kernel/ps/src/multitask/process_table/jobs.rs
  Hierarchical session limits/usage, client PID Job binding, stale Job
  retirement, memory high/min policy and parent reservation checks.
- kernel/mm/src/memory/charge.rs
  Resident-page high event counter, min controls, atomic fast ledger.
- kernel/object/src/handle.rs
  ACCOUNT_SESSIONS as a separate attenuable Job handle right.
- kernel/compat/src/user/syscall/linux/job_ops.rs and broker_ops.rs
  Rights-scoped slow syscalls, versioned user copyin/copyout.
- libs/rustos-user-abi/src/syscall.rs and linux.rs
  JOB_SESSION and JOB_MEMORY_QOS structures and dispatch IDs.
- services/loaderd/src/application_jobs.rs
  Per-application Job session ceiling and memory.high policy.
- services/rootd/src/session_accounting.rs, main.rs, jobs.rs
  Authenticated sessiond control, client Job admission/refund, token book,
  bounded release receipts, cancellation tombstones, provider exit cleanup,
  conservative protected memory.min OOM selection.
- services/runtimed/src/session_accounting.rs, spawn.rs, main.rs
  Pre-activation client charging, exact cleanup on failure/exit, retries
  and cancellation of ambiguous charges.
- The old inline fallback for a missing launch worker was removed;
  normal runtime launch is not allowed to block the supervisor on this path.
- Earlier generated-IDL cutovers retired scalar supervisor ops 8 and 9,
  but the rest of production service IPC is still largely legacy.

## P0 review / repair gates

1. Compile the cross-crate source and repair imports, kernel/ps/api exports,
   typed handle rights, conditional compilation, syscalls and rootd tests.
2. Prove charge admission observes exact live PID/Job membership under
   process table lock and checks every ancestor without partial increments.
3. Prove Job slot retirement waits for every subordinate charge, and no
   stale PID, JobId, or sessiond reporter can modify another Job's ledger.
4. Critical fault window: a kernel RELEASE may commit but its user copyout
   can still fail. Add kernel request-ID/token-level exactly-once resolution,
   not only rootd reply dedup, before claiming transactional safety.
5. Fault-inject lost charge ACK, lost release ACK, reordered cancel and
   charge, duplicate tokens, receipt eviction, full 256-token ledger,
   timeout on rootd IPC, worker ACK timeout and provider lifecycle exit.
6. Inspect simultaneous rootd loader-worker and supervisor mutation of
   checkpoint/session control state. Ensure no unsound aliasing or split
   epoch authority.
7. Rootd crash currently loses the session book while kernel Job counters
   survive. This is an unfinished recovery boundary. Require replayable
   journal or kernel-owned token reconciliation, not an in-memory wish.
8. Check memory.min parent/child reservation admission, root-min treatment,
   rootd OOM eligibility and whether a process kill would cross a reserve.
   The current policy is deliberately conservative, not physical guarantees.
9. Confirm memory.high events are monotonic and do not affect hot paths;
   wire event consumption to Ring3 policy before calling it complete.
10. Measure benchmark regressions after correctness repair. No IPC hot path
    should acquire any of these slow accounting controls.

## Larger missing integrations — NOT completed by this batch

- Stage 2: kernel-object byte charging (kmem.max_bytes) at real alloc/free
  ownership sites, plus pressure-policy response rather than passive events.
- Stage 3: complete positive capability grants and removal of legacy
  numeric service-role masks after all clients are migrated.
- Stage 4: generated-IDL bindings for remaining production protocols,
  plus runtime version adapters and actual removal of old service wires.
- Stage 5: unified manifest-controlled component activation, real named
  capability transfers, and one supervisor transaction owner across
  rootd/initd/sessiond.
- Stage 6: generic client-paid sessions for netd, vfsd and other providers.
- Stage 7: move only slow Ring0 policy/configuration and personality control
  to Ring3; DO NOT move syscall mechanism, IPC core, scheduler, faults,
  futex, clock/getpid or TLB.
- Stage 8: persistent crash-safe checkpoints, authenticated health evidence,
  signed image activation, restart transaction and rollback.
- Reconcile or delete legacy service-specific paths only when a live
  replacement has actually taken custody of the same authorization and state.

## Codex follow-up commands, only after repairs

- cargo xtask check
- cargo test -p rootd --features host-test --locked
- cargo test -p initd --locked
- cargo test -p runtimed --locked
- cargo test -p rustos-component-model --locked
- Existing targeted formal/PR lanes, 1/8-vCPU KVM smoke and measured
  performance baseline, with negative-right tests and fault injection.

**The draft author intentionally executed none of those checks.**

## Additional generated-IDL migration (schema 519)

The newly drafted client-Job session calls were also converted from scalar
arg0/arg1 encoding to the generated canonical CLIENT_JOB_SESSION_ACCOUNTING
schema (opcode 519). Both rootd and runtimed reject the old scalar request
shape; outer supervisor operation numbers 17/18 only select charge/release.
Codex must check generated descriptor drift, request/ack phase separation,
zero-JobId cancellation ACKs, wrong operation, mutated token, swapped sender
PID, truncation, oversized frames, unknown tags and replay across sessiond
incarnations. Charge/release must still be exactly-once across copyout fault.


## Latest continued draft — important additional gates

The [full eight-stage ledger](ring3-8stage-draft-status.md) supersedes stale
statements above implying every kernel-object byte is unimplemented. A real
narrow ProcessObject Box kmem dimension now has Job admission enforcement and
a rights-scoped slow syscall; other allocation classes are still missing.
Rootd has a kernel-memory checkpoint journal that survives rootd process death,
but **not power loss**. Rootd and runtimed exchange generated-IDL health; initd
has sticky rollback-required launch suppression, but **does not restore an
old image**. Kernel-owned provider/token session accounting survives rootd
process restarts, but requires fault-injection proof across PID reuse and
teardown. The full implementation is uncompiled and unbooted.
