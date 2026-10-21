# Benchmark results and lifecycle checkpoints

[Contract index](../README.md).

<a id="current-results"></a>

## Historical results

This file preserves historical unqualified snapshots, not current-tree
performance or release evidence. Exact revision/date provenance and full configuration are
unavailable for these snapshots; retain only the configuration and controls
explicitly recorded below. Current source ownership is documented in
`docs/ai/ownership/execution.md`; new performance claims need fresh anchored,
same-session measurements of the named binary and configuration.

One vCPU, the unrestricted probe table, from two consecutive runs of that
historical tree. The recorded anchor moved -2.2%, inside the 3% admission
bound; both columns are `min` readings:

| probe | min cycles | repeat |
| --- | ---: | ---: |
| `null_syscall_getpid` | 400 | 400 |
| `ipc_try_recv_empty` | 1,400 | 1,440 |
| `sched_yield` | 4,120 | 4,080 |
| `ipc_rt_intra_process` | 16,120 | 16,120 |
| `ipc_rt_intra_process_reply_recv` | 17,480 | 17,480 |
| `ipc_rt_cross_process_syscalld_getuid` | 23,000 | 23,360 |
| `vmexit_cpuid` (anchor) | 3,680 | 3,600 |

`p99` on every probe above routinely reads 15–40x the min even though
`ipc_rt_intra_process_reply_recv`'s min and p50 are nearly identical (the
fused-reply-recv fastpath hits 100% of calls). Traced with
`RUSTOS_SCHEDULER_PHASE_PROFILE=true` and `kernel-scheduler-hold-max`
(decode: `arg0` = `(hold_max_us<<32)|attributed_us`, `arg1` =
`(fnv1a32(caller file)<<32)|line`), the worst per-window lock hold in steady
state is 33–150us at two sites — `kernel/ps/src/multitask/irq.rs`'s timer
and voluntary-yield dispatch entries — both ordinary dispatch turns, not a
defect. This corroborates rather than explains away the standing conclusion
below: the remaining tail was attributed to fair-scheduling contention from
the live desktop's other runnable tasks (uiserver, WayClick, netd), which is correct
scheduler behavior, not a bug. The one confirmed exception:

**The debugcon-in-lock tail fix.** A milestone (`scheduling-budget-exhausted`)
was rendered from inside the global scheduler guard, firing ~60 times a
second; each render is a VM-exit-per-byte host write, and the emitter drains
parked records first. Removing it (now
`performance::SCHEDULER_GUARD_MAX_DEBUG_SINK_RECORDS = 0`, source-witnessed):
shipping-build minimum and p50 did not move, mean fell 39%, **p99 fell about
50%**. This is the only change in this lane's history that moved `p99`
attributably. The lesson it generalizes: unbounded/variable-cost work inside
a hot lock is a tail cause; per-operation cost reduction is a min/p50 cause.
Look for the former, not more of the latter, before attempting `p99` work
here again.

**The historical 17,480-cycle production-shaped local floor** was attributed
to the tracked-lock path; it is not a current-tree floor, and the old absolute
attribution is retired. The pre-relocation profile counted about forty-two acquisitions per round trip at
~735 *instrumented* ticks each. After the GOT fix, a fresh isolated diagnostic
run reads ~440 ticks per acquisition: `before-acquire` 162 + `spin` 31 +
`after-acquire` 32 + `release` 215. The diagnostic build itself raises the
probe from 17,600 to 31,040, so those 440 ticks are not shipping attribution
and must not be multiplied into that historical floor. They establish only that
the GOT removal changed the cost shape and that the former 735/91% split is
stale.

For the lock protocol itself, the durable lever remains acquiring fewer locks;
a direct-mapped hint over the held-class-stack scan measured no change and was
reverted. Diagnostic bookkeeping is a separate build-shape lever: a shipping
path must not update a census no live invariant can read. Two structural cuts
already landed and are not to be re-attempted: `ProcessTable`'s
`is_process_exiting` no longer takes the lock at all (a committed lifecycle
publication already proves "not exiting" without one; see the asymmetry note
below), and the busiest single acquisition site fell from ~10.7 to ~8.4
`ProcessTable` acquisitions per round trip when the own-thread process pin
stopped re-pinning what a thread's own process already held.

**The publication asymmetry is deliberate, not partial.** A committed
lifecycle publication may only prove a process **live** — an absent
publication is ambiguous (exiting, mid-exec, or unknown PID alike), so every
negative answer still goes through the locked lifecycle authority. Serving a
negative from publication would make the accelerator a second authority.
`LIVE_PROCESS_EXIT_QUERY_MAX_PROCESS_TABLE_ACQUISITIONS` pins the live
direction at zero with a source witness.

**Refuted, with a lesson that generalizes: fewer loads is only a lever when the
loads miss.** A derivation of this CPU's logical index read four separate
boot-immutable statics -- a publication flag, a dense count, and one admission
flag per token instruction -- before its `RDPID`, about twice per tracked lock
operation and roughly two million times per bench run. Packing all four into one
word (mode in the low byte, count above it) makes that one `Acquire` load, and
strictly simplifies the publication: one Release store publishes the map, the
count, and the admitted reader together. It measured as nothing. Those four
statics are read millions of times a second, so they are permanently L1-resident
and the four loads are independent and fully pipelined; collapsing them saves
two or three cycles inside a phase that costs about ninety. Within one run the
ratio `release-identity / release-unlock` moved 2.76 -> 2.73. This is the same
conclusion the tracked-lock census reached from the other direction -- making one
acquisition cheaper does not move this floor -- reached this time by a change
that removed real work and still could not be seen.

**Refuted before it was written:** skipping the scheduler's SIMD save/restore
pair on a same-task turn. The reasoning is sound — the interrupt stub already
carries `xmm0`-`xmm15` per task in the `SavedContext`, and
`nucleus_audit`'s post-link FPU custody audit proves the kernel disturbs no
x87, `MXCSR`, or `ymm` upper half outside a bracket, so the pair is a no-op when
the dispatch keeps its slot (this is the same argument
`formal/syscall-simd-lifecycle` used to delete the per-syscall pair at 829
ticks). The counter that already exists kills it:
`kernel-scheduler-transition` reports same-task dispatches at **0.0–2.9%** of
dispatches across steady-state shipping windows. `ArchSimd` is about 6.7% of
attributed scheduler time, so the reachable share is roughly 0.2% — far under
the floor, against a formally modelled path. Read the counter before touching
the path.

**Refuted, so not worth retrying:** reordering the reply-wait poll budget to
arm before polling (an arm costs more than the poll it would save); fusing
the enqueue chain's last unconditional acquisition (measured 5,050→5,048
ticks); a direct-mapped hint over `find_task_stack`'s sleepable-lock scan
(measured no change, reverted); raising `run-implementation-mutations.py`'s
shard cap on a 16-core host (21s at four shards, 22s at eight — not the
bottleneck); folding the user-copy bind's two per-slot identity observations
into one (below); fusing `service_deferred_work`'s three global-scheduler
acquisitions, which is unsafe rather than unprofitable — the completions between
them call `wake_task` and release pages and must run outside the guard, and
detaching the side effect and reaping in one guard would let a slot be reaped
before its side effect completed. Each was a plausible hypothesis a measurement killed; do not
re-derive them from source reasoning alone.

**The folded bind identity, refuted by `sched_yield`.** `current_user_address_space`
and `retain_current_user_process_binding` each read the published per-slot
identity twice — once for the binding, once for the PID — in two separate
interrupt-masking brackets, with a process-handle comparison to catch a writer
that landed in between. Taking both fields from one validated seqlock
observation makes them consistent by construction and cut
`usermem-phase-bind-retain` from 156 to **85** cycles per sample (−45%) at an
unchanged sample count, which is exactly the duplicate read disappearing. It
was still reverted: `sched_yield` moved 5,000–5,160 across five runs without it
to 6,760 and 7,160 across two runs with it, ranges that do not overlap, and the
revert reproduced 5,000 with the anchor held at exactly 0.0%. No IPC gain
survived the same scrutiny — the round-trip probes' apparent −5% did not
separate from this session's own drift on identical binaries. The likely
mechanism is layout: `kernel-ps` builds at `lto=thin`/`codegen-units=1`, and
adding one function changes inlining crate-wide. **The lesson: a phase counter
proving the intended work disappeared is not evidence that the change is a
win.** Re-attempt only with an inlining shape that leaves `sched_yield` alone.

## The same-CPU wake fastpath (seL4-style, landed)

The synchronous IPC handoff hint (`SYNC_HANDOFF_HITS` /
`kernel-scheduler-step-sync-hits`, gated behind
`rustos_scheduler_phase_profile`) hit only 28.3% of attempts, flat across 15
steady-state windows. Root cause, traced to 100% `Generation` mismatch on the
reply-wake side: `wake_task_slot` called `publish_remote_wake` — correct for
a genuinely remote wake, pure overhead for a same-CPU one — which bumps a
generation a second time when the next dispatch's Balance phase promotes
`RemoteQueued -> Local`, invalidating the reply token before it is ever
checked. Fixed per seL4's fastpath principle ("dest thread is set Running,
but not queued" — never two live representations of one thread's
schedulability): `runqueue::publish_local_wake` gives a same-CPU wake a
direct `Blocked -> Local` transition, reusing the same `publish_local` the
Balance phase already calls for the outgoing task. The cross-CPU mailbox path
is unchanged.

Result, isolated-probe method, 14 windows: hit rate 28.3% → **56.6%**
(exactly doubled), `DrainedStale`/`Custody` misses 99.5% → **0.0%**. End to
end, anchor held +1.0%: `ipc_rt_intra_process` −5.9%, `ipc_split_reply_to_return`
−10.1%, `ipc_rt_cross_process_syscalld_getuid` −3.7% (a real cross-process
server, not just the synthetic bench pair). Both controls
(`null_syscall_getpid`, `sched_yield`) read as noise.

## `V5-SCHED-GLOBAL-001` and the scheduler catalog

Current source status is in `docs/ai/ownership/execution.md` and
`docs/ai/structural-ownership-design.md`: the global `Scheduler` catalog, guard,
and consumers are deleted. Independent per-CPU and lifecycle/frame/funding
owners replace them; runtime/hardware/performance acceptance remains separate.

The following is an intermediate migration measurement, not a live backend
inventory. Catalog acquisitions per scheduler entry moved
4.65 → 2.59 → **1.99** at one vCPU and 3.27 → **2.02** at eight, by publishing
per-slot answers for own-task identity, IPC priority reservation, and wait-arm/
cancel. Dispatch, reply-wake handoff, hints, and cleanup still entered the
guard at that checkpoint. Its then-open “ordinary-path acquisition-zero”
target is superseded by the source deletion inventory, not newly qualified by
these old numbers. The runqueue uses owner-word custody plus per-CPU queue/
mailbox locks; absence of a global ready scan is not a claim that the entire
backend is lock-free.

## Phase 6 process-lifecycle and frame-settlement checkpoint

The Phase-6 lifecycle probes use the ordinary process ABI. An isolated run
also executes `vmexit_cpuid`, which changes no RustOS phase or frame counter
and anchors every target-only comparison. With `RUSTOS_LIFECYCLE_TRACE=true`,
a fresh one-vCPU `fork_exec_exit_wait` run carried all required exact-target
markers for 34 warmup-plus-measured children: spawn reserve/publish, exec
reserve/authorize/stage/publish, exit seal, and reap completion. The first
and last records retained distinct process generations and non-reused
lifecycle transactions; the measured 32 children also carried `reap-queued`.
Trace-enabled latency is diagnostic and is not compared with shipping builds.

Historical shipping one-vCPU results after the exact pre-map spawn reservation
and batched frame return:

| probe | min cycles | p50 cycles | p99 cycles |
| --- | ---: | ---: | ---: |
| `fork_exit_wait` | 61,186,880 | 132,670,840 | 197,724,360 |
| `fork_exec_exit_wait` | 266,364,360 | 335,289,200 | 754,290,400 |
| `thread_clone_exit_join` | 2,902,520 | 40,854,960 | 190,787,960 |
| `exec_replace_single_thread` | 160,459,120 | 222,626,440 | 348,398,880 |
| `spawn_activation_to_first_turn` | 28,249,800 | 61,506,600 | 83,408,280 |
| `exit_retire_to_reap` | 27,843,400 | 68,890,280 | 137,308,560 |

For 1,024-page map-touch-unmap: exact source-paired scalar-free/candidate/
scalar-free A-B-A controls held the anchor within 1.1%. The scalar source
requires 40,960 free-side allocator acquisitions; the candidate measured 640
exact 64-frame acquisitions — a structural 64x lock reduction without
weakening ownership checks — reporting min/p50 6,002,880/6,384,800 cycles at
one vCPU and 6,149,960/6,477,200 at eight, both 41,058 frames in 642 bounded
operations. Both SMP runs passed the kernel-stamped semantic isolation gate.

**These distributions were recorded as the Phase-6 checkpoint; unavailable
revision/date provenance prevents treating them as current closure evidence.
They do not claim that desktop-contention p99 is stable.** `p99` ordering across
A-B-A controls was not repeatable and makes no tail claim.

