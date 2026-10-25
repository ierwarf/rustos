# Scheduler, IPC and identity performance

[Contract index](../performance-hardening.md).

## Scheduler Dispatch

- KVM supports source-admitted RustOS topologies from one through eight vCPUs.
  `tools/xtask/src/kvm/guest.rs::RUSTOS_SMP_READINESS` is the executable
  enablement gate: more than one RustOS vCPU requires per-CPU scheduler and
  syscall state, a CPU-online state machine, reschedule IPI, TLB shootdown, and
  atomic robust-futex cleanup. Independent per-CPU owners are implemented and
  the global scheduler catalog is deleted; full-matrix hardware/performance
  and commercial acceptance remain separate. The Linux DVM's topology is
  independent; choose both topologies with host scheduling capacity in mind.

- The scheduler keeps a fixed 128-slot task table, but ordinary fair dispatch is
  selected from the current CPU's published runnable bitmap. The local picker
  records System/User global and same-CPU candidates in one pass, validates the
  complete publication epoch, and becomes authoritative only for that coherent
  fair decision. Atomic activation and exact IPC FIFO handoffs retain their
  independent custody owners. Incomplete evidence permits bounded fresh
  retries, then independently qualified current/fixed-idle custody, never a
  catalog peer. Historical legacy-picker divergence is diagnostic telemetry,
  not current authority or a release failure.
- At most two consecutive System dispatches may run while User work is ready;
  the next ordinary dispatch is reserved for the lowest-vruntime User task.
  Every ready System task has a 2 ms recovery rail, while User work has no
  unadmitted wall-clock deadline: under overload such a promise is impossible
  and made ready age override weight on nearly every turn. User progress comes
  from the bounded System and handoff bursts plus weight-normalized vruntime.
  Exact synchronous IPC may hand directly to its receiver/caller for at most
  eight turns; an overdue System continuation then runs before fresh spawn,
  latency, or generic IPC handoffs. Queued hints remain owned for a later turn.
  This follows Linux CFS/EEVDF's virtual-runtime/lag accounting rather than
  presenting a hard deadline without the bandwidth admission required by
  `SCHED_DEADLINE`.
- An authenticated netd local-socket completion may enqueue only a User task in
  the deduplicated 16-entry latency FIFO. At most eight such handoffs run
  consecutively, stale tasks are discarded, and a full queue drops the new
  hint rather than overwriting an older owner. The hint improves wake latency;
  it does not move AF_UNIX ownership or protocol policy into ring0.
- `rootd`'s immutable bootstrap manifest explicitly admits only the fixed
  syscall/VFS/loader/process/pager brokers to the System latency class.
  Package and desktop metadata cannot request that bit. The scheduler's 2 ms
  ready-wait rail therefore covers causal core servers without making dynamic
  applications strict-priority work.

- **Historical catalog-measurement rule (the catalog is now deleted): size a
  global-lock change against acquisitions per scheduler entry, never
  against acquisitions per second or against hold time.** Acquisitions per
  second move with how fast the probe itself runs, and hold time under KVM
  includes host descheduling of the owning vCPU, which is why one window can
  report 55% hold duty at a single vCPU with nothing contending.
  `kernel-scheduler-phase-select` `arg1` carries the window's guard
  acquisitions and `kernel-scheduler-entry` carries its entry causes; the ratio
  is stable across boots and is what makes a per-caller cut legible. The
  measured record is in `docs/benchmarks/README.md`.
- Removing an acquisition is a structural claim and is reported as one. The
  latency that follows it is a separate claim with its own anchored control,
  and at eight vCPUs the probe minimum has a 10% control spread of its own, so
  a minimum delta smaller than that says nothing there.
- A per-caller census that walks a shared table from slot zero costs the
  acquisition it is measuring: about ten shared cache lines per acquisition on
  every CPU. Hash the caller into its bucket. An instrument that is a
  measurable share of what it instruments is not free just because it is
  always on.

- **Never render a debug-sink record inside a tracked lock.** A milestone is a
  port write per byte, which is a VM exit per byte under KVM, and its emitter
  drains whatever deferred records are parked before it renders. The scheduler
  emitted one budget-exhaustion marker from inside the global guard about sixty
  times a second; it cost 5.9-27 microseconds *per dispatch* and 59% of the
  guard's hold total. Latch the event and let the profile drain, which runs
  outside every tracked lock, render it. The ceiling is
  `SCHEDULER_GUARD_MAX_DEBUG_SINK_RECORDS` and it is zero.
- **Count lock classes before choosing one to split.** `work_budget::take_class_census()`
  and the `kernel-lock-class-0..5` milestones report acquisitions per class per
  window. The class the lane had been splitting was fourth; the global process
  table was first, at about ten acquisitions per synchronous round trip.
- **Render and populate the complete ranked lock census only in a lock-profile
  build.** Ordinary builds retain exact work budgets, but increment only the
  explicitly registered class while a live scope can observe its counter;
  const-generic locks from every other class compile out the census path.
  Host tests keep free-running counts. Destructive census draining, per-site
  class rotation, and debugcon output require
  `RUSTOS_LOCK_PHASE_PROFILE=true`. At eight vCPUs unconditional rendering
  stretched a 15-second guest settle beyond a 90-second host timeout, so it is
  itself a p99 contaminant rather than free observability.
- **A running thread pins its own process object**, so the hot path takes no
  reference count: `reclaim_slot` refuses while `thread_count != 0`. An
  uncounted pin must re-read the published state pointer rather than cache it,
  because an exec replaces the object and no count holds the old one.
- **A rendezvous fastpath hit is not automatically a speedup.** Moving one
  probe from 21% to 100% fastpath raised its minimum and lowered its p50: the
  fastpath removes variance, not the critical path. Do not present a hit-rate
  change as a latency result.
- Performance invariants belong in `libs/rustos-user-abi/src/performance.rs` as
  named ceilings, in `formal/check-performance-contracts.sh` as source
  witnesses, and in `formal/implementation-mutations.tsv` with a host test that
  *counts* something. Acquisitions are charged on the host path for exactly
  that reason: a path that quietly reopens a global lock still produces correct
  bytes, so nothing but a count objects to it.

## Synchronous IPC and the Syscall Entry Path

The evidence for this lane is `cargo xtask bench`, not a debugcon capture, and
its findings live in `docs/benchmarks/README.md`. Read that before proposing a
change here; multiple plausible plans have been refuted by its numbers.

**Five rules, each of which was learned by getting it wrong:**

1. **The anchor is `vmexit_cpuid`.** It contains no RustOS code. A comparison
   whose anchor moved more than ~3% is not a measurement. `null_syscall_getpid`
   is *not* a control — it moved 7.8% on a lockdep-only change.
2. **The probe floor is ±2%**, and ~15% on `sched_yield`. Below that, judge by an
   `ipc-call-phase-*` / `usermem-phase-*` counter or not at all.
3. **A committed baseline is a record, not a control.** Unmodified HEAD once
   measured +5.4% against its own baseline file with the anchor held. Every
   comparison needs a same-session control run.
4. **Ablation and a shipped gate are different measurements.** Stubbing the
   syscall phase profile read −2.4%/−1.7% on the round-trip probes; the gate read
   +0.7%. The gate is the honest one.
5. **The phase counters have two denominators.** `copy-request`,
   `write-response`, `enqueue`, and `enqueue-deadline` are charged once per
   *syscall-path* call; `enqueue-runtime`, `enqueue-wake`, and every `wait-*`
   once per *endpoint* call, ~2.4x more often. Mixing them inflates every ratio
   by that factor.

**The historical fixed-address baseline used static relocation.** Its
`-no-pie -static` image executed at its 2 MiB link address. Configuring
`relocation_model = "none"` left library crates at the GNU target's PIC default,
while the nucleus binary used `"static"`. GOT double-loads under 617 statics
cost 25-41% across those probes; that result is not a current build requirement.
[Kernel KASLR](../kernel-kaslr.md) now requires PIC/PIE libraries and a static-PIE
image with a randomized virtual alias. The default, debug, and release presets
use `"pic"`; performance comparisons need a new same-shape baseline. The AP
trampoline remains hand-relocated assembly outside rustc's relocation model.

**Where the cost is**, per endpoint call, measured:

| phase | ticks/op | per call | ticks/call |
|---|---:|---:|---:|
| `wait-take` | 2,350 | 2.97 | 6,980 |
| `enqueue-wake` | 5,048 | 1.00 | 5,048 |
| `enqueue-runtime` | 4,051 | 1.00 | 4,051 |
| `wait-arm` | 2,897 | 1.00 | 2,897 |

**Stage 0 closed the caller-only blind spot in the pre-relocation image; that
historical round trip was 61-67% attributed, not 81% unmeasured.**
`cargo xtask bench --isolate-probe <name>`
(Stage 0a) makes four syscall-path phases (`copy-request`, `enqueue`,
`write-response`, `enqueue-deadline`) divide exactly into one round trip
(ratio 1.00). `kernel/compat/src/user/syscall/linux/ipc_server_profile.rs`
(Stage 0b) adds four receiver-side phases (`recv-take`, `recv-write`,
`reply-publish`, `reply-wake`). A later review corrected the original
four-site-only ablation: it did not include the caller's twelve TSC charges or
the fast-handoff's shared counters. All IPC attribution now sits behind
`[ipc_telemetry] phase_profile`, off in shipping builds. Stage 1 decoded one
`kernel-scheduler-phase-*` window
to size the dispatch chain and self-corrected a double-count: the 20.5% of
scheduler lock-hold time not covered by the seven named phases is not new
dark cost, it is six `current.rs` functions each called *from inside* a
phase already charged elsewhere. Net accounting: 4 clean caller phases
(14,224 ticks) + 4 Stage 0b receiver phases (~14,414, approximate) +
dispatch chain (~19,200-24,050, historical estimate, not independently
re-verified) ≈ 48,000-53,000 of 78,080 ticks. Those absolute values predate the
static-relocation fix and must not be used to explain the current 17,480-cycle
production-shaped local floor. What is still dark is the two
blocked transitions' architectural mechanics and the syscall entry/exit
floor (~4,920 ticks for three syscalls) — not a new target, the same one
this lane already had. Full detail, including the receiver-phase and
dispatch-chain writeups: `docs/benchmarks/README.md`, "Instrumenting the
receiver side" and "Sizing the dark ticks with what Stage 0 built".

**Do not propose another acquisition fusion.** Three attempts reached the floor:
the reply-wait poll budget is a *net loss* (an arm costs 2,897, a take 2,350, and
`commit_block_current_task` consumes `wake_armed` in both branches so every turn
must re-arm); the enqueue chain's last unconditional acquisition moved
`enqueue-wake` by 2 ticks; a take's third acquisition is worth ~1,560 against a
TOCTOU guard with formal models attached. 2% of 73,760 is ~1,500 ticks, which is
the size of everything that remains individually.

**Reuse the authorization snapshot for reply-deadline classification.** The
shipping disassembly showed that `try_fast_ipc_call_bytes` called
`service_reply_deadline_tick_for_endpoint`, a 510-byte helper that read the
service publication table again, called `is_process_exiting`, and sampled the
clock. This happened after `authorize_current_process_ipc_call` had already
scanned and validated the exact publication epoch. Authorization now returns
the matched service identity, and both fast and slow call paths derive their
deadline from that captured identity. A private owner-local endpoint carries
`None`; the direct internal syscalld route supplies its known service identity.
The source-conformance gate forbids restoring endpoint-to-service lookup in the
deadline helper.

On 2026-09-08, a same-session one-vCPU control and two candidate runs held the
`vmexit_cpuid` minimum at 3,600 cycles. Raw cross-process syscalld `getuid`
minimum/p50 moved from 26,400/67,920 to 23,560/60,520 and
23,720/60,720 cycles. The derived IPC portion moved from 25,960/67,480 to
23,120/60,040 and 23,280/60,280 cycles: 10.3-10.9% at the minimum and
10.7-11.0% at p50. Local and fused-local minima changed by only 1-2%, within
the declared noise floor, so no local-path claim is attached to this change.
After rebuilding, the old 510-byte lookup helper disappeared from the image;
the deadline selection was folded into the caller.

**Prefer the caller CPU for an admitted synchronous receiver.** On SMP, the
fast-call handoff previously used the blocked receiver's prior CPU
unconditionally. A caller on another CPU therefore paid a directed remote wake
and usually another directed wake on reply even though the caller blocks in the
same transaction. The scheduler now tries the caller CPU first, using the full
dispatch-admission predicate (affinity, scheduling-context and domain budget,
idle ownership, lifecycle, and foreign execution ownership). A rejection uses
the prior CPU and the unchanged remote path. This follows Linux's principle
that wake CPU selection should match final local dispatch when possible and
seL4 MCS's scheduler-bypass motivation for synchronous context donation, while
retaining RustOS's exact owner-word and bounded-handoff custody.

In 8-vCPU full-workload runs, controls placed cross-process syscalld `getuid`
at p50 91,280 and 109,080 cycles with p99 15,508,480 and 15,694,160. Two
wake-affine runs measured p50 45,160 and 73,040 cycles, a 20.0-58.6% reduction;
p99 was 11,620,760 and 15,520,760, a 25.1% and 1.1% reduction. The second
tail change is below the measurement threshold and does not support a p99
claim.
An isolated control/candidate pair moved IPC p50 from 109,360 to 44,040 cycles
(59.7%), while p99 remained 12,789,360 versus 12,738,560 (0.4%). Its
`vmexit_cpuid` p50 moved 3.2%, just outside the strict ~3% anchor gate, so this
pair is corroboration rather than acceptance evidence. The supported claim
comes from the two full-workload comparisons and is limited to p50. The p99
floor remains dominated by rare blocked-transition and global
scheduler-catalog stalls and needs a payload-ownership split rather than a
placement heuristic.

The published reply `message_id` hint is not a fusion and does not remove that
TOCTOU guard. It replaces only the preliminary `IpcReply` lookup needed to
choose message-then-reply lock order; the nested reply access still validates
the full generation, exact message id, and consumed state. A host counter pins
pending detailed-response polls at one reply acquisition instead of two. The
shipping timing remained below the two-percent floor, so the count is the
claim.

**Admit saved-frame backing and reader custody in one resource leaf.**
`frame_readers::pin_and_acquire` creates an owned `PinnedFrameReader` rather
than acquiring Resource53 separately for an allocation pin and a borrowed
reader. Selection, final frame capture, exact IPC receiver publication and
copied frame diagnostics use this admission. UID/geometry, closing/MM state,
reservation, writer, CPU bank and execution-owner checks remain authoritative;
publication epoch and final owner/RSP checks still surround byte access. The
same registration helper serves borrowed readers. Refusal and unwind release
custody, and `into_pin` keeps the original backing for final claim, never
re-resolves it. No resource leaf spans byte access, allocation or hardware work.

The counted native control takes two leaves; owned admission plus release or
pin transfer takes exactly one (`FRAME_READ_MAX_RESOURCE_ACQUISITIONS`). This
is a per-admission acquisition claim, not a 50% IPC latency claim, and does not
remove the reply/message TOCTOU guard discussed above. No extra shipping
telemetry is enabled. External design references:
[seL4 IPC fastpath](https://docs.sel4.systems/Tutorials/ipc#fastpath) motivates a
strictly admitted fast path rather than skipping safety checks;
[Linux refcount ordering](https://docs.kernel.org/core-api/refcount-vs-atomic.html)
explains the Release/Acquire distinction retained by reader release and
zero-reader claim admission. Neither reference proves RustOS's protocol; the
owner contract, original-resource race tests and mutation witnesses do.

The later Phase-3 ownership cut is not another fusion. Wait reason kind, arm,
block intent, and runnable intent now share `RunOwnerWord`; the ordinary commit
is a single CAS and a wake either clears the arm first (commit refuses sleep)
or restores runnable intent after the commit. The catalog remains only as the
invalid-owner fallback. A one-vCPU instrumented census measured catalog
acquisitions per scheduler entry at **1.99**, down from 2.59 before this cut.
The remaining normal-path entries are dispatch, reply-wake handoff, pick hints,
and retirement cleanup, so the Phase-3 zero-acquisition gate remains open.

**Five telemetry profiles are build switches, all off by default**:
`[lock_telemetry]`, `[scheduler_telemetry]`, `[syscall_telemetry]`, and
`[ipc_telemetry]`, plus `[usermem_telemetry]`
`phase_profile` in `config/rustos.toml`. Each cost more than the work it
measured. Turn one on for a diagnosis run and read the result as the cost of an
*instrumented* operation.

**FPU custody is an invariant, not a save.** Both kernel entry paths preserve
`xmm0`-`xmm15` and nothing else. x87, MXCSR, and the `ymm` upper halves are held
by `tools/xtask/src/build/nucleus_audit.rs`, which audits the linked image on
every build and fails it on any x87 instruction, any floating-point arithmetic,
or wide SIMD outside `kernel_hal::arch::simd::wide_simd_section`. The one
exception is the 32-bit `_start` transition stub: it runs before any user
register set exists, and x86-64 `objdump` misdecodes its far jump as x87 bytes.
Adding floating-point work after `rustos_multiboot_long_mode` is therefore a
build error, by design.

## Executable Snapshot Path

- Vfsd materializes one exact admitted executable file into a private memfd,
  capped at 128 MiB, and applies terminal write/grow/shrink/seal seals before
  transferring it to loaderd. The cache is mount-generation bound and bounded
  independently; loaderd never publishes a live VFS handle to ring0.
- Cache-hot FAT traversal after a DVM read-ahead completion yields after at
  most 64 KiB of aggregate bulk transfer. One executable snapshot must not
  retain a System-class direct-handoff chain long enough to miss an
  interactive frame merely because its individual block requests are small.
- Rootd's immutable manifest grants loaderd and vfsd System admission only for
  the boot phase. After the authenticated uiserver snapshot/spawn completes,
  both services irreversibly self-demote to User; runtimed does the same after
  its uiserver bootstrap transaction. A later System caller can still donate
  its class for one exact reply capability, while ordinary catalog launches
  and DVM bulk reads can no longer inherit a permanent boot-time priority.
- Loaderd parses and maps the transferred immutable snapshot. The commit broker
  allocates page-table backing and copies from that memfd only; it performs no
  vfsd or DVM storage call. Validation therefore cannot race a later path
  mutation, repeated segments do not repeat storage IPC, and executable commit
  latency is independent of service reply floods.

## Cleanup Rule

- Delete a marked path only after its broker callers and owning service prove
  that it is replaced. A marker or LOC total alone does not prove that ring0
  substrate is obsolete.

## Exact process identity hot path

- A running thread already pins its process object. Exact process/MM generation
  validation therefore reads the per-slot lifecycle publication and has a hard
  ceiling of zero `ProcessTable` acquisitions on the committed live path.
- Publication is fail-closed: the writer first revokes the identity word,
  updates PID and state-pointer payload, then release-publishes the exact
  process/MM generation. A reader observes the identity on both sides of the
  payload and rejects a changed or zero word.
- Revoked, incomplete, or damaged publication uses the locked lifecycle table
  as the correctness fallback. The out-of-scheduler-guard divergence sweep is
  the evidence that this fallback is exceptional rather than silently normal.
