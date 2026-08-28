# Session Handoff

**Role:** volatile routing note for resuming this checkout. It is not build,
runtime, formal, or hardware evidence. Source, the live goal tracker, and fresh
command output win when they disagree with this page.

## Current checkout snapshot

Recorded 2026-08-28. **This block is the only current-state section.** The
pre-commit worktree carries the Phase-3 per-slot payload work, the owner-word
wait state, and performance slices that count every tracked lock class, remove
a debugcon write from inside the global scheduler guard, remove the counted
process retain from the current-task path, and publish exact process/MM
identity per slot. Do not infer a Phase-0--6 closure from the older archive
below.

- The `pr` formal gate passes against this tree: 619 source-conformance
  checks and **587/587** implementation mutations killed, every lane passing.
  Any documentation edit after that run invalidates its source seal and the
  final commit must reseal it.
  `cargo test -p kernel-ps` passes 244 tests.
- **Count lock classes, not one lock.** `work_budget::take_class_census()` plus
  the `kernel-lock-class-0..5` milestones report tracked-lock acquisitions per
  class per window. One synchronous round trip takes about forty-two of them,
  and the most-acquired class was the **global process table** (~10.7/rt) --
  ahead of the endpoint object, the reply object, and the scheduler catalog the
  lane had been splitting (~4.0/rt).
- **A debugcon record was being rendered inside the global scheduler guard.**
  `scheduling-budget-exhausted` fired about sixty times a second from the
  runtime accounting; a milestone is a VM exit per byte under KVM and its
  emitter drains parked records first. Measured, instrumented build: the budget
  charge went 5.9-27 us per dispatch to 0.02-0.03, the `Account` phase 244,085
  us to 10,501 per window, and the guard's hold total 358,280 us to 146,492.
  Shipping build: **minimum and p50 unmoved, mean -39%, p99 about -50%.** It is
  a tail fix. The ceiling is now
  `performance::SCHEDULER_GUARD_MAX_DEBUG_SINK_RECORDS = 0` with a source
  witness.
- **Own-thread process pin.** A running thread pins its own process object, so
  `ProcessRef` now carries either a counted reference or an uncounted pin. The
  pin re-reads the published state pointer instead of caching it, because an
  exec replaces the object and no count holds the old one; every accessor
  reachable from it validates the exact process and MM generation.
  `ProcessTable` first fell ~10.7 -> ~8.4 acquisitions per round trip.
- **Exact process identity publication.** `live_process_identity` now resolves
  the committed live process/MM generation from a per-slot publication and
  uses the locked table only for revoked, stale, or torn observations. The
  counted exact-validation witness is **0 ProcessTable acquisitions**; lifecycle
  transitions revoke and republish under the table lock, and the profile drain
  reports divergence outside the scheduler guard. Focused publication/fallback
  mutants are killed and the profiled KVM run reported zero divergence records.
- **Where the round trip stands**, one vCPU, against this session's own
  control: `ipc_rt_intra_process` min 47,700 -> **43,560--46,720**, p50
  91,800 -> **75,200--83,360**. The historical best p99 is 5.65M, but three
  final bounded reruns read **16.95M--17.20M**. After exact identity publication,
  two fresh runs read min **44,360--44,640**, p50 **73,080--77,600**, and p99
  **17.32M--18.03M**: overlapping, so do not claim an end-to-end win from this
  slice. The tail remains host/KVM-deschedule sensitive.
  `ipc_try_recv_empty` 6,360 -> 5,440. Cross-process `syscalld getuid` 59,040
  -> 55,040. Full tables: `docs/benchmarks/README.md`.
- **The 20,000-30,000 target is not met and no single change reaches it.** The
  cost model the census supports, for the 44,000 minimum: three syscall floors
  (~4,900), two scheduler dispatches (~15,000), three IPC syscall shells
  (~11,500), and ~12,000 of copies and rendezvous. The next three slices, in
  the order their measurements justify:
  1. `IpcEndpoint` (~8.5/rt) and `IpcReply` (~6.5/rt): the endpoint and reply
     objects are entered several times per syscall. Fuse the authorization
     entry into the operation it authorizes.
  2. The dispatch pair (~15,000 of 44,000). seL4's fastpath does not run the
     scheduler at all; RustOS's direct handoff still traps into the full
     dispatch pipeline.
- **Do not try to make the tracked lock cheaper.** Per-acquisition attribution
  is ~735 instrumented cycles spread evenly across admission, the held-class
  stack, and release, with no dominant sub-phase. About forty-two acquisitions
  per round trip makes tracked locking roughly 20,000 of the 43,880 minimum,
  and the only lever is acquiring fewer. A direct-mapped hint over
  `find_task_stack`'s occupancy scan -- the same shape that paid for the task
  directory -- measured no change and was reverted.
- **A fastpath hit is not automatically a speedup.** `ipcbench`'s two-syscall
  server missed the rendezvous fastpath 79% of the time, every miss
  `FallbackNoFrame` -- the receiver was not parked, which is seL4's precondition
  too. The new `ipc_rt_intra_process_reply_recv` probe uses the fused
  reply-and-receive call every production service uses and hits 22,002/22,002;
  its **minimum is higher** (48,000 vs 44,000) and its p50 much lower (52,400 vs
  73,520). Keep both probes; averaging them hides exactly this.
- Performance invariants live in `libs/rustos-user-abi/src/performance.rs`
  (named ceilings), `formal/check-performance-contracts.sh` (source witnesses),
  and `formal/implementation-mutations.tsv` (host tests that *count*
  something). Lock acquisitions are charged on the host path so a ceiling is
  unit-testable; see
  `process_table::tests::an_own_thread_pin_enters_the_global_process_table_zero_times`.
- Ranked lock-class/site rendering is now compiled only with
  `RUSTOS_LOCK_PHASE_PROFILE=true`. Leaving it on in a shipping image emitted
  multiple debugcon records per second; at eight vCPUs the guest advanced only
  about 13 seconds during a 90-second host timeout and never left ipcbench's
  15-second isolation settle. The counters used by exact work-budget
  assertions remain active; only destructive ranking, site rotation, and
  rendering are diagnostic. The immediate eight-vCPU rerun completed: CPUID
  p50 3,760, IPC min 62,560, p50 202,560, p99 36,698,960 cycles. That proves
  liveness is restored, not that SMP latency improved; p50 is worse than the
  prior 172,640--175,640 controls.
- Runtime gates on the previous slice: `cargo xtask kvm-smoke --smp-iteration
  --smp-ring3-qualification` passed at 1, 2, 4, and 8 vCPU with zero
  run-authority mismatches and zero identity divergences. **Re-run the cohort
  before claiming this slice.**
- `docs/benchmarks/README.md` is the measurement record and
  `docs/ai/performance-hardening.md` owns the measurement rules. Both outrank
  this routing note and the historical archive.

## Session log

Historical, oldest first. Superseded by the snapshot above wherever they
disagree, and by `docs/benchmarks/README.md` on anything measured.

### Stage 0a landed, partially: `cargo xtask bench --isolate-probe <name>`

Restricts `ipcbench` to one probe per boot (private KVM contract at
`system/registry/system/ipcbench-probe-v1.env`, read directly by `ipcbench`
the way `uiserver` reads its own acceptance contract — no service mediates
it) plus a 15-second post-readiness settle and a new diagnostic syscall,
`SYS_RUSTOS_PHASE_PROFILE_DRAIN`, that flushes the IPC-call and user-copy
phase windows immediately instead of waiting for their once-per-second
housekeeping drain. Full writeup and the exact ratios:
`docs/benchmarks/README.md`, "Isolating one probe per boot".

**Result: two of the section's premises were wrong, found by booting rather
than by reasoning about it.** The four phases charged once per syscall-path
call now attribute **exactly** (ratio 1.00, verified against a real boot).
The phases charged once per *endpoint* call, and every `usermem-phase-*`,
still read 1.4x–7x high in the cleanest run — down from ratios in the
thousands, but not inside the 0.95–1.05 band. Tripling the settle from 15s to
45s moved these by under 10%, so it is **steady-state** desktop traffic
(most likely uiserver's own compositor/Wayland loop, which shares these
global counters), not a decaying startup burst, and more settle time will not
close it. `ipcbench` cannot run without `--gui-dvm-surfaces` — without it,
uiserver's `open_display` polls forever and the session catalog never
launches `ipcbench` at all — so this topology, and this noise floor, is not
avoidable by a smaller flag change.

**Decision made by the user (2026-08-18, same session):** (a) — proceed with
the four clean syscall-path phases; treat endpoint-call/usermem phases as
bounded-but-imprecise rather than blocking on (b) or (c).

### Stage 0b landed: receiver-side phases, ablated, unconditional

`kernel/compat/src/user/syscall/linux/ipc_server_profile.rs` adds four
phases mirroring the caller's four clean ones — `recv-take`, `recv-write`,
`reply-publish`, `reply-wake` — charged in `recv_with_sender_blocking_prepared`
and `syscall_linux_rustos_ipc_reply` (the exact two syscalls `ipcbench`'s
server uses; the plain `syscall_linux_rustos_ipc_recv` and combined
`ipc_reply_recv_with_sender` paths are not instrumented yet, a scoping
choice). Full writeup: `docs/benchmarks/README.md`, "Instrumenting the
receiver side".

**Historical four-site-only ablation** (stub `now`/`charge` to constants,
rebuild, boot, compare against the unstubbed build with the anchor held at
+1.0%):
`ipc_rt_intra_process` moved **-0.5% normalized** — inside the ±2% floor.
That result covered only receiver sites, not the caller's twelve TSC charges
or fast-handoff shared counters. It no longer justifies shipping all IPC
instrumentation unconditionally: `[ipc_telemetry] phase_profile` now gates the
complete diagnostic unit.

**Attribution:** all four land at 1.34x-1.35x per round trip under
`--isolate-probe` — the same band as the caller's endpoint-call phases, for
the same reason (any receiver on this path charges them, not only
`ipcbench`'s own server). They join the bounded-but-imprecise bucket, not the
four clean ones. Per-operation costs, less sensitive to the sample-count
contamination: `recv-write` ~4,535 cyc, `reply-publish` ~5,782 cyc,
`reply-wake` ~4,097 cyc. `recv-take` averages 897,796 — that is wait time
(the receiver blocked between requests), not a cost; the phase is closer in
spirit to the caller's `wait-blocked` than to its narrow `wait-take`, and the
doc explains why a narrower charge wasn't possible without re-adding the
per-retry contamination the caller's four clean phases specifically avoid.

**Bug found and fixed along the way, worth knowing if this recurs:** the
first boot with these phases live showed zero `ipc-server-phase-*` rows in
the rendered table even though the debugcon log had them. Cause:
`tools/xtask/src/bench.rs`'s `PHASE_PREFIXES` filter didn't include
`"ipc-server-phase-"`, so `parse_phase_milestone` silently dropped every one
of them. Not a kernel bug — check this list first if a new phase family
prints nothing.

### Stage 1 sizing attempted, no fusable target found

User chose to size the dark ticks rather than stop at Stage 0. Using
`RUSTOS_SCHEDULER_PHASE_PROFILE=true` layered onto `--isolate-probe`, decoded
one `kernel-scheduler-phase-*` window by hand (68,841 dispatches, 315,163
lock acquisitions, 999ms): the dispatch chain (account/balance/validate/
select/commit/arch_restore/prologue) summed to 207,395µs of 260,982µs total
lock-hold — 20.5% (53,587µs) uncovered by any of those seven phases.

**First pass at this was wrong and got corrected in the same session, not
after.** Initially read the uncovered 20.5% as a new dark chunk and added it
on top of the caller/receiver phase totals (~19,200-24,050 more ticks,
"scheduler in-lock ~25-31%"). The acquisition census (`kernel-scheduler-
acquire-0..7`, FNV-1a32 file hash + line, matched against the tree)
corrected this: two sites (`irq.rs:736` dispatch, `irq.rs:850` block-commit)
are dispatch/block; the other six, ~108,000 acquisitions, are all in
`kernel/ps/src/multitask/current.rs` (`arm_block_current_task`,
`inherit_ipc_priority`, `reserve_ipc_call_donation`, `release_ipc_priority`,
`complete_ipc_reply_wake_handoff`, `user_log_ids_for_task`) and every one is
called **from inside** a phase already instrumented this session
(`arm_block_current_task` inside `WaitArm`, etc.). **The 20.5% is already
inside the phase totals, not additive to them.**

Net effect: no new fusable target. Each of the six `current.rs` functions is
already minimal and single-purpose; fusing them repeats the shape of the
three acquisition-fusion attempts this lane already refuted. This
*reinforces* "lockdep dominates every operation, no single hot spot" rather
than finding an exception to it. Full writeup: `docs/benchmarks/README.md`,
"Sizing the dark ticks with what Stage 0 built".

**`perf record` was also tried** (user request), attached to the RustOS QEMU
process during an isolated run. `perf report`'s aggregation hung
indefinitely regardless of options (host kernel symbols are
permission-restricted in this environment — `/sys/kernel/debug/tracing/...`
denies access — root cause not fully diagnosed, `perf script` works fine as
a workaround). Findings from `perf script`: 88.7% of samples are inside the
KVM guest-run path (`[unknown]`, unresolvable — perf cannot see inside a
non-Linux guest's own execution, no guest symbol table exists for RustOS).
Of the resolved 11.3%, the largest cluster is QEMU's MMIO/character-device
dispatch machinery (`address_space_translate_internal`, `qemu_chr_write*`,
`io_channel_send_full`) — debugcon-adjacent host emulation cost, ~1.5% of
all samples. Small and corroborating (consistent with `vmexit_cpuid` already
ruling out hypervisor exits as dominant), not a new finding.

**State at session end: instrumentation and sizing work is done and
verified; no code change was made to the IPC path itself.** The four clean
phases plus the four Stage 0b receiver phases plus the dispatch chain
account for roughly 48,000-53,000 of 78,080 ticks (61-67%, receiver and
dispatch figures both approximate). What remains dark is the two blocking
transitions' architectural mechanics and syscall entry/exit floor
(~4,920 ticks for three syscalls) — the same target this lane already had,
not a newly discovered one.

### Stage 2: the seL4-bypass hypothesis was refuted, root-causing the sync-handoff miss rate found a real fix

User asked to take the IPC/scheduler lane seL4/QNX-style and accept
structural risk. The literal hypothesis — dispatch runs the full seven-phase
pipeline even when the decision is already O(1) via the IPC direct-handoff
hint — was **half right and the actionable half was different from what it
looked like**: the decision was already O(1) on a hint hit (pre-existing,
not a gap), and the pipeline stages that stay unconditional are load-bearing
for other CPUs/lock-ordering (a documented past corruption incident exists
from making them stale), not free CFS-scan residue to skip. A literal
"skip the pipeline" bypass was refuted by reading the dispatch phases before
any code was written — see `docs/benchmarks/README.md`, "Sizing the
synchronous handoff hit rate".

The real lever, found by measuring instead of designing around the
refuted hypothesis: only **28.3%** of dispatches hit the direct-handoff FIFO.
New counters (`kernel/ps/src/multitask/scheduler/locality.rs`, gated behind
`rustos_scheduler_phase_profile`, zero cost off) traced the miss precisely:
43.3% is unrelated CPU traffic (expected, system-wide counter), and **28.4%
was 100% attributable to reply-direction (caller-wake) tokens going stale on
`Generation` mismatch, every time, deterministically** — not contention.

Root cause: `wake_task_slot` unconditionally routes every wake, even a
same-CPU one, through `publish_remote_wake`'s cross-CPU mailbox protocol
(Blocked → `RemoteQueued`, one generation bump). The reply-wake token mints
correctly against that generation. But `drain_remote_wakes`, run
unconditionally by every dispatch's Balance phase (which runs *before*
Select in the same dispatch), promotes `RemoteQueued` → `Local` with a
*second* generation bump before Select ever checks the token once. Two
static-reasoning-only hypotheses along the way ("cross-entry-point racing",
"already dispatched via CFS") were both wrong and both refuted by counters
before landing on this — see `docs/benchmarks/README.md`,
"Root-causing the sync-handoff miss rate", for the full trace and the two
wrong turns, kept rather than deleted.

**Fix**: `runqueue::publish_local_wake` (new, `runqueue.rs`) — identical
terminal/dedup dispatch to `publish_remote_wake`, but its `Blocked` case
calls `publish_local` directly (the one-step transition Balance already uses
for the outgoing task) instead of minting a mailbox record.
`publish_runqueue_wake_to` branches on `target == current_dispatch_cpu()`.
Researched seL4's actual fastpath first (`docs.sel4.systems`,
`src/fastpath/fastpath.c`): its governing principle, "dest thread is set
Running, but not queued" — schedulability is exactly one structure at a
time, never two racing representations — is what this fix reproduces for the
same-CPU case, with the cross-CPU mailbox path (every actual liveness
guarantee) byte-for-byte unchanged. General wake-primitive change, not an
IPC-only patch — every same-CPU wake in the kernel takes the direct path now.

**Measured**: hit rate 28.3% → **56.6%** (exactly doubled — call-direction
was already ~100%, reply-direction now also hits reliably). `Custody`
mismatch: 99.5% of the stale bucket → **0.0%**, eliminated not reduced.
`ipc_rt_intra_process`: 73,760 → 70,120 (**-5.9% normalized**, anchor held).
`ipc_split_reply_to_return` (the half this targets): **-10.1%**. Both
control probes (`null_syscall_getpid`, `sched_yield`) read as noise.
`ipc_rt_cross_process_syscalld_getuid` (a real server, not the synthetic
bench pair) also moved (-3.7%), so this is not an `ipcbench` artifact.

Two source-conformance/mutation-anchor breaks along the way, both from
logically-equivalent refactoring changing literal text these tools pin
exactly — `formal/run-source-conformance.sh` and
`formal/implementation-mutations.tsv` both anchor on exact source strings,
not behavior. Fixed by preserving literal text with `!(...)` wrapping rather
than flipping operators, and by retargeting/adding `occurrence=N/M` rows
when a pattern got duplicated. Full detail:
`~/.claude/plans/radiant-stargazing-moon.md` (this session's plan file) and
the `formal-witness-anchors` memory.

Also landed, lower priority, not yet benchmarked against a workload:
`phys.rs` batched frame allocator (`alloc_frames_batch`/
`try_free_frames_batch`) wired into `address_space.rs`'s private-address-
space map/unmap paths, replacing one `PHYS_ALLOCATOR` lock acquisition per
4 KiB page with one per 64-page chunk. Unit-tested; no spawn/exec/exit bench
probe exists yet to measure it end to end.

### Deleted surfaces — do not re-create

- `kernel/ps/src/multitask/syscall_simd.rs` and `SyscallUserSimdSnapshot`. The
  syscall entry stub's sixteen `movdqu` are the whole of the syscall path's FPU
  custody. See `nucleus_audit.rs` for what holds the rest.
- `kernel/compat/src/user/linux.rs`. Truth is `kernel/ps/src/user/linux.rs`.

### Root cause of the SMP collapse: `CPUID` in the raw-lock path

`hardware_apic_id()` derives the architectural identity with `CPUID`, which is
an unconditional VM exit under hardware virtualization. The raw guard captured
it on acquisition, on failed acquisition, and again on release, so every
tracked lock paid up to twelve exits. A dispatch takes roughly fourteen locks.

Measured at 8 vCPU, per one-second window:

| metric | before | after |
| --- | ---: | ---: |
| scheduler lock wait total | 5,900,000 us | 28,782 us |
| scheduler lock hold total | 688,000 us | 49,714 us |
| hold per dispatch | 329 us | 24-42 us |
| dispatches per second | 2,100 | 8,607 |

The guard now reads the admitted identity for the dense logical index
(`current_apic_id`), and the remaining `CPUID` uses are confined to panic
message arguments, which Rust evaluates only on failure.

This means `V5-SCHED-GLOBAL-001`'s measured symptom was lock *implementation*
cost, not the global lock *structure*. The average local runnable set is 1.76
slots, so no per-candidate scan could have been responsible.

### Other landed changes

- SMP invariant-TSC clocksource admission through a bounded zero-warp AP
  rendezvous in `kernel/hal/src/arch/clock.rs`. All seven APs measure warp 0.
  Previously SMP fell back to the HPET main counter, making every timestamp an
  MMIO exit. Fail-closed: a nonzero warp keeps HPET.
- Lockdep validates each dependency edge once (Linux's design) instead of
  republishing it and re-running the global reachability search per
  acquisition. Extracted to `util/lockdep/dependency_graph.rs`.
- Spin loops sample their deadline in batches (`clock::SpinDeadline`) instead
  of reading the clocksource every iteration: scheduler, TLB protocol and ACK,
  APIC ICR.
- Permanent scheduler phase instrumentation: eight disjoint in-owner segments
  plus the local runnable-set size, emitted as `kernel-scheduler-phase`,
  `-phase-select`, and `-phase-tail`. This is what made the diagnosis possible.
- `scheduler/ipc_donation.rs` extracted from `scheduler.rs`; the large-file
  ledger entry was tightened from 5446 to the current value.
- Seven `scheduler/affinity.rs` tests lacked process-table isolation and made
  `cargo test -p kernel-ps` non-deterministic under parallel execution.

A per-slot execution-owner word was added and then **removed**: it produced no
measurable benefit and caused an SMP regression (initd restart at 2 and 8 vCPU
from `endpoint not ready`). Do not reintroduce it without a measurement that
justifies it. The CPU-indexed current/transition arrays remain the sole
ownership authority.

### Open blocker: input-ring drain stall

1 vCPU passes fully. 2 and 8 vCPU now reach complete readiness and then fail
with the host relay's `fixed input-ring credit timeout outstanding=1279
limit=1279 timeout_ms=50`. This is new: the session's first 8 vCPU run showed
`input-ring=1707/1699`, so the drain kept up before the speedup.

Evidence gathered so far:

- inputd drains about one record per broker turn (`records=257
  batch_seq=248`), which was adequate before and is not after.
- The `dvm-input-ring: backlog high-water` warning never fires, so no drain
  turn ever observes a large backlog. The guest stops calling the drain rather
  than falling behind inside it.
- Ruled out by direct evidence: transport revoke (no warning); publish-stage
  blocking (one `session authority sync retry errno=38` at attempt 1 only);
  drain-claim contention (`service_dvm_input_pending` has exactly one caller);
  the retry-without-wait threshold (`INPUTD_INGEST_MAX_EVENTS` and
  `MAX_RECORDS_PER_BROKER_TURN` are both 256, so a full batch does retry); the
  wait broker's check-arm-recheck, which is correctly ordered; and the
  readiness mapping claim, which never emits `readiness claim refused`.
- The host re-kicks during backlog through `recovery_kick`, so the "ring at
  most once per wake generation" rule is not the cause on its own.

The dominant remaining signal is **run-to-run variance**. Three consecutive
runs of the same sealed build failed three different ways:

1. `input-ring credit timeout` after forwarding 1659 events,
2. `input-ring credit timeout` after forwarding exactly 1279 events with only
   one record drained,
3. `runtime trace step input-policy missed its absolute deadline: 1862 > 1500`.

Before the lock-path fix the system was too slow for any of these bounds to be
approached. Treat the next step as characterizing that jitter, not as finding
one more logic bug: sample where the 1.9 s input-policy step actually goes,
using the same phase-attribution method that resolved the scheduler question.
Do not add further speculative fixes; each unverified guess in this session
cost a full seal, build, and run cycle and none of the first three moved the
measurement.

Still worth closing regardless of the outcome: `arm_consumer_wake` documents
"L0 rings at most once per generation" while `start_dvm_ingestion_worker`
documents "L0 rings every committed record". Those cannot both be the contract.

### Current blocker: inputd stops draining while retrying a session sync

With the devmgrd registration fix in place, 8 vCPU reaches full readiness
including storage, GPU, and UI, and then fails with the host relay's
`fixed input-ring credit timeout outstanding=1279 limit=1279`.

The guest side says exactly why:

```
inputd: DVM transport progress records=1 batch=1 batch_seq=1 stage=decoded
inputd: DVM session authority sync retry errno=38 attempt=1
inputd: DVM session authority sync retry errno=38 attempt=2
inputd: DVM session authority sync retry errno=38 attempt=4
```

One record is decoded and `stage=published` never follows. The ingestion loop
in `start_dvm_ingestion_worker` drains, decodes, then calls
`dvm_session_sync::apply`, and retries that call with backoff until a five
second deadline. While it retries it does not return to the drain, so the ring
fills and the host fails closed at its 50 ms credit window.

Two separate defects here, and they should not be conflated:

1. `errno=38` is `ENOSYS`, which the caller treats as retryable. Earlier runs
   did eventually reach `stage=published`, so netd is returning "not
   implemented" to mean "not ready yet". A terminal errno standing in for a
   transient condition is exactly what `V5-DEADLINE-012` forbids: the caller
   cannot distinguish a provider that will never implement the op from one
   that is still starting. netd should return a distinguishable transient
   status, and inputd should fail fast on genuinely terminal ones.
2. The larger defect is the coupling. `start_dvm_ingestion_worker` documents
   that "device progress is independent of any application's read cadence",
   and it is not: a downstream service handshake blocks device ingress. The
   drain and the publish need to be separated so a stalled publish cannot stop
   the ring from being emptied. Fixing only the errno semantics would leave
   this intact and the next slow downstream would reproduce it.

### Earlier blocker, now fixed: devmgrd registered before it could serve

After the two commits above, boot on 8 vCPU stops with

```
initd: fatal service endpoint not ready exec=services/devmgrd/devmgrd.elf pid=37 errno=1
rootd: restart budget exhausted
```

`errno=1` is `EPERM`, not a timeout, which initially looks like an authorization
denial. It is not. The evidence bisects cleanly:

- `initd -> devmgrd` is explicitly permitted by `service_dependency_allowed`.
- The rootd authorization paths return `EINVAL` (22) or `EACCES` (13); none
  returns `EPERM`.
- The log contains **zero** `rootd: service capability denied` lines.
- rootd logs `service capability request received` six times and
  `service capability replied ok` three times. That second line additionally
  requires `replied >= 0`, so all six requests were *authorized* and three
  replies **failed to send**.

That is the same phenomenon as the `ipc-reply-rejected ... InvalidHandle`
records: the caller's deadline expires, it abandons the reply capability, and
rootd's late reply is rejected by the kernel. The caller then surfaces a
permission-shaped error for what is really a latency failure.

The last link is in the kernel: `ipc_error_to_linux_errno` maps
`IpcError::PermissionDenied` to `LINUX_EPERM`, and `enqueue_call_and_wake`
returns that when the caller may not call the target endpoint. initd's
capability to call rootd is therefore absent at the moment of the devmgrd
lookup — because the grant reply that would have installed it was one of the
three that failed to send.

So the complete chain is: a rootd round trip exceeds its caller's deadline;
the caller abandons the reply capability; rootd's reply is rejected with
`InvalidHandle`; the capability is never installed; initd's next rootd call
returns `PermissionDenied`, surfacing as `EPERM`; the devmgrd endpoint lookup
fails; initd treats the endpoint barrier as fatal; rootd exhausts its restart
budget. Every step of this is confirmed by log evidence rather than inferred.

Two things to fix, in this order:

1. Find why the rootd round trip exceeds the caller deadline. Use the phase
   attribution method that resolved the scheduler question rather than
   guessing; the scheduler is no longer the constraint, so this is either
   rootd-side work or a deadline that was always too tight and only now gets
   reached.
2. Stop reporting a late reply as `EPERM`. A rejected reply caused by an
   expired deadline must surface as a timeout, or every future occurrence will
   be misdiagnosed as an authorization bug exactly as it was here.

### Attributed: the `input-policy` boot overrun is IPC reply rejection

The `input-policy` step in `formal/product-scenarios.tsv` is measured from the
boot origin to `name=dvm-input-policy-ready` with a 1500 ms budget. It landed
at 1861 ms. Decoding `ipc-reply-rejected` from the same log
(`arg0` = reply capability, `arg1` = receiver pid in the high half, reason in
the low half, per `ipc_reply_diagnostics.rs`):

```
1623.0 ms  reply=0x20047  receiver_pid=12  InvalidHandle
1743.2 ms  reply=0x20060  receiver_pid=12  InvalidHandle
1814.5 ms  reply=0x20073  receiver_pid=12  InvalidHandle
1852.5 ms  reply=0x2007a  receiver_pid=12  InvalidHandle
2011.7 ms  reply=0x5      receiver_pid=12  InvalidHandle
3138.7 ms  reply=0xa      receiver_pid=12  InvalidHandle
```

The four gaps preceding those rejections are 191, 120, 71, and 38 ms and carry
no other log output, so they are waiting, not work. Together they account for
roughly 420 ms, which more than covers the 361 ms overrun.

Reading: process 12 is a pre-initd bootstrap service (its pid precedes initd at
17). Its calls time out, it abandons the reply capability, the service replies
late, and the kernel rejects that reply with `InvalidHandle`. The reply
capability ids climb (`0x20047` to `0x2007a`) and then restart low (`0x5`),
which is the signature of process 12 restarting.

Note that `receiver_process_id` is `multitask::current_user_process_id()`, so
it names the process *making* the reply, not the caller. Process 12 is
therefore a bootstrap service whose own replies are rejected.

### Root cause of that chain: vfsd's snapshot worker owns a second VfsState

`services/vfsd/src/snapshot_worker.rs::snapshot_worker_entry` begins with
`let mut state = VfsState::new();`, while `service_main` separately holds its
own `VfsState`. `open_executable_snapshot` in `state_storage.rs` resolves
through `self.executable_snapshot_cache`, which is per-state. The worker's
cache is therefore always cold and permanently disjoint from the receive
owner's, so every snapshot redoes path resolution, metadata, and full block
I/O instead of hitting a warm entry.

That produces the observed chain end to end:

1. worker snapshot takes on the order of seconds (`ipc slow call:
   endpoint=65539 wait_ms=2006`),
2. loaderd's snapshot call times out (`executable snapshot call failed
   exec=services/initd/initd.elf errno=110`),
3. vfsd replies late and the kernel rejects it (`ipc-reply-rejected ...
   InvalidHandle`, the receiver pid being vfsd itself),
4. loaderd retries, initd never reaches endpoint readiness, and rootd
   exhausts its restart budget.

This is `V5-VFSD-HOL-007` implemented with exactly the dual authority the audit
forbids: the worker must not carry mutable global vfsd state. The audit's own
design in section 5.2 F is the fix and should be followed rather than
short-cut:

- `VfsNamespaceOwner` briefly holds the single state to resolve the request
  into an immutable `SnapshotPlan { mount_gen, inode_gen, extents,
  expected_hash, reply_cap, deadline }`,
- `SnapshotWorkerPool` executes that plan with no mutable global state held,
- `VfsCompletionOwner` re-takes the state briefly to validate generations,
  commit the cache entry, and reply.

Do not "fix" this by sharing one `VfsState` under a lock held across the bulk
read: that restores the head-of-line blocking `V5-VFSD-HOL-007` exists to
remove. Split `open_executable_snapshot` into a plan phase and a commit phase
instead.

Also enrich the rejection milestone while doing this: it currently carries only
the reply id, replier pid, and reason, which is why identifying process 12
required cross-referencing spawn order and endpoint ids.

### vfsd storage/namespace split (closes the dual-authority defect)

`VfsState` was carrying both namespace state (`cwd`, `handles`, `epolls`,
checkpoints) and storage state (`volume` and every cache derived from it).
Because the receive owner and the snapshot worker each constructed their own
`VfsState`, each also got its own `FatVolume` over the same device and its own
caches: the worker could never hit a warm entry, and two independent mutable
views of one device existed simultaneously.

Storage is now a separate `VfsStorage` behind one shared, yielding-locked owner
(`lock_vfs_storage`). The worker holds no `VfsState` at all. A namespace-only
request never touches the storage lock, so it is not delayed by a bulk read,
which preserves what `V5-VFSD-HOL-007` actually requires.

Two hazards were caught while doing this and are worth remembering:

- A blanket rewrite of `self.metadata(...)` to `lock_vfs_storage().metadata(...)`
  also hit call sites *inside* `impl VfsStorage`, which would have re-acquired
  the guard re-entrantly and deadlocked the worker on its first snapshot.
- A guard temporary in a `match` scrutinee lives for the whole `match` in Rust,
  so any arm that re-acquires deadlocks. Both were checked mechanically and are
  clean; re-check them after any edit to these files.

### vfsd and syscalld binary tests do not run

`services/vfsd/Cargo.toml` and `services/syscalld/Cargo.toml` set
`test = false` on their `[[bin]]` target because the binaries are `#![no_std]
#![no_main]`. Five `#[test]` functions in `services/vfsd/src/main.rs` and one in
`services/syscalld/src/main.rs` therefore never execute. They read as coverage
and provide none. Either move the testable logic into the crate's `lib.rs`,
which is where vfsd's 21 executing tests live, or make the binary host-testable.
Do not add new tests to those files until this is resolved.

### SCHED-GLOBAL-001 is now validated by measurement, and only now

Early in this session the global scheduler lock looked saturated, but the cause
was `CPUID` exits inside every tracked lock: each hold was 329 us of VM exits at
2,100 dispatches per second. Removing them cut lock wait from 5.9 s/s to
0.029 s/s, which is why the lock stopped being the constraint at that moment.

With the rest of the boot path fixed the system now does far more scheduling
work, and the lock is saturated again for the real reason:

| metric, per second at 8 vCPU | value |
| --- | ---: |
| dispatches | 29,481 |
| lock hold total | 682,222 us |
| hold per dispatch | 23 us |
| lock hold duty on one lock | 68 percent |
| lock wait total | 3,647,336 us |
| share of all CPU time spent waiting | 46 percent |

The volume is genuine work, not redundant entries: 23,261 of 29,481 dispatches
are real task switches and only 6,220 are same-task, while entry causes are
18,648 software yields, 10,628 reschedule IPIs, and 205 timer leaves. A
same-task fast path would therefore recover at most 21 percent and would not
change the outcome.

In-owner attribution per dispatch is roughly 8.8 us selection, 2.2 us balance,
1.4 us validation, and 0.2 us accounting, so about 12.6 us of the 23 us hold is
attributed and the critical section is not dominated by any single scan.

The conclusion is that one lock held 68 percent of the time and acquired 29,481
times per second across eight CPUs cannot be repaired by shortening the
critical section. Per-CPU dispatch authority, the audit's patch E and its
P1.4 to P1.6 staging, is the correct fix and is now justified by measurement
rather than by the earlier misattribution.

It is deliberately not attempted here. It is a staged migration that needs
shadow-read validation against the legacy backend, a boot-selected backend, and
its own refinement model for `V5-FORMAL-SCHED-019`, and landing a partial
version without that validation would repeat the per-slot owner-word mistake
recorded above, which passed every unit test and only failed in KVM.

**Size it against acquisitions, not dispatches.** Dispatch counts undercount
the contention badly. A guard-acquisition counter now reports **76,738
acquisitions per second against 4,198 dispatches** in the same window, so the
lock is taken roughly eighteen times more often than it makes a scheduling
decision. The remainder is non-dispatch traffic on the same lock: `wake_task`,
pick-hint publication, IPC donation, affinity, and lifecycle.

This changes the plan. The audit's patch E is written around dispatch
selection, and sharding only the dispatch path would leave the large majority
of acquisitions on the global lock and would not deliver the expected relief.
Whatever is built has to move the wake and hint paths off the global lock too,
or measure first which of those callers dominate. The acquisition counter is
emitted in `kernel-scheduler-phase-select` arg1 low half specifically so that
breakdown can be taken before any code is moved.

### The uiserver owner findings do not reproduce as described

`V5-GPU-UI-OWNER-014` states that `GpuCompositor::present` performs a full
scene rebuild, atlas copy, and submit, then waits synchronously through
`retire_oldest(true)`, stalling the UI loop for tens to hundreds of
milliseconds. On current source that blocking retire is reached only in the
`!self.active` branch, which runs once on the activation frame to publish the
compositor-active contract. Steady state runs the non-blocking
`while self.retire_oldest(false)? {}` and returns `EAGAIN` rather than owning
the UI thread, which the code states directly.

The frame instrumentation added for `V5-UI-PIPELINE-011` measures the same
thing. Across **3120 frames** in one 8 vCPU run there was exactly **one** slow
full present, **one** slow partial present, and **one** slow loop, about
0.03 percent. The single slow present is the activation frame. Steady frames
run 4.8 to 6.8 ms with Wayland around 2 to 3 ms and present around 2 ms, so the
loop sustains roughly 125 iterations per second, comfortably inside a 55 FPS
budget.

`V5-WAYLAND-HOL-013` describes protocol dispatch, render, and present sharing
one owner. That is still structurally true: the main loop runs input, then
Wayland, then render and present in sequence. What does not hold is the
consequence. Because present never blocks in steady state, protocol progress
is not delayed behind it, and no head-of-line stall appears in the frame
records.

Read these two as **risk mitigated, not architecture changed**. The single
owner remains, so a future blocking call added to the present path would
reintroduce the stall with nothing structural to prevent it. Splitting the
protocol, scene, and submission owners is still the more defensible end state.
But it is a large refactor of an 18k line service, and on this evidence it
would not move the frame rate, so it should not be done as a performance fix.
If it is done, justify it as ownership hardening and keep the frame records as
the before-and-after check.

This is the same pattern as the other corrections in this document: the audit
was written against a snapshot where every path was roughly two orders of
magnitude slower, which made a once-per-activation blocking wait look like a
per-frame stall.

### Audit v5 closure, verified against source

**Fourteen closed:** `SCHED-DONATION-002`, `SCHED-RESCHED-003`,
`IPC-STREAM-004`, `DVM-LIFECYCLE-005`, `DVM-INPUT-MAP-006`, `VFSD-HOL-007`,
`WAITSET-HERD-008`, `UI-CLASS-010`, `DEADLINE-012`, `FORMAL-RESCHED-016`,
`FORMAL-DVM-017`, `FORMAL-IPC-018`, `FORMAL-EXEC-020`, `MM-STACK-DEAD-021`.

**Three partial:** `TLB-SCALE-009` (fail-stop policy now explicit in source and
contract; host-stall runtime evidence outstanding), `UI-PIPELINE-011`
(scheduler-side phase attribution now exists; the end-to-end `frame_seq` that
joins wake, IPC, Wayland, GPU, and present does not), `IPC-AUTH-015`
(service/channel/range binding present; the receiver-set epoch is absent, and
adding it needs a kernel channel registry because the sender cannot reach the
peer's open description at send time).

**Four open:** `SCHED-GLOBAL-001` (the global dispatch lock remains, though
measurement showed lock *implementation* cost dominated it, not the lock
structure), `WAYLAND-HOL-013` and `GPU-UI-OWNER-014` (uiserver still runs one
sequential loop: input, then Wayland dispatch, then render, then present, with
no separate protocol/scene/submission owners), `FORMAL-SCHED-019` (blocked on
the per-CPU backend existing).

Verification here was source inspection, not counterexample reproduction.

**Warning about how this matrix was produced.** Two entries were initially
misclassified by grepping for identifiers taken from the audit's *proposed*
design rather than reading the implementation:

- `WAITSET-HERD-008` was recorded as partial after searching for `WaitKey` and
  `object_gen`. The implementation is complete and uses `object_id`: the kernel
  filters wakes with `wake_matching(provider, Some(object_id), Some(generation))`
  and providers signal exact objects (netd per socket token, inputd per input
  object, vfsd keyed by `(provider, object_id)`).
- `ChannelIdentity.generation` was called a defect for duplicating `channel_id`.
  It is redundant, not broken: `allocate_unix_channel_id` is a monotonic
  never-reused counter, so channel identities cannot alias.

Read the code before trusting a name-based search against this audit.

### SCHED-GLOBAL-001: what the acquisition census actually proved

The audit called for a per-CPU runqueue to replace global dispatch
serialization. Measuring the callers first changed the shape of the fix, and
the measurement is reproducible from the milestones described below.

`Scheduler::scheduler_mut` records its `#[track_caller]` caller into a 64-slot
census (`kernel/ps/src/multitask/cpu_local.rs`), and the once-per-second
runtime profile emits the top four as `kernel-scheduler-acquire-0..3`
milestones, packing an FNV-1a32 hash of the caller file in the high half of
`arg1` and the line in the low half. Milestones are required here: the ordinary
`debug::info!` channel does not reach the debug transport in the product
configuration, so a census emitted through it produced no output at all and
briefly read as a broken counter.

The result at 8 vCPU: the dominant callers were not dispatch. They were
read-only identity queries about the task already running on the asking CPU,
issued by syscall entry and return, and `scheduler_ref()` is literally
`scheduler_mut()`, so each one took the exclusive global lock:

| caller | acquisitions/s |
| --- | --- |
| `current.rs` `current_task_id` | 68302 |
| `current.rs` `retain_current_user_process_binding` | 58504 |
| `current.rs` `current_user_abi` | 40245 |
| `current.rs` `current_linux_thread_state` | 26991 |

Sharding dispatch would not have moved any of these. Linux answers the same
questions from `current`, FreeBSD from `curthread`, and Zircon from
`Thread::Current::Get()` — a published per-thread cell, never the run-queue
lock — so the fix follows that design instead.

`kernel/ps/src/multitask/current_identity.rs` publishes a seqlock-protected
identity record per task slot, written only under the scheduler lock and read
with interrupts masked. A reader that catches a writer mid-update, or an
unpublished slot, returns `None` and falls back to the locked query.

A missed publication site would serve a *stale* record rather than an absent
one, which is a correctness fault, so completeness is not left to inspection:
`Scheduler::divergent_published_identity` re-derives every slot from the
authoritative tables under the lock each drain and reports the first
disagreement as `kernel-scheduler-identity-divergence`. It immediately found
one — `ROOT_TASK_SLOT` (slot 0, the BSP boot task, below
`FIRST_DYNAMIC_TASK_SLOT`) is installed outside the dynamic allocation paths
and had no publication call. Do not remove this audit; it is the standing proof
that the eleven publication sites are complete.

Syscall *return* was the remaining hot path: `deliver_pending_signals_if_needed`
took the lock on every exit only to read `pending_signals == 0`, which is
nearly always true. There is exactly one site that raises a pending signal
(`scheduler/linux_thread_state.rs`, the `|=` pair) and it runs under the
scheduler lock, as does the site that lowers the hint, so a conservative
per-slot flag is safe in the same way `TIF_SIGPENDING` is in Linux: it may read
`true` when nothing is pending, costing one locked recheck, and can never read
`false` while something is pending.

Measured at 8 vCPU, per one-second drain:

| | baseline | identity published | + signal hint |
| --- | --- | --- | --- |
| lock wait total | 3 650 000 us | 1 394 404 us | 585 880 us |
| lock hold max | 169 us | 89 us | 298 us |
| top caller | 94 065/s | 59 932/s | 14 736/s |
| identity divergences | n/a | 0 | 0 |

Lock wait fell 6.2x against the session baseline. The remaining top callers are
`multitask/irq.rs` (~14.7k/s), the syscall SIMD capture/restore pair in
`multitask/spawn.rs` (~13.1k/s each, both touching only the current task's own
slot and therefore the next candidates for the same treatment), and the
`current_linux_thread_state` fallback now that the hint gates it.

Note `lock hold max` rose to 298 us. That is a maximum, not a total, and the
total hold fell; it has not been attributed yet and should be before this is
called finished.

### The lock-hold maximum is spawn, not dispatch

The maximum is now attributed rather than left open. `record_runtime_profile_lock_hold`
carries the acquiring `Location` and the in-owner segment time that same owner
turn charged, and `kernel-scheduler-hold-max` reports all three: `arg0` packs
(hold max us, attributed us), `arg1` packs (FNV-1a32 of the caller file, line).
A maximum without the attributed share cannot distinguish a long critical
section from a vCPU the host descheduled mid-hold.

Every acquisition also charges an explicit `Prologue` segment covering owner
publication and the deferred wake drain, emitted as
`kernel-scheduler-phase-prologue` (arg0 = us, arg1 = wakes drained). Without it
that work read as unattributed time and would have looked like a stall.

Measured at 1 vCPU, per one-second window:

| window | hold max | attributed | site |
| --- | ---: | ---: | --- |
| 0 | 2447 us | 2 us | `spawn.rs:321` (`start`, the one-time scheduler reset) |
| 1 | 2435 us | 0 us | `spawn.rs:275` (`reserve_user_thread_slot`) |
| 2 | 162 us | 160 us | `irq.rs:712` (software-yield dispatch) |
| 3 | 155 us | 136 us | `irq.rs:266` (timer dispatch) |

So the maxima split cleanly. In steady state the worst turn is a dispatch and
about 95 percent of it is attributed in-owner work, which is the honest cost of
the critical section. During boot the worst turns are *spawn* paths — slot
allocation, stack zeroing, and address-space setup under the global lock — which
carry no phase instrumentation at all, which is why their attributed share reads
as zero. The 298 us in the table above was one of those, not a dispatch, and it
does not indicate a regression in the identity work.

`irq.rs:712` is `software_schedule_interrupt_dispatch`: it is the voluntary-yield
dispatch itself, so its acquisitions are the scheduler doing its job. It cannot
be removed the way the identity queries were; reducing it means reducing yields.

### Syscall SIMD custody was removed, not relocated

The per-slot entering-user SIMD image once lived in two `Scheduler` fields, so
both the syscall entry capture and the syscall return restore took the exclusive
global lock, about 13.1k each per second, each carrying a full `XSAVE`/`XRSTOR`
inside the critical section. It then moved to a per-slot module outside the lock.

Both steps priced the pair rather than asking whether it was needed. Measured at
829 ticks of every syscall, it is now gone entirely: the entry stub's sixteen
`movdqu` are the whole of the syscall path's FPU custody, and the state they do
not cover is held by an invariant the build checks
(`tools/xtask/src/build/nucleus_audit.rs`) rather than by a save.

### The 8 vCPU blocker is not the input-ring stall, and it is not new

Five 8-vCPU runs were taken this session, two of them on a tree stashed back to
`36ab344` so the baseline is measured rather than assumed. The build is unstable
at 8 vCPU **at HEAD**, and it fails three different ways:

| tree | outcome |
| --- | --- |
| HEAD `36ab344` | readiness timeout; missing `gpu-compositor active`, `wayclick: first frame presented`, `smp-cpu-first-user-dispatch arg0=0x6` |
| HEAD `36ab344` | panic `handoffs.rs:85` — `suspended task 33 has invalid context: saved rflags lost the reserved bit` |
| this session's tree | panic `handoffs.rs:85`, identical, task 36 |
| this session's tree | `loaderd: executable snapshot call failed errno=110`, 17 retries, rootd restart budget exhausted |
| this session's tree | readiness timeout; missing GPU/compositor markers, same snapshot timeouts |

Two conclusions follow, and neither was visible before the baseline was taken.

**The activation panic is pre-existing.** Identical site and message at HEAD.
`activate_suspended_user_tasks` preflights a just-spawned suspended service and
finds its saved frame zeroed: `saved_rsp` is inside the stack and the canary is
intact, so the stack is the right one, but the frame at the top of it was never
written or was cleared afterwards. `report_invalid_activation_context` now emits
the slot, `saved_rsp`, both stack bounds, the frame's `rflags`/`cs`/`rip`/`rsp`,
and whether the frame is entirely zero, so the next occurrence is attributable
without another run. It did not recur in the three runs after it was added.

**The dominant failure is `V5-VFSD-HOL-007` again, not the input ring.** The
common thread in the runs that get furthest is
`ipc slow call: endpoint=65539 wait_ms=2005` followed by
`loaderd: executable snapshot call failed errno=110`, repeating for `storaged`
until initd exhausts its retries. Meanwhile inputd is draining and publishing
normally in the same run (`records=2305 batch_seq=2290 stage=published`), so the
inputd drain/publish coupling the previous handoff named as the blocker does not
reproduce as the thing holding the boot back. vfsd emits nothing at all on the
snapshot path, which is why this keeps having to be inferred from the caller's
timeout. Instrument `open_executable_snapshot` with the plan/first-block/
last-block/seal/reply timestamps the audit's section 5.2 F prescribes before
changing anything there.

**The milestone ring drops records under 8 vCPU load.** 90 of 351 milestone
sequence numbers are missing from one 8-vCPU log, and the drops fall at the end
of the once-per-second drain, which is exactly where the per-caller acquisition
census is emitted. The census therefore reads as empty at 8 vCPU while it works
at 1 vCPU. Any 8-vCPU scheduler measurement has to fix this first or it is
reading a truncated record.

### Structural session: what closed, and three corrections

Audit items closed with a mutant that fails without them: `V5-DEADLINE-012`
(one `AbsoluteDeadline` in `rustos-user-abi`, carried across the loaderd→vfsd
wire), `V5-VFSD-HOL-007` structure (mount-generation revalidation at commit,
bounded two-worker pool), `V5-TLB-SCALE-009` structure (CR4.PCIDE asserted clear
per admitted CPU), `V5-MM-STACK-DEAD-021` (nothing owed), and the milestone
loss reporting `V5-UI-PIPELINE-011` depends on. Implementation mutations 108/108.

**The vfsd 2005 ms overrun is not in vfsd.** The phase probe added with the
deadline work reports plan/read/commit per snapshot:

```
netd.elf     683360B  plan=976us read=0us    commit=1953us  total=2929us
runtimed.elf 737424B  plan=976us read=1953us commit=1953us  total=4882us
```

Two to five milliseconds. Two sessions of inference that "the snapshot takes
seconds" is refuted by measurement. The caller's `wait_ms=2005` is queueing,
scheduling, or the reply path — not the snapshot work. Do not re-open that
inference without a probe.

**Three corrections, all the same mistake in different clothes.**

1. `V5-IPC-AUTH-015` and `V5-FORMAL-IPC-018` were declared open/reopened because
   `rg receiver_set_epoch` finds nothing. The property is implemented and
   stronger: the bind pins `receiver_open_description` and the claim requires
   the identical one. Refinement map now in the model header.
2. The scheduler shadow validator was built as a second per-slot owner word.
   `scheduler/runqueue.rs` already owns those words, and its header forbids
   exactly that shadow. Replaced with a comparison of the two authorities that
   exist.
3. `V5-SCHED-GLOBAL-001` was taken at its title. The per-CPU runqueue,
   owner-word state machine, remote wake mailboxes, and per-CPU selection all
   exist and are already lock-free; `publish_remote_wake` and
   `drain_remote_wakes` never take the global lock, and no path scans a global
   ready set. What the global lock still protects is the `Scheduler` struct's
   per-task arrays, so the item is a **data-structure split**, not a scheduler
   rewrite.

The rule that keeps failing to be applied: read the implementation, not the
item title and not an identifier lifted from the audit's proposed design.

### SCHED-GLOBAL-001 stage 1 is done, stage 2 is scoped

`run_authority::compare` sweeps every slot once per drain, under the lock, and
reports the first disagreement between `runqueue::owner(slot)` and the legacy
tables with a direction. The calling CPU's in-flight dispatch pair is excluded,
and that exclusion is load-bearing: the sweep runs from `take_runtime_profile`,
after the runqueue claims the incoming slot and before the guard publishes the
new current/transition pair, so both halves disagree by construction. The first
run reported exactly that, two per second, every second.

With the exclusion: **zero mismatches at 1 vCPU and zero at 8 vCPU**, no
identity divergence, no panic. That is the refinement evidence the cutover
needs.

Stage 2 is retiring `context.ready` as authority — it duplicates the owner word
and stage 1 proved they agree. It has about 25 non-test readers across seven
files, and the semantics differ per site: `ready` is false for the currently
running task while the owner word says `Running`, so a blanket substitution is
wrong. Sites inside `local_runnable_slots(cpu)` loops are redundant filters and
are safe; the rest need per-site review. This is deliberately not started
half-way.

### Five blockers fixed to root cause, and where the FPS gate now stands

8 vCPU reaches `RustOS missing=[]` and `Linux-DVM missing=[]`. It could not boot
at the start of this session. Five defects were found, each to a mechanism
rather than a symptom:

1. **Activation panic, intermittent across three sessions.**
   `reserve_user_thread_slot` guards its scan with `!thread_slot_reserved[slot]`;
   the four allocation scans in `scheduler.rs` tested only for an absent
   context, so a process spawn could take a slot a pending thread commit owned.
   Both wrote the same stack and activation found a frame with correct bounds,
   an intact canary, and all-zero contents. Fixed; three consecutive runs clean.
2. **WayClick died with `wl_display` `Invalid new_id`.** The frame-callback send
   gate also required a populated pixel cache, so a callback requested before
   the first buffer copy was held forever while the client reused the protocol
   id. Fixed by gating on visibility only.
3. **`ENOSPC` from a full IPC donation table** failed the whole call. Priority
   inheritance is an optimisation; it now degrades to `Ordinary` and reports
   `ipc-donation-capacity-degraded`.
4. **`ENOSPC` from a full netd replay queue made `close` fail**, which POSIX
   does not permit and `wayland-server` treats as fatal. The acknowledgement is
   dropped with a milestone instead; the retry-exhausted path returns the real
   transport error.
5. **Backpressure withheld the frame-callback permit.** `wayland_frame_permit`
   was regranted only by `Rendered`, so a backpressured display stopped frame
   callbacks and the client blocked in `blocking_dispatch` with no error.

Items 3 and 4 are the same defect shape the audit already names in
`V5-DEADLINE-012`: a transient capacity condition expressed as a terminal
errno, which the caller cannot distinguish from a permanent failure.

**Where the FPS gate stands.** WayClick's continuous frame loop and its
`wayclick profile:` lines — which the gate's predicate reads — are both gated on
`RUSTOS_WAYCLICK_PROFILE`. That variable *is* delivered: the client logs
`wayclick: acceptance profile enabled`. So the remaining failure is not
configuration. The loop still does not complete a one-second window on every
run: one run reached 1232 frame callbacks, others reach a handful, and the
client is still occasionally dropped. Attribute the next stall with the
`frame_seq` join `V5-UI-PIPELINE-011` calls for rather than by inspection —
every guess in this area so far has been wrong, and the two that were measured
were both answered in a single run.

### Gate status

- 1 vCPU: passes, with zero owner-word mismatches and zero identity divergence.
- 8 vCPU: reaches readiness and now stops only on `inputd: DVM keyboard ingress
  observed` / `DVM pointer ingress observed`. Zero mismatches, zero panics, no
  identity divergence. This is further than any previous run.
- The `handoffs.rs:85` activation panic reproduces at HEAD and did not appear in
  the last three runs. `report_invalid_activation_context` is now reliable
  output, so the next occurrence names the slot geometry and frame contents.
- 55 FPS WayClick gate: not met and not yet measurable — no 8-vCPU run reaches a
  presented frame.
- The milestone sink is still saturated under 8 vCPU load: `dropped=69` on the
  last run. Scheduler and activation records are in the reliable set; the rest
  are not.
- No hook or signing bypass has been used at any point.

## Resume sequence

1. Read the stable prefix: `AGENTS.md`, `docs/ai-map.md`, `token-policy.md`, and
   `task-router.md`.
2. Query the live goal state, then run `git status --short` and a focused
   `git diff --stat`. Treat both as inspection only; do not normalize the
   checkout.
   When the worktree is intentionally dirty, preserve it as an explicit
   boundary: never reset, checkout, clean, or otherwise discard its changes.
3. Route the new user request through `task-router.md`. Read this page again
   only for continuation or handoff work, not as a universal fifth prefix.
4. Use Serena or ripgrep for scoped discovery. If either MCP server is absent
   or fails, continue with local `rg`; MCP availability is not a product gate.
5. After edits, run `cargo xtask dev-plan` and execute only the relevant lanes.
   For AI-infrastructure changes, also run `.codex/hooks/selftest.sh` and
   `tools/check-dev-environment.sh --ai`.

## Refresh rule

Update this page only when preparing another handoff or when the live goal,
major blocker, hardware safety boundary, or validation ownership changes.
Keep durable architecture in the focused AI contracts and detailed pass/fail
evidence in its owning ledger; do not duplicate either here.

## Session: the transient-as-terminal family, and the callback throttle

Ten commits, `bef5f85..7aef2a2`. One change was reverted unshipped and is
recorded below because the reason it failed is the useful part.

### Scheduler critical section

The `SelectHandoff` chain was self-timed per member. Every step cost 0.95 to
2.5 us regardless of its work, and two controls explained it: an empty timed
span read 0.032 us, one acquisition of the per-CPU dispatch policy read
0.724 us. The chain took that lock about ten times per dispatch. It now takes it
once and threads the guard; `reserved_user_pick` reaching `user_reservation_due`
was the re-entrant path lockdep caught with `recursive lock-class acquisition
class=42`.

Separately, `take_overdue_system_or_pick_hint` re-ran `overdue_system_pick` with
the same arguments four arms after `mandatory_overdue_system_pick`, so it could
only return `None`. Deleted.

Chain cost per dispatch: **7.39 -> 2.50 -> 1.50 us**. Lock hold duty at 8 vCPU:
**60% -> 27%**. `SelectHandoff` is no longer the dominant segment (20%, against
Commit 17% and Balance 15%).

### Five defects of one shape

Each was a transient condition returned as a terminal error, each reproduced,
each verified gone:

1. `bind_reserved_ipc_priority` failure cancelled its own reply and returned
   `ENOSPC`, which surfaced as uiserver dying in a thread spawn with "failed to
   allocate an alternative stack: No space left on device".
2. `read_header` sampled the ring cursors producer-first, so a concurrent
   consumer advance looked like corruption and revoked the transport.
3. An all-zero header prefix is an unpublished aperture, not a corrupt one. The
   field-level rejection record (`arg1=0x5f`) is what separated them.
4. The inputd read took the bulk rail, 30,000 ms, against uiserver's own 3,000 ms
   input watchdog. Nonblocking reads now take the interactive rail.
5. Both halves of a nonblocking socket took the bulk rail. The send half was the
   one still blocking Wayland dispatch.

### The one that was reverted

Bounding the socket receive while letting `ETIMEDOUT` reach the caller. Two
windows, a 30,013 ms callback gap. The Wayland dispatch model treats `EAGAIN` as
the ordinary answer on a ready-but-empty socket and anything else as a broken
client, so the bound has to report `EAGAIN`. Re-landed with that mapping.

### Frame rate

`PresentUpdateResult::Idle` did not regrant the frame-callback permit, so a
client with nothing new to show blocked in `blocking_dispatch`, sent no protocol
input, and left `wayland_service_required` false - each side waiting for the
other. Regranting doubled the measured rate: **111.8-141.0 Hz -> 259.2-288.0 Hz**,
worst callback gap 39 ms, against a 55 Hz floor.

### Where the gate stands

Not met. Rates are far above target and window numbers are contiguous, so the
shortfall is run length, not lost evidence. Runs still end for varying reasons;
the last two were an input-ring credit timeout with the ring at its 1279-slot
limit after 1695 relayed events, and the plain 90-second deadline at nine
windows. The credit timeout is a consumer falling behind, not a frame-path
problem.

### Open

- `V5-GPU-UI-OWNER-014`, the general owner split. Every fix above is the narrow
  form of it; the design doc §4.2 has the shape.
- `V5-SCHED-GLOBAL-001` stage 4. Optional by §2.1c's own criterion now that hold
  duty is 27%, but the item is open.
- Why WayClick accumulates only single-digit consecutive windows in 85 seconds.
  `frame_seq` now joins both completions, which is the instrument for it.

### The failure the gate now ends on

With every per-frame diagnostic bounded, discarded bytes fell 10,821 -> 5,347 ->
3,097 and no log shape exceeds five occurrences in a run. The evidence channel
is no longer the constraint.

Two of the last three runs ended the same way:

    RustOS input relay failed after forwarding 1692 events:
    fixed input-ring credit timeout outstanding=1279 limit=1279 cleanup=false timeout_ms=50

The ring is full at its 1279-slot limit and the host's 50 ms credit window
expires. This is the consumer falling behind a burst, not a frame-path problem
and not an error the relay can retry.

The drain is `service_pending`, bounded to `MAX_RECORDS_PER_BROKER_TURN` = 256
records per broker call, and it only runs when inputd calls the broker. A burst
of 1692 needs at least seven consecutive turns. The thing to check first is
whether a turn that hits its 256-record bound tells inputd there is more waiting,
or whether inputd goes back to sleep and waits for the next MSI-X wake - the
latter would cap drain throughput at one turn per interrupt regardless of
backlog, which matches an outstanding count pinned exactly at the limit.

`OUTSTANDING_HIGH_WATER` and `DRAIN_CLAIM_LOST` already exist in `dvm_ring.rs`
for this question and are not yet reported anywhere.

### Where the remaining 60 seconds go

Boot is not the constraint. Measured timeline of the best run:

    uiserver main loop      t=3.2s
    wayclick spawn          t=3.6s
    gpu-compositor active   t=3.6s
    wayclick main enter     t=4.1s
    first wayclick window   t=4.9s
    last wayclick window    t=29.5s

Twenty-six windows spanning 4.9 to 29.5 seconds, roughly one per second, then
nothing for the remaining sixty seconds of a ninety-second run. The gate wants
sixty consecutive and the run offers about eighty-five, so the shortfall is not
launch latency and not window loss - the window numbers are contiguous.

The compositor is not the one stopping. After the peer-ready rebind log was
bounded, uiserver held 66.9 Hz across all fifty-seven of its own one-second
windows in the same configuration, with no collapse.

So the question is what stops WayClick at about thirty seconds while the
compositor keeps running. Two things end near there and are worth checking in
this order: the L0 input relay finishes its ~1700-event square and tears down,
and the `--exercise-input` pointer stream stops with it. WayClick's later windows
already show `redraw_requests=0 pointer_updates=0`, so it was self-driving by
then and should not care - but that is an assumption, not a measurement.

`frame_seq` now joins both completions, so the next step is to read whether
WayClick's commits keep arriving at the compositor past t=30s. If they do, the
loss is on the client's profile emission; if they stop, it is the client's
dispatch loop and the join will say which completion it last saw.

## The input-ring credit window is a consumer-turn problem, not a drain-rate one

The last run of this session ended on the failure the previous handoff already
named, but further along: `outstanding=1279 limit=1279` after **3673** forwarded
events, against 1692 before the backlog re-arm landed. The consumer now gets
more than twice as far and still pins at the ring size, so the re-arm was
necessary and is not sufficient.

The arithmetic is worth writing down because it says where to look.
`MAX_RECORDS_PER_BROKER_TURN` is 256 and inputd's `ingest_scratch` is
`INPUTD_INGEST_MAX_EVENTS`, also 256, so one broker call moves at most 256
records and the 1279-slot ring needs five of them. The consumer cursor - which
is the only acknowledgement L0 sees - is published to the shared aperture once
per broker call, immediately after the copy loop. That part is prompt.

What is not prompt is what inputd does *between* broker calls. Each turn runs
`dvm_session_sync::apply`, which takes the queue lock, decodes the batch, pushes
into the bounded queue, and may reach netd; then progress reporting; then
`thread::yield_now()` before retrying. No cursor advances during any of it, so
the credit window is governed by the consumer's whole turn latency rather than
by how fast it can copy. L0's window is 50 ms.

**Measured, and it overturns the arithmetic above.** Splitting the turn gave
`batch=1 drain_us=0 decode_us=0 sync_us=1953 turn_us=1953`, repeated at every
sampled point across a whole run. The consumer was taking **one record per
wake** and paying a full ~2 ms session-sync turn for it, so it ran at roughly
five hundred records per second no matter how far behind it was.
`MAX_RECORDS_PER_BROKER_TURN` never bound anything, because only one record was
ever available at the moment of the call - which also means
`ingest_batch_needs_immediate_retry`, testing for an exactly-full 256 batch,
could never fire. The consumer was strictly interrupt-paced.

The turn's fixed cost belongs to the batch it covers, not to each record in it.
**The obvious fix was tried and regressed; do not repeat it unchanged.** Looping
`drain_transport` into the space left in the scratch buffer until the transport
reports empty, with decode and session sync once for the whole batch, stopped
the consumer outright: the run failed at 1281 forwarded events against 3673
before it, the ring pinned at 1279 with about two records ever taken, and not a
single `DVM turn split` line was emitted - `report_progress` needs `drained !=
0`, so the drain was returning nothing at all.

The likely reason is the arm/recheck protocol rather than the batching idea.
`service_pending` clears `IRQ_PENDING` on entry and `arm_consumer_wake`
publishes a generation the producer samples; extra drain calls inside one turn
consume wake state the protocol expects one caller to consume once, and the
consumer then sleeps holding a full ring. Read `arm_consumer_wake` and the
`IRQ_PENDING` handshake before batching again - the amortisation is still the
right goal, but it has to be expressed on the kernel side of that handshake,
not by calling the broker in a loop from ring 3.

The two candidates below were written before that measurement. Candidate 1 was
right about where the time goes and wrong about why it mattered; candidate 2 was
right that the retry test never fires, for a different reason than expected.
Kept for the record:

1. Time one ingestion turn end to end and split it into copy, session sync,
   decode, and publish. If session sync dominates, the fix is to advance the
   cursor before it rather than after the turn.
2. Check whether the five broker calls needed to clear a full ring actually
   issue back to back. `ingest_batch_needs_immediate_retry` returns true only on
   an exactly-full batch, so a 255-record batch waits for the next wake even
   with a backlog behind it.

Do not raise `MAX_RECORDS_PER_BROKER_TURN` first. It is the constant that makes
the ring drainable in a bounded number of turns, and moving it hides whichever
of the two above is real.

### The 2 ms is a syscall count, and it links the two open regressions

`dvm_session_sync::apply` does nothing expensive for a single record with no
session action: clear a vector, skip two empty epoch slots, take one event, take
the queue lock, apply one wire. Two milliseconds does not live in any of that.

It lives in `InputQueue::push` and `InputQueue::pop_front`, which call
`advance_readiness_generation`, which calls `publish_readiness`, which is a
`SYS_RUSTOS_WAITSET_SIGNAL_BROKER` syscall issued **once per object id** - and
there are two, native and evdev. So each readiness transition is two syscalls.

Before `f69a80f` only the arrival edge published, so a run of arrivals cost two
syscalls total. `f69a80f` made both edges publish, and with the consumer taking
one record per wake and uiserver reading it straight back out, the queue
oscillates empty to non-empty and back on **every single record**: two
transitions, four syscalls, per record. That is the shape of the measured
`sync_us=1953`.

This connects the two things left open:

- The ingestion turn cost is not session sync and not the drain. It is
  readiness publication amplified by an oscillating queue.
- `f69a80f`'s own documented regression - uiserver's reader spinning on
  `input read slow elapsed_ms=102 events=0` with the published bit latched
  asserted - is the same change seen from the other side.

Two things to try, in this order, and measure each:

1. Publish both object ids in one syscall. The wire already carries
   `object_id`; a publication that means "inputd's queue" does not need to be
   sent twice. Halves the cost without changing any semantics.
2. Do not publish a deassert that no one can be waiting on. A transition to
   empty wakes nobody; its only purpose is keeping ring0's published bit
   current for the next `epoll` scan. If the scan is rare relative to the
   record rate - and at one record per wake it is - the deassert can be folded
   into the next assert, or the bit can be published without a wake.

Zircon's `ZX_CHANNEL_READABLE` deasserts for free because the kernel owns the
queue. Ours is in ring 3, so every edge costs a crossing, and the design has to
price that in rather than mirror the shape.

### `cfg(test)` was disabling privileged code for the wrong crate

Trying to pin the donation depth bound with a unit test produced a repeatable
`SIGSEGV` in `kernel-ps --lib`. It was not the harness and not the process
count: probes creating seven processes and seven scheduler contexts both pass.
The whole failure reduces to one line.

    crate::debug::record_milestone(LogCategory::Sched, "probe-alone", 1, 2);

`print_bytes_unlocked` guards its `rep outsb` with `#[cfg(not(test))]`, and
`cfg(test)` is true only while compiling the crate that owns the test. Every
*dependent's* test binary links `nucleus-core` with `cfg(test)` false, so a host
process executed a port-I/O instruction and died. `try_debug_output_lock`,
`DebugOutputGuard::drop`, and `println_emergency` had the same guard.

The consequence is larger than one test: **no `kernel-ps` test could touch any
path that records a milestone**, which is most of the donation, handoff, and
degrade logic - including every degrade path added this session. That is why the
scheduler's most interesting properties had no coverage.

The repo already has the right predicate. `cfg(rustos_boot_image)` is set by
`xtask`'s kernel build and asserted there, and `formal/`'s source-conformance
lane rejects `target_os = "none"` in its place. Three cases now: bare metal runs
the port I/O, this crate's own tests print to stderr, and a dependent's test
binary discards - correct, since `nucleus-core` is `no_std` and a host process
was never meant to drive a debug port.

Check the other `cfg(not(test))` sites in `kernel/hal` against the same
question before writing tests that reach them.

### Still on `cfg(test)`: the heap

`kernel/mm/src/memory/heap.rs` splits the entire allocator on `cfg(test)` -
`LockedHeap`, `HEAP_ORDER`, the size constants, and the `phys`/`kernel_vm`
imports are all `#[cfg(not(test))]`, with a separate slot-tracking allocator
under `#[cfg(test)]`. That means a dependent crate's test binary compiles the
*kernel* allocator, `#[global_allocator]` and all, into a host process.

It has not crashed, which is why it went unnoticed, but it is the same mistake
as the debug port and a much larger surface. It was left alone deliberately:
changing which allocator a test binary selects is not a change to make at the
end of a session. Do it on its own, with `kernel-ps` and `kernel-compat` test
suites as the check, and use `rustos_boot_image` as the predicate the way
`debug/mod.rs`, `input/wait_queue.rs`, and `memory/phys.rs` now do.

## The 2 ms was the diagnostic, not the syscalls, and the sink has a price

`sync_us=1953` was never session sync. The split took `decode_done_ns` *before*
the `stage=decoded` line and `sync_done_ns` *after* the `stage=published` line,
so both debugcon writes sat inside the window labelled session sync. Progress is
reported on one turn in 256, which made the sampled turn the only expensive one
in the run and charged its cost to the phase under investigation.

Moving the two timestamps inside the lines - changing nothing else, and touching
no readiness path - gives the turn as it always was:

    drain_us=21 decode_us=7 sync_us=14 turn_us=40 log_us=1215 log_bytes=82

**This retires "The 2 ms is a syscall count, and it links the two open
regressions" above.** `apply` is 14 to 380 us, and `publish_readiness` is inside
that figure, so the readiness publication cannot be two milliseconds and the two
items it proposed - one syscall for both object ids, and folding the deassert -
are not worth the 2 ms they were costed against. They may still be worth doing
on their own merits; they are not a fix for a cost that does not exist. The
regression `f69a80f` recorded from the reader side is a separate observation and
stays open on its own evidence.

### What a debugcon line costs

`log_us` self-times one `debug_line`. Two lengths, same run, 1 vCPU:

| line bytes | cost |
| ---: | ---: |
| 82 | 1246 us |
| 145 | 1946 us |

That is **~335 us fixed plus ~11.1 us per byte** - the fixed part is the syscall
and the serialisation lock, the slope is the port write per byte reaching a host
file. A diagnostic on a per-event path is therefore twenty-five times the event
it describes, and the rule "keep debugcon off hot paths" now has a number behind
it rather than a reputation.

Applied to the kernel's own output, measured over a 104 s 1 vCPU run: 41.0
lines/s and 10.3 KiB/s in total, which the model prices at **~131 ms/s, or 13
percent of one CPU**. The scheduler runtime profile is 21.6 lines/s at a mean of
326 bytes, so **~85 ms/s of that is the profiler**, and it is 67 percent of all
log bytes.

Two things follow, and neither is "delete the profiler":

1. This is a harness cost, not a product one. `rustos_debug_print_enabled` gates
   the whole sink and a product image has no debugcon. But it is 13 percent of
   one CPU *inside the runs that certify performance*, so every acceptance
   measurement is taken on a machine the instrument is loading.
2. The cheap reduction is the envelope, not the content. A milestone line
   carries `seq`, `ts_us`, `tick`, `cat`, `pid`, and `tid` in the outer log
   frame *and* again inside `milestone-begin ... milestone-end`, plus a constant
   `mod=nucleus_core::debug line=0`. That is roughly 95 of the 326 bytes, about
   28 percent, for no information. It was left alone deliberately: the inner
   frame is checksummed and parsed by `check-kvm-runtime-trace.py`, so changing
   it is an evidence-format change and belongs in its own session with those
   parsers as the check.

### The acquisition census only named half the contention

At 8 vCPU, 38,105 dispatches/s, median of 107 windows:

| | |
| --- | ---: |
| lock acquisitions | 144,602 /s |
| per dispatch | 3.79 |
| hold | 738.6 ms/s |
| wait | 2059.5 ms/s |
| attributed in-owner | 652.6 ms/s |

In-owner segments, ms/s: select 242.8 (of which handoff 165.1, vruntime 76.4,
pick 1.4), commit 136.7, balance 110.2, validate 73.1, arch-restore 56.4,
prologue 25.1, account 9.4. Inside select-handoff: pick-scan 57.0 over 54,559
calls, handoff-scan 35.5 over 67,044, step-overdue 36.3 over 33,526,
step-acquire 24.1 over 38,118, step-sync 16.0 over 38,120, and
**step-activation 0.0 over 0 calls** - `7118ea1` removed it entirely.

The four emitted census sites were `irq.rs:736` 24,546/s, `current.rs:416`
(`arm_block_current_task`) 16,435/s, `irq.rs:850`
(`commit_block_current_task`) 15,904/s, `irq.rs:676` 13,830/s. That is 70,715
of 144,602: **half the acquisitions had no caller attached**, because the census
tracks 64 sites and emitted 4. Widened to 8 behind a one-percent floor, so a
quiet window still pays for nothing and the next measurement can name the rest.

Do not start the `Scheduler` per-task array split until that report is read. The
two block sites above are already 22 percent of acquisitions and are structural
- the arm/recheck/commit protocol requires the recheck to happen outside the
lock - so whether they can move to a CPU-owned lock is a different question from
whether the struct needs splitting, and the unnamed half may contain something
cheaper than either.

### Correction to "The lock-hold maximum is spawn, not dispatch"

The heading overstates what that section's own body says. At 8 vCPU in steady
state the maximum is attributed to `irq.rs:736` in 67 windows and `irq.rs:676`
in 28, against `spawn.rs` in 2 - so it is dispatch, and the spawn maxima are the
boot-time observation the body already described. Median hold max is 97 us with
89 us attributed. The line numbers in that table have also moved: `irq.rs:712`
is now `irq.rs:736`.

### The UI input gate could not pass, for a formatting reason

`ui_input_ready` was false in every run at every vCPU count, and it was not the
input pipeline. The ring 3 debug syscall copies `USER_DEBUG_CHUNK_BYTES` (256)
per pass and gives each chunk its own kernel-owned `user-debug payload=`
envelope, so a service line longer than that reaches the log as several records
split wherever byte 256 landed - usually mid-token:

    user-debug payload=... cursor_mismatches=0 cur
    user-debug payload=sor=992,594 presented_cursor=992,594 background_thread_demotions=13 ...
    user-debug payload=_ms=0 part_ren_ms=0 part_prs_ms=1 mpix=2 spins=0\n

`uiserver profile:` is 562 bytes, so `cursor`, `presented_cursor`,
`cursor_moves`, and `background_thread_demotions` all landed on a record with no
prefix. `parse_ui_profile_input_window` uses `?` on every field, so it returned
`None` for **every** window and the gate could not pass whatever the guest did.
The line grew past 256 bytes at some point and took the gate with it silently;
the existing tests fed the predicate whole lines, so they never saw it.

Fixed on the reader side, in `rejoin_user_debug_records`, because the chunking
is a deliberate property of the transport - the envelope is what stops ring 3
forging a milestone frame - and every line-oriented consumer has to undo it, not
just this one. Records are joined until one ends with the escaped newline the
producer wrote, bounded at 16. With that, the same predicate at the same
thresholds reports **`input=true`** at 8 vCPU: five consecutive windows at
input 61.6/s against a floor of 55, cursor 60.6/s against 50, and a cursor span
of 192 against 96.

Note this also un-hides evidence for every other predicate that scans the log,
including the stall and crash markers. A gate that was green only because its
failure marker was split will now go red, and that is the correct reading.

### Open, and now the top defect: uiserver sends a malformed Wayland message

With input green, the 8 vCPU run ends on `wayclick=false` because the client
died five seconds in:

    Protocol error 0 on object @0: Malformed Wayland message.
    wayclick: dispatch failed: Backend(Protocol(ProtocolError { code: 0,
        object_id: 0, object_interface: "", message: "Malformed Wayland message." }))

It is intermittent - an earlier 8 vCPU run on the same build sustained 113
one-second wayclick windows - and it is not the compositor crashing: uiserver
keeps logging `wayland dispatched count=3` after the client is gone, and there
is no watchdog panic in that run. The compositor's last acts were ordinary
`wl_callback.done` sends at `frame_seq=115`.

What it is not: a concurrent-writer race. `flush_clients` takes `&mut self`, so
two threads cannot be inside the same `WaylandServer`.

What to check first, in order:

1. **A silent short write in the socket ABI.** `display.flush_clients()`
   returned `Ok` - there is not one `uiserver: wayland flush failed` in the run
   - so `wayland-server` believes it wrote every byte while the client's parser
   desynchronised. That is the signature of a write that reports success for
   fewer bytes than it transferred, or a returned count the library trusts.
   This is a userspace ABI question, not a compositor one: read the AF_UNIX
   `sendmsg`/`writev` return path and check the short-write and `EAGAIN` cases
   against what `wayland-backend` assumes.
2. Ancillary data. `object_id: 0` means the client faulted on the header before
   it could attribute the message, which fits a byte-offset slip more than a bad
   argument in one event.
3. Only then look at event construction.

Reproduce with `--rustos-vcpus 8 --min-ui-fps 30 --ui-proof-windows 5`; expect
to need several runs.

### Chasing the malformed message: what is eliminated, and the ABI defect it found

Three candidates ruled out by reading the code, so the next session does not
repeat them:

- **Concurrent writers.** `flush_clients` takes `&mut self`; two threads cannot
  be inside the same `WaylandServer`.
- **A stream-position desync on an error path.** `begin_stream_send(len)` only
  reserves `[start, start+len)` and sets `send_in_flight`; `send_position` moves
  only in `commit_send`, and `SocketStreamGuard::drop` clears the busy flag
  without touching the position. An error between reservation and commit is
  therefore safe - it loses a turn, not a byte.
- **netd's short-write accounting.** `send_socket_message` returns
  `room.min(bytes.len())`, and refuses a partial write outright when the message
  carries control data. Both are correct AF_UNIX stream semantics.

The search did find a real ABI defect on that path, now fixed. Marshalling a
`sendmsg` answered `EINVAL` whenever header + data + control exceeded
`NETD_IPC_PAYLOAD_CAPACITY`. Linux never answers a stream `sendmsg` that way: an
internal buffer bound is not the caller's error, and a stream writer is built to
retry a short write but not an argument fault. It now takes a prefix and reports
it - the reservation already permits committing fewer bytes than reserved - and
reserves `EMSGSIZE` for the case a prefix cannot express, which is a message
carrying descriptors, since those attach to the message and would be delivered
against the wrong bytes.

**This is not confirmed as the cause of the malformed message.** A Wayland
connection buffer flushes in 4 KB chunks against a 32 KB payload, so the bound
should not be reached on that socket; the defect was found while looking, and is
worth having on its own. The symptom remains intermittent - one 8 vCPU run on
the same build sustained 113 wayclick windows - so reproduce before concluding
anything. What is left to check, in order: the receive side's segment
reassembly, whether `recv_socket_bytes` can split a segment across a control
boundary, and only then event construction.

## IPC round trip: both targets met (1 vCPU p50, 8 vCPU floor)

`ipc_rt_intra_process_reply_recv` is the production-shaped fused probe, and the
one to quote. Anchor `vmexit_cpuid` stayed 3,640-3,720 across every figure
below, so these are comparable without renormalizing.

| topology | metric | before | now |
| --- | --- | ---: | ---: |
| 1 vCPU | p50 | 33,160 | **27,360-28,120** |
| 1 vCPU | min | 30,480 | 25,360 |
| 8 vCPU | min | 31,320 | 26,440-27,280 |
| 1 vCPU | cross-process `syscalld getuid` p50 | 39,840 | 35,800 |

Three changes account for it, in order of size:

1. **`is_process_exiting` no longer takes the global process-table lock.** It
   was the busiest acquisition site in the kernel -- a global lock plus a walk
   of all 32 slots to read one bool, several times per IPC syscall. A committed
   lifecycle publication already means "not exiting", so the live answer needs
   no lock. Only the live direction is served that way; see
   `docs/benchmarks/README.md` for why the asymmetry is what keeps the
   publication an accelerator rather than a second authority.
2. **`RDPID` replaces `RDTSCP`** for the logical-CPU token, admitted at boot
   only after it is observed returning the identical `IA32_TSC_AUX`. The
   `cpu-index-reader` boot milestone records which reader is serving.
3. **The tracked-lock wait clock starts at the first failed attempt**, so an
   uncontended acquisition issues no `RDTSC` at all.

### Do not re-derive these

- `timed_handoff_step` is **gone**, and with it the whole per-step handoff
  ledger. It was `rustos_scheduler_phase_profile`-gated, so it never cost
  production anything; the `selh` phase reading ~1,500 cycles was measurement
  inflation, not a bottleneck. The hit/miss split now carries its own attempt
  count (hits plus the three miss causes partition every attempt).
- The scheduler's own dispatch is the round trip. The sync pick hint already
  hits ~60% and skips *selection*; the remaining cost is accounting, the arch
  switch, and commit, which a real switch must do. There is no large structural
  skip left to find there.
- `scheduler.rs` large-file debt: 5,711 -> 5,653. The budget commit moved to
  `scheduling_context.rs`, which owns it, and the SMP required-sequence contract
  followed it there rather than pinning a call the file no longer contains.

### The 8-vCPU domain-budget panic is fixed; watch the milestone

`scheduler selected a task without eligible domain budget` was selection and the
budget commit disagreeing about a refill that was due at scan time and spent by
commit time. The commit now declines and the dispatch reselects. Watch
`scheduling-domain-budget-refused` (arg0=refusals this window,
arg1=`(slot<<32)|(domain<<8)|cause`): a low rate is the expected lost race, a
rising rate means the two predicates are drifting apart and is worth chasing.

One 8-vCPU bench failure was seen in 24 runs and did not reproduce in the 18
after the fix; its message was not captured. If it returns, capture
`build/kvm/rustos-debugcon.log` before rerunning -- the log is overwritten.

## The 8-vCPU fail-stops are fixed; two rules replaced them

Both panics were the same shape -- an admission predicate and the commit that
spends what it admitted, read at two different times, with the caller
fail-stopping on the disagreement:

| panic | admit | commit | now |
| --- | --- | --- | --- |
| `without eligible domain budget` | `scheduling_domain_is_eligible` | `prepare_scheduling_domain_dispatch` | commit declines, dispatch reselects |
| `fallback task lost local rq custody` | `is_current_cpu_dispatchable` | `claim_dispatch` | fallback takes this CPU's idle slot |

Twenty-five 8-vCPU runs after both: no panics, no missed boot deadlines. Before:
two panics in thirteen, then two missed deadlines in twenty-four.

`formal/smp-source-contracts.toml` registers the pairs and
`check-smp-source-assumptions.py` rejects any call site where a registered
commit sits inside `assert!`, `panic!`, or `expect`. Verified by reintroducing
the assert. **Registering a pair means its disposition is established** --
`publish_runqueue_wake`, `rollback_direct_handoff`, and
`materialize_direct_handoff` have the same shape and are deliberately
unregistered backlog.

Read `scheduler-fallback-idle` and `scheduling-domain-budget-refused` before
chasing any 8-vCPU stall. The refusal fires one or two times per window and is
routine; the idle landing has not fired in a clean run.

## What actually costs time here, and the four fixes that would stop it

The formal seal binds to the source tree and is a precondition for the bench, so
**verification and reproduction are mutually exclusive in time**: no tracked
file -- including a markdown file -- can change while an 8-vCPU repro loop runs.
Everything below follows from that.

1. ~~**`build/kvm/rustos-debugcon.log` is overwritten every run.**~~ **Done.**
   Every bench run now copies its log to `build/kvm/debugcon-history/`, bounded
   to the newest 48, and it archives *before* propagating a launch failure --
   which is the case that mattered, since the failing run is the one whose
   evidence used to be gone by the time anyone looked.
2. **Bind the bench's seal to the built image, or exempt non-code paths.** A
   markdown edit invalidating a boot image's provenance is pure friction and is
   what makes the two activities exclusive.
3. **`cargo xtask soak --rustos-vcpus 8 --runs N`**, keeping per-run logs and
   summarizing panics. This session hand-rolled that loop twice.
4. **Prefer structural contracts to literal source pins.** Four literal pins
   were retargeted this session (`sed` ranges in `run-source-conformance.sh`,
   `timed_handoff_step`, `.prepare_dispatch(now_ns),`, and a fresh
   `commit_token`); each cost a gate failure and a reseal, and none caught a
   defect. Every contract that earned its keep was structural: the required
   sequences, the acquisition ceilings, and the new admit/commit register.

The remaining cost is the problem, not the tooling: a defect that appears at 8
vCPU roughly one run in ten needs ~25 runs to confirm a fix. The four items
above do not remove that hour; they make it possible to do something else
during it.


## Verification lane costs, measured

The earlier claim in this document that a 25-run 8-vCPU loop costs ~50 minutes
was wrong; one bench run is 25 seconds, so the loop is 12-20 minutes. The rest
of the incremental cycle, warm:

| step | before | after |
| --- | ---: | ---: |
| `cargo xtask check` (one file) | 6s | 6s |
| `cargo xtask build` | 13s | 13s |
| `formal/verify-all.sh --profile pr` | 46s | **18s** |
| `formal/run-source-conformance.sh` | 21s | **6s** |
| `cargo test` (7 packages) | 5s | 5s |
| one `cargo xtask bench` | 25s | 25s |
| `run-implementation-mutations.sh` (incremental) | 1s | 1s |

Two changes account for it, both in `run-source-conformance.sh`, which was 76%
of `verify-all`. Its 14 per-package/feature `cargo test --exact` groups now run
concurrently and are verified afterwards in registry order, so a failure still
names the same group; and the per-witness `jq` -- 619 process spawns to build
619 one-line objects -- became one invocation over the `|`-separated rows that
were already validated on the way in. `summary.json` is unchanged, which is the
check that matters. `check-concurrency-triangle.py` lists its two independent
manifests concurrently, which only shows up on a cold build.

**Refuted, so it is not retried.** Raising `shard_count`'s cap in
`run-implementation-mutations.py` looked obvious on a 16-core machine with
330 GB free. Sixteen mutants measured 21s at four shards and 22s at eight. The
cap is not the bottleneck and was left alone.

Both remaining items are now done.

**The binding no longer covers prose no lane reads.**
`formal/binding-exempt-paths.txt` lists four documents excluded from the
verification-run source hash, and `formal/check-binding-exemptions.py` proves
nothing under `formal/` or `tools/` mentions an exempt path -- so a document
that ever becomes an input fails the gate until the exemption is withdrawn. The
list is tracked and therefore inside the hash it governs.
`docs/benchmarks/README.md` is deliberately *not* exempt: `formal/CONFORMANCE.md`
cites it as phase-closure evidence. Verified in both directions at 8 vCPU --
editing an exempt document leaves the seal valid and the bench runs; editing a
non-exempt one still refuses with a binding mismatch.

Doing that turned up four separate implementations of the same tree hash --
`write-verification-run.py`, `reuse-verification-run.py`,
`check-kvm-runtime-trace.py`, and `evidence.rs` -- which is four chances to
disagree, and a disagreement fails every binding check at once. Changing only
the Rust one is exactly what happened first, and every 8-vCPU bench refused
until the others matched. The three Python copies now share
`formal/source_binding.py`; the Rust one reads the same list.

**`cargo xtask soak --runs N --rustos-vcpus 8`** repeats the bench lane, keeps
going past a failure, and names every failed run with the guest's own panic
line taken from the per-run archive. It derives no measurement; the per-run
`bench` tables remain the only measurement surface.

### A flaky test that was not flaky

`work_budget::tests::a_scope_that_lost_the_cpu_or_the_task_declines_to_judge`
failed once in a loaded seven-package run and passed every isolated run. It and
`a_scope_that_exceeds_its_declared_ceiling_panics` both charged
`LockClass::ProcessState` on the same host CPU index, and `ACQUIRES` is one
process-global array against which `cargo test` runs both on parallel threads,
so each could observe the other's charges. The classes are now disjoint and the
module's test note states the rule. Ten consecutive seven-package runs pass.
