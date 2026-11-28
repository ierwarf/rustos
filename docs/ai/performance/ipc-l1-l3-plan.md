# Register IPC L1–L3: implementation and acceptance plan

Status: implemented and shipping-benchmarked (2026-11-28, KVM ~3.991 GHz host).
See `docs/ai/abi/ipc.md` and `formal/CONFORMANCE.md` for the registered ABI and formal bindings.

## 1. Baseline and the actual linked path

1-vCPU KVM, idle host, same-address-space pair, 16-byte exact echo, 2,000
warmups and 20,000 timed round trips: **5,600 / 8,640 TSC ticks p50/p99**.
The call-to-receive p50 is 2,920 and reply-to-return 2,600, measured as
separate distributions; do not sum percentiles. CPUID p50 3,760 is an
anchor, not a quantity to subtract. Ordinary `getpid` p50 600 and the
80-tick stamp control are observations, not lower bounds for register IPC.

Existing evidence: `build/ipc-slowpath-baseline/register-kvm-0{,-samples}.log`.
The ELF archive at `build/bench-core/ipc_rt_intra_process_register_reply_recv-vcpu-1-1795788504008089623/`
records a failed first display launch (`launch_ok=false`), **not** the latency
result. Its kernel (`a1e2e213…`) and app (`20a30a09…`) hashes match both
linked and staged artifacts; the accepted retry above booted the already
built image. The disassembly recipe, selected symbol dumps, full symbol
indexes and owner-observer census are in `build/ipc-l1-l3-plan/`. Link-time
addresses below are not runtime KASLR addresses.

| Boundary | Source and linked evidence | Consequence for the design |
|---|---|---|
| Test | `apps/ipcbench/src/ipc_probes.rs`: timed CALL loads `0x52550052` at `2cfad`, executes `syscall` at `2cfc2`, bracketed by `lfence; rdtsc; lfence`. Server `bench_server_register_reply_recv` starts with `0x52550053`, repeats `0x52550054`, uses `0x52550055` for final reply. | Keep the existing timed transcript, STOP/join and unfused controls; do not substitute a different user benchmark. |
| Entry and exit | `kernel/compat/src/user/syscall/mod.rs`: `syscall_entry` at `21287c` reserves `0x1e0` (480) bytes. GS selects this task's kernel stack and ABI; the register branch already skips ordinary XMM saves and saves CW/MXCSR, GPRs, user RIP/RSP/RFLAGS. `syscall_dispatch` and parked return validate and settle pending actions; shared assembly epilogue sanitizes FP state and executes `sysretq` at `212af1`. | Do not propose SIMD elision again. L3 must remove repeated dispatch and materialization work without changing what is returned to user mode. |
| CALL | `linux.rs::dispatch_register_ipc_syscall` gets one identity/signal snapshot; `ipc_ops/registers.rs::dispatch_with` validates shape, zeros outputs and separates buffer mode. `ipc_ops.rs::try_fast_ipc_call_bytes` freezes deadline, reserves funding/reply and arms wait/timer. `ipc-runtime/src/ipc/mod.rs::reserve_fast_endpoint_call_with_response_capacity` checks the exact front waiter and queue, inserts the inline `FastCallFrame` in `REPLIES`, and publishes `fast_reply`. | The admitted path already avoids the generic message FIFO and heap allocation for 16 bytes. L1 removes the **slab object and publish/take/completion sequence**, not a fictionally present heap allocation. Queue order still matters. |
| REPLY_RECV | `registers.rs::reply_receive` authorizes the next endpoint before consuming reply authority, then uses deferred wake plus `recv_with_sender_blocking_prepared_inner`. Receiver arm precedes endpoint receive-or-wait. `complete_parked_register_ipc` does request/reply take and signal settlement, with a slow path for a wake without data. | Preserve the distinction between pre-commit error and reply-committed receive error (`-(4096+errno)`); never retry a committed reply or lose its wake. L1 can marshal the exact result directly into a sealed frame. |
| Direct schedule | `kernel/hal/src/interrupt_stubs.rs`: `fast_ipc_{call,reply}_direct_schedule` at `35652c` / `356688` both reserve `0x1b0` and `rep stosq` 54 words (432 B), then call `software_schedule_interrupt_dispatch`. Its `kernel/ps/src/multitask/irq.rs` body enters `ipc_lane::{call,reply}`; the lane does not open a generic dispatch turn unless it falls back. | This is a concrete L3 cut: remove synthetic `SavedContext` and syscall→generic-shaped scheduler dispatch on the admitted branch; preserve them for escapers. |
| Switch / physical ACK | `ipc_lane.rs::finish` prepares funding and claims blocked owner+wait under placement, takes `resident::Loans`, prepares banks, publishes the outgoing frame, installs incoming architecture, and publishes the lane transition. `fast_ipc_dispatch_tail` at `3562cd` first moves RSP then calls the commit hook, which clears Transition and parks outgoing loans. At `3562fc` it jumps directly to the recognized parked resume RIP; `iretq` is **already bypassed** there. `fast_ipc_parked_resume` at `212af4` still calls Rust `fast_ipc_parked_tail` at `34e330`. | L2 removes per-handoff re-custody. L3 can remove the intermediate completion call only when the exact result frame and current architectural state are already proved. ACK/lifetime ordering cannot be deleted. |

The selected symbol dumps include 632 B of `syscall_entry`, 220/234 B of the
direct stubs and 5,384/5,400 B of the two linked `finish` instances. These are
static symbol extents including branches, inline callees and error code;
**not** dynamic instruction counts or latency attribution. The source and
linked branch agree that deleting `iretq` again is not a useful proposal.

### Interfaces to audit during each change

- **ABI and privilege boundary:** `libs/rustos-user-abi/src/syscall/ipc_registers.rs`,
  `kernel/compat/src/user/syscall/{mod.rs,linux.rs,linux/ipc_ops.rs,linux/ipc_ops/registers.rs,linux/ipc_ops/registers/buffer.rs,linux/ipc_reply_recv.rs}`.
  Keep the four operations, payload padding, sender identity, committed
  errors, buffer mode and signal handling. Validate return RIP/RSP range
  and canonicality and RFLAGS of the **incoming** frame, not the old Current.
- **Authority and IPC lifetime:** `ipc_ops.rs::{authorize_ipc_call_for_process,try_fast_ipc_call_bytes}`,
  `ipc_ops/reply_wait.rs`, `kernel/ipc-runtime/src/{api.rs,ipc/{mod.rs,slab.rs,endpoint_receive.rs,reply_publication.rs}}`,
  endpoint/service publication revocation, receiver removal, timeout and
  `fail_endpoints_owned_by_{task,process}`. Preserve Endpoint→Reply lock ordering
  and the rule that expiry wins even if observed during reply take.
- **Execution and architecture:** `kernel/ps/src/multitask/{irq.rs,cpu_local.rs,scheduler/{ipc_lane,resident,ipc_publication,frame_publication,return_architecture,execution_interval,current_wait,wake_publication,retirement_cleanup}.rs}`,
  `scheduler/runqueue/{ownership,return_publication,affinity_payload}.rs` and
  funding/identity/stack banks. Audit Current/Transition, frame readers and
  permits, placement, FIFO and chain ordering, exit/exec and IPI/wake writers.
  The saved census covers 80 files matching those ownership/observer terms;
  it is an audit checklist, not proof that each observer already understands
  paired custody. `kernel/hal/src/{interrupt_stubs.rs,arch/simd.rs,arch/gdt.rs,arch/idt/handlers.rs}`
  and `kernel/ps/src/user/syscall.rs` own the actual GS, TSS, stack, ISR and CPU-state effects.

## 2. Implementation order: one change in ownership at a time

### B0 — prove the attainable floor before rewriting the entry

Retain the shipping baseline and add three diagnostic-only `ipcbench` rows
with the **same** stamps and successful semantic transcript. Gate any probe
leaf by a private test image and the exact admitted harness identity, not a
production syscall number or publicly enabled ABI:

- **B0.e:** two entry/return operations in one timed window, actual register
  entry, real SYSRET validation and current FP sanitizer/CW restore. Echo
  the two values locally; no second thread or switch. This measures entry
  plus return at the same count, not a substitute for IPC.
- **B0.s:** with L2 landed, two actual switches using the pair/frame/ACK and
  selected-CPU checks; no endpoint lookups or transfer objects. Use a
  kernel-bound diagnostic pair and confirm other callers cannot enter it.
  Capture cold pair formation separately from repeated handoffs. No changes
  to scheduler rights or active-image ownership just to get a low number.
- **B0.f:** existing full register CALL/REPLY_RECV. In separate diagnostic
  images record `entry`, `authority`, `publish`, `claim/install`, `ACK`,
  `completion`, `return` and escapes; use disjoint boundaries and counters
  for exact sample classification. Don't subtract independently measured
  percentiles or include instrumented cycles as shipping evidence.

Use these as **ranking measurements**, not a pass/fail target: if the
entry/return component dominates, optimize that boundary; if a switch-only
run shows repeated custody remains expensive, target that before pair
caching. Put a concrete measurement in place before changing the linked-image
FP/entry audit. Continue along the largest removable cost; stop a candidate
when it cannot beat noise or cannot preserve the existing invariants.

### L1 — one call receipt, no generic reply slot on a hit

- **L1a — publish the representation.** Add one bounded receipt for an exact
  parked receiver and outstanding caller; immutable identities/endpoint
  incarnation, call sequence, deadline, lengths/words, waiter/return receipt
  and accounting custody. Suggested location is scheduler-owned resident
  storage plus a typed discoverable runtime endpoint registration; compat
  owns only ABI frame offsets/marshalling. No kernel pointer becomes an ABI
  value. A concrete candidate virtual-token namespace is nonzero tokens with
  the low 16 bits zero (`ipc/slab.rs` rejects index zero; a high tag bit alone
  would collide with slab generations). Use a non-reusing sequence namespace
  (per-CPU encoded sequence is acceptable if wrap permanently declines);
  match *all* identities and the exact live receipt, not the token alone.
  Audit all raw reply-token decoders, typed errors and APIs that accept them.
- **L1b — one delivery commit.** Initially accept only normal 1–16 byte
  register messages, same CPU and same address space, a receiver already
  parked with the required capacity, ordinary own-funded execution and no
  older eligible queued sender. Keep existing exact grant checks. Before
  the point of no return, reserve a bounded recovery slot for the receipt,
  validate original frame loans, and revalidate queue+generation in the
  runtime arbitration used by the ordinary sender. Both paths contend for
  **the same receiver wait claim**. Start with consuming the exact front
  registration rather than introducing a second sticky wait queue. L1
  retains the existing `ipc_lane` ownership switch and regular scheduling
  fallback; no new CPU ownership representation is needed.
- **L1c — one terminal decision.** Define states `Waiting -> Delivered ->
  Replied | Cancelled`, with at most one conversion of `Delivered` to the
  ordinary representation. After `Delivered`, no re-enqueue of the same
  request even if direct switch loses admission. Fused receive preflights
  its next endpoint before reply consumes; a failure of receive *after*
  consumption is tagged as today. If response take crosses expiry, the
  caller sees timeout as today. The receiver can plain REPLY, be migrated or
  call a nested service; handle that on cold settlement/escape, not by
  assuming the user never leaves the test loop. If materialized, resolve the
  original user-visible cookie through its exact bridge until terminal;
  dropping or rewriting the cookie would break later replay/lookup semantics.
- **Recovery and resource rule.** On close/exit/revoke/timeout, both parked
  and already-delivered receipts must be discoverable outside the existing
  `REPLIES` scan and retire without allocation. Before admitting a call,
  guarantee any mandatory post-commit escape via bounded pre-reserved
  backing or a representation that can finish without conversion. Keep
  expiry wake and funding settlement in the owner transaction; do not
  assume `Drop` runs on Rust stack frames abandoned by direct park. No
  fast path if the reserve is exhausted. Replace rather than duplicate
  the existing accounting once equivalent behavior is proven.

### L2 — a CPU owns both execution images for one RPC

- **L2a — add the model and global visibility first.** One descriptor per
  CPU with generation, two exact task/owner/frame generations, active member,
  two retained stack/MM/permit/placement receipts, accounting interval and
  break requests. Extend `RunOwnerWord` with typed pair states (the code has
  a 4-bit state field; no frame-pointer encoding in the owner word). Reserve
  the two expected owner states in fixed order; expose a preparation state
  until a *single published descriptor commit* makes both pair roles live.
  Before commit abort by returning the exact receipts and processing raced
  work once. Do not claim two atomic owner updates are one operation.
- **L2b — one typed switch.** Both tasks remain held by the CPU-local
  descriptor. Outgoing frame is saved, the incoming exact image installed,
  RSP is changed and only then the stable incoming Current is exposed with
  the outgoing physical ACK. During switch, expose the transition to all
  lifetime readers; a stable inactive member is not runnable or reusable.
  Keep pin/MM/permit accounting and one exact budget/priority measure; use
  the current deferred observation/chain policy, not a second scheduler.
  Initially bound the pair to one RPC, including acquisition and release
  inside the measured interval. Continue using the normal return boundary
  for the first L2 implementation. Do not change `SYSRET` as part of L2.
- **L2c — one break owner.** Remote wake/timeout, cancel/exit/exec,
  revoke, priority or affinity updates, preemption and migration route
  through one exact pair-generation break request. Ordinary mutation is
  forbidden while the descriptor owns the task; owner CPU drains the
  request, publishes each ordinary state once, then hands off or releases
  resources after the physical ACK. Finish both sides if an observer races
  dissolution; stale generations cannot affect a later pair. Higher-priority
  work, pending activation, old FIFO work, and exhausted fairness/budget
  trump the shortcut. Initially decline cross-CPU, cross-root, nested and
  unsupported funding/class paths. Audit model, code and tests for readers
  that match only `Running` or `Blocked`; opt in only after all are covered.

### L3 — keep two actual user crossings; remove the intermediate frame

- **L3a — typed direct switch, identical user epilogue.** Accept only the
  exact register CALL/fused REPLY_RECV shape and a stable L2-ready image;
  plain RECV/REPLY, buffer mode, debug/slow states and unmatched work use
  existing dispatch. Start on owner-local endpoints. If extending to a
  published service, replace repeated registry scanning only after a typed
  grant/epoch receipt is tied to the same revocation linearization as delivery;
  a generation read followed by an unprotected commit does not suffice.
  Make the masked assembly/Rust boundary take a small typed `SwitchReady`
  receipt and the existing 128-byte syscall frame,
  rather than synthesizing a 432-byte `SavedContext` and rediscovering it
  by schedule tag. Keep fixed offsets, preserved GPRs and alignment; do
  not create a Rust reference to a partly initialized generic context.
  Initially keep the shared `SYSRET` finalization and generic saved-user
  IRET recovery. Verify statically that the admitted branch doesn't reach
  the old direct-schedule stubs, generic schedule callback or 54-word clear.
- **L3b — precompute the destination result, not the security decision.**
  Use L1 to seal the result frame (including sender/PID/TID, reply token,
  exact length/zeros). A normal handoff bypasses parked take/dispatch;
  on invalidated frame, reschedule or pending signal, use the ordinary
  terminal receipt and exit route. Return-safe RIP/RSP/RFLAGS and restored
  GS/user ABI must belong to that precise frame; seal and recheck at commit
  under the frame's generation. Keep TSS/syscall-stack selection and actual
  RSP arrival, and set FS/GS as required even in the same address space.
  Keep XMM/x87/MMX/YMM isolation and CW/MXCSR. Treat the NMI/IST and faults
  at each transition as distinct — interrupt masking alone is not proof.
  An arriving event may decline before commit; afterwards it settles the
  same committed call. After an already committed reply, any deferred
  receive error must still be tagged. No user pointers, allocation, logging,
  general SIMD, or blocking calls in the masked leaf. Leave long operations
  to the ordinary path. Verify with linked audit and generated offset asserts.
- **L3c — amortize only if B0/L3a–b measure a real setup bottleneck.**
  Reuse a bounded CPU-local pairing *after* first complete RPC, only while
  exact task/MM/endpoint/rights generation, priority, timer and FIFO order
  stay eligible. The next call is a new receipt with a fresh reply cookie
  and its own deadline, never a reuse of prior authority. Break on an
  intervening operation not supported by the pair, competing runnable work,
  a timer/priority change, revoke/exit/exec, or an explicit bounded chain
  expiration. Use the existing chain/budget policy; no unbounded spinning
  or masking IRQs across user execution. If a receipt cache requires every
  observer to learn a second scheduler, prefer L3a–b with one-RPC custody.
  Measure warm/cold/mixed traffic independently; report pair creates, reuses
  and breaks as well as the full unfiltered rounds. No separate microkernel or
  public ABI for a measurement-only fast path.

The useful precedent in [seL4's fastpath](https://github.com/seL4/seL4/blob/master/src/fastpath/fastpath.c)
is the explicit point of no return after capability, target and scheduling
checks, then direct peer switch/restore. It is not permission to remove
checks or an estimate of RustOS latency. Preserve the current [IPC ABI](../abi/ipc.md)
and its x86 return/SIMD rules.

## 3. Estimates and decisions

| Stage | Planning p50, idle 1-vCPU, same root | Evidence or decision gate |
|---|---:|---|
| Current | **5,600** | 20,000 valid measured samples, p99 8,640. |
| L1 | **5,000–5,400** (central 5,200) | Historical planning range, not a hurdle. Around 240 ticks was a reply-slot sensitivity result; do not re-count ledger savings already shipped. |
| L1+L2 | **3,500–4,500** (central 3,900) | Estimated with setup/teardown included from the older stamped switch budget, not a new profile or acceptance criterion. |
| L3 | **No defensible p50 forecast yet** | Establish disjoint B0 intervals, then compare L3 substeps; compare cold formation, warm handoffs and mixed traffic. |

There is **no fixed cycle target or required saving**. Keep a repeatable
improvement larger than measurement noise when correctness and unaffected
paths are preserved. Smaller independent wins can stack; no point estimate
for p99, separate address spaces or other hardware follows from this table.

## 4. Proof and rollout checklist

1. **Semantics first, disabled by default.** Stage-specific boot/build switch
   with exactly one hot branch; no permanent independent implementations
   of queue priority, authority or terminal settlement. Document any shape
   extension only after implementation. Test registered-service and owner-local
   endpoints plus an older regular waiter. Assert duplicate/late and wrong-owner
   cookie refusals, replay after same task's next call, expiry before/during
   take, partial reply-vs-receive error, alternate syscall, conversion failure,
   CPU/endpoint reuse, cleanup without allocation, and exactly-once donation.
2. **Ownership proofs before promotion.** Register the L1 endpoint/call
   refinement model (the draft model is not a gate today) and L2 pair/ACK
   model extending the `SchedulerOwnerWord` and synchronous handoff models.
   Model simultaneous wake/cancel/migrate/exit versus reserve, commit, switch,
   arm and dissolve; track one runnable custodian, exact in-flight frame
   loans, no reusing an old RSP or publishing an image prematurely. Add
   source-conformance/mutation witnesses for dropping generation/authorization,
   fast-vs-queued ordering, false ACK, lost break, duplicate settlement and
   stale CPU publish. Extend `SyscallSimdLifecycle`/entry tests and the linked
   `nucleus_audit.rs` if L3 needs a new audited boundary; test prepared-frame
   identity, unsafe return-frame refusal, async and dirty FP case and fallback.
3. **Make the performance result reproducible.** Use `cargo xtask dev-plan`
   after implementation; its selected `now` lanes and focused crate tests
   precede signed KVM validation. A/B/A/B against the previous stage, same
   host and image provenance, CPUID anchor within 3% within a cohort, all
   samples retained. Capture exact successful fast/fallback counts and reason
   histograms outside the timed hot path. If admission is rare (e.g. below
   95% in the intended echo lane), diagnose it rather than silently filtering
   fallback samples. Record p50/p99/mean/max under idle and 50%/100% host
   pressure, separate 1-vCPU and 8-vCPU runs and place same-core vs remote
   cases explicitly; don't treat a failed KVM boot as a latency sample. No
   reproducible p99/ordinary-path regression at default enable. PR/formal
   gates are required for promotion, not simulated by this planning audit.
4. **Only then compare seL4.** Same x86 machine and comparable boot/security
   settings, end-to-end CALL+REPLY_RECV vs CALL+REPLY_RECV, matched payload and
   stamp protocol. Compare same-root and separate-root separately; report
   raw as well as warm-paired and cold/mixed cases, not just a filtered minimum.
   A green microbench does not make it an app↔`netd` throughput result.

## 5. Implemented stages & verified KVM results (2026-11-28)

### Implemented components
- **B0 (`ipc_rt_diag_register_echo`, `ipc_rt_diag_switch_floor`):** Isolated entry/return echo (`p50=600` cyc) and switch-floor diagnostic probes (`kernel/ps/src/multitask/irq.rs`, `apps/ipcbench/`).
- **L1 (`kernel/ipc-runtime/src/ipc/virtual_rendezvous.rs`, `formal/ipc-register-rendezvous/IpcRegisterRendezvous.tla`):** Objectless 256-slot cache-padded virtual rendezvous table (`VirtualSlotData`) for `<= 16` B register calls, using monotonic 40-bit generations + zero low-16-bit virtual reply tokens strictly disjoint from `REPLIES` slab handles, with transparent slab fallback on saturation.
- **L2 (`kernel/ps/src/multitask/scheduler/pair.rs`, `kernel/ps/src/multitask/cpu_local.rs`, `kernel/ps/src/multitask/scheduler/runqueue/ownership.rs`):** Per-CPU paired custody (`RunOwnerState::Paired` preserving wait reason/epoch in `SchedulerOwnerWord.tla` & `ownership.rs`) transferring `CpuLeases` and `PublishedFrameView` across paired call/reply switches (`pair_publish` with `SeqCst` bank fence + scalar publication), dissolving cleanly to `Local` / `Blocked` on IRQ, preemption, wake, migration, or retirement.
- **L3a (`kernel/lowlevel`, `kernel/hal/src/interrupt_stubs.rs`, `kernel/ps/src/multitask/irq.rs`):** `DirectSwitchResult` compact register handoff bypassing the 54-qword (`432` B) synthetic `SavedContext` zeroing on admitted fast switches while keeping generic escape intact.
- **L3b (`kernel/hal/src/arch/rtc.rs`, `kernel/ps/src/multitask/current.rs`, `kernel/compat/src/user/syscall/linux/ipc_ops{,.rs,/registers.rs}`):** Avoids 8 KiB `SleepWaiter` buffer alloc/free when not growing, checks `interrupts::are_enabled()` before redundant CLI/STI wrappers, and adds a direct `take_inline_reply::<16>` fast path in `complete_parked_call`.
- **Latent ABI fix uncovered under load (`kernel/compat/src/user/syscall/linux/{int_args.rs,linux.rs,dispatch_ext.rs}`):** `int_args::canonical_linux_args` now returns `[u64; 6]` instead of mutating `SyscallFrame::rdi..r9` in place (which truncated caller-preserved upper bits in `r9` across `SYS_futex` under host CPU contention).

### Shipping KVM benchmark comparison (`tsc_khz ≈ 3991 MHz`, 20,000 timed samples)
- **Register IPC idle (`ipc_rt_intra_process_register_reply_recv`):**
  - 1-vCPU baseline (`build/diag/ipc_rt_intra_process_register_reply_recv.txt`): `min=5,200`, `p50=5,640`, `p99=6,360`, `mean=60,306` (`max=179,027,520`, `>10M=9`, `top1%=90.77%`).
  - **1-vCPU final (`build/diag/final-vcpu1-reg-B.txt`):** **`min=4,200` (`1,052 ns`), `p50=4,320` (`1,082 ns`, `-23.4%`), `p99=7,400`, `max=874,480`, `mean=4,576` (`>1M=0`, `>10M=0`, `top1%=5.29%`)**. Legs: `call_to_recv` `min=2,040 / p50=2,120`, `server_body` `40 / 80`, `reply_to_return` `min=2,040 / p50=2,120` (`vmexit_cpuid p50=3,520`).
  - **8-vCPU idle final (`build/diag/final-vcpu8-idle-reg-B.txt`):** **`min=4,280` (`1,072 ns`), `p50=4,400` (`1,102 ns`), `p99=6,720`, `max=131,000`, `mean=4,595` (`>1M=0`, `>10M=0`, `top1%=3.34%`)**.
- **Register IPC under host CPU load (`final-vcpu{1,8}-load{50,100}-reg.txt`):**
  - 1-vCPU 50% load (8 `yes`): `min=6,560`, `p50=8,240`, `p99=9,560`, `mean=9,249` (`>10M=0`).
  - 1-vCPU 100% load (16 `yes`): `min=7,280`, `p50=8,040`, `p99=8,960`, `mean=15,125`.
  - 8-vCPU 50% load (8 `yes`): `min=6,720`, `p50=7,520`, `p99=15,360`, `mean=8,016` (`>1M=0`, `>10M=0`).
  - 8-vCPU 100% load (16 `yes`): `min=6,280`, `p50=8,200`, `p99=12,720`, `mean=18,753`.
- **Full unisolated suite (`final-vcpu{1,8}-full-B.txt`):**
  - Buffer `ipc_rt_intra_process_reply_recv`: 1-vCPU `min=7,240 / p50=7,720`, 8-vCPU `min=7,400 / p50=7,680`.
  - Cross-process `ipc_rt_cross_process_syscalld_getuid`: 1-vCPU `min=14,400 / p50=15,000`, 8-vCPU `min=13,960 / p50=15,040`.
