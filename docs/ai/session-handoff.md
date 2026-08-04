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

### Gate status

- 1 vCPU: passes, boot terminal 2824 ms.
- 2, 4, 8 vCPU: reach readiness, fail on the input-ring drain stall.
- 55 FPS WayClick gate: not met, not yet measurable past the stall.
- Nothing is committed. No hook or signing bypass has been used.

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
