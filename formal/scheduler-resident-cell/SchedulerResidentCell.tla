------------------- MODULE SchedulerResidentCell -------------------
EXTENDS Naturals, FiniteSets

(***************************************************************************
Owner: the per-slot resident cell, kernel/ps/src/multitask/scheduler/resident.rs.
A Blocked IPC task keeps its stack pin and execution permit parked in one cell
instead of dropping them at the RSP ACK; the same-CPU lane resumes it from the
cell. The cell word is `0` (empty), an owner generation (published for it), or
`Busy` (one CPU is moving the payload).

Linearization points: every successful compare-exchange or swap of the cell
word. A payload access is legal only for the CPU that holds the word `Busy`.
Loops over the word (claim, take) are modeled as their atomic effect on the
current word, because a lost CAS changes nothing.

Concrete owners:
  * resident.rs     park, claim_empty, take, on_owner_transition, permit_units,
                    commit_outgoing
  * runqueue.rs     RunOwnerWord::compare_exchange (the revoking hook runs AFTER
                    the owner CAS), lane_cas (raw CAS, no hook)
  * ipc_lane.rs     lane take after the owner claim
  * exec_transaction.rs evict_parked_target (eviction without an owner transition)

What the model exists to rule out (the 2026-11-09 4-vCPU panics): the hook for a
generation is a separate, arbitrarily delayed actor, so a task can be woken, run
elsewhere, block again and have its next park arrive while the older generation's
cell is still published or being drained; and a park can itself be delayed
between its ACK and its publication so two parks of one slot overlap. The
pre-fix protocol asserted an empty cell at park (a panic) and freed the word
before the taker read the payload (a parker could overwrite it).

Out of the model: weak-memory ordering of the Dekker pair between the parker's
publication and the hook's load. That handshake is SeqCst on both sides and is
checked by the Loom kernel `resident_cell_delayed_hook_reparks_without_overwrite`.
Assumption: an exec intent excludes the lane's owner CAS (the claim runs under
the binding leaf that carries the intent), so eviction without a transition can
never race a lane that already passed its cell check.
***************************************************************************)

CONSTANT MaxG

Gens == 1..MaxG
Busy == MaxG + 1
ASSUME MaxG \in 1..7
Fixed == {"hook", "lane", "evict", "stale"}
SelfNames == <<"s1", "s2", "s3", "s4", "s5", "s6", "s7">>
SelfD(g) == SelfNames[g]          \* a parker's own re-take of its generation
Drainers == Fixed \cup {SelfD(g) : g \in Gens}

Sites == {"none", "held", "cell", "drained", "dropped", "moved"}
DrainPhases == {"idle", "won", "read", "released"}
ParkPhases == {"none", "owed", "staling", "filling", "filled", "published", "taking", "done"}
Crashes == {"none", "parked-over-live-loans", "payload-torn", "payload-overwritten",
            "double-drop", "fill-claim-lost", "busy-lost", "lane-lost-cell"}

VARIABLES og, ost, cw, pl, lsite, ppc, hooks, dph, dgen, dld, stalePk,
          lanePh, laneGen, execIntent, crash

vars == <<og, ost, cw, pl, lsite, ppc, hooks, dph, dgen, dld, stalePk,
          lanePh, laneGen, execIntent, crash>>

TypeOK ==
    /\ og \in Gens
    /\ ost \in {"blocked", "other"}
    /\ cw \in 0..Busy
    /\ pl \in 0..MaxG
    /\ lsite \in [Gens -> Sites]
    /\ ppc \in [Gens -> ParkPhases]
    /\ hooks \subseteq Gens
    /\ dph \in [Drainers -> DrainPhases]
    /\ dgen \in [Drainers -> 0..MaxG]
    /\ dld \in [Drainers -> 0..MaxG]
    /\ stalePk \in 0..MaxG
    /\ lanePh \in {"idle", "checked", "won"}
    /\ laneGen \in 0..MaxG
    /\ execIntent \in BOOLEAN
    /\ crash \in Crashes

Init ==
    /\ og = 1 /\ ost = "other"
    /\ cw = 0 /\ pl = 0
    /\ lsite = [g \in Gens |-> "none"]
    /\ ppc = [g \in Gens |-> "none"]
    /\ hooks = {}
    /\ dph = [d \in Drainers |-> "idle"]
    /\ dgen = [d \in Drainers |-> 0]
    /\ dld = [d \in Drainers |-> 0]
    /\ stalePk = 0
    /\ lanePh = "idle" /\ laneGen = 0
    /\ execIntent = FALSE
    /\ crash = "none"

CellLoans == {l \in Gens : lsite[l] = "cell"}
Published(g) == cw = g   \* g in Gens

(* ---- owner transitions (the abstract owner word: one generation, Blocked or not) ---- *)

\* publish_return: the executing task is published Blocked and owes an RSP ACK
\* that either parks its loans or releases them. A task whose lane claim has won
\* the owner CAS does not execute before the lane has taken its cell (the resume
\* is the lane's next step), so it cannot block again while the lane is "won".
Block ==
    /\ ost = "other" /\ lanePh # "won" /\ og < MaxG
    /\ og' = og + 1 /\ ost' = "blocked"
    /\ lsite' = [lsite EXCEPT ![og + 1] = "held"]
    /\ ppc' = [ppc EXCEPT ![og + 1] = "owed"]
    /\ UNCHANGED <<cw, pl, hooks, dph, dgen, dld, stalePk, lanePh, laneGen, execIntent, crash>>

\* Any custody transition out of a Blocked generation (wake, claim, terminal,
\* migration). Its revoking hook runs later, as a separate step.
Wake ==
    /\ ost = "blocked" /\ og < MaxG
    /\ og' = og + 1 /\ ost' = "other"
    /\ hooks' = hooks \cup {og}
    /\ UNCHANGED <<cw, pl, lsite, ppc, dph, dgen, dld, stalePk, lanePh, laneGen, execIntent, crash>>

(* ---- drains: whoever wins the word reads the payload, releases the word, drops ---- *)

\* take / eviction: CAS(generation -> Busy).
DrainWin(d, t) ==
    /\ dph[d] = "idle" /\ cw = t /\ t \in Gens
    /\ cw' = Busy
    /\ dph' = [dph EXCEPT ![d] = "won"]
    /\ dgen' = [dgen EXCEPT ![d] = t]

DrainRead(d) ==
    /\ dph[d] = "won"
    /\ dph' = [dph EXCEPT ![d] = "read"]
    /\ dld' = [dld EXCEPT ![d] = pl]
    /\ IF pl # dgen[d]
          THEN /\ crash' = "payload-torn" /\ lsite' = lsite
          ELSE IF lsite[pl] = "cell"
          THEN /\ lsite' = [lsite EXCEPT ![pl] = "drained"] /\ crash' = crash
          ELSE /\ lsite' = lsite /\ crash' = "double-drop"
    /\ UNCHANGED <<og, ost, cw, pl, ppc, hooks, dgen, stalePk, lanePh, laneGen, execIntent>>

\* store(0, Release) once the payload has been read.
DrainRelease(d) ==
    /\ dph[d] = "read" /\ d # "stale"
    /\ dph' = [dph EXCEPT ![d] = "released"]
    /\ IF cw = Busy THEN crash' = crash ELSE crash' = "busy-lost"
    /\ cw' = 0
    /\ UNCHANGED <<og, ost, pl, lsite, ppc, hooks, dgen, dld, stalePk, lanePh, laneGen, execIntent>>

\* The drained loans are dropped by the winner, outside the cell; the lane moves
\* them into the CPU banks instead.
DrainDrop(d) ==
    /\ \/ (dph[d] = "released" /\ d # "stale")
       \/ (dph[d] = "read" /\ d = "stale")
    /\ dph' = [dph EXCEPT ![d] = "idle"]
    /\ IF lsite[dld[d]] = "drained"
          THEN /\ lsite' = [lsite EXCEPT ![dld[d]] = IF d = "lane" THEN "moved" ELSE "dropped"]
               /\ crash' = crash
          ELSE /\ lsite' = lsite /\ crash' = "double-drop"
    \* The parker that retired a stale cell now owns the Busy word and fills it.
    /\ ppc' = IF d = "stale" THEN [ppc EXCEPT ![stalePk] = "filling"] ELSE ppc
    /\ UNCHANGED <<og, ost, cw, pl, hooks, dgen, dld, stalePk, lanePh, laneGen, execIntent>>

(* ---- the revoking hook: runs after the owner CAS, arbitrarily late ---- *)

HookRun ==
    /\ hooks # {} /\ dph["hook"] = "idle"
    /\ \E h \in hooks :
        /\ hooks' = hooks \ {h}
        /\ IF cw = h
              THEN /\ cw' = Busy
                   /\ dph' = [dph EXCEPT !["hook"] = "won"]
                   /\ dgen' = [dgen EXCEPT !["hook"] = h]
              ELSE UNCHANGED <<cw, dph, dgen>>
    /\ UNCHANGED <<og, ost, pl, lsite, ppc, dld, stalePk, lanePh, laneGen, execIntent, crash>>

(* ---- park: claim the cell, fill it, publish, re-read the owner ---- *)

\* The RSP ACK chooses not to park (not an IPC wait, no valid frame...): release.
ParkRelease(g) ==
    /\ ppc[g] = "owed"
    /\ ppc' = [ppc EXCEPT ![g] = "done"]
    /\ lsite' = [lsite EXCEPT ![g] = "dropped"]
    /\ UNCHANGED <<og, ost, cw, pl, hooks, dph, dgen, dld, stalePk, lanePh, laneGen, execIntent, crash>>

ParkClaim(g) ==
    /\ ppc[g] = "owed"
    /\ IF cw = 0
          THEN /\ cw' = Busy
               /\ ppc' = [ppc EXCEPT ![g] = "filling"]
               /\ UNCHANGED <<lsite, dph, dgen, stalePk, crash>>
       \* busy, or a newer generation already holds the cell: this park is obsolete
       ELSE IF cw = Busy \/ cw > g
          THEN /\ ppc' = [ppc EXCEPT ![g] = "done"]
               /\ lsite' = [lsite EXCEPT ![g] = "dropped"]
               /\ UNCHANGED <<cw, dph, dgen, stalePk, crash>>
       \* a published older generation is stale: win it, retire it, keep the word
       ELSE /\ dph["stale"] = "idle"
            /\ cw' = Busy
            /\ dph' = [dph EXCEPT !["stale"] = "won"]
            /\ dgen' = [dgen EXCEPT !["stale"] = cw]
            /\ stalePk' = g
            /\ ppc' = [ppc EXCEPT ![g] = "staling"]
            /\ UNCHANGED <<lsite, crash>>
    /\ UNCHANGED <<og, ost, pl, hooks, dld, lanePh, laneGen, execIntent>>

ParkFill(g) ==
    /\ ppc[g] = "filling"
    /\ pl' = g
    /\ lsite' = [lsite EXCEPT ![g] = "cell"]
    /\ crash' = IF CellLoans \ {g} # {} THEN "payload-overwritten" ELSE crash
    /\ ppc' = [ppc EXCEPT ![g] = "filled"]
    /\ UNCHANGED <<og, ost, cw, hooks, dph, dgen, dld, stalePk, lanePh, laneGen, execIntent>>

ParkPublish(g) ==
    /\ ppc[g] = "filled"
    /\ cw' = g
    /\ crash' = IF cw = Busy THEN crash ELSE "fill-claim-lost"
    /\ ppc' = [ppc EXCEPT ![g] = "published"]
    /\ UNCHANGED <<og, ost, pl, lsite, hooks, dph, dgen, dld, stalePk, lanePh, laneGen, execIntent>>

\* Re-read the owner after publishing: still Blocked in this generation means
\* the task is resident; otherwise take the cell back.
ParkRecheck(g) ==
    /\ ppc[g] = "published"
    /\ ppc' = [ppc EXCEPT ![g] = IF og = g /\ ost = "blocked" THEN "done" ELSE "taking"]
    /\ UNCHANGED <<og, ost, cw, pl, lsite, hooks, dph, dgen, dld, stalePk, lanePh, laneGen, execIntent, crash>>

ParkSelfTake(g) ==
    /\ ppc[g] = "taking" /\ dph[SelfD(g)] = "idle"
    /\ ppc' = [ppc EXCEPT ![g] = "done"]
    /\ IF cw = g
          THEN /\ cw' = Busy
               /\ dph' = [dph EXCEPT ![SelfD(g)] = "won"]
               /\ dgen' = [dgen EXCEPT ![SelfD(g)] = g]
          ELSE UNCHANGED <<cw, dph, dgen>>
    /\ UNCHANGED <<og, ost, pl, lsite, hooks, dld, stalePk, lanePh, laneGen, execIntent, crash>>

(* ---- the lane: check the cell, claim the owner with the raw CAS, take the cell ---- *)

\* The lane resumes only a cell parked for the CPU it runs on (`parked_cpu ==
\* Some(cpu)`), and that CPU's own park() call has returned by then: the park runs
\* inside the RSP ACK with interrupts masked, so the lane cannot interleave with
\* it. Parks of other generations, from other CPUs, still overlap freely.
LaneCheck ==
    /\ lanePh = "idle" /\ ost = "blocked" /\ cw = og /\ ppc[og] = "done" /\ ~execIntent
    /\ lanePh' = "checked" /\ laneGen' = og
    /\ UNCHANGED <<og, ost, cw, pl, lsite, ppc, hooks, dph, dgen, dld, stalePk, execIntent, crash>>

LaneCas ==
    /\ lanePh = "checked" /\ ~execIntent
    /\ IF ost = "blocked" /\ og = laneGen /\ og < MaxG
          THEN /\ og' = og + 1 /\ ost' = "other" /\ lanePh' = "won"
          ELSE /\ UNCHANGED <<og, ost>> /\ lanePh' = "idle"
    /\ UNCHANGED <<cw, pl, lsite, ppc, hooks, dph, dgen, dld, stalePk, laneGen, execIntent, crash>>

LaneTake ==
    /\ lanePh = "won" /\ dph["lane"] = "idle"
    /\ lanePh' = "idle"
    /\ IF cw = laneGen
          THEN /\ cw' = Busy
               /\ dph' = [dph EXCEPT !["lane"] = "won"]
               /\ dgen' = [dgen EXCEPT !["lane"] = laneGen]
               /\ crash' = crash
          ELSE /\ UNCHANGED <<cw, dph, dgen>> /\ crash' = "lane-lost-cell"
    /\ UNCHANGED <<og, ost, pl, lsite, ppc, hooks, dld, stalePk, laneGen, execIntent>>

(* ---- exec: an intent excludes the lane, then evicts the current generation ---- *)

ExecBegin ==
    /\ ~execIntent /\ lanePh \in {"idle", "won"}
    /\ execIntent' = TRUE
    /\ UNCHANGED <<og, ost, cw, pl, lsite, ppc, hooks, dph, dgen, dld, stalePk, lanePh, laneGen, crash>>

ExecEvict ==
    /\ execIntent /\ dph["evict"] = "idle" /\ cw = og
    /\ DrainWin("evict", og)
    /\ UNCHANGED <<og, ost, pl, lsite, ppc, hooks, dld, stalePk, lanePh, laneGen, execIntent, crash>>

Next ==
    \/ Block
    \/ Wake
    \/ \E d \in Drainers : DrainRead(d)
    \/ \E d \in Drainers : DrainRelease(d)
    \/ \E d \in Drainers : DrainDrop(d)
    \/ HookRun
    \/ \E g \in Gens : ParkRelease(g)
    \/ \E g \in Gens : ParkClaim(g)
    \/ \E g \in Gens : ParkFill(g)
    \/ \E g \in Gens : ParkPublish(g)
    \/ \E g \in Gens : ParkRecheck(g)
    \/ \E g \in Gens : ParkSelfTake(g)
    \/ LaneCheck
    \/ LaneCas
    \/ LaneTake
    \/ ExecBegin
    \/ ExecEvict
    \/ UNCHANGED vars

Spec == Init /\ [][Next]_vars

(***************************************************************************
Safety.
***************************************************************************)

\* The panics and corruptions the protocol exists to prevent.
NoPanic == crash = "none"

\* Reference units the cell holds in the execution-permit count (`permit_units`).
PermitUnits == IF cw \in Gens THEN 1 ELSE 0

\* `permit_units` only discounts a unit that really is parked in a published
\* cell, so quiescence observers never see a loan in flight (held by a parker, or
\* held by a winner that has not dropped it) as already gone.
PermitUnitsAreParkedUnits ==
    /\ PermitUnits <= Cardinality(CellLoans)
    /\ (cw \in Gens => lsite[cw] = "cell" /\ pl = cw)

\* A published cell belongs to the live generation, or to one whose revoking hook
\* is still pending, or whose park is still re-reading the owner, or that the
\* lane has just claimed: it never outlives its generation unobserved.
NoCellOutlivesItsGeneration ==
    cw \in Gens =>
        \/ cw = og
        \/ cw \in hooks
        \/ ppc[cw] \in {"published", "taking"}
        \/ (lanePh = "won" /\ laneGen = cw)

\* Only the CPU that holds the word Busy may touch the payload: a loan sitting in
\* the cell is always the one the word is published for, or is being filled.
CellLoanIsPublishedOrBeingFilled ==
    \A l \in CellLoans :
        \/ cw = l
        \/ (cw = Busy /\ ppc[l] \in {"filled", "filling"})
        \/ (cw = Busy /\ \E d \in Drainers : dph[d] = "won" /\ dgen[d] = l)
        \/ ppc[l] = "published" /\ cw = Busy

\* Every loan is dropped or moved exactly once: nothing is left behind once all
\* parks, hooks, drains and the lane are at rest.
NoLoanLeakedAtRest ==
    ( /\ \A g \in Gens : ppc[g] \in {"none", "done"}
      /\ hooks = {}
      /\ \A d \in Drainers : dph[d] = "idle"
      /\ lanePh \in {"idle"} )
    => \A l \in Gens :
        \/ lsite[l] \in {"none", "dropped", "moved"}
        \/ (lsite[l] = "cell" /\ cw = l /\ l = og /\ ost = "blocked")
        \/ (lsite[l] = "cell" /\ cw = l /\ execIntent = FALSE)

\* A held loan always has a park still deciding its fate.
HeldLoanHasAPark ==
    \A l \in Gens : lsite[l] = "held" => ppc[l] \in {"owed", "staling", "filling"}

====
