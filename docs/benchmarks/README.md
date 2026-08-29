# Ring3 cost benchmarks

`cargo xtask bench` boots the ordinary interactive topology, runs
`apps/ipcbench` as a session-startup program, and parses its debugcon output.

```sh
cargo xtask bench --baseline docs/benchmarks/ipc-baseline.txt
```

Every probe uses an already-published ABI. There is no bench-only kernel path
and no privileged capability grant, so what the harness measures is what an
ordinary application pays.

This document is pruned to current, reproducible findings and the methodology
needed to reproduce them. It is not the research log that produced them — that
history is in git (`git log -- docs/benchmarks/README.md`,
`git log --oneline | grep -i perf`). Do not restate a superseded number here;
delete it and let the current one stand alone. The Phase 6 checkpoint section
is cited by `formal/CONFORMANCE.md` as phase-closure evidence — this file is
**not** exempt from the formal verification-run source binding
(`formal/binding-exempt-paths.txt`), so trimming it invalidates an in-progress
seal, and that section specifically must keep stating a closure, not just a
number.

## Probes

| probe | what it costs |
| --- | --- |
| `tsc_overhead` | the measurement itself; every other row includes it |
| `null_syscall_getpid` | syscall entry and exit, answered inside ring0 |
| `sched_yield` | yield and be rescheduled, so at least one full switch |
| `ipc_try_recv_empty` | the IPC object path with no blocking and no reschedule |
| `ipc_rt_intra_process` | a blocking round trip with no address-space switch |
| `ipc_rt_intra_process_reply_recv` | the same client against a server using the fused reply-and-receive call every production service (`syscalld`, `loaderd`, `inputd`) uses — this is the production-shaped probe to quote |
| `ipc_rt_cross_process_syscalld_getuid` | `getuid`, which `syscalld` answers over IPC |
| `vmexit_cpuid` | one hypervisor exit, as a scale for every other row and the required anchor for any cross-run comparison |
| `ipc_split_*` | the round trip cut at the server's own timestamps: `call_to_recv`, `server_body`, `reply_to_return` |
| `fork_exit_wait`, `fork_exec_exit_wait`, `thread_clone_exit_join`, `exec_replace_single_thread`, `spawn_activation_to_first_turn`, `exit_retire_to_reap` | Phase 6 process-lifecycle probes over the ordinary process ABI |

## Reading the numbers

**`min` is the structural cost; the tail is contention.** The harness runs
while the desktop is live, so `p99` and `mean` include time other runnable
tasks consumed. A `min` that stays flat across separate boots is a fixed cost
in the path, not scheduling luck. **`p99` and `mean` move with desktop
contention and are not a regression signal on their own** — every attempt in
this lane's history to attribute a `p99` swing to a specific code change
failed to reproduce except once (below, "the debugcon-in-lock tail fix").

**The anchor, and why a run without one proves nothing.** Every figure is an
invariant-TSC tick, not a core cycle. A host clock shift moves every probe at
once, including probes with no RustOS code in them — `vmexit_cpuid` catches
that and nothing else. `cargo xtask bench --compare <baseline>` reports
whether the anchor held; when it did not, treat the run as uninformative
rather than reading the normalized column as a measurement. **A committed
baseline is a record, not a control** — `docs/benchmarks/ipc-baseline.txt`
goes stale while the anchor holds, because host cache/memory state, KVM, and
background load all move the guest without moving a hypervisor exit. Every
comparison needs a same-session control run of the unmodified tree, not just
a held anchor.

**The probe table has a noise floor of about two percent** (`sched_yield`
needs about fifteen). A change smaller than the floor needs a phase counter
(below), not a more confident reading of one pair of runs.

The probes are chosen so that differences isolate one layer:

- `ipc_try_recv_empty` − `null_syscall_getpid` = the IPC object path alone
  (under 2% of a round trip).
- `ipc_rt_intra_process` − `ipc_try_recv_empty` = block, switch, wake, switch.
- `ipc_rt_cross_process_*` − `ipc_rt_intra_process` = the address-space switch
  (about 5%).

Those two layers are tightly bounded; they do **not** prove where the
remaining ~93% goes — treating that residue as "the scheduler" is an
inference the in-kernel phase profile below refutes.

## The in-kernel phase profile

`cargo xtask bench` decodes the `ipc-call-phase-*`, `usermem-phase-*`, and
`lock-phase-*` milestones and prints any profiles enabled for that build.
Shipping images leave them off; set `RUSTOS_IPC_PHASE_PROFILE=true` and/or
`RUSTOS_SCHEDULER_PHASE_PROFILE=true` for one diagnosis run. **Every phase
profile in this kernel has cost more than it measured** when checked
(`[lock_telemetry]`, `[scheduler_telemetry]`, `[syscall_telemetry]`,
`[ipc_telemetry]` — the lock-phase profiler alone was 26% of a round trip).
Ablate before trusting a phase-profiled number: stub the profile's
`now`/`charge` calls to constants, rebuild, and compare against the
unstubbed build in the same session. Three of the four are gated build
switches for exactly this reason; call sites stay unconditional (so a phase
cannot be silently forgotten at its boundary) but only the counter read and
accumulator compile in.

`cargo xtask bench --isolate-probe <name>` reboots with `ipcbench` restricted
to one probe, with a 15-second post-readiness settle and an explicit
mid-boot phase-counter drain (`SYS_RUSTOS_PHASE_PROFILE_DRAIN`), so the
counters describe that probe rather than every probe summed across the boot.
The four syscall-path phases charged exactly once per call
(`copy-request`, `enqueue`, `write-response`, `enqueue-deadline`) land at a
clean 1.00 ratio under isolation and are what `isolation check: PASS/FAIL`
verifies. Phases charged once per *endpoint* call or per `usermem` access
remain contaminated by the live desktop's own steady-state traffic (uiserver
and WayClick never stop running) and are reported but not gated.

## Current results

One vCPU, isolated `ipc_rt_intra_process_reply_recv`, same-session anchor
held within 1.1% throughout — this is the current repeatable state, not a
single run:

| probe | min cycles | p50 cycles |
| --- | ---: | ---: |
| `null_syscall_getpid` | ~1,600 | — |
| `ipc_try_recv_empty` | ~2,560 | — |
| `ipc_rt_intra_process` | ~23,000 | ~43,000 |
| `ipc_rt_intra_process_reply_recv` | ~25,400 | ~27,400 |
| `ipc_rt_cross_process_syscalld_getuid` | ~32,700 | ~35,900 |

`p99` on every probe above routinely reads 15–40x the min even though
`ipc_rt_intra_process_reply_recv`'s min and p50 are nearly identical (the
fused-reply-recv fastpath hits 100% of calls). Traced with
`RUSTOS_SCHEDULER_PHASE_PROFILE=true` and `kernel-scheduler-hold-max`
(decode: `arg0` = `(hold_max_us<<32)|attributed_us`, `arg1` =
`(fnv1a32(caller file)<<32)|line`), the worst per-window lock hold in steady
state is 33–150us at two sites — `kernel/ps/src/multitask/irq.rs`'s timer
and voluntary-yield dispatch entries — both ordinary dispatch turns, not a
defect. This corroborates rather than explains away the standing conclusion
below: the remaining tail is fair-scheduling contention from the live
desktop's other runnable tasks (uiserver, WayClick, netd), which is correct
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

**What sets the ~23,000-cycle floor**, per the tracked-lock class census
(`work_budget::take_class_census()`, rendered as `kernel-lock-class-0..5`):
about forty-two tracked-lock acquisitions per round trip at ~735 instrumented
cycles each with no dominant sub-phase (admission, held-class-stack
bookkeeping, and release each contribute roughly a third) — call this ~20,000
of the floor. **The only lever that moves it is acquiring fewer locks, not
making one cheaper**; a direct-mapped hint over the held-class-stack scan
measured no change and was reverted. Two structural cuts already landed and
are not to be re-attempted: `ProcessTable`'s `is_process_exiting` no longer
takes the lock at all (a committed lifecycle publication already proves
"not exiting" without one; see the asymmetry note below), and the busiest
single acquisition site fell from ~10.7 to ~8.4 `ProcessTable` acquisitions
per round trip when the own-thread process pin stopped re-pinning what a
thread's own process already held.

**The publication asymmetry is deliberate, not partial.** A committed
lifecycle publication may only prove a process **live** — an absent
publication is ambiguous (exiting, mid-exec, or unknown PID alike), so every
negative answer still goes through the locked lifecycle authority. Serving a
negative from publication would make the accelerator a second authority.
`LIVE_PROCESS_EXIT_QUERY_MAX_PROCESS_TABLE_ACQUISITIONS` pins the live
direction at zero with a source witness.

**Refuted, so not worth retrying:** reordering the reply-wait poll budget to
arm before polling (an arm costs more than the poll it would save); fusing
the enqueue chain's last unconditional acquisition (measured 5,050→5,048
ticks); a direct-mapped hint over `find_task_stack`'s sleepable-lock scan
(measured no change, reverted); raising `run-implementation-mutations.py`'s
shard cap on a 16-core host (21s at four shards, 22s at eight — not the
bottleneck). Each was a plausible hypothesis a measurement killed; do not
re-derive them from source reasoning alone.

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

Current status and the full measurement trail: `docs/ai/structural-ownership-design.md`
§2. Summary as of the source check in this file's last revision: the per-CPU
runqueue, owner-word state machine, and remote-wake mailboxes are lock-free
and already do not scan a global ready set; the global `SCHEDULER` lock now
protects lifecycle/catalog bookkeeping, not dispatch selection. Catalog
acquisitions per scheduler entry (the stable normalizer — comparable across
boots whose absolute round-trip rate differs, unlike acquisitions per
second) moved 4.65 → 2.59 → **1.99** at one vCPU and 3.27 → **2.02** at eight,
by giving per-slot published answers to questions that used to require the
global lock (own-task identity, IPC priority reservation, wait-arm/cancel).
What still enters the guard on the ordinary path: dispatch itself, the
reply-wake handoff, pick hints, and retired-task cleanup — reducing that
further is "ordinary-path acquisition-zero," open, and is a data-structure
split, not a scheduling-algorithm change or a lock removal. Do not restate
the measurement trail here; it is kept current in the design doc, not this
one.

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

Fresh shipping one-vCPU results after the exact pre-map spawn reservation and
batched frame return:

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

**These distributions establish the Phase-6 closure; they do not claim that
desktop-contention p99 is stable.** `p99` ordering across A-B-A controls was
not repeatable and makes no tail claim.

## Multi-vCPU notes

- Lock **contention** proper (`lock-phase-spin`, the only phase measuring two
  CPUs wanting the same word) rises only 72→98 cycles from one to eight vCPU
  — 36% on 10% of an acquisition. Sharding for contention alone would buy
  almost nothing; the acquisition cost is bookkeeping, not waiting.
- `hardware_apic_id` (a `CPUID` triple VM-exit) was invisible at one vCPU
  (35 samples) and dominant at eight (931,626 samples, ~11B cycles) — called
  unconditionally by `send_private_fixed_ipi`, behind every reschedule IPI
  and TLB shootdown, to check whether the destination was the sending CPU.
  Fixed via the dense identity map's `current_apic_id`, which answers without
  leaving the guest; steady-state count is now zero. One vCPU cannot observe
  this class of cost at all — recheck any single-CPU-only profiling
  conclusion at 8 vCPU before trusting it.
- Ranked lock-class/site rendering (`RUSTOS_LOCK_PHASE_PROFILE=true`'s
  `drain_class_and_site_census`) is diagnostic-only and gated: left on, its
  debugcon volume made an 8-vCPU guest advance only ~13s of a 90s host
  timeout. The counters used by exact work-budget assertions stay compiled
  in; only rendering is gated.

## Cost invariants

Correctness invariants in this kernel panic; cost invariants did not by
default, which is how several of the defects above survived undetected.
Four places now assert cost directly, each with a source witness or a
mutation that kills it:

- `kernel/nucleus-core/src/util/lockdep/work_budget.rs`: a ceiling on how many
  times a scope may take a lock class (preemption/migration-safe: the guard
  records the CPU and task and declines to judge when either changed).
- `usermem`'s batched validate/write: a ceiling of one bind each; synchronous
  receive, two.
- `ipc_ops/reply_wait.rs`: polls per turn against `POLLS_PER_WAIT_TURN`
  (`PollsPerTurn` in the `IpcReplyDeadline` TLA+ model).
- The same module: a lock acquisition derives this CPU's logical index no
  further times after the one its caller already made
  (`declare_identity_derivations_on`, which asserts interrupts are masked —
  the property is static, not really a count, so an interruptible scope is
  not countable and is excluded by construction rather than measured).

`formal/ipc-reply-deadline/IpcReplyDeadline.tla` carries the same three
statements as invariants, with matching entries in
`formal/spec-mutations.toml` that each kill exactly one of them. A cost
invariant no mutation kills is decoration.

## Caveat

A single vCPU and a live desktop are the measured default condition unless a
table above states otherwise. The phase counters are global: any task
running during a window contributes to them, so read them as system-wide
costs of an operation, not a benchmark-private tally. `min` in the probe
table is the structural cost; `p99` and `mean` move with desktop contention
and are not a regression signal on their own.
