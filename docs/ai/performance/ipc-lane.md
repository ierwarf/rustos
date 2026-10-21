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
