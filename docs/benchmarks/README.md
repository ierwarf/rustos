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

## Core-path expansion

The broader syscall/memory/data/scheduling and persistent-state collection lane
is documented in [Core-path bundle](core-path-bundle.md). Its
[progress ledger](core-paths-progress.md) separates actual evidence, incomplete
coverage, and optimization acceptance; a collected suite is not a performance
pass. The ordinary bench now also checks null/IPC cost after lifecycle changes.

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

## Raw tail evidence

Each sample-backed result also reports p99.9, p99.99, strict counts above
80,000 / 1,000,000 / 10,000,000 raw TSC ticks, and the fraction of total
elapsed time in the slowest `ceil(N/100)` samples. Quantiles use the existing
`floor(N*q)` index capped at `N-1`; 20,000 samples leave only about 20 samples
at p99.9 and two at p99.99. Repeat complete runs, report every maximum, and
never trim outliers or substitute a minimum for a worst-case result.

The two intra-process round trips retain the eight worst **same-request**
records, with sequence, start/end TSC and paired call-to-receive / server-body /
reply-to-return spans. Independent leg percentiles cannot reconstruct a slow
round trip. An unpaired outlier stays in the complete distribution and worst
set, labelled unpaired rather than assigned invented leg times. A slow leg
locates wall time, not its cause: guest queue/budget waits, IRQ work and host
vCPU descheduling need separate evidence. CPUID tails are a host-exit control,
not proof that an IPC outlier had the same cause.

Tail collection adds no clock read or syscall inside the timed call. The
bounded top-eight update runs between requests; keep the same measurement
infrastructure on both sides of an A/B comparison because pacing can still
change. Extra text is buffered until all probes finish, before the end marker,
so diagnostic printing cannot contaminate the next probe. The host validates
versions, sample counts, quantile order, duplicate identity, exact maximum and
paired sums. Legacy logs without tail records remain readable but do not
acquire fabricated tail evidence. `tail/` and `worst/` table prefixes keep the
new rows out of legacy baseline-minimum parsing.

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
to one probe, with a 5-second post-readiness settle (added to the 25-second
readiness budget, not taken from it) and an explicit
mid-boot phase-counter drain (`SYS_RUSTOS_PHASE_PROFILE_DRAIN`), so the
counters describe that probe rather than every probe summed across the boot.
The four syscall-path phases charged exactly once per call
(`copy-request`, `enqueue`, `write-response`, `enqueue-deadline`) land at a
clean 1.00 ratio under isolation and are what `isolation check: PASS/FAIL`
verifies. Phases charged once per *endpoint* call or per `usermem` access
remain contaminated by the live desktop's own steady-state traffic (uiserver
and WayClick never stop running) and are reported but not gated.

## Current results

See [Benchmark results and lifecycle checkpoints](evidence/results.md#current-results).

## The same-CPU wake fastpath (seL4-style, landed)

See [Benchmark results and lifecycle checkpoints](evidence/results.md#the-same-cpu-wake-fastpath-sel4-style-landed).

## `V5-SCHED-GLOBAL-001` and the scheduler catalog

See [Benchmark results and lifecycle checkpoints](evidence/results.md#v5-sched-global-001-and-the-scheduler-catalog).

## Phase 6 process-lifecycle and frame-settlement checkpoint

See [Benchmark results and lifecycle checkpoints](evidence/results.md#phase-6-process-lifecycle-and-frame-settlement-checkpoint).

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

See [Linked-code cost experiments and rejected candidates](evidence/code-cost.md#the-single-walk-user-copy-landed).

## The synchronous handoff's stale pending flag (landed)

See [Linked-code cost experiments and rejected candidates](evidence/code-cost.md#the-synchronous-handoffs-stale-pending-flag-landed).

## Fusing a slot's decision with the retirement it authorizes (landed)

See [Linked-code cost experiments and rejected candidates](evidence/code-cost.md#fusing-a-slots-decision-with-the-retirement-it-authorizes-landed).

## Logical-CPU identity, and the snapshot that answered one word with six

See [Linked-code cost experiments and rejected candidates](evidence/code-cost.md#logical-cpu-identity-and-the-snapshot-that-answered-one-word-with-six).

## The GOT indirection under every kernel static (landed)

See [Linked-code cost experiments and rejected candidates](evidence/code-cost.md#the-got-indirection-under-every-kernel-static-landed).

## The seven copies of a six-instruction function (landed)

See [Linked-code cost experiments and rejected candidates](evidence/code-cost.md#the-seven-copies-of-a-six-instruction-function-landed).

## Shipping cost budgets without a free-running census

See [Linked-code cost experiments and rejected candidates](evidence/code-cost.md#shipping-cost-budgets-without-a-free-running-census).

## Refuted: gating the identity counter on a live ceiling

See [Linked-code cost experiments and rejected candidates](evidence/code-cost.md#refuted-gating-the-identity-counter-on-a-live-ceiling).

## Published reply message id removes one lock acquisition

See [Linked-code cost experiments and rejected candidates](evidence/code-cost.md#published-reply-message-id-removes-one-lock-acquisition).

## Refuted: GS-relative per-CPU identity

See [Linked-code cost experiments and rejected candidates](evidence/code-cost.md#refuted-gs-relative-per-cpu-identity).

## Where the floor comes from, against the reference designs

See [Linked-code cost experiments and rejected candidates](evidence/code-cost.md#where-the-floor-comes-from-against-the-reference-designs).

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

## Caller-local fast reply allocation

Fast replies use the kernel-stamped caller task ID as an advisory starting
slot in the existing generational reply pool. A completed synchronous call can
therefore reuse its hot slot rather than rotating through all 128 reply slots.
Slot locking, live-value checks, generation retirement, and full-pool collision
fallback are unchanged; no capacity is reserved per caller and a hint grants no
authority. Ordinary replies retain their existing allocation order.

This follows the locality principle of [Linux slab caches](https://github.com/torvalds/linux/blob/master/mm/slub.c)
and [seL4's per-thread reply objects](https://github.com/seL4/seL4/blob/master/src/fastpath/fastpath.c),
not their locking model. The regression witnesses verify that
hinted allocation never touches the shared round-robin cursor, collisions can
use the full pool, concurrent allocations cannot share live authority, and
retired/exhausted generations cannot alias. Static disassembly is evidence of
instruction removal, not a cycle estimate; shipping benchmarks remain required.

Same-session shipping evidence for `ipc_rt_intra_process_reply_recv` (20,000
samples, 8 CPU, `--isolate-probe`; no phase profiling):

| run | min | p50 | p99 | `vmexit_cpuid` min |
| --- | ---: | ---: | ---: | ---: |
| control `b44fe9e8` | 14,600 | 17,680 | 314,360 | 3,640 |
| caller-local | 14,520 | 15,440 | 109,280 | 3,640 |
| caller-local repeat | 14,640 | 15,960 | 188,400 | 3,680 |

Both successful candidate boots passed isolation with exit 0. An intervening
candidate boot failed compositor readiness and is **not** accepted performance
evidence. The control full 8-CPU table also timed out before `ipcbench: end`.
The static fast branch skips the shared `lock xadd`; ordinary replies retain it.
The minimum is effectively unchanged, and the tail remains variable: these runs
do **not** establish the requested p50 <10,000 / p99 <20,000 targets or a stable
tail-latency improvement. The full 1-CPU candidate recorded min 14,160 / p50
19,920 / p99 77,640; its control anchor moved 4,200 -> 3,600, so no 1-CPU
before/after improvement is attributed. Evidence is in `build/bench-ipc-struct/`.

## Caveat

A single vCPU and a live desktop are the measured default condition unless a
table above states otherwise. The phase counters are global: any task
running during a window contributes to them, so read them as system-wide
costs of an operation, not a benchmark-private tally. `min` in the probe
table is the structural cost; `p99` and `mean` move with desktop contention
and are not a regression signal on their own.
