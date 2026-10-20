# Linked-code cost experiments and rejected candidates

[Contract index](../README.md).

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
a ranked per-class acquisition census whose rows are *counts*. The caller-site
table and its atomic increments compile only in that diagnostic profile; a
shipping scheduler acquisition does not perturb the cache lines it measures.
That matters more
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

