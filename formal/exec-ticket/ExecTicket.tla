------------------------------- MODULE ExecTicket -------------------------------
EXTENDS Naturals, FiniteSets

(*******************************************************************************
Models procd's exact exec-ticket transaction with loaderd and Linux threads.

Concrete owners:
  * services/procd/src/main.rs
  * services/loaderd/src/main.rs
  * kernel/compat/src/user/syscall/linux/proc_broker_ops.rs
  * kernel/compat/src/user/syscall/linux.rs
  * kernel/compat/src/user/syscall/linux/support.rs

Authorize binds a newly allocated ticket to one live (PID, TID) pair. Cancel
or loader commit may consume it only with that same pair; a mismatched request
is non-destructive. Successful commit publishes the saved register handoff
before it changes the target image. A normal thread exit removes that exact
target's state, while Linux exec retires all sibling threads and their tickets
or handoffs. Process exit prunes any remaining target-bound authority.

The bounded configuration has two PID owners with two TIDs each. It abstracts
executable bytes, address-space construction, loader prepare ownership, and
scheduler fairness. ProcBrokerSession models prepare lifetime; this model
checks the ticket/target/handoff lifecycle boundary.
*******************************************************************************)

CONSTANTS Pids, Tids, Tickets, Prepares, MaxTickets

NoPid == 0
NoTid == 0
NoTicket == 0

(*******************************************************************************
The cfg fixes PIDs to {1, 2} and TIDs to {1, 2, 3, 4}: two Linux threads per
process. Keeping the ownership function in the module makes every action use
the exact pair rather than a service-name or PID-only approximation.
*******************************************************************************)
TargetPid(tid) == IF tid \in {1, 2} THEN 1 ELSE 2

Running == "running"
Exited == "exited"

ThreadAlive == "alive"
ThreadExited == "exited"

TicketUnused == "unused"
TicketPending == "pending"
TicketCancelled == "cancelled"
TicketExecuted == "executed"
TicketRejected == "rejected"
TicketTargetExited == "target-exited"

PrepareReady == "ready"
PrepareConsumed == "consumed"

NoTransition == "none"
PendingTransition == "pending"
AppliedTransition == "applied"
DroppedTransition == "dropped"

OldImage == "old-image"
HandoffPending == "handoff-pending"
RegistersApplied == "registers-applied"
TargetTerminated == "target-terminated"

VARIABLES processState,
          threadState,
          ticketState,
          ticketPid,
          ticketTid,
          ticketUses,
          prepareState,
          transitionState,
          transitionPid,
          transitionTid,
          transitionTicket,
          imageState

vars == <<processState, threadState, ticketState, ticketPid, ticketTid,
          ticketUses, prepareState, transitionState, transitionPid,
          transitionTid, transitionTicket, imageState>>

ExactLiveTarget(pid, tid) ==
    /\ pid \in Pids
    /\ tid \in Tids
    /\ processState[pid] = Running
    /\ threadState[tid] = ThreadAlive
    /\ TargetPid(tid) = pid

PendingTicketCount ==
    Cardinality({ticket \in Tickets : ticketState[ticket] = TicketPending})

TerminalTicket(state) ==
    state \in {TicketCancelled, TicketExecuted, TicketRejected, TicketTargetExited}

Init ==
    /\ processState = [pid \in Pids |-> Running]
    /\ threadState = [tid \in Tids |-> ThreadAlive]
    /\ ticketState = [ticket \in Tickets |-> TicketUnused]
    /\ ticketPid = [ticket \in Tickets |-> NoPid]
    /\ ticketTid = [ticket \in Tickets |-> NoTid]
    /\ ticketUses = [ticket \in Tickets |-> 0]
    /\ prepareState = [prepare \in Prepares |-> PrepareReady]
    /\ transitionState = [tid \in Tids |-> NoTransition]
    /\ transitionPid = [tid \in Tids |-> NoPid]
    /\ transitionTid = [tid \in Tids |-> NoTid]
    /\ transitionTicket = [tid \in Tids |-> NoTicket]
    /\ imageState = [tid \in Tids |-> OldImage]

Authorize(ticket, pid, tid) ==
    /\ ticket \in Tickets
    /\ ticketState[ticket] = TicketUnused
    /\ ExactLiveTarget(pid, tid)
    /\ PendingTicketCount < MaxTickets
    /\ ticketState' = [ticketState EXCEPT ![ticket] = TicketPending]
    /\ ticketPid' = [ticketPid EXCEPT ![ticket] = pid]
    /\ ticketTid' = [ticketTid EXCEPT ![ticket] = tid]
    /\ UNCHANGED <<processState, threadState, ticketUses, prepareState,
                  transitionState, transitionPid, transitionTid,
                  transitionTicket, imageState>>

CancelExact(ticket, pid, tid) ==
    /\ ticket \in Tickets
    /\ ticketState[ticket] = TicketPending
    /\ ticketPid[ticket] = pid
    /\ ticketTid[ticket] = tid
    /\ ticketState' = [ticketState EXCEPT ![ticket] = TicketCancelled]
    /\ ticketUses' = [ticketUses EXCEPT ![ticket] = @ + 1]
    /\ UNCHANGED <<processState, threadState, ticketPid, ticketTid,
                  prepareState, transitionState, transitionPid, transitionTid,
                  transitionTicket, imageState>>

(*******************************************************************************
The concrete cancel and exec-target paths compare stored PID/TID before
removal. These actions intentionally preserve state: malformed input must not
destroy a different live ticket.
*******************************************************************************)
RejectMismatchedCancel(ticket, pid, tid) ==
    /\ ticket \in Tickets
    /\ ticketState[ticket] = TicketPending
    /\ pid \in Pids
    /\ tid \in Tids
    /\ <<ticketPid[ticket], ticketTid[ticket]>> # <<pid, tid>>
    /\ UNCHANGED vars

(*******************************************************************************
Commit makes its current exact ticket terminal, publishes the current TID's
register handoff, and retires every sibling thread. The sibling cleanup models
the post-exec sweep that removes any ticket/transition for scheduler-cleared
TIDs before loaderd can return.
*******************************************************************************)
CommitExact(ticket, pid, tid, prepare) ==
    /\ ticket \in Tickets
    /\ prepare \in Prepares
    /\ ticketState[ticket] = TicketPending
    /\ <<ticketPid[ticket], ticketTid[ticket]>> = <<pid, tid>>
    /\ ExactLiveTarget(pid, tid)
    /\ prepareState[prepare] = PrepareReady
    /\ transitionState[tid] = NoTransition
    /\ ticketState' =
        [other \in Tickets |->
            IF other = ticket THEN TicketExecuted
            ELSE IF ticketState[other] = TicketPending /\ ticketPid[other] = pid
                    /\ ticketTid[other] # tid
                 THEN TicketTargetExited
                 ELSE ticketState[other]]
    /\ ticketUses' =
        [other \in Tickets |->
            IF other = ticket \/ (ticketState[other] = TicketPending
                                   /\ ticketPid[other] = pid
                                   /\ ticketTid[other] # tid)
            THEN ticketUses[other] + 1 ELSE ticketUses[other]]
    /\ prepareState' = [prepareState EXCEPT ![prepare] = PrepareConsumed]
    /\ threadState' =
        [other \in Tids |->
            IF TargetPid(other) = pid /\ other # tid THEN ThreadExited
            ELSE threadState[other]]
    /\ transitionState' =
        [other \in Tids |->
            IF other = tid THEN PendingTransition
            ELSE IF transitionState[other] = PendingTransition
                    /\ transitionPid[other] = pid
                 THEN DroppedTransition
                 ELSE transitionState[other]]
    /\ transitionPid' = [transitionPid EXCEPT ![tid] = pid]
    /\ transitionTid' = [transitionTid EXCEPT ![tid] = tid]
    /\ transitionTicket' = [transitionTicket EXCEPT ![tid] = ticket]
    /\ imageState' =
        [other \in Tids |->
            IF other = tid THEN HandoffPending
            ELSE IF TargetPid(other) = pid THEN TargetTerminated
                 ELSE imageState[other]]
    /\ UNCHANGED <<processState, ticketPid, ticketTid>>

(*******************************************************************************
After loaderd has presented the exact ticket, later validation may reject the
attempt; that exact ticket is one-shot. Wrong PID/TID never reaches this action
and therefore remains live for procd to cancel correctly.
*******************************************************************************)
RejectExactCommit(ticket, pid, tid) ==
    /\ ticket \in Tickets
    /\ ticketState[ticket] = TicketPending
    /\ <<ticketPid[ticket], ticketTid[ticket]>> = <<pid, tid>>
    /\ ~ (\E prepare \in Prepares :
            /\ ExactLiveTarget(pid, tid)
            /\ prepareState[prepare] = PrepareReady
            /\ transitionState[tid] = NoTransition)
    /\ ticketState' = [ticketState EXCEPT ![ticket] = TicketRejected]
    /\ ticketUses' = [ticketUses EXCEPT ![ticket] = @ + 1]
    /\ UNCHANGED <<processState, threadState, ticketPid, ticketTid,
                  prepareState, transitionState, transitionPid, transitionTid,
                  transitionTicket, imageState>>

RejectMismatchedCommit(ticket, pid, tid) ==
    /\ ticket \in Tickets
    /\ ticketState[ticket] = TicketPending
    /\ pid \in Pids
    /\ tid \in Tids
    /\ <<ticketPid[ticket], ticketTid[ticket]>> # <<pid, tid>>
    /\ UNCHANGED vars

ApplyHandoff(tid) ==
    /\ tid \in Tids
    /\ transitionState[tid] = PendingTransition
    /\ ExactLiveTarget(transitionPid[tid], tid)
    /\ transitionTid[tid] = tid
    /\ imageState[tid] = HandoffPending
    /\ transitionState' = [transitionState EXCEPT ![tid] = AppliedTransition]
    /\ imageState' = [imageState EXCEPT ![tid] = RegistersApplied]
    /\ UNCHANGED <<processState, threadState, ticketState, ticketPid, ticketTid,
                  ticketUses, prepareState, transitionPid, transitionTid,
                  transitionTicket>>

(*******************************************************************************
A non-final SYS_EXIT retires only one Linux TID. Its PID remains alive for
siblings, but no pending ticket or handoff may retain the vanished TID.
*******************************************************************************)
ExitThread(tid) ==
    /\ tid \in Tids
    /\ threadState[tid] = ThreadAlive
    /\ threadState' = [threadState EXCEPT ![tid] = ThreadExited]
    /\ ticketState' =
        [ticket \in Tickets |->
            IF ticketState[ticket] = TicketPending /\ ticketTid[ticket] = tid
            THEN TicketTargetExited ELSE ticketState[ticket]]
    /\ ticketUses' =
        [ticket \in Tickets |->
            IF ticketState[ticket] = TicketPending /\ ticketTid[ticket] = tid
            THEN ticketUses[ticket] + 1 ELSE ticketUses[ticket]]
    /\ transitionState' =
        [other \in Tids |->
            IF other = tid /\ transitionState[other] = PendingTransition
            THEN DroppedTransition ELSE transitionState[other]]
    /\ imageState' = [imageState EXCEPT ![tid] = TargetTerminated]
    /\ UNCHANGED <<processState, ticketPid, ticketTid, prepareState,
                  transitionPid, transitionTid, transitionTicket>>

(*******************************************************************************
Normal last-thread exit and default fatal-signal termination use the process
cleanup path. It prunes every pending ticket/transition for that PID.
*******************************************************************************)
ExitTarget(pid) ==
    /\ pid \in Pids
    /\ processState[pid] = Running
    /\ processState' = [processState EXCEPT ![pid] = Exited]
    /\ threadState' =
        [tid \in Tids |->
            IF TargetPid(tid) = pid THEN ThreadExited ELSE threadState[tid]]
    /\ ticketState' =
        [ticket \in Tickets |->
            IF ticketState[ticket] = TicketPending /\ ticketPid[ticket] = pid
            THEN TicketTargetExited ELSE ticketState[ticket]]
    /\ ticketUses' =
        [ticket \in Tickets |->
            IF ticketState[ticket] = TicketPending /\ ticketPid[ticket] = pid
            THEN ticketUses[ticket] + 1 ELSE ticketUses[ticket]]
    /\ transitionState' =
        [tid \in Tids |->
            IF transitionState[tid] = PendingTransition /\ transitionPid[tid] = pid
            THEN DroppedTransition ELSE transitionState[tid]]
    /\ imageState' =
        [tid \in Tids |->
            IF TargetPid(tid) = pid THEN TargetTerminated ELSE imageState[tid]]
    /\ UNCHANGED <<ticketPid, ticketTid, prepareState, transitionPid,
                  transitionTid, transitionTicket>>

Next ==
    \/ \E ticket \in Tickets, pid \in Pids, tid \in Tids : Authorize(ticket, pid, tid)
    \/ \E ticket \in Tickets, pid \in Pids, tid \in Tids : CancelExact(ticket, pid, tid)
    \/ \E ticket \in Tickets, pid \in Pids, tid \in Tids :
        RejectMismatchedCancel(ticket, pid, tid)
    \/ \E ticket \in Tickets, pid \in Pids, tid \in Tids, prepare \in Prepares :
        CommitExact(ticket, pid, tid, prepare)
    \/ \E ticket \in Tickets, pid \in Pids, tid \in Tids : RejectExactCommit(ticket, pid, tid)
    \/ \E ticket \in Tickets, pid \in Pids, tid \in Tids :
        RejectMismatchedCommit(ticket, pid, tid)
    \/ \E tid \in Tids : ApplyHandoff(tid)
    \/ \E tid \in Tids : ExitThread(tid)
    \/ \E pid \in Pids : ExitTarget(pid)

TypeOK ==
    /\ Pids = {1, 2}
    /\ Tids = {1, 2, 3, 4}
    /\ Tickets \subseteq Nat
    /\ Prepares \subseteq Nat
    /\ \A tid \in Tids : TargetPid(tid) \in Pids
    /\ MaxTickets \in Nat
    /\ processState \in [Pids -> {Running, Exited}]
    /\ threadState \in [Tids -> {ThreadAlive, ThreadExited}]
    /\ ticketState \in [Tickets -> {TicketUnused, TicketPending, TicketCancelled,
                                     TicketExecuted, TicketRejected, TicketTargetExited}]
    /\ ticketPid \in [Tickets -> Pids \cup {NoPid}]
    /\ ticketTid \in [Tickets -> Tids \cup {NoTid}]
    /\ ticketUses \in [Tickets -> 0..1]
    /\ prepareState \in [Prepares -> {PrepareReady, PrepareConsumed}]
    /\ transitionState \in [Tids -> {NoTransition, PendingTransition,
                                      AppliedTransition, DroppedTransition}]
    /\ transitionPid \in [Tids -> Pids \cup {NoPid}]
    /\ transitionTid \in [Tids -> Tids \cup {NoTid}]
    /\ transitionTicket \in [Tids -> Tickets \cup {NoTicket}]
    /\ imageState \in [Tids -> {OldImage, HandoffPending, RegistersApplied,
                                 TargetTerminated}]

TicketBindingIsAnExactPidTidPair ==
    \A ticket \in Tickets :
        ticketState[ticket] # TicketUnused =>
            /\ ticketPid[ticket] \in Pids
            /\ ticketTid[ticket] \in Tids
            /\ TargetPid(ticketTid[ticket]) = ticketPid[ticket]

PendingTicketNamesExactlyOneLiveTarget ==
    \A ticket \in Tickets :
        ticketState[ticket] = TicketPending =>
            ExactLiveTarget(ticketPid[ticket], ticketTid[ticket])

TicketIsConsumedAtMostOnce ==
    \A ticket \in Tickets : ticketUses[ticket] \in 0..1

TerminalTicketHasNoPendingAuthority ==
    \A ticket \in Tickets :
        TerminalTicket(ticketState[ticket]) => ticketState[ticket] # TicketPending

TicketCapacityIsBounded == PendingTicketCount <= MaxTickets

PendingTransitionHasExactLiveTarget ==
    \A tid \in Tids :
        transitionState[tid] = PendingTransition =>
            /\ transitionTid[tid] = tid
            /\ ExactLiveTarget(transitionPid[tid], tid)
            /\ transitionTicket[tid] \in Tickets
            /\ ticketState[transitionTicket[tid]] = TicketExecuted
            /\ <<ticketPid[transitionTicket[tid]], ticketTid[transitionTicket[tid]]>>
               = <<transitionPid[tid], tid>>

NewImageNeverLacksItsRegisterHandoff ==
    \A tid \in Tids :
        imageState[tid] \in {HandoffPending, RegistersApplied} =>
            transitionState[tid] \in {PendingTransition, AppliedTransition}

ExitedThreadRetainsNoLiveExecAuthority ==
    \A tid \in Tids :
        threadState[tid] = ThreadExited =>
            /\ \A ticket \in Tickets :
                ticketTid[ticket] = tid => ticketState[ticket] # TicketPending
            /\ transitionState[tid] # PendingTransition

ExecRetiresSiblingThreadAuthority ==
    \A tid \in Tids :
        imageState[tid] \in {HandoffPending, RegistersApplied} =>
            \A sibling \in Tids :
                TargetPid(sibling) = TargetPid(tid) /\ sibling # tid =>
                    /\ threadState[sibling] = ThreadExited
                    /\ imageState[sibling] = TargetTerminated
                    /\ \A ticket \in Tickets :
                        ticketTid[ticket] = sibling =>
                            ticketState[ticket] # TicketPending
                    /\ transitionState[sibling] # PendingTransition

ExitedTargetRetainsNoLiveExecAuthority ==
    \A pid \in Pids :
        processState[pid] = Exited =>
            /\ \A ticket \in Tickets :
                ticketPid[ticket] = pid => ticketState[ticket] # TicketPending
            /\ \A tid \in Tids :
                transitionPid[tid] = pid => transitionState[tid] # PendingTransition

AppliedHandoffIsBoundToTheExecutedTicket ==
    \A tid \in Tids :
        transitionState[tid] = AppliedTransition =>
            /\ imageState[tid] \in {RegistersApplied, TargetTerminated}
            /\ transitionTicket[tid] \in Tickets
            /\ ticketState[transitionTicket[tid]] = TicketExecuted

=============================================================================
