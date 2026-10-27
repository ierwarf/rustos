# Session Handoff — 2026-10-27

Volatile snapshot, not a stable instruction prefix. On resume inspect
`git status --short`, `git log -1` and the relevant owner sources before reusing
evidence. Root `AGENTS.md` owns repository policy. The owner stopped IPC redesign;
this checkpoint records the implemented changes and unresolved boundaries.

## Current implementation

- Generic dispatch trims and deferred standalone reply wake are integrated with
  IPC queued/rendezvous convergence; policy remains in ring3 services.
- Bounded byte CALLs may attempt rendezvous. One absolute deadline caps fallback;
  expiry before or during response observation wins over publication.
- Register queued/rendezvous CALLs share collection and direct syscall-frame
  parking. Plain RECV and spurious re-parks also use direct continuation; plain
  versus reply-committed receive errors retain separate kinds.
- Queued and rendezvous byte-only REPLY_RECV share deferred completion/receive.
  Removed the separate eager-wake receive helper, constant-true donation
  branches and queued-inline double copy. The neutral direct schedule initializes
  its full 432-byte frame; independent review accepted the correction.
- Queue/message/reply objects, asynchronous donation materialization, handle
  cleanup and remote wake/IPI remain live. Objectless virtual rendezvous and
  paired-custody switching are **not implemented**.

Current contract and chronology: [IPC ABI](abi/ipc.md),
[IPC lane](performance/ipc-lane.md). The [virtual rendezvous draft](performance/ipc-rendezvous.md)
is future design, not shipped behavior. Older phase measurements are historical,
not an optimization baseline for this checkpoint.

## Validation actually completed

- Native: compat 198, HAL 81, PS 986, IPC runtime 81, ipcbench 21 tests.
- `cargo xtask check`, all 60 selected now commands, source conformance 1349,
  four focused implementation mutants, full PR (45 sealed artifacts), and SMP
  iteration (26 sealed artifacts) passed. Docs-only cleanup uses fresh focused
  context/layout and source-contract checks, not new runtime evidence.
- Signed-image dual-guest register benchmarks passed at 1 and 8 RustOS vCPUs
  with authenticated DVM input relay. These do **not** establish queued-only
  8-vCPU success. No physical GPU or exhaustive cancellation/SIMD stress claim.

## Current queued-only measurement

Probe: `ipc_rt_intra_process_queued_reply_recv`; unchanged kernel, same-process
16-byte echo, fused server, real 272-byte reply capacity forces rejection of the
256-byte fast frame. 2,000 warmups, 20,000 samples, exact STOP/join and untouched
tail required. Shipping profiles all off; source/linked/staged ELF freshness passed.

- **1 vCPU:** p50 **24,080 ticks / 6.033 us**, p99 **36,280 / 9.090 us**;
  observed max **52,866,240 / 13.246 ms**. CPUID p50 4480; no outliers removed.
- Separate leg p50s: request-to-receive 18,760; reply-to-return 5,160 ticks.
- Diagnostic-only run: 22,001 admission attempts, fallbacks and shape refusals;
  no fast reservation publication. Instrumented timings are not shipping results.
- **8 vCPU: failed twice.** Both reached anchors but emitted no queued result or
  `ipcbench: end` within 30s; repeat also missed wayclick first-frame. Cause is
  unresolved. Preserve both failures; no more silent retries or inferred latency.
- No matched queued-before cohort: do not claim improvement over the old ~40k.
  The earlier 7–10k discussion is an aspirational design budget, not evidence.

Durable result: `build/bench-core/ipc_rt_intra_process_queued_reply_recv-vcpu-1-1793082845527969983/`.
Diagnostic and failed-run paths, other probe results and full method are in
[the IPC lane measurement record](performance/ipc-lane.md#measuring-the-current-queued-slowpath-alone).
`/tmp/ipc-slow-only-*` and `/tmp/ipc-converge-final-*` are convenience logs only.

## Resume boundaries

Do not restart optimization without a new task. For further IPC work, first
diagnose the two queued-only 8-vCPU non-completions; retain normal readiness and
end-marker acceptance. Cross-process IPC and loaded/handle-bearing latency were
not measured here. Prior lifecycle/state-rail and physical-GPU questions are
separate scope: consult [core-path gaps](performance/core-paths-gaps.md),
[benchmark progress](../benchmarks/core-paths-progress.md) and
[physical GPU status](physical-gpu-status.md), then obtain fresh evidence.
