------------------------------ MODULE MmRequestLease ------------------------------
EXTENDS Naturals, FiniteSets

(***************************************************************************
Bounded projection of kernel/compat mm_request_lease.rs: one lock-free lease
slot, tickets 1..3 issued monotonically (0 is the free/retired sentinel and is
a guessable echo), two processes with two incarnations each (exec/exit under
the same PID), and two concurrent syscalld appliers that echo a ticket plus
the broker's target pid. Holders are caller-side lease handles; a holder whose
lease was reclaimed because its incarnation died may still run its close.
Actors: owner caller, two appliers, an exec/exit actor, the reclaimer, and a
delayed/replayed/guessed echo (Begin ranges over every ticket and pid).
The projection does not model memory ordering of the SeqCst announce/re-read
(the native test covers the retire-versus-began-effect order), IPC, the
page-table mutation itself (page-table-map-transaction), or table exhaustion
beyond one slot.
***************************************************************************)
Pids == 1..2
Tickets == 1..3
Appliers == 1..2

VARIABLES slot, ticket, retiredBy, boundPid, boundInc, next, holders, reclaimed,
          incOf, guardInc, guardPid, guardTkt,
          acceptedStale, acceptedWrongPid, acceptedNotLive, landedStale
vars == <<slot, ticket, retiredBy, boundPid, boundInc, next, holders, reclaimed,
          incOf, guardInc, guardPid, guardTkt,
          acceptedStale, acceptedWrongPid, acceptedNotLive, landedStale>>

NoGuards == \A a \in Appliers: guardInc[a] = 0

Init ==
    /\ slot = "free" /\ ticket = 0 /\ retiredBy = 0
    /\ boundPid = 1 /\ boundInc = 1 /\ next = 1
    /\ holders = {} /\ reclaimed = {}
    /\ incOf = [p \in Pids |-> 1]
    /\ guardInc = [a \in Appliers |-> 0]
    /\ guardPid = [a \in Appliers |-> 1]
    /\ guardTkt = [a \in Appliers |-> 0]
    /\ acceptedStale = FALSE /\ acceptedWrongPid = FALSE
    /\ acceptedNotLive = FALSE /\ landedStale = FALSE

\* The caller opens a lease bound to its target's current incarnation.
Open(pid) ==
    /\ slot = "free" /\ next \in Tickets
    /\ slot' = "live" /\ ticket' = next /\ next' = next + 1
    /\ boundPid' = pid /\ boundInc' = incOf[pid]
    /\ holders' = holders \cup {next}
    /\ UNCHANGED <<retiredBy, reclaimed, incOf, guardInc, guardPid, guardTkt,
                    acceptedStale, acceptedWrongPid, acceptedNotLive, landedStale>>

\* exec or exit under one PID: a new process/MM incarnation.
Reincarnate(pid) ==
    /\ incOf[pid] < 2
    /\ incOf' = [incOf EXCEPT ![pid] = @ + 1]
    /\ UNCHANGED <<slot, ticket, retiredBy, boundPid, boundInc, next, holders,
                    reclaimed, guardInc, guardPid, guardTkt, acceptedStale,
                    acceptedWrongPid, acceptedNotLive, landedStale>>

\* syscalld echoes (t, pid): honoured only for the live ticket of that pid.
Begin(a, t, pid) ==
    /\ guardInc[a] = 0 /\ t \in 0..3 /\ pid \in Pids
    /\ slot = "live"
    /\ t = ticket
    /\ pid = boundPid
    /\ guardInc' = [guardInc EXCEPT ![a] = boundInc]
    /\ guardPid' = [guardPid EXCEPT ![a] = boundPid]
    /\ guardTkt' = [guardTkt EXCEPT ![a] = t]
    /\ acceptedStale' = (acceptedStale \/ t /= ticket)
    /\ acceptedWrongPid' = (acceptedWrongPid \/ pid /= boundPid)
    /\ acceptedNotLive' = (acceptedNotLive \/ slot /= "live")
    /\ UNCHANGED <<slot, ticket, retiredBy, boundPid, boundInc, next, holders,
                    reclaimed, incOf, landedStale>>

\* One effect ends: it mutates only if the exact incarnation is still current;
\* otherwise the process-state primitive refuses and nothing changes.
Finish(a) ==
    /\ guardInc[a] /= 0
    /\ guardInc[a] = incOf[guardPid[a]]
    /\ landedStale' = (landedStale \/ guardInc[a] /= incOf[guardPid[a]])
    /\ guardInc' = [guardInc EXCEPT ![a] = 0]
    /\ UNCHANGED <<slot, ticket, retiredBy, boundPid, boundInc, next, holders,
                    reclaimed, incOf, guardPid, guardTkt, acceptedStale,
                    acceptedWrongPid, acceptedNotLive>>

FinishRefused(a) ==
    /\ guardInc[a] /= 0
    /\ guardInc[a] /= incOf[guardPid[a]]
    /\ guardInc' = [guardInc EXCEPT ![a] = 0]
    /\ UNCHANGED <<slot, ticket, retiredBy, boundPid, boundInc, next, holders,
                    reclaimed, incOf, guardPid, guardTkt, acceptedStale,
                    acceptedWrongPid, acceptedNotLive, landedStale>>

\* Close step one: the owner retires its own live ticket.
CloseRetire(t) ==
    /\ t \in holders /\ slot = "live" /\ ticket = t
    /\ slot' = "retired" /\ ticket' = 0 /\ retiredBy' = t
    /\ UNCHANGED <<boundPid, boundInc, next, holders, reclaimed, incOf, guardInc,
                    guardPid, guardTkt, acceptedStale, acceptedWrongPid,
                    acceptedNotLive, landedStale>>

\* Close step two: only after began appliers drain does the slot become free.
CloseDrain(t) ==
    /\ t \in holders /\ slot = "retired" /\ retiredBy = t
    /\ NoGuards
    /\ slot' = "free" /\ retiredBy' = 0
    /\ holders' = holders \ {t}
    /\ UNCHANGED <<ticket, boundPid, boundInc, next, reclaimed, incOf, guardInc,
                    guardPid, guardTkt, acceptedStale, acceptedWrongPid,
                    acceptedNotLive, landedStale>>

\* A full table reclaims a lease whose incarnation died (owner never closes).
Reclaim ==
    /\ slot = "live" /\ incOf[boundPid] /= boundInc
    /\ slot' = "retired" /\ retiredBy' = 0 /\ ticket' = 0
    /\ reclaimed' = reclaimed \cup {ticket}
    /\ UNCHANGED <<boundPid, boundInc, next, holders, incOf, guardInc, guardPid,
                    guardTkt, acceptedStale, acceptedWrongPid, acceptedNotLive,
                    landedStale>>

ReclaimDrain ==
    /\ slot = "retired" /\ retiredBy = 0 /\ NoGuards
    /\ slot' = "free"
    /\ UNCHANGED <<ticket, retiredBy, boundPid, boundInc, next, holders,
                    reclaimed, incOf, guardInc, guardPid, guardTkt,
                    acceptedStale, acceptedWrongPid, acceptedNotLive, landedStale>>

\* A reclaimed owner's late close touches nothing but its own handle.
CloseStale(t) ==
    /\ t \in holders /\ t \in reclaimed
    /\ holders' = holders \ {t}
    /\ UNCHANGED <<slot, ticket, retiredBy, boundPid, boundInc, next, reclaimed,
                    incOf, guardInc, guardPid, guardTkt, acceptedStale,
                    acceptedWrongPid, acceptedNotLive, landedStale>>

Stutter == UNCHANGED vars

Next ==
    \/ \E p \in Pids: Open(p) \/ Reincarnate(p)
    \/ \E a \in Appliers, t \in 0..3, p \in Pids: Begin(a, t, p)
    \/ \E a \in Appliers: Finish(a) \/ FinishRefused(a)
    \/ \E t \in Tickets: CloseRetire(t) \/ CloseDrain(t) \/ CloseStale(t)
    \/ Reclaim \/ ReclaimDrain
    \/ Stutter

Spec == Init /\ [][Next]_vars

TypeOK ==
    /\ slot \in {"free", "live", "retired"}
    /\ ticket \in 0..3 /\ retiredBy \in 0..3 /\ next \in 1..4
    /\ boundPid \in Pids /\ boundInc \in 1..2
    /\ holders \subseteq Tickets /\ reclaimed \subseteq Tickets
    /\ incOf \in [Pids -> 1..2]
    /\ guardInc \in [Appliers -> 0..2] /\ guardPid \in [Appliers -> Pids]
    /\ guardTkt \in [Appliers -> 0..3]

\* A guessed, zero, replayed or recycled-slot ticket is never honoured.
NoStaleTicketAccepted == ~acceptedStale
\* A live ticket of one process never authorizes another process.
NoCrossProcessTicket == ~acceptedWrongPid
\* A retired or free slot never begins an effect.
NoBeginAfterRetire == ~acceptedNotLive
\* No effect lands in an incarnation other than the one the lease named.
NoCrossIncarnationEffect == ~landedStale
\* After close completes (slot free) no applier is still inside an effect.
CloseDrainsBeforeFree == slot = "free" => NoGuards
\* A held applier belongs to a slot that is not free, so no effect outlives it.
NoEffectAfterClose == \A a \in Appliers: guardInc[a] /= 0 => slot /= "free"
\* A reclaimed or stale holder cannot disturb the lease of a later owner.
OwnerLeaseIntact ==
    \A t \in holders \ reclaimed:
        \/ slot = "live" /\ ticket = t
        \/ slot = "retired" /\ retiredBy = t
===============================================================================
