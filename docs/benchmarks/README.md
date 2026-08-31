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
Shipping images leave them off; set `RUSTOS_IPC_PHASE_PROFILE=true`,
`RUSTOS_USERMEM_PHASE_PROFILE=true`, and/or
`RUSTOS_SCHEDULER_PHASE_PROFILE=true` for one diagnosis run. **Every phase
profile in this kernel has cost more than it measured** when checked
(`[lock_telemetry]`, `[scheduler_telemetry]`, `[syscall_telemetry]`,
`[ipc_telemetry]`, `[usermem_telemetry]` — the lock-phase profiler alone was
26% of a round trip).
Ablate before trusting a phase-profiled number: stub the profile's
`now`/`charge` calls to constants, rebuild, and compare against the
unstubbed build in the same session. All five are gated build
switches for exactly this reason; call sites stay unconditional (so a phase
cannot be silently forgotten at its boundary) but only the counter read and
accumulator compile in.

`[usermem_telemetry]` was the fifth and last one still compiled into shipping
images, and it instruments the hottest path there is: every syscall that reads
or writes a user buffer. Measured by flipping only its build switch, with the
anchor held and `null_syscall_getpid` flat at 680 in all three runs:
`ipc_try_recv_empty` 2,680 → **2,480** → 2,680, `ipc_rt_intra_process` 22,880 →
**22,600** → 22,880, `ipc_rt_cross_process_syscalld_getuid` 32,920 → **32,200**
→ 33,000. The "on" column reproduced exactly across two separate boots, which
is what makes the middle column a measurement rather than a pair of runs.

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

One vCPU, the unrestricted probe table, from two consecutive runs of the
current tree. The anchor moved -2.2%, inside the 3% admission bound; both
columns are `min` readings, so their spread is the instrument's own:

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

**What sets the current 17,480-cycle production-shaped local floor** is still
the tracked-lock path, but the old absolute attribution is retired. The
pre-relocation profile counted about forty-two acquisitions per round trip at
~735 *instrumented* ticks each. After the GOT fix, a fresh isolated diagnostic
run reads ~440 ticks per acquisition: `before-acquire` 162 + `spin` 31 +
`after-acquire` 32 + `release` 215. The diagnostic build itself raises the
probe from 17,600 to 31,040, so those 440 ticks are not shipping attribution
and must not be multiplied into the current floor. They establish only that
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

## The single-walk user copy (landed)

`ValidatedUserRead`/`ValidatedUserWrite` claimed in their own doc comment to
let a caller "copy without walking the same page tables a second time", and
then did exactly that: admission walked every page of the span, the proof kept
only the virtual start and length, and `copy_into`/`copy_from` re-entered
`translate_user` for a translation admission had already resolved. Every user
copy in the kernel paid two software page walks — eight dependent loads — for
one span.

The proof now carries `start`'s own translation, offset included, and the byte
movers consume it for the first page before walking anything else. Copies that
stay inside one page — every fixed-layout IPC request, reply, and typed struct
— now walk once. The retained MM generation is what makes the carried
translation exact: the mapping cannot change between admission and copy without
invalidating the bind that produced the proof.

The whole-round-trip effect is below this table's two-percent floor, and the
`usermem-phase-read-copy` mean does **not** attribute it — that mean tracks the
live desktop's copy-size mix, not the path's per-copy cost (1,009 / 782 / 1,021
cycles per sample at 202k / 262k / 202k samples across three runs, the cost
moving with the sample count rather than with the code). What justifies the
change is that the removed walk is redundant by construction, not a measured
delta; no probe regressed, and `sched_yield` held at 5,000 across the extraction.

This is also the first split boundary
`formal/rust-large-files.tsv` named for `kernel/mm/src/memory/address_space.rs`:
the proofs and their admission now live in `address_space/user_copy.rs`, which
took the parent back under the 1,300-line threshold and retired its debt row.

## The synchronous handoff's stale pending flag (landed)

The per-CPU synchronous-handoff FIFO is guarded by a lock-free
`SYNC_HANDOFF_PENDING` flag whose whole purpose is to keep an ordinary dispatch
from taking that FIFO's lock only to read `len == 0`. The consumer cleared it on
`taken.is_none() && state.len == 0` — so a take that *succeeded* and emptied the
queue left the flag set, and the next dispatch paid exactly the acquisition the
flag exists to avoid. It then charged that acquisition to `DrainedStale`, a
reason whose name asserts a queued record was discarded when none existed.

The clear now keys on `state.len == 0` alone. The one-sided invariant and its
proof are unchanged: an enqueue publishes `true` only after releasing this lock,
so a queue observed empty while holding it has no completed insert to strand,
and a capped handoff streak still returns with records queued and `len != 0`.

Measured with `RUSTOS_SCHEDULER_PHASE_PROFILE=true` and
`--isolate-probe ipc_rt_intra_process_reply_recv`, same instrument on both sides:

| outcome | before | after |
| --- | ---: | ---: |
| hits | 70.1% | 71.4% |
| `miss-empty` (no lock taken) | 16.9% | 23.4% |
| `miss-stale` | 8.3% (5,919) | **0.1% (90)** |
| `miss-flag-stale` (new) | — | **0.0% (0)** |
| `miss-streak` | 4.7% | 5.1% |

The stale bucket did not move to the new row; it disappeared. Those dispatches
now short-circuit at the lock-free check instead of taking the FIFO lock, which
is why `miss-empty` absorbed them. **Its end-to-end effect is below this table's
two-percent floor** and is not claimed from the probe table: the change is a
strict reduction in work with no mechanism by which it costs more, and the
counter measurement above is the evidence.

`DrainedStale` also folded two structurally different events — "held records and
discarded all of them" and "was already empty" — and 98% of it was the second,
which its own doc comment called a narrow race. `StartedEmpty`
(`kernel-scheduler-step-sync-miss-flag-stale`) now names that separately, so a
stale-*hint* claim and a stale-*flag* claim can be told apart. It is gated behind
`rustos_scheduler_phase_profile` and costs a shipping build nothing.

## Fusing a slot's decision with the retirement it authorizes (landed)

`RUSTOS_LOCK_PHASE_PROFILE=true cargo xtask bench --isolate-probe <name>` renders
a ranked per-class acquisition census whose rows are *counts*. That matters more
than it looks: counts do not drift with host load, so on a host too noisy to
resolve two percent of a round trip, the census still measures the one lever
this file says moves the floor.

It named `take_fast_endpoint_response` immediately: two sites, `with_mut` and
the `remove` that followed it, at **identical** counts (8,711 each), which is
the signature of one slot's lock taken twice to settle one decision.
`GenerationalSlab::with_mut_take` now runs the decision and, when the decision
is terminal, retires the slot under the same acquisition. The retired object is
returned rather than dropped inside, so its destructor still runs after the
guard releases -- the property the two-step shape existed to get -- and the
window where another CPU could split a decision from its own retirement is
closed. The `remove` site is gone from the census.

**No timing claim is attached.** The host had drifted ~25% by the time this
landed (`vmexit_cpuid` 3,480 -> 4,360; `null_syscall_getpid`, exactly 680 in
twelve of thirteen prior runs, read 840), which is far more than the change is
worth. The count is the evidence, and the count is exact.

The equal-count pair at `take_endpoint_response_detailed` is now resolved by
the generation-checked published `message_id` hint described below. The send
path's reply-id binding remains deliberately separate: it is registered
verbatim as the `endpoint-enqueue-reply-binding-and` mutant, so fusing it edits
`formal/implementation-mutations.tsv` for 0.35 acquisitions per round trip.

## Logical-CPU identity, and the snapshot that answered one word with six

**Size the whole class before optimizing inside it.** At one vCPU the logical
CPU index is always zero, so stubbing `derive_cpu_index` to a constant is a
semantically exact ablation of every identity derivation in the kernel at once.
Anchor held within 2.2%:

| probe | shipping | identity ablated | delta |
| --- | ---: | ---: | ---: |
| `null_syscall_getpid` | 680 | 400 | **-41.2%** |
| `ipc_try_recv_empty` | 2,480 | 1,920 | -22.6% |
| `ipc_rt_cross_process_syscalld_getuid` | 33,120 | 28,280 | -14.6% |
| `ipc_rt_intra_process` | 21,600 | 19,440 | -10.0% |

A second ablation of only `charge_identity_derivation_count` moved
`null_syscall_getpid` to 600, so the counter is about 80 of those 280 cycles and
the derivation itself is the rest. Since packing the four boot-immutable statics
a derivation reads measured as nothing (above), **the lever is the count of
derivations, not the cost of one** -- each is roughly nine cycles, and a null
syscall was paying about thirty.

The per-site census that names them is
`RUSTOS_LOCK_PHASE_PROFILE=true cargo xtask bench --isolate-probe <name>`,
decoding `kernel-identity-site-0..5` (`arg1` = `(fnv1a32(file)<<32)|line`). It
ranked, in one window: `preemption.rs`'s `disable_preemption` (1,124,615),
`cpu_local.rs`'s `current_cpu_task_slot` (663,592), the tracked guard's release
derivation (557,555), **`preemption_snapshot` (459,428)**, `irq.rs` (261,891),
and `current_cpu_task_slot_admitted` (155,835).

**The root cause was the fourth row.** `preemption_disabled` -> `preemption_depth`
-> `preemption_snapshot`, which masks interrupts, derives the index, looks up an
APIC identity, takes three atomic loads, scans the held-lock stack twice, and
asserts a depth/held/pending correspondence -- to answer a question that is one
word. `preemption_depth` now derives once and loads that word. The units assert
is not lost: every `disable_preemption` makes the identical assertion, and an
acquire is the transition that can break the correspondence, where a read cannot.
The three `irq.rs` callers that want the coherent six-field snapshot still take
it directly.

Measured against the quiet-host baseline, reproduced across a control pair whose
anchor held:

| probe | before | after | repeat |
| --- | ---: | ---: | ---: |
| `null_syscall_getpid` | 680 | **600** | 600 |
| `ipc_rt_cross_process_syscalld_getuid` | 33,120 | **31,040** | 30,960 |
| `ipc_rt_intra_process_reply_recv` | 23,560 | 23,320 | 23,280 |
| `ipc_rt_intra_process` | 21,600 | 21,400 | 21,360 |

`null_syscall_getpid` had read exactly 680 in thirteen consecutive runs before
this, which is what makes 600 a measurement rather than a pair of runs.

**Refuted immediately afterwards:** fusing the `preemption_disabled` /
`current_cpu_task_slot_admitted` pair that opens eight `current.rs` entry points
into one derivation under one mask. It measured as nothing with the anchor held
at exactly 0.0%, because the fix above had already made the first half of that
pair cheap. Worth recording as the confirmation it is: the snapshot, not the
duplication, was the cost.

## The GOT indirection under every kernel static (landed)

The largest single change measured in this file. It is a build-configuration
defect, not an algorithmic one, which is why four rounds of algorithmic work
walked past it.

`[kernel.build] relocation_model` read `"none"`, which does not mean "no
relocation" -- it means *do not pass the flag*, leaving rustc on the target
default. The kernel target is `x86_64-unknown-linux-gnu`, whose default is
**PIC**. So every kernel library crate reached its own statics through a GOT
slot. Disassembling the shipping image, one word of admitted policy cost two
dependent loads:

```
mov    0x58681(%rip),%rax      # load a pointer out of the GOT
movzbl (%rax),%eax             # then dereference it
```

The `nucleus` *binary* crate never paid this: `nucleus_rustc_args` already
passes `-C relocation-model=static`. But those are `cargo rustc` arguments, and
they stop at the binary crate. Every kernel library below it -- which is where
all the hot code lives -- was compiled PIC.

Nothing about the image wanted PIC. It links `-no-pie -static` against
`kernel/linker-multiboot2.ld` at a fixed `KERNEL_LOAD_BASE = 0x200000` and
executes at its link address, which is exactly the condition the static model
requires; the binary crate had been relying on that since it was written.
Setting `relocation_model = "static"` makes the libraries agree with the binary
and with the link rather than adding an assumption. The one piece of kernel code
that genuinely runs somewhere other than its link address is the AP trampoline,
and it is hand-written assembly that does its own relocation arithmetic against
`RUSTOS_AP_TRAMPOLINE_PHYS` -- rustc's relocation model does not reach it.

Same-session final pair, anchor held at -1.1%:

| probe | PIC | static | Δ |
| --- | ---: | ---: | ---: |
| `null_syscall_getpid` | 600 | **400** | **-33.3%** |
| `ipc_try_recv_empty` | 2,440 | **1,440** | **-41.0%** |
| `ipc_rt_intra_process` | 21,800 | **15,440** | **-29.2%** |
| `ipc_rt_intra_process_reply_recv` | 23,360 | **17,480** | **-25.2%** |
| `ipc_rt_cross_process_syscalld_getuid` | 31,400 | **23,440** | **-25.4%** |
| `sched_yield` | 5,000 | **4,160** | **-16.8%** |

The structural evidence is host-noise-immune and agrees: `.got` went from
0x1348 (4,936 bytes, 617 entries) to **0x30 (48 bytes, 6 entries)**, and `.text`
shrank by 138 KB (1,586,730 -> 1,448,122).

`null_syscall_getpid` landing on exactly 400 is worth noting: that is the same
floor the identity-derivation ablation reached in the previous round. That
ablation removed the *count* of derivations; this removed the *loads inside*
each one. Two different cuts, one floor -- which is the strongest available
evidence that logical-CPU identity was the null syscall's dominant cost and that
the remaining 400 is something else.

`config/presets/release.toml` and `config/presets/debug.toml` carried the same
`"none"` and were fixed with it; otherwise a release-shaped build would silently
keep the GOT.

SMP bring-up is the risk this change actually carries, and one vCPU never
exercises it -- confirmed separately by booting and running at
`--rustos-vcpus 4`, where the null syscall also reads 400.

## The seven copies of a six-instruction function (landed)

With the GOT gone, `derive_cpu_index` was still emitted **seven times
out-of-line**, each with a full stack frame, despite carrying `#[inline]`. The
attribute was not being ignored; the `CPUID` fallback and the two panics
inflated LLVM's cost estimate past the inlining threshold, so every derivation
in the kernel paid a call and a return to reach six instructions. The per-site
census counts roughly thirty derivations on a null syscall, so there is nowhere
for that call to amortize.

Moving the fallback into a `#[cold] #[inline(never)]`
`derive_cpu_index_by_apic` and the token panic into `token_outside_topology`
left a hot body LLVM inlines: seven out-of-line copies became one.

Confirmed by a same-session A-B-A. The anchor moved on the B runs, so the raw
column is not directly attributable -- but `null_syscall_getpid` read exactly
400 on every run in both directions, which is what rules out a uniform guest
speedup, and the A-control reproduces the loss when the change is removed:

| probe | A (static only) | B | B repeat | A control |
| --- | ---: | ---: | ---: | ---: |
| `ipc_rt_cross_process_syscalld_getuid` | 23,760 | 22,120 | 22,080 | 24,000 |
| `ipc_rt_intra_process_reply_recv` | 17,600 | 16,560 | 16,520 | 17,720 |
| `ipc_split_reply_recv_reply_to_return` | 8,440 | 7,920 | 7,920 | 8,520 |
| `ipc_try_recv_empty` | 1,480 | 1,440 | 1,360 | 1,480 |
| `null_syscall_getpid` | 400 | 400 | 400 | 400 |

`null_syscall_getpid` is unmoved at 400 in every column: that probe has reached
a floor this class of change no longer touches.

## Shipping cost budgets without a free-running census

The lock-class census used to increment one per-CPU counter on every tracked
lock acquisition in every shipping build, even though only three user-memory
scopes create a live `LockBudget`, all for `ProcessState`. That made a
diagnostic population counter part of every IPC endpoint, message, reply,
runqueue, policy, and scheduler lock.

Shipping now registers the exact budgeted class, activates its counter only
for a live nested budget scope, and routes const-generic tracked locks through
that registration. LLVM can erase the counter path entirely for every other
class. `declare` fails closed on an unregistered shipping class. Host tests
retain free-running counters, and `RUSTOS_LOCK_PHASE_PROFILE=true` retains the
complete class/site census; a fresh diagnostic run still reported all lock
phases and passed the isolated attribution gate.

Same-session control/candidate/repeat (`min`; anchors 3,680 / 3,680 / 3,600):

| probe | control | candidate | repeat |
| --- | ---: | ---: | ---: |
| `null_syscall_getpid` | 400 | 400 | 400 |
| `ipc_try_recv_empty` | 1,440 | 1,400 | 1,440 |
| `ipc_rt_intra_process` | 16,320 | 16,120 | 16,120 |
| `ipc_rt_intra_process_reply_recv` | 17,600 | 17,480 | 17,480 |
| `ipc_rt_cross_process_syscalld_getuid` | 23,760 | **23,000** | **23,360** |

The first cross-process candidate is -3.2%; the repeat is -1.7%, just inside
the two-percent floor. Treat this as an exact removal of shipping-only work
with an end-to-end effect bounded at roughly three percent, not as a new
latency floor. The local round-trip changes remain below the floor and carry
no timing claim.

## Refuted: gating the identity counter on a live ceiling

A shipping-only no-op of `charge_identity_derivation_count` bounded the whole
counter at 240 cross-process cycles: 23,200 -> 22,960 with the anchor fixed at
3,640. Preserving the runtime zero-derivation ceiling with a per-CPU active
scope instead replaces every free-running increment with an active-depth load.
That candidate measured 23,160 and 23,480 across two boots; p50 moved from
24,760 to 25,160 and 25,280. It was reverted. The load remains on every
identity derivation, so the invariant-preserving form recovers none of the
ablation reliably enough to ship.

## Published reply message id removes one lock acquisition

`take_endpoint_response_detailed` must lock the endpoint message before the
reply object. It formerly locked the reply once to discover `message_id`, then
locked it again under the message lock to validate and consume the response.
Reply insertion now publishes an advisory per-slot `message_id`; the nested
`REPLIES.with_mut` still validates the full generational handle, the exact
message id, and `consumed`, so the mirror cannot authorize a stale handle.

The host counter pins a pending poll at exactly one `IpcReply` acquisition
instead of two. Same-session shipping measurements (`min`; anchors 3,640 /
3,640 / 3,680) show the structural cut but remain below the timing floor:

| probe | control | candidate | repeat |
| --- | ---: | ---: | ---: |
| `null_syscall_getpid` | 400 | 400 | 400 |
| `ipc_rt_intra_process_reply_recv` | 17,520 | 17,280 | 17,400 |
| `ipc_split_reply_to_return` | 7,040 | 7,000 | 7,000 |
| `ipc_rt_cross_process_syscalld_getuid` | 23,200 | 23,160 | 23,280 |

No end-to-end latency claim is attached: the cross-process result is neutral
and the local improvement is 0.7-1.4%. The exact one-acquisition removal and
the preserved authoritative validation are the acceptance evidence.

## Refuted: GS-relative per-CPU identity

The queued high-risk item from the previous round was replacing the identity
derivation with a `mov rax, gs:[...]` per-CPU load. It was **not attempted**,
because the measurement that would justify it came back negative first.

Replacing only the `RDPID` instruction with a constant -- semantically exact at
one vCPU, where the admitted token is always 1 -- while keeping every policy
load, branch, and the decode, isolates the instruction from its surroundings:

| probe | shipping | RDPID elided | normalized |
| --- | ---: | ---: | ---: |
| `null_syscall_getpid` | 600 | 560 | -3.4% |
| `ipc_rt_cross_process_syscalld_getuid` | 31,400 | 32,120 | +5.8% |

At most about forty cycles on a null syscall and nothing on a round trip. A
GS-relative read replaces exactly that instruction, and it would cost a `swapgs`
on every IDT entry -- in a kernel whose `prepare_for_context_return` already
carries a comment about double-faulting on the first GS-relative kernel access
after a cached-pair mistake. Forty cycles does not buy that.

The ablation also pointed at what *was* expensive, since the full identity
ablation was worth about 280: not the instruction, and (per the earlier refuted
policy-word packing) not the number of statics either. That left the loads
themselves, which is what the disassembly then showed.

## Where the floor comes from, against the reference designs

seL4 takes **no locks at all** on its IPC fastpath: it is a single-kernel-stack
event kernel, and its multicore story is a big kernel lock chosen precisely
because "system calls are short, so lock contention will be low, at least for
IPC" ([From L3 to seL4](https://flint.cs.yale.edu/cs428/doc/L3toseL4.pdf),
[An Evaluation of Coarse-Grained Locking for Multicore
Microkernels](https://arxiv.org/pdf/1609.08372)). QNX Neutrino makes the same
structural bet from the other direction: keep one simple synchronous
`MsgSend`/`MsgReceive`/`MsgReply` primitive with a deliberately simplified
microkernel code path, and build every richer IPC service on top of it ([The QNX
Neutrino Microkernel](https://www.qnx.com/developers/docs/6.5.0SP1/neutrino/sys_arch/kernel.html)).

The pre-relocation census counted about forty-two tracked lock acquisitions per
synchronous round trip. Its often-quoted 735 instrumented ticks/acquisition and
"91% bookkeeping" ratio are both retired: the GOT double-load was inside that
bookkeeping, so neither the absolute figure nor the ratio describes the current
image. A fresh post-fix isolated profile reads about 440 instrumented ticks per
acquisition (`before-acquire` 162, `spin` 31, `after-acquire` 32, `release`
215), while raising the production-shaped probe from 17,600 to 31,040. It is a
diagnostic cost shape, not shipping latency attribution.

What survives is narrower: removing a whole acquisition or a whole category of
shipping-only work can move the floor; rearranging one cached load or one lock
instruction has repeatedly measured below it. The GOT fix and the budgeted
census gate above are examples of the former.

## Cost invariants

Correctness invariants in this kernel panic; cost invariants did not by
default, which is how several of the defects above survived undetected.
Four places now assert cost directly, each with a source witness or a
mutation that kills it:

- `kernel/nucleus-core/src/util/lockdep/work_budget.rs`: a ceiling on how many
  times a scope may take a registered lock class (preemption/migration-safe:
  the guard records the CPU and task and declines to judge when either
  changed). Shipping increments the counter only while such a scope is live;
  host tests and lock-profile builds retain the complete free-running census.
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
