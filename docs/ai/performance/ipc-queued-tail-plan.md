# Queued IPC and latency tails: optimization plan

Status: implemented and shipping-benchmarked (2026-11-28, KVM ~3.991 GHz host).
Companion to the [L1–L3 fastpath plan](ipc-l1-l3-plan.md). Optimize the lowest
repeatable cost the architecture permits without weakening ordering, authority,
accounting or fallback. There is no target tick count that ends the work and no
promise that any of the candidates below yields a specific number. Optimize
CPU cost and service latency separately: the TSC measures elapsed time, including
periods when a guest vCPU is not executing.

## 1. What was measured, and exactly what it means

1-vCPU KVM, host idle, 16-byte intra-process echo, 2,000 warmups and 20,000
timed round trips. These are guest invariant-TSC ticks. Historical runs here
are from the same checkout but are not a newly interleaved A/B cohort:

| Transport | p50 | p99 | p99.9 | p99.99 | Raw maximum |
|---|---:|---:|---:|---:|---:|
| Forced queued (handle-form CALL, zero descriptors) | 23,680 | 48,080 | 555,720 | 23,226,640 | 56,160,560 |
| Buffer rendezvous with fused receive | 8,400 | 11,280 | 102,400 | 1,358,880 | 1,833,040 |
| Register rendezvous | 5,600 | 8,640 | — | — | 797,520 |

The register capture used here retained the primary summary but not the top-eight
records. Do not fill in its tail by analogy. The queued accepted archive is
`build/bench-core/ipc_rt_intra_process_queued_reply_recv-vcpu-1-1795788091060517364/`:
`launch_ok=true`, profiles off, staged and linked hashes match, `guest.log`
byte-matches `build/ipc-slowpath-baseline/queued-debugcon.log`. ELF SHA-256 starts
`a1e2e213…` (kernel) and `20a30a09…` (app). Selected objdump windows, replay
script and symbol indexes: `build/ipc-l1-l3-plan/queued-tail/`.

The queued call→receive p50 is 18,560, server body 80, reply→return 5,000;
leg p50s are separate distributions. For each of the eight longest measured
rounds, the call→receive leg accounts for about **91–99.8%** of that round.
The largest (sequence 11221, 56.16M ticks ≈ 14.1 ms) has 51.04M on that leg
and 5.12M on return. There are 88 samples above 80k, 14 above 1M and 9 above
10M. This locates the long delay **between caller entry and server receipt**;
it does not distinguish host vCPU descheduling, guest scheduler delay, allocator,
IRQ, logging, or lost/retried wake. The separately timed CPUID anchor has its
own tail and cannot label any particular IPC outlier. Under the existing 50%
and 100% host-CPU-pressure profiles, queued p99 was 65,160 and 482,480;
CPUID also shifted, so do not attribute the differences to an IPC code change.

The queued probe exercises the handle-aware caller front end without transferring
FDs; its server uses ordinary buffer RECV and fused REPLY_RECV. Real transfers
and 32 KiB service traffic are not modeled by this row. `netd`'s bounded
kernel-originated service IPC also uses the queue: in `ipc_ops.rs`, the
`service_replies_on_receive_thread` rendezvous allowlist is VFSD, PROCD and
LOADERD, and `netd` can defer a response to another worker. Do not move `netd`
onto task-bound rendezvous based on this microbench. Compare service-shaped
calls separately; frame-broker and DVM costs are outside this plan.

## 2. Static queued path audit

Addresses are linked addresses. Symbol sizes or listing lengths include error
branches, inlined callees and sometimes references to adjacent symbols; they
are not retired instruction counts. The source and the selected linked
instructions establish that these operations exist, not their cycle share.

| Boundary | Static/linked evidence | Opportunity or required invariant |
|---|---|---|
| Workload and syscall entry | In `ipcbench`, queued CALL uses `0x52550010` (timed branch at `2d062`), server initial receive `0x52550038`, then fused `0x52550048` at `2c080`. `syscall_entry` takes ordinary XMM save/restore for these operations; they are not the four register ABI operations. | Preserve ordinary SIMD semantics. Include `REGISTER_CALL` **forced to queue** as a separate diagnostic shape later; it uses `collect_inline_reply` and direct park, not this caller's ABI. |
| CALL validation/copy | `ipc_ops.rs::syscall_linux_rustos_ipc_call_with_handles_timeout`, `2b09d0`: `2b0cd4` calls `copy_from_current_user_to_vec`, then write range, FD count/output and endpoint-shape preflight (`2b0d59`, `2b0da3`, `2b0dd0`). The owning buffer is already filled directly from user memory; the path then passes it as a slice. | Remove the duplicate *kernel* request copy, not fault checks. Consider one retained UserAccess preflight, but do not retain a user-memory bind or borrow across wait, export, lock handoff or sleep. Zero FD capacity still requires count-word semantics. |
| Queue storage and publication | `ipc/mod.rs::enqueue_endpoint_call_with_handles_faultable` is inlined into the linked enqueue family at `3ae2f0`. It charges the sender Job, checks endpoint shape/space, calls `copy_endpoint_bytes` at `3ae5eb` (reserve + `memcpy`), inserts two bounded slab entries (`ENDPOINT_MESSAGES`, `REPLIES`), and revalidates/publishes under endpoint then message custody. Handle vector reserve is conditional, not an allocation in the zero-FD case. | A moved payload can avoid the second allocation/copy. Do not drop the publish-time capacity/generation check or exactly-once rollback/refund. Queue-full hits are observability, not failure authority. |
| Receiver selection/wake | `enqueue_call_and_wake_with_handles_for_task` at `2aff20` reserves funding before publication, attaches it to the reply and invokes the typed scheduler handoff (`2b01b7`; retry branch at `2b0233`). Published work may be taken on another CPU before return. Cold refused-wake paths restore/re-wake only exact waiting receivers. Caller is still Running/unarmed here. | Avoid refetch/bind churn only when a single receipt spans the same lifetime. A queue entry is not permission to schedule a worker. Keep exact deferred retry and donation, including a receiver that already completed. |
| Wait and block | `reply_wait::wait_for_reply_with_deadline` at `346530`: take before arm and after arm; `take_endpoint_response_for_wait` calls `take_response_detailed` (`346ac5`), which uses the advisory published message ID then locks Message→Reply. Blocking reaches `commit_block_current_task_and_yield` (`4a1b20`) and software scheduling, not a preselected direct task switch. | The earlier reply-slot lookup was already removed by the advisory mirror. Optimize *negative* takes with an exact readiness observation, not deletion of the check-arm-recheck protocol. Preserve deadline wins even if a response is observed during expiry. |
| Server receive | `receive_or_wait_inline::<256>` at `22a940` checks any addressed rendezvous then calls `take_queued_request` (`3b24a0`). A true queued take checks endpoint, capacity and exact message, takes owned bytes/handles, then consumes the selected head. Its endpoint has Ordinary/System lanes and bounded class burst. Server copyout uses ABI preflight and owned receive state. | Queue ordering and head/capacity refusal are semantic, not diagnostic. No copying a user pointer while holding the endpoint/slot lock or before confirming exact frame ownership. |
| Reply and caller return | Fused receive preflights next receive outputs (`238260` closure) and tries fast completion; for a queued reply it falls back to normal completion for the **receiving process**, not just the receive thread. `complete_endpoint_reply_with_handles_for_owner_with_custody` at `3adc60` prepares response backing and publishes under Message→Reply. `take_response_detailed` at `3b0010` consumes/retire-checks both slots; destructors run after guards. `finish_ipc_call_response` (`2a4e30`) revalidates/copies response and even the zero-FD count; slow-call logging is rate-limited. | Move owned response bytes, then specialize short storage, without re-reading user buffers after publication. Keep worker replies, one-shot committed receive error, capacity rejection without consume when specified, and descriptor copyout rollback. |

Read the owners together before editing: `kernel/compat/src/user/syscall/linux/{ipc_ops.rs,ipc_ops/reply_wait.rs,ipc_reply_recv.rs,ipc_ops/handle_receive.rs,ipc_ops/fast_reply.rs}`;
`kernel/ipc-runtime/src/ipc/{mod.rs,slab.rs,endpoint_receive.rs,endpoint_priority.rs,reply_publication.rs}`;
`kernel/ps/src/multitask/{current.rs,current/ipc.rs,irq.rs,scheduler/{wake_publication,ipc_publication,ipc_lane}.rs}`.
Cancellation, provider replacement, queue close and retired task cleanup must
consume the same identities as the success path. User-owned FD entry lifetimes
and kernel transfer custody are not interchangeable.

## 3. Queued work packages (Q)

1. **Q0 — isolate the controllable costs.** Separate end-to-end controls:
   handle ABI with zero FDs and with real FDs; buffer vs register forced queue;
   empty/parked endpoint, backlog with no idle worker, and mixed ordinary/system
   senders; same root and separate process, same CPU and remote CPU. Keep real
   queue publication (not rendezvous) for the queued probes, exact sequence and
   reply checks, STOP/join and transfer/refund witnesses. Add 16 B, 256 B and app-
   shaped service request/reply sizes (up to the current 32 KiB netd inline cap),
   and a real `netd recv/poll` control only after the synthetic matrix. Diagnose
   enqueue/copy/dequeue/ready-wait/owner-switch/return separately; use counters for
   allocation count and bytes, requests per wake, actual/refused wake, poll
   empty/ready transitions and scheduler route. Do not use the old 3,197-cycle
   comment in `reply_wait.rs` as a current cost or multiply a static instruction
   count; both require a shipping control with matching workload.
2. **Q1 — move kernel-owned payloads instead of copying them again.** Have an
   internal enqueue/reply preparation API accept ownership of already validated
   bytes. Return that ownership on a pre-publication retry so queue-full doesn't
   re-copy user memory or recreate exported handles. Leave borrowed producers a
   single `copy` adapter. Move the large fused response snapshot into queued
   completion on fast refusal; for short messages, test a compact inline carrier
   in a separate experiment, so response/request heap allocation can disappear
   for hot short traffic. Choose inline threshold from measured payload mix and
   slab/cache footprint; don't inflate every 512-slot object by the 256-byte
   fast frame. Preserve charge and allocation-failure behavior, fallible backing,
   put/drop outside guard, declared length and all copyout fault/protection rules.
   Measure small and large cases separately. Also measure reusing one retained
   preflight proof in the handle caller, with final fault-aware copyout and no
   import/export semantic changes; do not combine with payload ownership A/B.
3. **Q2 — shorten the common wait and saturation loops.** Add an exact
   reply-generation-tagged pending/ready observation in the owner transaction
   if two negative takes are measurably expensive. A positive or uncertain hint
   authorizes only the existing full take; a negative can cheaply satisfy a
   poll only if proven exact and ordered. Keep both logical observations
   (before and after arm), and recheck readiness vs commit with early wake
   ordering; never let a pending hint hide a terminal result. Model cancel,
   expiry, publish and re-use; a ready reply cannot beat expiry if it crosses
   observation. Separately evolve internal admission result to distinguish
   `QueueFull` at the exact rejected operation from quota/allocator failure;
   retain old ABI errno and deadline. Compare typed rejection first; add
   generation-scoped queue-space wake on full→nonfull, including cancel/revoke,
   only if measured 1 ms retry remains a tail. Use arm/recheck and one original
   absolute deadline; no third poll, busy-spin or unbounded retry.
4. **Q3 — keep transport queued; improve the wake-to-run leg.** On an already
   published call, first remove avoidable handoff refetches only through a
   lifetime-bound receipt. Then measure a typed same-CPU handoff to the exact
   receiver selected by queue order, optionally integrating caller wait commit
   after publication. Admission must observe existing higher-priority/older
   work, affinity, chain/budget and exact live waiter. A receiver remains free
   to serve work in queue order; after publication no rendezvous replay, and
   decline leaves durable ordinary wake/retry. Do not route all queued calls
   through L2 pairs or change process-worker authority to task authority. A
   direct scheduler switch does not turn a queued message into rendezvous.
5. **Q4 — only if slot traffic dominates after Q1–Q3.** Compare an owning
   compact queued record plus a generation-bound reply reference against the
   two-slot representation. Prefer one ownership model rather than a permanent
   third fast transport; prototype under a build option, preserve logical
   message/reply lifetime and exact cancel/close/receiver-capacity semantics.
   Keep charge, descriptor preparation, deferred reply, FIFO/priority lanes
   and all published/backpressure behavior. Do not combine slot and scheduler
   ownership changes in one A/B. Abort this redesign if counters show little
   remaining slot cost or semantic/evidence complexity outweighs a real gain.

Failure and load matrix for promotion: header/payload faults and COW/raced
unmap; zero/nonzero FD counts, capacity refusal and failed FD copyout;
per-Job exhaustion vs queue saturation, cancellation before/after take,
reply vs expiry, process worker replay, service/provider restart and quota
refund; lost/early wake, contested queue and saturated FIFO with fair user
progress; 1-vCPU and SMP same/remote CPU, but no mandatory remote fastpath.
Map witnesses to existing IPC endpoint, reply deadline, priority, transfer
custody and scheduler ownership contracts rather than weakening those models.

## 4. Tail work packages (T): explain delays before fixing them

**T0 — retain evidence without making the tail.** The existing `tails.rs`
retains worst eight and p99.9/p99.99, counts and top 1% share, and flushes
output after measurement. Extend a diagnostic lane to retain a bounded exact
sequential record (run, sample/call sequence, start/end guest TSC, per-leg
stamps, guest CPU) and label unpaired samples; the current split's timestamp
interval check is not a per-request sequence match. Never drop an unpaired
*total* sample. Reject overflow as incomplete attribution, not as a fast run;
keep raw shipping samples separate. Publish p50/p90/p99/p99.9/p99.99,
mean/max, outlier counts and top 1% share; 20k samples leave only about two
at the p99.99 rank, so repeat/extend dedicated tail cohorts with raw maxima.

**T1 — classify by the exact affected window.** Keep a private, fixed-size
per-CPU event ring, no `debugcon`, formatting, heap or cross-CPU shared log
lock in the window. Record enqueue/ready publication, exact wake attempts
and commit, switch/ACK and return, plus scheduling/IRQ/reschedule intervals,
queue depth, relevant signalling and debug-output drain windows. Use owner
IDs or the exact reply/sequence, clock domain/CPU ID, bounded sampling and
an overwrite/loss counter. IRQ writers must not share an unprotected cursor
with preemptible task writers: use separate fixed writer records or a bounded
publication protocol, and keep NMI on its existing separate path. Only if
those intervals cannot explain a stall,
collect filtered host scheduler and KVM entry/exit evidence for the actual
vCPU TID (and DVM threads only if causally involved). Align guest TSC and
host trace time with matched run markers/clock mappings sampled before and
after the window; report skew/uncertainty and gaps. Do not align by a log
line that arrived late. Report three disjoint *elapsed-time* classes:
confirmed guest work/wait, confirmed host off-CPU/exit, unresolved; never
subtract them from shipping p99/max. If permissions do not allow host
tracepoints, record the limit; do not change host policy. Linux's
[timerlat](https://docs.kernel.org/trace/timerlat-tracer.html) motivates the
IRQ-to-thread split; [event tracing](https://docs.kernel.org/trace/events.html)
and [ftrace](https://docs.kernel.org/trace/ftrace.html) are capture references,
not evidence about these samples.

**T2 — optimize one observed class at a time.** If a refused/retried wake or
queue-space timer explains it, take Q2/Q3 first and verify durable retry and
unchanged fairness. If remote runnable wait is dominant, measure targeted
owner-CPU wake, affinity and idle placement under genuine competing work,
not just pinning away the problem in the benchmark. If scans, allocation,
lock hold or logging account for the window, shorten the identified operation
or defer bounded diagnostics while preserving failure evidence/loss counters.
If host descheduling dominates, report deployment sensitivity (idle,
50%/100%, optional bare-metal control), not a guest fastpath speedup; retain
those samples. Test signal, migration and async park/return as independent
events in L1–L3 because escape latency matters as much as admitted latency.

**T3 — measure the return tail separately from the statistical tail.**
The shared `syscall_entry` epilogue and `fast_ipc_parked_tail` are reviewed
in the L1–L3 linked audit. On the queued side, the return leg also includes
ready-take/retirement, FD-aware copyout and `syscall_dispatch`'s normal SIMD
restore/return handling; the approximately 5,000-tick leg is not a pure
SYSRET measurement. Stamp `reply published → caller runnable → resumed →
copyout complete → userspace`, retain the cold signal/reschedule route as
its own cohort, and compare the common no-reschedule return with the B0
entry/return control. Consider factoring duplicate stable-frame checks or
reporting-only work only with the same exact-frame/generation proof; never
remove the return contract, fault-aware copyout or SIMD isolation to make
an epilogue benchmark pass.

**Ordering across tracks:** establish B0/Q0/T0; take Q1 and evidence-led T1
first for the existing queued workload, then Q2/Q3 where the evidence ranks
them. L1→L2→L3 remains the register route and can reuse proven observations,
not silently change its queue fallback. Q4 and reusable L3 pairing are last,
and need independent evidence. Alternate A/B/A/B on one host with matching
profile/topology and a CPUID anchor within 3%; keep slow samples and fallback
counts. Re-measure after each accepted optimization and choose the next
largest removable cost. Accept a measured improvement larger than repeat
noise with correctness and no reproducible regression in the relevant p99 or
ordinary fallback; no fixed saving, floor or benchmark-only fast route.

## 5. Implemented Q0–Q4 & T0–T3 results (2026-11-28)

### Implemented changes
- **Q0 (`apps/ipcbench/src/ipc_probes.rs`, `tools/xtask/src/bench.rs`):** Added isolated matrix probes `ipc_rt_queued_{0fd_16b,fd_16b,reg_16b,256b,32k,backlog,cross_proc}`.
- **Q1–Q4 (`kernel/ipc-runtime/`, `kernel/compat/`):** Replaced duplicate request/response copies with moved `EndpointPayload`, inline short-message carrier specialization avoiding heap allocation on short queued calls/replies, waiter readiness fast paths, and shared dequeue/wake/reply retirement cleanup.
- **T0–T3 (`apps/ipcbench/src/tails.rs`, `kernel/ps/`, `kernel/compat/`):** Retained per-request sequential ring attribution (`seq-summary/*` with 0 unpaired/overflow in isolated runs), fixed latency-tail causes in queued wake/wait/return paths, and isolated return-leg tails.

### Shipping KVM benchmark comparison (`tsc_khz ≈ 3991 MHz`, 1-vCPU & 8-vCPU)
- **`ipc_rt_intra_process_queued_reply_recv`:**
  - 1-vCPU baseline (`build/diag/ipc_rt_intra_process_queued_reply_recv.txt`): `min=21,400`, `p50=24,240`, `p99=39,240`, `mean=99,752` (historical `queued.log`: `min=21,840`, `p50=23,680`, `p99=48,080`).
  - **1-vCPU final (`build/diag/final-vcpu1-queued-B.txt`):** **`min=9,720` (`2,435 ns`), `p50=10,320` (`2,585 ns`, `-57.4%` vs `24,240`), `p99=18,320`, `mean=11,531` (`>10M=0`, `top1%=10.49%`)**.
  - **8-vCPU final (`build/diag/final-vcpu8-idle-queued-B.txt`):** **`min=9,400` (`2,362 ns`), `p50=10,160` (`2,553 ns`), `p99=18,120`, `max=231,680`, `mean=10,962` (`>1M=0`, `>10M=0`, `top1%=3.36%`)**.
- **Q0 1-vCPU isolated matrix (`build/diag/ipc_rt_queued_*.txt` -> `build/diag/final-ipc_rt_queued_*.txt`):**
  - `ipc_rt_queued_0fd_16b`: `min 21,680 -> 9,960`, `p50 24,360 -> 10,520` (`-56.8%`), `p99 42,960 -> 13,560`, `mean 102,550 -> 11,030`.
  - `ipc_rt_queued_reg_16b`: `min 6,840 -> 6,360`, `p50 7,560 -> 6,680` (`-11.6%`), `p99 8,360 -> 7,920`, `mean 81,290 -> 7,161`.
  - `ipc_rt_queued_fd_16b`: `min 38,120 -> 29,960`, `p50 48,120 -> 45,560`, `p99 88,960 -> 65,320`, `mean 132,233 -> 62,960`.
  - `ipc_rt_queued_256b`: `min 32,560 -> 24,600`, `p50 43,080 -> 41,040`, `mean 129,236 -> 46,600`.
  - `ipc_rt_queued_32k`: `min 44,360 -> 30,680`, `p50 53,120 -> 48,200`, `p99 79,240 -> 68,000`, `mean 137,108 -> 49,958`.
  - `ipc_rt_queued_backlog`: `min 34,720 -> 26,560`, `p50 44,680 -> 42,840`, `p99 65,000 -> 62,880`, `mean 146,221 -> 88,913`.
