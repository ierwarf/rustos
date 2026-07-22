-------------------------- MODULE ExecTicketPilot --------------------------
EXTENDS Integers

(***************************************************************************
Typed symbolic refinement pilot for the exact-target, one-shot core of
formal/exec-ticket/ExecTicket.tla. It intentionally excludes fleet cardinality,
sibling retirement, and prepare ownership; TLC remains authoritative for those.
***************************************************************************)

VARIABLES
    \* @type: Str;
    ticketState,
    \* @type: Int;
    ticketPid,
    \* @type: Int;
    ticketTid,
    \* @type: Int;
    ticketUses

vars == <<ticketState, ticketPid, ticketTid, ticketUses>>

Init ==
    /\ ticketState = "unused"
    /\ ticketPid = 0
    /\ ticketTid = 0
    /\ ticketUses = 0

Authorize ==
    /\ ticketState = "unused"
    /\ ticketState' = "pending"
    /\ ticketPid' = 1
    /\ ticketTid' = 11
    /\ UNCHANGED ticketUses

CommitExact ==
    /\ ticketState = "pending"
    /\ ticketPid = 1
    /\ ticketTid = 11
    /\ ticketState' = "executed"
    /\ ticketUses' = ticketUses + 1
    /\ UNCHANGED <<ticketPid, ticketTid>>

CancelExact ==
    /\ ticketState = "pending"
    /\ ticketPid = 1
    /\ ticketTid = 11
    /\ ticketState' = "cancelled"
    /\ ticketUses' = ticketUses + 1
    /\ UNCHANGED <<ticketPid, ticketTid>>

RejectWrongTarget ==
    /\ ticketState = "pending"
    /\ UNCHANGED vars

Next == Authorize \/ CommitExact \/ CancelExact \/ RejectWrongTarget

TypeOK ==
    /\ ticketState \in {"unused", "pending", "executed", "cancelled"}
    /\ ticketPid \in 0..1
    /\ ticketTid \in {0, 11}
    /\ ticketUses \in 0..1

PendingIsExactlyBound == ticketState = "pending" => ticketPid = 1 /\ ticketTid = 11
TicketIsOneShot == ticketUses <= 1
=============================================================================
