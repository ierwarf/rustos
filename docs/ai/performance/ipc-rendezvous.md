# IPC register rendezvous: P3 design draft

[Contract index](../performance-hardening.md). Owner: `kernel/ps` scheduler lane
(`scheduler/ipc_lane*`, `scheduler/resident.rs`) with `kernel/ipc-runtime` endpoints.
Status: **design draft, not implemented.** Written 2026-10-26 after the stage 1 package
of `ipc-lane.md` ("P3 stage 1"); it records what was measured, what the proposed
mechanism would buy, and the formal model that bounds its correctness argument.
The existing resident scheduler lane, direct parking and queued/rendezvous reply
convergence are implemented separately in [IPC lane](ipc-lane.md). This draft's
**objectless virtual reply and paired-custody mechanisms remain unimplemented**;
its cost estimates are historical projections, not current measured latency.

## Implementation plan and cycle budget — 2026-11-27

**Planned, not implemented.** The [L1–L3 implementation and acceptance
plan](ipc-l1-l3-plan.md) specifies the implementation order, disassembly
anchors and recovery boundaries. The companion [queued and tail plan](ipc-queued-tail-plan.md)
covers the general transport and worst-latency work. There is no fixed tick gate. This section
supersedes the historical draft below where its admission, reply identity,
or cycle budget differs. Start
from the `f9a70967` isolated 16-byte
register IPC echo on 1-vCPU KVM: host idle, 2,000 warmups, 20,000 round
trips, p50 **5,600 guest invariant-TSC ticks**, p99 **8,640**; CPUID p50
**3,760** (not subtracted). The call-to-receive and reply-to-return p50s
are 2,920 and 2,600, respectively. They are separate distributions.
The evidence is `build/ipc-slowpath-baseline/register-kvm-0-samples.log`.

### L1 — objectless register rendezvous, existing scheduler ownership

1. **Define admission and one source of truth.** Start with a 1–16 byte
   register CALL and matching `REGISTER_REPLY_RECV`, exact endpoint
   incarnation, same CPU/address space, no older eligible queued sender,
   an already parked receiver, and ordinary own-funded execution. Retain
   the current scheduler's priority, affinity, placement, timing and
   revocation policy. Keep one sequence-bound receipt in the parked call
   record. The virtual reply must identify the exact call and receiving
   task, not just the caller's slot or task generation (which can survive
   multiple requests). Keep a sealed discriminator disjoint from ordinary
   reply handles. The scheduler owns the cell; IPC runtime arbitrates
   endpoint order; compat marshals register results. Keep the user ABI.
2. **Use one claim for both paths.** In `ipc_lane.rs` / `resident.rs`,
   integrate a shared fast/ordinary claim for the parked receiver. Validate
   authorization, queue order and both participants' current generations
   before delivering. Publish the two words and the virtual reply in the
   server's saved register frame and use the existing lane switch. Fused
   reply atomically consumes the exact outstanding call, writes the reply
   words to the caller, and arms receive. Do not enqueue or allocate a reply
   object on the normal case; keep the existing lazy funding semantics and
   put the required accounting custody in the call record, not in an
   unaccounted caller/server interval.
3. **Specify escape before admitting work.** Before claim, fall back without
   IPC side effects. After claim, normal dispatch must complete the *same*
   request, never replay it. Exit/revoke/timeout must be able to terminate
   the virtual call without allocating. For operations that need a real
   reply object (mixed ABI, remote or nested use), publish it at most once
   from the recorded custody. Provide reserved capacity where completion
   is mandatory, or decline entry if that recovery cannot be guaranteed.
   Test late/duplicate replies, same-task new-call reuse, FIFO vs fast claim,
   fallback once-only, OOM/timeout/exit/revoke, and donation accounting.

### L2 — one CPU-owned pair for both handoffs

1. **Model ownership before changing dispatch.** Add a `Paired` custody
   representation to `RunOwnerWord`, with both task generations referring
   to one bounded per-CPU pair record. Only that CPU selects and publishes
   the active member. Other CPUs either see an ordinary owner or the pair
   record, not two independent runnable owners. Define entry, paired
   switch, break request and dissolution; model the intermediate claims of
   both owner words, stale notifications, rollback before publication and
   ABA on slot/pair reuse. Initially bound pairing to a single RPC (at most
   two handoffs), same address space and no nesting.
2. **Move repeated work out of the pair.** On admission, pin both exact
   frame/architecture images and reserve execution and placement custody.
   During each handoff, publish the departing frame and active member in
   one ordered switch transaction. Retain the actual RSP, TSS/syscall
   stack, FS/GS, SIMD/return-sanitization and accounting work that the
   architecture requires. Do not re-pin or repeat the generic
   resident-park/take/bank handoff when the pair already owns those
   resources. Account for *both* pair setup and teardown in the measured
   round trip; don't claim steady-state savings by timing only its middle.
3. **Break through a single owner.** Wake/cancel, revoke, signal, preemption,
   priority/affinity change, migration and retirement request a break via
   the exact pair generation. The owner CPU publishes each task's ordinary
   custody once, transfers pending work once and retains its resources
   until the switch acknowledgement. No remote task operation may mutate
   the old owner word behind the pair. Higher-priority execution, expired
   budget/deadline, or outstanding migration wins over the shortcut. Audit
   every owner-word reader and writer (including async return/IRQ paths)
   before enabling; start opt-in on 1 vCPU, then exercise remote-event races
   on SMP. Cross-address-space admission and any reuse across multiple RPCs
   are follow-ups, not requirements for the first pair implementation.

### Projected p50 (same workload, idle host; not acceptance evidence)

| Scenario | Net L1 saving from 5,600 | Additional net L2 saving after L1 | L1 result | L1 + L2 result |
|---|---:|---:|---:|---:|
| Conservative | 200 | 900 | 5,400 | 4,500 |
| Planning estimate | 400 | 1,300 | 5,200 | 3,900 |
| Favorable | 600 | 1,500 | 5,000 | 3,500 |

L1 is based on the later ~240-tick reply-slot removal sensitivity check plus
possible queue/fixed-work savings, minus new receipt checks. Do **not** reuse
all of the original 800–1,000 tick L1 projection: the shipped P8 path already
removed the common ledger-lock cost. L2 uses the older ~2,900-tick stamped
budget for both lane switches only as an upper bound for budgeting: count
roughly 35–60% as removable repeated custody work, then budget roughly
100–250 ticks for pair setup, teardown and guards. Architecture work stays,
and any cost already counted in L1 is excluded. The ranges are scenarios,
not confidence intervals or guarantees. In particular, no p99 or seL4
parity is predicted.

Gate L1 and L2 independently: freeze same-host A/B/A/B images, keep the
existing verified echo/STOP/join workload and CPUID anchor within 3%, retain
all samples, and publish p50/p99/mean/max plus fast-path admission/refusals.
Use separate diagnostic builds for leg/custody attribution. Require a
repeatable win beyond measurement noise with no reproducible p99 or
fallback regression before enabling. These estimates do not set thresholds.
Run 1 and 8 vCPU cohorts independently, at idle and under host pressure;
include mixed queued traffic and correctness scenarios as separate tests,
not as filtered-away samples. Validate L1 against the existing IPC
abstraction and L2 against scheduler ownership and asynchronous return;
add registered models, native and mutation witnesses, and KVM validation
before the affected path ships. Compare with seL4 only for matched round-trip
workloads on the same hardware, separating shared and separate address spaces.

## What the measurements say (historical draft)

Same-session A/B, 1 vCPU `ipc_rt_intra_process_register_reply_recv`, p50, anchor 3.5-3.7k.
Start of the series 7.38k; stage 1 package 6.27k.

| Change (each measured alone, 3-4 alternating runs) | p50 effect |
|---|---|
| `task_execution_owner` scans `0..ADMITTED_CPU_BOUND`, not 16 CPUs | -600 |
| lane diagnostic rows batched per CPU, no migration swap | -280 |
| `IrqLeaf` release keeps no IF re-proof in shipping images | -170 |
| `DispatchPreemption` keeps no release-time IF / CPU-index proofs | -180 |
| `wait::reason` pre-filters without the epoch leaf | -107 |
| retire a non-`Drop` slab value in place (600-byte reply object) | -150 |
| own-context call admission in one ledger acquisition, reply frame built in slot | -70 |

Ablations (delete the thing, compare p50) bound what each *planned* stage can buy on
the current object model: both lane `admitted` lifecycle reads -120; the lane reply
context-custody filter -150; `recent_sleeper` -15; reply deadline waiter arm+disarm
-120; placement admission lock -70. Masked lock variants for the lane, a pending-
migration pre-check and a lock-free identity read were not measurable.

Unit costs in this guest (hot loop, ticks): tracked lock pair 36-46 (masked 36),
`pushf` 8, `cli`+`sti` 17, `rdtsc` 27, uncontended `lock cmpxchg` 6. A round trip takes
~28 tracked acquisitions inside masked sections plus 6-8 with IF set (endpoint, reply,
waiter table) and ~16k retired instructions at IPC ~1.97 (host `perf stat` of the
vCPU thread). There is no single dominant function: the profile is hundreds of
0.3-1.5 % items. That is why stamp-by-stamp trimming stops paying and why the
remaining levers are *whole objects* the call does not need.

Consequence for the originally planned stages: the "cell + epoch" precondition
replacement (stage 2) and the "16-byte direct delivery" (stage 3) are worth about
-120..-250 and -150..-250 respectively *on the present object model*, because the
costs they target are loads that hit cache; the cost is in lock pairs, locked RMWs and
the instruction volume of the layers around them. The mechanism below is the one that
removes objects.

## The mechanism: virtual call through the resident cell (L1)

A same-CPU register `CALL` to a server that is parked resident in `REGISTER_RECV` /
`REPLY_RECV` is delivered **without** the endpoint queue pop, a reply object and a
donation-ledger edge:

* **Arm (server, once per park).** The park record already names the endpoint. The
  resident cell (generation-bound by the owner word, evicted by *any* other owner
  transition of that generation) additionally records the endpoint handle (the
  handle carries the incarnation) and "cell-backed". The server's endpoint-queue
  registration stays *sticky*: it is not popped by fast callers, only by slow ones.
* **Claim (caller).** After the unchanged authentication, one comparison (cell endpoint
  == called endpoint, incarnation by handle) and the existing lane claim CAS of the
  server's owner word (Blocked -> Running at the cell's generation). Slow callers that
  pop the sticky registration claim the server through the same owner-word CAS, so fast
  and slow callers exclude each other; a registration whose server is already running
  is stale and dropped by the popper.
* **Deliver.** The two request words go into the server's saved syscall frame
  (`rdx/r10`), the reply capability is a **virtual reply cap** (tag + caller slot +
  caller generation) written to `r8`, caller pid/tid to `r9/rsi`; the caller's park
  record becomes `CALL_VIRTUAL`.
* **Reply.** `REPLY_RECV(cap)` recognises the tag, checks `cap == (caller slot,
  generation)` against the caller's park record (task-bound: only the receiving task
  may reply, as today), writes the response words into the caller's saved frame and
  switches through the existing reply lane. No reply object is read or retired.
* **Funding.** The charge token and inherited class are published for the server slot
  from the cell, without a ledger entry; a ledger entry is created on demand
  (materialization) by anything that needs one.

### Materialization (the only way a virtual call acquires objects)

Anything that is not the plain request/response hand-off first calls
`materialize(cap)` under the caller's park custody: it creates the reply object (state
`RequestTaken`, receiver = the server task, caller custody = the caller's scheduling
context) and the ledger edge bound to the server, then proceeds exactly as the slow
path would. Sites: client deadline/cancel (`fast_call_collect_response`, `cancel_reply_wait`),
server exit and process exit cleanup (`fail_endpoints_owned_by`, `release_task`),
endpoint revocation, a handle-carrying / oversized / buffer-mode reply on the cap,
`reply` from a different CPU than the caller's, a nested `CALL` by the server
(`Loans::capture` / `funding_basis` need the edge), `donation_ledger::find_reply` on a
virtual cap, diagnostics. A spurious wake of the blocked caller re-blocks (today's
loop). Failure to materialize changes nothing.

### Invariants (checked by the draft model below)

1. A request is delivered only to a parked server of the current endpoint incarnation
   (`DeliveryNeedsReceivingServer`); delivery happens at most once per call.
2. Fast and slow callers exclude each other through one claim word (`SingleOwner`,
   `PendingIsServed`, `ServedIsPending`).
3. A response exists iff the call is `AReplied`; a cancelled call never replies
   (`ResponseOnlyForReplied`, `FailedHasNoResponse`).
4. A virtual call holds no reply object and no ledger edge, a materialized pending call
   holds both (`VirtualHasNoObjects`, `FundingIffMaterializedPending`); the only
   representation change is materialization (`VirtualOnlyBecomesRealByMaterializing`,
   `VirtualStaysObjectless`).
5. Every implementation step is a step of the slow path's abstract call
   `ANone -> APending -> AReplied | ACancelled` (`RefinesSlowPath`).
6. A stale cell is inert: a waiting cell is either the parked server's or belongs to an
   older incarnation (`CellMatchesServer`); a revoked endpoint has no pending call
   (`RevokedHasNoPendingCall`); an exited server holds nothing.

### Expected effect and what it does not reach

Removed per round trip: endpoint pop and request take (2 endpoint + 1-2 reply pairs),
reply completion and response take (2 reply pairs), 3 ledger pairs, the waiter-table
arm/disarm if the deadline rides on the park record (2), and the ~600-byte reply object.
About 9-11 lock pairs and the surrounding instructions: **-0.8..-1.0k, p50 ~5.3k.**
L1 does not touch the two lane switches (~2.9k of the stamped 6.2k: owner-word CAS
pair, banks, interval, placement grant, frame publication, architecture install).
Getting to ~3.5-4k needs **L2**: the pair stays `Paired` in both owner words for the
duration of the call and only the per-CPU active member flips, so a switch is a stack/
TSS/FS flip plus one cell store. Every owner-word observer (wake, stop, migrate,
terminal) would have to understand `Paired` or materialize it first. That is a
custody-model change of the scheduler, larger than L1 and the real price of 3k; this
draft does not propose it.

### Verification plan

Register `formal/ipc-register-rendezvous/IpcRegisterRendezvous` in `models.tsv` with the
mutation corpus entries below; add flow-contract rows for the cell arm/claim/deliver/
reply/materialize transitions; pin the lane and `complete_parked_*` sources in
`smp-source-contracts.toml`; add implementation mutants for the epoch compare, the claim
CAS, the sticky-registration staleness test, the virtual-cap tag/generation check and
each materialization site; native witnesses for every materialization site comparing
the resulting reply object/ledger state with the slow path's; keep the P7 differential
lane test and add a virtual differential. TLC: 1300 distinct states, depth 16, all
invariants and properties hold; 8 of 8 hand mutants are killed
(`M1` ignore the epoch compare, `M2` slow caller does not claim the cell, `M3` cancel
leaves the call virtual, `M4` revocation keeps a pending call, `M5` exit leaves a waiting
cell, `M6` fast reply keeps the served client, `M7` materialize without a ledger edge,
`M8` fast caller claims nothing).

```tla
---- MODULE IpcRegisterRendezvous ----
EXTENDS Naturals, FiniteSets

\* DRAFT (design evidence only, not registered in formal/models.tsv).
\*
\* One endpoint (re-publishable), one server S, two clients, one CPU.
\*
\* A register CALL to a same-CPU resident server may complete through a rendezvous
\* CELL instead of the endpoint queue, a reply object and a donation-ledger edge (a
\* "virtual" call). The cell word is the arbiter of the SAME exclusive claim the
\* endpoint queue arbitrates: a cell-backed server is also queue-registered, and
\* whoever (fast caller, slow caller, server cancel) moves the cell waiting->x first
\* wins. A cell carries the endpoint epoch it was validated against; revocation
\* advances the epoch and does NOT have to find the cell (it may be on another CPU).
\*
\* Every abnormal event the slow path models (client cancel/deadline, server exit,
\* endpoint revocation) observes a virtual call through MATERIALIZATION: the virtual
\* state becomes, in the same atomic step, exactly the reply object and ledger edge the
\* slow path would hold, and the slow path then finishes it. The claim is that the
\* visible call history is a history of the slow path:
\*
\*   ANone -> APending -> AReplied | ACancelled
\*   (a request is delivered only to a parked server of the CURRENT endpoint
\*    incarnation; reply object + ledger edge exist iff APending and materialized)

CONSTANTS MaxEpoch, MaxCalls
ASSUME MaxEpoch \in Nat /\ MaxEpoch >= 3 /\ MaxCalls \in Nat /\ MaxCalls >= 1

Clients == {1, 2}

VARIABLES
    epoch, live,       \* endpoint incarnation epoch / currently published
    srv,               \* server: parked | running | gone
    srvKind,           \* how the parked server is registered: cell | queue
    srvEpoch,          \* incarnation the server's queue registration belongs to
    cell, cellEpoch,   \* rendezvous cell: empty|waiting|claimed, validated at epoch
    cur,               \* client whose request the running server holds (0 = none)
    cli, call, realState, funding, got, calls

vars == <<epoch, live, srv, srvKind, srvEpoch, cell, cellEpoch, cur, cli, call,
          realState, funding, got, calls>>

Init ==
    /\ epoch = 1 /\ live = TRUE /\ srv = "parked" /\ srvEpoch = 1
    /\ srvKind \in {"cell", "queue"}
    /\ cell = IF srvKind = "cell" THEN "waiting" ELSE "empty"
    /\ cellEpoch = IF srvKind = "cell" THEN 1 ELSE 0
    /\ cur = 0
    /\ cli = [c \in Clients |-> "idle"] /\ call = [c \in Clients |-> "none"]
    /\ realState = [c \in Clients |-> "none"] /\ funding = [c \in Clients |-> FALSE]
    /\ got = [c \in Clients |-> <<FALSE, FALSE>>] /\ calls = 0

Pending(c) == cli[c] = "blocked" /\ (call[c] = "virtual" \/ (call[c] = "real" /\ realState[c] = "pending"))

\* The server is genuinely waiting for a request of the current incarnation.
Receiving == srv = "parked" /\ live /\ srvEpoch = epoch

\* ---------------------------------------------------------------- normal path ----
\* FastCall: one cell CAS (waiting -> claimed) plus one epoch compare. No reply object,
\* no ledger edge, no endpoint queue operation.
FastCall(c) ==
    /\ cli[c] = "idle" /\ calls < MaxCalls
    /\ cell = "waiting" /\ cellEpoch = epoch          \* <- the epoch compare
    /\ cell' = "claimed"
    /\ cli' = [cli EXCEPT ![c] = "blocked"] /\ call' = [call EXCEPT ![c] = "virtual"]
    /\ got' = [got EXCEPT ![c] = <<TRUE, FALSE>>]
    /\ srv' = "running" /\ cur' = c /\ calls' = calls + 1
    /\ UNCHANGED <<epoch, live, srvKind, srvEpoch, cellEpoch, realState, funding>>

\* SlowCall: pops the queue registration (reply object + ledger edge). A cell-backed
\* server is claimed through its cell too, so fast and slow callers exclude each other.
SlowCall(c) ==
    /\ cli[c] = "idle" /\ calls < MaxCalls /\ Receiving
    /\ srvKind = "queue" \/ cell = "waiting"
    /\ cell' = IF srvKind = "cell" THEN "claimed" ELSE cell
    /\ cli' = [cli EXCEPT ![c] = "blocked"] /\ call' = [call EXCEPT ![c] = "real"]
    /\ realState' = [realState EXCEPT ![c] = "pending"]
    /\ funding' = [funding EXCEPT ![c] = TRUE]
    /\ got' = [got EXCEPT ![c] = <<TRUE, FALSE>>]
    /\ srv' = "running" /\ cur' = c /\ calls' = calls + 1
    /\ UNCHANGED <<epoch, live, srvKind, srvEpoch, cellEpoch>>

FastReply(c) ==
    /\ srv = "running" /\ cur = c /\ cli[c] = "blocked" /\ call[c] = "virtual"
    /\ cli' = [cli EXCEPT ![c] = "done"] /\ call' = [call EXCEPT ![c] = "none"]
    /\ got' = [got EXCEPT ![c] = <<got[c][1], TRUE>>] /\ cur' = 0
    /\ UNCHANGED <<epoch, live, srv, srvKind, srvEpoch, cell, cellEpoch, realState, funding, calls>>

SlowReply(c) ==
    /\ srv = "running" /\ cur = c /\ cli[c] = "blocked" /\ call[c] = "real" /\ realState[c] = "pending"
    /\ realState' = [realState EXCEPT ![c] = "replied"] /\ cli' = [cli EXCEPT ![c] = "done"]
    /\ funding' = [funding EXCEPT ![c] = FALSE]
    /\ got' = [got EXCEPT ![c] = <<got[c][1], TRUE>>] /\ cur' = 0
    /\ UNCHANGED <<epoch, live, srv, srvKind, srvEpoch, cell, cellEpoch, call, calls>>

\* The server parks: it registers with the CURRENT incarnation and (optionally) arms a
\* cell validated once, here, against the current epoch.
ServerPark(kind) ==
    /\ srv = "running" /\ cur = 0 /\ live
    /\ srv' = "parked" /\ srvKind' = kind /\ srvEpoch' = epoch
    /\ cell' = IF kind = "cell" THEN "waiting" ELSE "empty"
    /\ cellEpoch' = IF kind = "cell" THEN epoch ELSE 0
    /\ UNCHANGED <<epoch, live, cur, cli, call, realState, funding, got, calls>>

\* Server-side cancellation of a parked receive (timeout/signal): it must win the cell
\* before it may leave; a claimed cell means the request is already on its way.
ServerCancelPark ==
    /\ srv = "parked" /\ cell # "claimed"
    /\ srv' = "running" /\ cell' = "empty"
    /\ UNCHANGED <<epoch, live, srvKind, srvEpoch, cellEpoch, cur, cli, call, realState, funding, got, calls>>

\* A woken server that was not served (revocation, spurious wake) retires its own stale
\* cell when it resumes; until then the epoch compare keeps the cell inert.
ServerResumeClearsCell ==
    /\ srv = "running" /\ cell = "waiting"
    /\ cell' = "empty"
    /\ UNCHANGED <<epoch, live, srv, srvKind, srvEpoch, cellEpoch, cur, cli, call, realState, funding, got, calls>>

\* ---------------------------------------------------------------- abnormal ------
ClientNext(c) ==
    /\ cli[c] \in {"done", "failed"}
    /\ cli' = [cli EXCEPT ![c] = "idle"] /\ call' = [call EXCEPT ![c] = "none"]
    /\ realState' = [realState EXCEPT ![c] = "none"]
    /\ got' = [got EXCEPT ![c] = <<FALSE, FALSE>>]
    /\ UNCHANGED <<epoch, live, srv, srvKind, srvEpoch, cell, cellEpoch, cur, funding, calls>>

Materialize(c) ==
    /\ call[c] = "virtual" /\ cli[c] = "blocked"
    /\ call' = [call EXCEPT ![c] = "real"] /\ realState' = [realState EXCEPT ![c] = "pending"]
    /\ funding' = [funding EXCEPT ![c] = TRUE]
    /\ UNCHANGED <<epoch, live, srv, srvKind, srvEpoch, cell, cellEpoch, cur, cli, got, calls>>

ClientCancel(c) ==
    /\ Pending(c)
    /\ call' = [call EXCEPT ![c] = "real"] /\ realState' = [realState EXCEPT ![c] = "cancelled"]
    /\ cli' = [cli EXCEPT ![c] = "failed"] /\ funding' = [funding EXCEPT ![c] = FALSE]
    /\ cur' = IF cur = c THEN 0 ELSE cur
    /\ UNCHANGED <<epoch, live, srv, srvKind, srvEpoch, cell, cellEpoch, got, calls>>

ServerExit ==
    /\ srv # "gone"
    /\ srv' = "gone" /\ cur' = 0
    /\ cell' = IF cell = "waiting" THEN "empty" ELSE cell   \* the exit path wins the cell
    /\ cli' = [c \in Clients |-> IF Pending(c) THEN "failed" ELSE cli[c]]
    /\ call' = [c \in Clients |-> IF Pending(c) THEN "real" ELSE call[c]]
    /\ realState' = [c \in Clients |-> IF Pending(c) THEN "cancelled" ELSE realState[c]]
    /\ funding' = [c \in Clients |-> IF Pending(c) THEN FALSE ELSE funding[c]]
    /\ UNCHANGED <<epoch, live, srvKind, srvEpoch, cellEpoch, got, calls>>

\* Revocation: the epoch advances, the queue registration is failed (the server is woken
\* with an error), pending calls fail like a revoked reply object. The cell is NOT
\* touched: it may belong to another CPU and is rejected by the epoch compare instead.
Revoke ==
    /\ live /\ epoch < MaxEpoch
    /\ epoch' = epoch + 1 /\ live' = FALSE
    /\ srv' = IF srv = "parked" THEN "running" ELSE srv
    /\ cli' = [c \in Clients |-> IF Pending(c) THEN "failed" ELSE cli[c]]
    /\ call' = [c \in Clients |-> IF Pending(c) THEN "real" ELSE call[c]]
    /\ realState' = [c \in Clients |-> IF Pending(c) THEN "cancelled" ELSE realState[c]]
    /\ funding' = [c \in Clients |-> IF Pending(c) THEN FALSE ELSE funding[c]]
    /\ cur' = 0
    /\ UNCHANGED <<srvKind, srvEpoch, cell, cellEpoch, got, calls>>

\* The endpoint is published again (same handle, new incarnation).
Publish ==
    /\ ~live /\ epoch < MaxEpoch
    /\ epoch' = epoch + 1 /\ live' = TRUE
    /\ UNCHANGED <<srv, srvKind, srvEpoch, cell, cellEpoch, cur, cli, call, realState, funding, got, calls>>

SpuriousWake(c) == Pending(c) /\ UNCHANGED vars   \* signal: the blocked call re-blocks

Stutter == UNCHANGED vars

Next == \/ \E c \in Clients : \/ FastCall(c) \/ SlowCall(c) \/ FastReply(c) \/ SlowReply(c)
                              \/ ClientNext(c) \/ Materialize(c) \/ ClientCancel(c) \/ SpuriousWake(c)
        \/ \E k \in {"cell", "queue"} : ServerPark(k)
        \/ ServerCancelPark \/ ServerResumeClearsCell \/ ServerExit \/ Revoke \/ Publish \/ Stutter

Spec == Init /\ [][Next]_vars

\* ------------------------------------------------------------ refinement map ----
AbsCall(c) ==
    IF cli[c] = "idle" THEN "ANone"
    ELSE IF cli[c] = "done" THEN "AReplied"
    ELSE IF cli[c] = "failed" THEN "ACancelled"
    ELSE "APending"

AbsStep(c) ==
    \/ AbsCall(c)' = AbsCall(c)
    \/ AbsCall(c) = "ANone" /\ AbsCall(c)' = "APending"
    \/ AbsCall(c) = "APending" /\ AbsCall(c)' \in {"AReplied", "ACancelled"}
    \/ AbsCall(c) \in {"AReplied", "ACancelled"} /\ AbsCall(c)' = "ANone"

RefinesSlowPath == [][\A c \in Clients : AbsStep(c)]_vars

\* The slow path's receive rule: a request is delivered only to a parked server of the
\* current published incarnation (this is what the epoch compare must preserve).
DeliveryNeedsReceivingServer ==
    [][(cur = 0 /\ cur' # 0) => Receiving]_vars

\* ------------------------------------------------------------------ invariants ----
TypeOK ==
    /\ epoch \in 1..MaxEpoch /\ live \in BOOLEAN /\ cur \in {0} \cup Clients
    /\ srv \in {"parked", "running", "gone"} /\ srvKind \in {"cell", "queue"}
    /\ cell \in {"empty", "waiting", "claimed"}
    /\ cli \in [Clients -> {"idle", "blocked", "done", "failed"}]
    /\ call \in [Clients -> {"none", "virtual", "real"}]
    /\ realState \in [Clients -> {"none", "pending", "replied", "cancelled"}]
    /\ funding \in [Clients -> BOOLEAN] /\ calls \in 0..MaxCalls

SingleOwner == Cardinality({c \in Clients : Pending(c)}) <= 1
PendingIsServed == \A c \in Clients : Pending(c) => (srv = "running" /\ cur = c)
ServedIsPending == (cur # 0) => Pending(cur)

ResponseOnlyForReplied == \A c \in Clients : got[c][2] <=> (cli[c] = "done")
ResponseNeedsDelivery == \A c \in Clients : got[c][2] => got[c][1]
FailedHasNoResponse == \A c \in Clients : cli[c] = "failed" => ~got[c][2]

FundingIffMaterializedPending ==
    \A c \in Clients : funding[c] <=> (call[c] = "real" /\ realState[c] = "pending")
VirtualHasNoObjects == \A c \in Clients : call[c] = "virtual" => (~funding[c] /\ realState[c] = "none")
BlockedClientHasACall == \A c \in Clients : cli[c] = "blocked" => call[c] \in {"virtual", "real"}
BlockedRealIsPending == \A c \in Clients : (cli[c] = "blocked" /\ call[c] = "real") => realState[c] = "pending"
TerminalConsistent == \A c \in Clients :
    /\ cli[c] = "done" => (call[c] = "none" \/ realState[c] = "replied")
    /\ cli[c] = "failed" => (call[c] = "real" /\ realState[c] = "cancelled")

\* A waiting cell belongs to a parked server; a claimed cell has a running server or a
\* completed claim; a cell is never newer than the endpoint.
CellMatchesServer ==
    /\ (cell = "waiting") => ((srv = "parked" /\ srvKind = "cell" /\ cellEpoch = srvEpoch)
                             \/ cellEpoch < epoch)   \* a stale cell can only be an OLD one
    /\ cellEpoch <= epoch
RevokedHasNoPendingCall == ~live => \A c \in Clients : ~Pending(c)
GoneServerHoldsNothing == srv = "gone" => cur = 0 /\ cell # "waiting"

TerminalIsFinal == [][\A c \in Clients : (cli[c] \in {"done", "failed"}) => cli'[c] \in {"done", "failed", "idle"}]_vars
VirtualOnlyBecomesRealByMaterializing ==
    [][\A c \in Clients : (call[c] = "virtual" /\ call'[c] = "real") => realState'[c] \in {"pending", "cancelled"}]_vars
VirtualStaysObjectless ==
    [][\A c \in Clients : (call[c] = "virtual" /\ call'[c] = "virtual") => ~funding'[c]]_vars
====
```

```
CONSTANTS
    MaxEpoch = 3
    MaxCalls = 3

SPECIFICATION Spec

INVARIANTS
    TypeOK
    SingleOwner
    PendingIsServed
    ServedIsPending
    ResponseOnlyForReplied
    ResponseNeedsDelivery
    FailedHasNoResponse
    FundingIffMaterializedPending
    VirtualHasNoObjects
    BlockedClientHasACall
    BlockedRealIsPending
    TerminalConsistent
    CellMatchesServer
    RevokedHasNoPendingCall
    GoneServerHoldsNothing

PROPERTIES
    RefinesSlowPath
    TerminalIsFinal
    DeliveryNeedsReceivingServer
    VirtualOnlyBecomesRealByMaterializing
    VirtualStaysObjectless
```
