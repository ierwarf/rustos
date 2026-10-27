# IPC lane: resident same-CPU synchronous call/reply switch

[Contract index](../performance-hardening.md). Owner: `kernel/ps` scheduler
(`scheduler/ipc_lane*`, `scheduler/resident.rs`). Status: design of record;
phases below land one at a time and each leaves the tree shippable.

## Why a lane

Measured 2026-10-21 (1 vCPU, isolated `ipc_rt_intra_process_reply_recv`): a
round trip is ~31k TSC ticks, ~25k of it in two software-interrupt dispatches
of ~12.7k each (publication 3.3k, prologue 1.6k, claim 3.5k, complete 0.5k,
install 1.4k, drop 0.7k, enter 0.3k) and ~4.5k in the syscall layers. The cost
is not any one lock (a lean lock pair is 24 ticks) but the number of independent
re-validations of the same facts: every handoff re-pins the task, re-reads its
context, re-opens funding and re-admits MM, placement and affinity although
nothing changed since the same task blocked microseconds earlier.

Reference points (EuroSys'19 cross-process IPC round trip, as cited by the
LionsOS paper, arXiv 2501.06234; numbers disagree between secondary sources):
seL4 ~0.4-1.0k cycles, Fiasco.OC ~2.7k, Zircon ~8.2k. seL4's fastpath
(`src/fastpath/fastpath.c`) checks every precondition before a point of no
return, falls back to the slowpath with no state changed on any failure, and
then switches threads directly (Liedtke's direct process switch and lazy
queueing: the TCB, not the run queue, is the truth). The lane adopts exactly
that shape on RustOS custody rules. Target: Fiasco.OC class (<= ~3-4k) as the
end state; each phase is measured and reported, none is promised.

## Architecture (final form)

1. **Resident blocked IPC tasks.** A task that blocks in an endpoint receive
   or reply wait on a CPU keeps its CPU-bank loans (allocation pin and
   execution permit, later placement grant) parked in `RESIDENT[slot]` instead
   of releasing them at the RSP ACK. Residency is one bit in the owner word
   (`resident`, valid only for `Blocked`/`DirectHandoff`). Every owner CAS that
   clears the bit evicts (drops) the cell inside `RunOwnerWord::compare_exchange`,
   so wake, terminal, migration and every legacy claim stay correct without
   edits; only the lane's `compare_exchange_taking` takes the loans. Flows that
   need quiescence (pins == 0) call `resident::evict(slot)` first.
2. **Lane trap.** A dedicated software-interrupt vector and stub run
   `ipc_lane_dispatch` with IRQs masked and no `CpuDispatchTurn`. It either
   switches C -> S (returns S's saved RSP) or declines (returns C's own frame
   with an outcome code in RAX, exactly like `stamp_fast_ipc_call_outcome`) so
   the unchanged syscall slowpath continues. All fallible checks precede the
   single point of no return (the CAS of S to Running); after it only
   infallible moves remain.
3. **Preconditions (all cheap, lock-free).** Same CPU; S resident on this CPU
   and waiting on exactly this endpoint/reply; no older sync FIFO work, no
   activation prefix, chain not owed to a fair competitor (the DirectSwitch
   admission already shipped); both published lifecycles dispatchable; affinity
   admits the CPU with no migration pending; class publication sequence even;
   inline message; S's effective funding stamp equals C's; budget available
   after charging C's elapsed time.
4. **Effects at the point of no return.** C: Running -> Blocked(resident)
   (frame published, wait committed). S: Blocked(resident) -> Running via the
   taking CAS; banks move cell -> Current, Current -> cell; interval: the one
   FundingPin shuttles (charge in place, relabel the reply token, no
   pin/unpin); Current slot, Transition, scratch and lockdep owner updated as
   `complete()` does today; architecture install (CR3 only when the root
   differs, TSS/syscall stack, FS/GS base via WRFSBASE when available, SIMD).
5. **Differential obligation.** The lane must leave the same externally
   observable state as the generic dispatch. A native test runs the same
   scenario through the generic path and through the lane and compares a state
   snapshot (owner words, banks, interval, Current/Transition, installed
   placement, frame publication, ledger).

## Non-goals

No change to capabilities, the ring-3 service model, endpoint semantics, the
slow/cross-CPU/handle-bearing/oversized paths, or the 8-turn fairness bound.
Policy stays in userspace services.

## Phases and status (2026-10-21)

- P0 declining lane entry: shipped as the first statement of
  `software_schedule_interrupt_dispatch`; a decline changes nothing.
- P1 resident cells, eviction hook in `RunOwnerWord::compare_exchange`, parking
  at the RSP ACK (`resident.rs`, `cpu_local::commit_context_switch_at_rsp`).
- P2 lane call switch (`ipc_lane::call`), P3 lane reply switch (`ipc_lane::reply`).
- P4 funding: the interval is forwarded along a donation chain
  (`execution_interval::{can_forward, forward}`), the funder is charged once for
  the whole span when the chain ends (`charge_from_*`), and CurrentWait pins are
  one cleanup leaf instead of three.
- P5 shipping lock fast paths (2026-10-21, owner-approved removal of runtime
  lock validation in shipping images): `TrackedSpinLock::lock` with IRQs on
  takes `lock_lean_irq` (cli/sti-bracketed depth + held-class push, spin with
  IRQs as the caller left them; the order graph and IRQ-usage classification
  stay in host, test and `rustos_lock_phase_profile` builds); the lean release
  no longer re-derives the CPU index; `ProcessStateLock` takes and releases with
  one CAS/swap (no owner metadata, wait-context assertions or sleepable-class
  registry) and skips the waiter queue unless a task is blocking (`waiting`
  count, ordered by the full barriers of the swap and the blocker's `fetch_add`);
  service-endpoint authorization prefilters slots on the raw endpoint word.
- P6 (2026-10-22, owner-approved check removal and a new kernel-hal contract):
  - `kernel-hal` `user_access`: the calling task's own user memory is read and
    written in place with `rep movsb`, writes are proven with an atomic no-op
    `lock cmpxchg` probe, and a kernel-mode fault inside one of the three routines
    resumes at its fixup through a dedicated vector-14 entry (a table of
    relative insn/fixup pairs). Any fault re-runs the operation once on the
    validated software-walk path, which resolves COW and returns the exact errno;
    CR0.WP is forced on every CPU first. `ipcbench` proves the recovery at start
    (`uaccess-recovery pass`: unmapped, read-only and out-of-range ranges).
  - `UserAccess` runs a syscall's argument read, range admission and request copy
    under one bind (or none), with a per-bind page-translation memo.
  - The software-schedule trap no longer executes `int`: the same five-word frame
    is built by `SOFTWARE_TRAP` and the unchanged handler is entered (~0.1k each).
  - `CurrentSlot` arms, cancels and commits the executing task's wait without a
    pin; lane clock reads are one `ClockSample`; the lane bind and the SIMD
    save/restore skip re-proofs the lane already holds; the RSP-ACK parking no
    longer re-validates the frame bytes; admission pins the Current donor lock-free.
  - Not done: request delivery from the lane (the remaining endpoint/reply
    transport is ~1.3k spread over four small critical sections, so a mailbox
    rewrite would save well under 0.5k), `iretq` replacement (~95 ticks each),
    the split recv/reply probe (it measures the unfused fallback by design; only
    `vfsd`, `procd` and `pagerd` use it - converting them to `reply_recv` is the
    effective fix).

- P7 (2026-10-26) direct block of register IPC. A register `CALL` waiting for its
  reply and a register `REPLY_RECV` waiting for its next request no longer keep a
  kernel continuation: before the lane trap they write a **park record** (kind,
  endpoint, reply, deadline) beside their syscall frame and enter the lane through
  `fast_ipc_{call,reply}_direct_schedule`, which builds only the words the handler
  reads in a minimal kernel `SavedContext` whose RIP is `fast_ipc_parked_resume`
  and whose RSP is the syscall frame. A committed block never returns: the Rust
  frames below the syscall frame are abandoned (the SIMD marker stays set; the
  resume entry re-adopts it). When the scheduler (or the lane tail) enters the
  resume stub, `fast_ipc_parked_tail` completes the operation from the record
  (`complete_parked_register_ipc`: disarm the deadline waiter, take the response
  or the addressed request, write the return registers, deliver a pending signal)
  and leaves through the same `syscall_return` and epilogue as `syscall_dispatch`.
  A wake that carried no response or request (signal, cancel, deadline) takes the
  ordinary slow completion from the same record, so no wake path needs to know the
  task has no continuation. A declined handoff returns to the caller exactly as
  the trap form does. The lane tail enters a context whose RIP is the registered
  resume entry directly on its frame (`FAST_IPC_RESUME_RIP`), skipping the 31
  register reloads and the `iretq`. Plain `RECV`, buffer mode and the legacy
  handlers keep their in-place continuation.
  Same-session A/B, 1 vCPU `ipc_rt_intra_process_register_reply_recv` p50: trap form
  7.75k -> direct block 7.65k -> with the resume shortcut 7.24k -> with the lane
  trims (request publication after the lane, masked per-CPU stack/profile writes)
  7.04k (p99 8.3k -> 7.5k).
  Lane trims after P7 (no ownership or custody change): `admitted` is lifecycle-only
  (`current_identity::read_task_dispatch`; the placement/affinity proof stays in the
  claim's `with_placement_admission`), the CALL donor is qualified against its
  lifecycle (`execution::qualified`) and bound by
  `donation_publication::bind_lane_current` instead of being pinned (the pinned
  `bind_original` remains the fallback), and the non-test SIMD marker claim is an
  assert plus a relaxed store (the CPU's single IPC marker is owned by the one task
  running there). Together they measure p50 6.76k / p99 7.4k (3 runs, vmexit_cpuid anchor 3.5-3.7k): the remaining round trip is
  ~40 stages of 80-400 cycles (call-side reserve ~1.2k, each lane switch ~1.7k,
  server tail ~1.7k, entry ~0.9k), so further gains need the P3 rendezvous cell
  (epoch-validated receiver, fused reserve/bind/release), not more stage trimming.

- P3 stage 1 (2026-10-26, no custody change): the call-side admission of a task funded by
  its own context (`donation_publication::reserve_call_current`) takes one masked section
  and one ledger acquisition (`donation_ledger::reserve_own`, same `FundingBasis` entry,
  borrowed bases still take the full capture); the fast-call reply object is built inside
  its slab slot (`GenerationalSlab::insert_with_hint_built`) instead of moving ~600 bytes
  through three frames; the lane's per-switch diagnostic rows (dispatch, runtime, switch and
  address-space counts) are batched in a per-CPU cell and folded into the diagnostic leaf
  every 64 lane switches (`runtime_profile::owner::lane_{account,dispatch,flush}`), and the
  lane no longer swaps the per-slot migration word (both ends of a lane switch last ran on
  this CPU). Method note: stamp deltas overstate savings that sit under a later locked
  instruction and understate stores that precede one; only same-session A/B counts. Removing
  the four diagnostic leaf round trips per switch pair measured -280 (p50 7.15k -> 6.9k,
  anchor 3.5-3.7k, 3 alternating runs); the admission/endpoint changes are about -70.
  The largest single item was found with guest `perf record -e cycles:G` on the vCPU
  thread (flat profile; the stamp timeline had hidden it): `cpu_local::task_execution_owner`
  scanned all `MAX_TRACKED_CPUS` (16 since the CPU-cap commit) Current/Transition publications
  on every frame-token and wake check, ~600 ticks per round trip on a 1-vCPU guest. The scan
  now covers `0..ADMITTED_CPU_BOUND`, one past the highest CPU ever admitted (raised with
  Release before that CPU's first admission store), so a never-admitted CPU cannot own a task.
  With the scan bound and the diagnostic batching: p50 7.27k -> 6.47k (control 7.40/7.20/7.32k vs 6.40/6.40/6.60k). Profiling recipe:
  `perf record -e cycles:G -c 20000 -t <CPU 0/KVM tid>` while `cargo xtask bench
  --isolate-probe ...` runs, `perf script -D` for guest IPs (the image is KASLR-slid by a
  2 MiB multiple; match it against `nm` of the archived `nucleus.elf`). Guest LBR is not
  virtualized, so callers of `memcpy`/`memset` cannot be recovered that way.
  Three further trims, each measured on its own (same-session A/B, 3-4 alternating runs):
  `IrqLeaf` release no longer re-reads IF (`pushf`, 17 ticks) in shipping images, -170;
  the lane's three `wait::reason` pre-filters read the payload without the epoch leaf
  (`reason_snapshot`; the claim re-reads it under the leaf before its CAS), -107; and
  `DispatchPreemption` keeps its release-time IF and CPU-index proofs, and the CPU-index
  check in `mask_irqs`, for `rustos_lock_phase_profile` builds only, -180. Not measurable
  (reverted): masked `IrqLeaf`/interval/epoch/ledger/placement lock variants for the lane,
  a pending-migration pre-check in `PlacementGrant::acknowledge`, and a lock-free identity
  read in the lane's reply custody filter. Ablations (delete the check, compare p50) bound
  what stage 2/3 style validation fusion can buy: both `admitted` checks -120, the reply
  context-custody filter -150, `recent_sleeper` -15, the reply deadline waiter arm+disarm -120.
  `GenerationalSlab::with_mut_take` retires a value with no destructor in place instead of
  moving the 600-byte reply object out to be dropped, -150. What stages 2 and 3 as planned
  (epoch cell, direct delivery) would buy on this object model, the rendezvous mechanism
  that removes the objects instead, its TLA+ model and the open decision are in
  [ipc-rendezvous.md](ipc-rendezvous.md).

Measured (1 vCPU isolated `ipc_rt_intra_process_reply_recv`, p50 ticks):
session start 41.9k -> DirectSwitch 31.9k -> lane call+reply 19.2k -> funding
shuttle 17.9k -> lock-light CurrentWait pin 16.9k -> interval forwarding 15.1k ->
direct request take on wake 14.4k (min 14.0k).
8 vCPU: p50 16.6-16.9k, mean 18.8k (was 36.7k / 43.0k) before P5; with P5 1 vCPU p50
13.1-13.4k (min 12.5k) and 8 vCPU p50 13.6-14.4k, mean 14.6-14.8k; with P6 1 vCPU p50
9.7-10.0k (min 9.4k, same host window) and 8 vCPU p50 9.8-10.9k, mean 10.6-11.2k,
cross-process getuid mean 17.2-17.6k; cross-process getuid
mean 27.8k (was 55k). The split recv/reply probe `ipc_rt_intra_process` still
uses the generic reply path and is unchanged (p50 ~52k).

Where the 15k goes: two lane traps ~3.4k each (lane 2.4-2.5k, `int`/frame
save/restore/iretq ~0.9k), syscall layers ~3.9k (call admission .7, endpoint
reserve .5, arm .35, reply-side args/validate/complete 1.2, receive arm .6,
response take .5), and ~4.6k of syscall entry/exit, user copies and user code.
The next large steps are structural: deliver the request into the receiver's
context from the lane instead of the endpoint round trip, build the frame
without `int`, and lazy SIMD.

Profile after P5 (1 vCPU, ordered TSC stamps, ~25 ticks of stamp cost each, so
absolute values run ~1.3k high): lane::call 3.1k and lane::reply 2.5k (of which
install ~0.6k each - SIMD restore ~0.4k - bind_original 0.6k, precondition reads
0.3-0.4k, interval 0.25k, complete 0.23k); client syscall layer: admission 0.7k,
endpoint reserve 0.6k, response write 0.7k, request copy 0.4k; server syscall
layer: args read + retain 0.7k, copy-and-validate 0.55k, output write 0.8k, recv
arm/commit ~1.4k; 54 tracked lock pairs per round trip at ~45 ticks. A null
syscall is ~480 ticks, so entry/exit is not the remaining cost.

- P3 stage 3 (2026-10-27, no custody change), 1 vCPU `ipc_rt_intra_process_register_reply_recv`
  p50 5.28-5.32k -> 5.08-5.12k (same-session alternating, ring-3 anchors alu 3.08k / atomic
  3.24k / cpuid 3.72k unchanged). Found by microbenchmarking the instructions on the path
  (TSC ticks per execution, in-kernel loop of 1000): `fninit` 89, `wrmsr` (FS/KernelGS base)
  79, `rdmsr` 127, `fnclex` 49, `rdtsc` ~27, `swapgs` 28, `cli`+`sti` 13, `pushf`+`pop` 4,
  `fldz` x8 + `emms` 7, `vzeroall` 7, `xrstor` init 313, `fldenv` 97. The register-IPC return
  sanitizer ran two `fninit` (170 ticks per call, twice per round trip); the opening one only
  guaranteed that the eight `fldz` pushes would meet neither a pending exception nor a live
  stack, so it is now `fnstsw` (+ `fnclex` only when ES is set) and `emms`, and the closing
  `fninit` still resets control, status, tag and instruction/data pointers: 93 ticks, -200 per
  round trip. Smaller: the entry reads identity and the pending-signal hint in one masked
  section (`current_user_ipc_entry`), the lane qualifies and admits each end with one
  publication read (`qualified_dispatchable`), and a deadline measured from now is not
  re-checked against the clock it was read from. The lane's two per-switch clock samples no longer start with `lfence`
  (`clock::tsc_views` reads unordered, like `ticks()`): -140 per round trip; the draining fence
  was the cost, not the `rdtsc`. Measured and dropped: unrolled 65-256 byte `memcpy`/`memset`
  (-50, noise), shrinking the fast-call frame arrays (0), the `fence(SeqCst)` in
  `resident::park` (0), lock-free staging of reply deadlines (the 99 % staged arm/disarm pair
  saved ~30, so the table lock pair costs ~15 in context and the change was not kept),
  `WRFSBASE` in place of the FS-base `wrmsr` (-65 for a CR4.FSGSBASE change that also lets
  ring 3 write the base behind the task record; not kept). Sensitivity check for the
  reply-object removal (L1): six extra read-only locks of the reply slot add +170, so one
  uncontended slot lock pair is ~28 ticks and the five reply/endpoint pairs of a round trip
  are ~140 plus the 584-byte frame copy: about -240 for a change that touches reply
  cancel, revoke, exit and the donation ledger, which is why it was not built. Cycle-weighted guest profiles over-weight the
  ~4 % of round trips that take the generic dispatch (+15k each): use ablation or
  instruction microbenchmarks for p50 decisions.

## Other paths found after P6 (2026-10-22)

- **Reaped child tax (fixed).** The kernel rejected procd's `SELECT_SIGNAL` reply
  because it carries the selected SIGCHLD events in `reserved0`, so a parent whose
  child exited kept SIGCHLD pending and every later syscall re-issued a failing
  procd call (`getpid` 480 -> ~99k ticks forever after one fork+wait; fused IPC
  9.9k -> 107k). Only `SELECT_SIGNAL` may carry `reserved0` now;
  `uaccess_selftest` fails if a post-fork syscall exceeds 20k ticks.
- **Direct-copy safety net.** `ipcbench` starts with `uaccess_selftest`: fault
  recovery (unmapped/read-only/out-of-range), COW copyout across a fork (the other
  side keeps its bytes), and a bounded map/unmap race against a calling thread.
- **Service loops.** `procd` and `vfsd` now reply-and-receive in one syscall through
  `rustos_svc_runtime::ipc::reply_then_recv` (shared recovery rules); snapshot
  replies stay on the plain path. `pagerd` interleaves a fault rendezvous with
  `try_recv`, so it cannot be fused. `rootd`, `storaged`, `devmgrd`, `netd` and
  `uiserver` still use plain replies (per-request work dwarfs the IPC path).
- **`sched_yield`.** Returns without entering the dispatcher when nothing else can
  run on the CPU (`other_dispatch_work_pending`); the benchmark keeps background
  services runnable, so its number does not show the idle-CPU case.
- **Tail (8 vCPU).** Fused IPC p99 ~18k vs p50 ~10k; the >1M samples are debug-port
  milestone lines (one VM exit per byte) landing inside a call, the >80k ones match
  the host noise `vmexit_cpuid` shows. Not yet addressed: wake placement is not
  idle-aware (`affinity_target_cpu` keeps a busy last CPU unless the queue is
  deeper by two), and topology (SMT/LLC) is not modelled.
- P8 (2026-10-27) no new custody model; stages that stopped paying for themselves were
  removed whole. Own-funded, ordinary-class register-IPC donation edges are reserved,
  bound and released without the ledger leaf (`donation_ledger/lazy.rs`: a slot word per
  donor, `settle` under the leaf inserts exactly the eager entries, every locked ledger
  operation calls `locked()`). Reply custody for a live caller is answered from the
  published identity and the derived scheduling-context identity before the Identity51 /
  Resources53 leaves. The lane reads the receiver/caller frame token no more than the
  resident cell already proves, and the RSP ACK parks from a ticket (saved RSP + Blocked
  generation) instead of re-reading the owner word and token. Single-writer bank, lease
  and ACK words move with plain stores and one fence per publish; the execution interval
  cell is CPU-private; `acknowledge` skips the binding leaf without a pending migration;
  the x87 clear is skipped when XINUSE reports the init state. Same-session A/B, 1 vCPU
  register round trip p50: 5760 -> 4840 ticks (alu anchor 3720). Measured and left
  alone: `xchg` -> store, deadline-waiter removal and the `pushf` bracket moved nothing;
  host `perf` profiles misattribute those, so decisions use removal experiments.
  What remains is ~40 protocol steps in the lane switch (owner CAS, frame publication,
  resident park/take, banks), which only a fused switch can cut.
