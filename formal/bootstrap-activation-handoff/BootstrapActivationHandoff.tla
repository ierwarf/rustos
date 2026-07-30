---------------- MODULE BootstrapActivationHandoff ----------------
EXTENDS Naturals, FiniteSets, Sequences

(***************************************************************************
Models the scheduler's first-turn handoff for supervisor-committed children.

Every exact activation is retained in one allocation-free FIFO. Later
activations and unrelated IPC hints cannot overwrite older children. A task
that retires before its first turn is removed without disturbing the order of
the remaining queue. An absolute ready-age dispatch may interrupt the handoff
sequence, but it cannot consume or reorder pending activation authority.

Concrete owner:
  * kernel/ps/src/multitask/scheduler.rs
***************************************************************************)

CONSTANT Tasks

Absent == "absent"
Pending == "pending"
Dispatched == "dispatched"
Retired == "retired"

VARIABLES status, queue, activationRank, nextRank,
          lastDispatchedRank, overdue, ordinaryTurnObserved

vars ==
    <<status, queue, activationRank, nextRank,
      lastDispatchedRank, overdue, ordinaryTurnObserved>>

QueueSet == {queue[index] : index \in 1..Len(queue)}

Init ==
    /\ status = [task \in Tasks |-> Absent]
    /\ queue = <<>>
    /\ activationRank = [task \in Tasks |-> 0]
    /\ nextRank = 1
    /\ lastDispatchedRank = 0
    /\ overdue = FALSE
    /\ ordinaryTurnObserved = FALSE

Activate(task) ==
    /\ status[task] = Absent
    /\ Len(queue) < Cardinality(Tasks)
    /\ status' = [status EXCEPT ![task] = Pending]
    /\ queue' = Append(queue, task)
    /\ activationRank' = [activationRank EXCEPT ![task] = nextRank]
    /\ nextRank' = nextRank + 1
    /\ UNCHANGED <<lastDispatchedRank, overdue, ordinaryTurnObserved>>

IgnoreDuplicateActivation(task) ==
    /\ status[task] = Pending
    /\ UNCHANGED vars

RetirePending(task) ==
    /\ status[task] = Pending
    /\ status' = [status EXCEPT ![task] = Retired]
    /\ queue' = SelectSeq(queue, LAMBDA candidate: candidate # task)
    /\ UNCHANGED
        <<activationRank, nextRank, lastDispatchedRank,
          overdue, ordinaryTurnObserved>>

PublishOverdueTurn ==
    /\ ~overdue
    /\ overdue' = TRUE
    /\ UNCHANGED
        <<status, queue, activationRank, nextRank,
          lastDispatchedRank, ordinaryTurnObserved>>

DispatchOverdueTurn ==
    /\ overdue
    /\ overdue' = FALSE
    /\ ordinaryTurnObserved' = TRUE
    /\ UNCHANGED
        <<status, queue, activationRank, nextRank,
          lastDispatchedRank>>

DispatchActivationHead ==
    /\ ~overdue
    /\ Len(queue) > 0
    /\ LET task == Head(queue) IN
        /\ status' = [status EXCEPT ![task] = Dispatched]
        /\ queue' = Tail(queue)
        /\ lastDispatchedRank' = activationRank[task]
    /\ UNCHANGED
        <<activationRank, nextRank, overdue,
          ordinaryTurnObserved>>

TerminalStutter ==
    /\ \A task \in Tasks: status[task] # Pending
    /\ UNCHANGED vars

Next ==
    \/ \E task \in Tasks:
        Activate(task)
        \/ IgnoreDuplicateActivation(task)
        \/ RetirePending(task)
    \/ PublishOverdueTurn
    \/ DispatchOverdueTurn
    \/ DispatchActivationHead
    \/ TerminalStutter

Spec == Init /\ [][Next]_vars

TypeOK ==
    /\ status \in [Tasks -> {Absent, Pending, Dispatched, Retired}]
    /\ queue \in Seq(Tasks)
    /\ activationRank \in [Tasks -> 0..Cardinality(Tasks)]
    /\ nextRank \in 1..(Cardinality(Tasks) + 1)
    /\ lastDispatchedRank \in 0..Cardinality(Tasks)
    /\ overdue \in BOOLEAN
    /\ ordinaryTurnObserved \in BOOLEAN

QueueIsBounded == Len(queue) <= Cardinality(Tasks)

QueueHasNoDuplicates ==
    \A left, right \in 1..Len(queue):
        left # right => queue[left] # queue[right]

QueueMatchesPendingAuthority ==
    \A task \in Tasks: (task \in QueueSet) <=> (status[task] = Pending)

QueuePreservesActivationOrder ==
    \A left, right \in 1..Len(queue):
        left < right =>
            activationRank[queue[left]] < activationRank[queue[right]]

DispatchRequiresActivation ==
    \A task \in Tasks: status[task] = Dispatched => activationRank[task] > 0

RetirementRequiresActivation ==
    \A task \in Tasks: status[task] = Retired => activationRank[task] > 0

=============================================================================
