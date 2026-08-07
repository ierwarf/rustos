# Session Handoff

**Role:** volatile routing note for resuming this checkout. It is not build,
runtime, formal, or hardware evidence. Source, the live goal tracker, and fresh
command output win when they disagree with this page.

## Current checkout snapshot

Recorded on 2026-08-04 during the active SMP performance investigation.

- The worktree is intentionally dirty. Preserve all tracked and untracked work.
  Never use `reset`, `clean`, or a broad `restore`.
- The tree did not compile when this session started: a previous session had
  moved `Scheduler::current_task` behind `#[cfg(test)]` without updating
  `scheduler/affinity.rs`, `scheduler/linux_thread_state.rs`, and
  `scheduler/smp.rs`. That migration is now finished; `cargo xtask check`,
  `cargo xtask build`, and `formal/verify-all.sh --profile pr` all pass.

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

### Syscall SIMD custody moved out of the scheduler lock

`kernel/ps/src/multitask/syscall_simd.rs` now owns the per-slot entering-user
SIMD image. It previously lived in two `Scheduler` fields, so both the syscall
entry capture and the syscall return restore took the exclusive global lock,
about 13.1k each per second, and each one carried a full `XSAVE`/`XRSTOR` inside
the critical section.

The buffer for a slot is reachable only by the CPU executing that slot's task at
a syscall boundary with interrupts masked, and by `reset` when the scheduler
binds or rebinds the slot under its lock. A slot cannot be current on two CPUs —
`SchedulerAccessGuard::drop` fails the kernel if it ever is — so the exclusion
does not need the lock. `SyscallUserSimdSnapshot` carries both the slot and the
exact task bound to it, because the syscall body may block and resume on another
CPU, and an exec rebind between entry and return must still be refused.

1 vCPU passes end to end with this in place, which exercises the path on every
syscall.

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
