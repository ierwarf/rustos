# Performance evidence, limits and boot

[Contract index](../performance-hardening.md).

## Evidence Order

1. Run or reuse one bounded KVM-smoke debugcon capture. Prefer focused `rg`;
   never read full logs.
2. Rank by elapsed evidence: service startup, DVM transport readiness, UI boot stages,
   update ticks, input errors/slow reads, watchdog/stall lines.
3. Before patching, state whether the named owner already implements the needed
   property. An absent completion fence, direct scanout path, recovery state,
   or test oracle is an implementation gap, not a tuning opportunity; report
   it explicitly rather than fabricating a pass with fallback behavior.
4. Patch only the owner boundary that the log names. Do not harden unrelated
   helpers after the measured bottleneck is fixed.
5. Re-run `cargo xtask check`; run KVM smoke only after code and docs agree.

## Component diagnostics and linked-code attribution

`cargo xtask bench-components --rustos-vcpus <1|8>` runs a closed diagnostic
suite (60s bounded diagnostic timeout, configurable with `--timeout 1..120`;
ordinary bench/SMP acceptance deadlines are unchanged): CPUID and stamp
overhead, local getpid/gettid, clock output copy,
scheduler yield, empty endpoint receive, and both actual same-process IPC
server loops. Primitive probes use 10,000 samples and 2,000 warmups; CPUID/stamp
and both full 20,000-round-trip IPC results retain their published counts.
Identity, clock and empty-receive semantics are checked after the end timestamp.
Empty receive must return exactly EAGAIN and no reply capability: ENOSYS, EINVAL,
permission failure or success with no request cannot become cheap samples.

Each component has explicit app begin/end records, with profile drains outside
its timed operations. The collector preserves only closed, correctly ordered
windows. A later timeout may retain earlier components, but neither an open
window nor a partial suite is reported complete. Kernel phase/counter rows are
still system-wide observations, not a private attribution claim. CPU count is
the configured topology; unconstrained placement does not prove a remote-CPU
transfer or migration. Report min/p50/p99/raw max, not just a minimum or a sum
of component medians. Stamp cost is visible, never subtracted from the maximum.

The runner keeps the ordinary device/service/policy path and does not create a
new pre-UI launch exception. UI investigation is deferred: a later ordinary
readiness timeout is recorded separately from a complete diagnostic suite. If
that launch never starts ipcbench, there is no measurement. Crashes, stale logs,
changed source/ELFs, wrong counts, skips and malformed statistics still fail.
This lane never accepts the shipping IPC maximum or publishes product evidence.

Private unique archives bind the dirty source fingerprint, linked/staged ELF
hashes, profile flags and complete raw log. Bounded `nm`/`objdump` extraction
records the actual syscall entry, original claim/return/architecture owners and
app probes from those archived ELFs. Missing/inlined symbols and truncation are
explicit. Instruction/callsite counts are STATIC, not executed-path counts;
conditional/panic branches cannot be charged as hot work without measurement.
The ordinary shipping gate below is unchanged and remains the final check.

The normal scheduler renderer omits only phase rows whose two arguments are
both zero when explicit scheduler phase profiling is disabled. Actual nonzero
phase observations (including the default SIMD-save/prologue observations),
errors, budgets, task/census rows and all counters are unchanged. Enabling
scheduler phase profiling retains even explicit zero rows. This avoids
formatting/transporting uninformative rows; it does not remove timed samples,
filter outliers, suppress failures or turn absence into a zero measurement.

Methodology follows the [Google Benchmark user guide](https://google.github.io/benchmark/user_guide.html#runtime-and-reporting-considerations)
for explicit warmups, reported iteration counts, and repeated measurements.
System state can move during a serial suite: use the CPUID anchor and repeat the
same topology before interpreting a delta, then alternate control/candidate
runs for an optimization claim. Do not subtract isolated component medians to
claim a measured end-to-end saving. The production-helper native tests and
registered component mutations cover evidence rejection, not CPU execution.

## IPC worst-case evidence

`cargo xtask bench --rustos-vcpus <1|8> --isolate-probe
ipc_rt_intra_process_reply_recv --ipc-max-cycles 15000` is an opt-in shipping
acceptance gate, not a min-only comparison. Run both topologies independently;
a passing one-CPU run does not admit eight CPUs. It preserves the ordinary
interactive topology, 2,000 warmups, all 20,000 round trips, the 50,000-sample
CPUID anchor, raw outliers, and the 25s readiness/end gate. The fixed 16-byte
request is echoed without changing payload size: exact response length, operation
and sequence are checked after the end timestamp. All warmups, STOP echo and
server join must also succeed before any primary/derived result is emitted.
Zero/short/stale replies cannot become cheap samples; an invalid transcript
fails as a whole, not by filtering samples. Use a fresh post-echo control rather
than treating older status-only measurements as semantic acceptance. The
sample uses `LFENCE; RDTSC; LFENCE` with an asm memory clobber, bounding later
instructions and compiler memory motion as well as earlier loads; see the
[Intel-derived RDTSC instruction reference](https://www.felixcloutier.com/x86/rdtsc).
This does not claim global store-buffer persistence or remove KVM host delays.
Inspect the linked shipping ELF's actual stamp instructions for timing evidence.
Exactly 15,000 ticks is allowed; 15,001 fails even if p99 is lower. Diagnostic
profiles are refused.
Repeat shipping measurements with the same workload/topology and at most 3%
CPUID-anchor drift before claiming an improvement. The anchor diagnoses host
movement; it never rescales or subtracts a failed raw maximum into a pass.

A successful reverse IPC reply now requests the same FIFO-qualified fast
attempt as the actual call path. It does not manufacture Direct custody: the
original typed wake, FIFO generation/order, atomic first-turn priority, chain
limit, frame/funding claim and architecture checks still decide execution.
Remote/early-Current wakes can fall back normally. The native full-return
witness derives its hint from this same outcome rather than manually supplying
`Some(caller)`, which had hidden the missing production route.

The scheduler diagnostic profile now times the actual original IPC publication,
outgoing return capture, frame capture, funding and final claim boundaries.
`kernel-dispatch-phase-{ipc,claim,commit}-us` carry paired system-wide wall
microseconds (call/reply, capture/funding, claim/return-capture), not private
per-round-trip samples. Nested segments must not be summed as disjoint time.
They compile to no instrumentation in the shipping configuration.

Housekeeping and anonymous-frame refill are ordinary kernel workers, not CPU
idle owners. A zero-work turn parks its own continuation through the existing
RTC arm/register/recheck/commit and exact task wake; a productive turn yields.
They must not yield and then execute HLT when selected again: eligible user
work may have appeared meanwhile, and the halted interval would be billed to
the worker's System context. RTC admission refusal falls back to scheduler
yielding, not a CPU halt. The native action witness and source-route gate cover
this choice; actual kernel wait/ACK and KVM evidence remain separate obligations.
See the [Intel-derived HLT reference](https://www.felixcloutier.com/x86/hlt): HLT
stops the logical processor until an interrupt, not just the calling thread.

External methodology used for this lane:
[KVM timekeeping](https://www.kernel.org/doc/html/latest/virt/kvm/x86/timekeeping.html)
explains invariant TSC and virtualization caveats;
[KVM steal time](https://www.kernel.org/doc/html/latest/virt/kvm/x86/msr.html)
identifies vCPU time not executed (not permission to discard outliers);
[seL4 benchmarking](https://docs.sel4.systems/projects/sel4-tutorials/benchmarking-guide.html)
distinguishes instrument overhead from workload cost. Linux's
[lockdep design](https://www.kernel.org/doc/html/latest/locking/lockdep-design.html)
requires IRQ-safe and IRQ-unsafe classifications to be exclusive. RustOS now
publishes a new classification before rechecking the opposite set in the same
SeqCst order as new edges, closing the two-CPU stale-precheck race; native
production-helper and fault-injection witnesses retain this obligation.

## Hard Limits

The source of truth is `rustos_user_abi::performance`. These limits are
acceptance contracts, not tuning defaults:

| Path | Target | Hard limit | Synchronous policy IPC |
|---|---:|---:|---:|
| Kernel entry to interactive UI | 3 s | 10 s | One classified turn at a time |
| UI CPU frame preparation | 8 ms | 16.667 ms completed frame | 0 |
| First local DVM GPU activation after CPU boot frame | 750 ms | Included in 10 s boot ceiling | 0 |
| Input arrival to visible cursor | One frame | 50 ms | 0 in frame/present |
| Deferred VFS maintenance per foreground turn | 1 ms | 1 ms | 1 replay |
| Deferred netd reference maintenance per turn | 1 ms | 1 ms | 1 replay or ACK |
| Readiness observation | 1 frame | 16 ms | 1 per deduplicated provider |
| Interactive policy-only control | 16 ms | 100 ms | 1; service drains at most 32 already-queued controls per turn |
| Boot/control transaction | 100 ms | 5 s | 1 |
| Bulk external-device data | 5 s | 30 s | 1 |

The first GPU completion is governed by the five-second activation deadline,
not the 50 ms steady-state recovery bound. Once the epoch is active, every
later completion again uses the 50 ms hard limit; the 16.667 ms target remains
an independent performance failure rather than provider-revocation timing.

Every kernel-owned service call names one class in source. A shorter caller
deadline is allowed; widening the class is not. Repeated endpoint lookup with
an exact current-epoch grant performs zero rootd IPC. UI render/present performs
zero filesystem, catalog, or policy-service calls. During SMP qualification,
`cargo xtask kvm-smoke` records boot timestamps but admits readiness until its
outer timeout instead of terminating at the ten-second product target.
`formal/run-source-conformance.sh` checks the class ordering and reply
cancellation witnesses.

Provider lifecycle limits apply to the complete turn, not to each retry.
Netd dup/close may retry only an early transport break inside one remaining
100 ms interactive-control budget and moves the committed operation's ACK to
the housekeeping maintenance queue. VFS and netd
maintenance each process at most one 100 ms-bounded control item per yielded
housekeeping turn, IPC
transfer disposal releases at most one entry, and retirement acknowledges at
most four exact task records. Backlog ownership remains explicit, but cleanup
cannot monopolize the scheduler by multiplying individually bounded calls.
The transfer-drop queue reserves its full admitted capacity up front and
allocates drain output before taking the queue spin lock; exit-time descriptor
disposal never invokes the allocator from that critical section.

Scheduler-aware wait locks spin only for the short optimistic window, then use
the scheduler's arm-publish-recheck-commit state transition. Raw spin locks are
reserved for non-sleeping leaves. In particular, MMIO cache-mode transactions
use one wait lock and perform allocation/page-table work with interrupts
enabled; large BAR setup cannot become whole-transaction IRQ-off latency.

The kernel IPC runtime has no global object-table critical section. Endpoint,
message, reply, and shared-region registries use fixed-capacity generational
slabs with one tracked spin lock per cache-line-aligned slot. Queue capacity
and message storage are fallibly allocated before publication, failed
transactions destroy that storage only after releasing their slot locks,
owner retirement walks one slot at a time, and descriptor batches move
directly into their cleanup owner. Endpoint
traffic on unrelated objects therefore neither waits behind a BTreeMap
allocator nor extends one whole-registry IRQ-off interval.
Final shared-region release only transfers ownership to a fixed reclaim queue.
Housekeeping frees at most 64 backing pages per turn, preventing a large
surface close or exec from monopolizing a process-state lock or allocator.
Process-broker prepare/ticket/transition/activation registries and remote-VFS
descriptor references are fixed-capacity hash tables. Prepare mapping pointer
capacity and each mapping payload are allocated before registry acquisition.
Deferred VFS/netd retry queues use fixed rings with one private retry slot, so
the advertised capacity remains exact even when a producer races a popped
item's requeue. No first-use `BTreeMap` or `VecDeque` growth occurs in those
tracked critical sections.

Per-CPU lock diagnostics are indexed only by the dense admitted logical CPU
identity; raw APIC IDs never index their storage. A configured vCPU count is
not multicore evidence by itself: every admitted CPU must publish its online,
clockevent, idle, user-dispatch, and reschedule-IPI witnesses, and the 1/2/4/8
runtime matrix remains the release authority.

Slow-IPC diagnostics are an observation rail, not a second workload. Each of
the generic and typed-service paths emits at most four records per second after
its early sample set. A sustained input or storage load therefore remains
diagnosable across the run without paying for hundreds of emergency debugcon
writes during the same boot second.
Rejected one-shot IPC replies follow the same rule at both sides of the
boundary. Ring0 preserves four exact reply-capability samples, then emits at
most one cumulative rejection summary per second. Each ring3 service reply
lane preserves its first four failures and only power-of-two cumulative totals
afterward. A cancelled caller therefore remains diagnosable without allowing
the synchronous debug port and its VM exits to amplify the timeout storm.
The private UI profile attributes GPU completion separately and reports wall
time not covered by named phases. A slow total with many independently inflated
small phases points to preemption or diagnostic contention; it must not be
misreported as GPU execution time merely because the completion query begins
the owner turn.
The scheduler profile accumulates measured task runtime, dispatch counts,
same-task decisions, real task switches, address-space switches, cross-CPU
migrations, timer/RTC/reschedule-IPI/software entry causes, and aggregate/max
global-lock wait and hold time in fixed arrays and counters. The first eligible
CPU destructively snapshots only the top four tasks once per second. IRQ code
publishes it into one release/acquire pending slot after the global scheduler
owner is released; housekeeping owns the fixed header, transition, lock,
packed entry-cause, and four task milestone records. The BSP is not assumed to
retain steady-state user work. Locality history is keyed by the exact task id
across windows and is observation only; slot reuse is reset and no scheduler
policy may consume profiler state.
This distinguishes a permanently runnable Ring3 worker from Ring0 dispatch
churn without adding logging or allocation to the scheduling critical section.
Ordinary same-class fair selection may retain the exact task's last CPU only
within one scheduler minimum-granularity unit of the global least vruntime.
This cache-locality bound is source-tested, model-checked, and mutation-tested;
it cannot override exact IPC/activation handoffs, strict-class recovery,
affinity, or remote running ownership. Runtime evidence must still show that
migrations fall without unfair ready-age growth before this becomes accepted
as a useful optimization.
The serialized SMP scheduler returns an exact source/destination-slot token.
When both slots are equal, the IRQ leaf retains the already-active
CR3/TSS/syscall-stack/segment/FS/GS state. A real slot change restores the
task-specific stack and segment state, but reloads CR3 only when the incoming
address-space root differs from the outgoing root. Address-space activation
also preserves the TLB for an identical release-published root, while AP Online
admission and generation-bound shootdowns keep their mandatory flushes. Do not skip SIMD restore merely
because the task slot stayed equal: ring0 compiler code executes after the
save boundary and may use vector registers.
When software scheduler entries dominate timer, RTC, and reschedule-IPI
entries, inspect synchronous service loops before changing timer or IPI policy.
A separate byte-only `reply` followed by `recv_with_sender` costs an avoidable
ring3 boundary and scheduler transition per request. The admitted fused
`reply_recv_with_sender` path preflights both phases before commit, preserves
the exact caller handoff, and then blocks or dequeues under the existing
endpoint check-arm-recheck protocol. Measure its effect first on one service;
do not mass-migrate multi-source supervisors or handle-bearing endpoints.
Inputd uses the fully fused single-endpoint loop. Loaderd uses the same path
only for byte-only requests with no reply-dependent descriptor cleanup or
bootstrap demotion; spawn replies retain the split boundary so those actions
complete before loaderd can block again. A zero-byte dequeued loader call is a
malformed live request and receives terminal `EINVAL`, never an idle hint.
Each uiserver helper emits one post-demotion `(thread name, kernel-stamped
TID)` record. Scheduler top-task samples can therefore identify a hot helper
without adding periodic tracing to the presentation loop or trusting the name
as authority.

The kernel service registry publishes an endpoint last and clears it first.
The steady-state lookup path takes an epoch/endpoint snapshot, rechecks both
after reading the owner, and acquires no global mutation lock. Three unstable
reads fail as transient service absence; publication, revoke, and restart stay
serialized on the writer side. This keeps the global authority transition
explicit without turning every VFS, network, or input IPC into a shared
cache-line write. Public service-handle calls use the same stable publication
snapshot and an exact `(caller PID, service epoch)` last-grant cache. A cache
miss rechecks the bounded grant table; a hit never takes the registry or grant
lock, and service restart invalidates it by advancing the epoch.

## Driver Boot

- Linux DVM owns device drivers. RustOS validates only the fixed DVM transport.
- Missing or invalid DVM input, display, or network transport must leave that
  device unavailable; do not install a native, firmware, or direct-virtio
  fallback.
- PCI transport topology is fixed before services start. Once enumeration has
  found ivshmem functions but no exact block-aperture shape, the kernel caches
  that topology absence instead of rescanning every PCI function on every
  storaged readiness probe. A correctly shaped aperture whose signed header is
  not ready remains retryable. Storaged records the first readiness errno and
  later errno transitions, not every identical 50 ms retry.

- Storaged starts a first or random cache miss with one 64-KiB DVM block
  ticket. A later miss must equal the exact preceding window end before it can
  expand into at most eight contiguous tickets. It publishes that complete
  bounded batch before waiting, so the 32-entry transport queue overlaps
  device latency for actual sequential reads while random FAT metadata does
  not amplify into 512-KiB traffic. Partial submission is cancelled; a
  completion failure cancels the remainder and clears cached generation
  authority. This is service-owned read-ahead, not a larger kernel transfer
  slot or a bootstrap-file bypass.

