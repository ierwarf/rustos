----------------------- MODULE SynchronousIpcHandoff -----------------------
EXTENDS Naturals, FiniteSets, Sequences

(*******************************************************************************
Models scheduler custody for both halves of synchronous IPC execution transfer.

Concrete owners:
  * kernel/compat/src/user/syscall/linux/ipc_ops.rs publishes the exact
    receiver after call enqueue and the exact caller after successful reply.
  * kernel/ps/src/multitask/scheduler/handoff_queue.rs retains every distinct
    task in a fixed MAX_TASK FIFO.
  * kernel/ps/src/multitask/scheduler.rs dispatches the FIFO before unrelated
    overdue work, but forces a fairness turn after MaxHandoffBurst handoffs.

A synchronous call/reply transfer is different from a speculative wake hint:
the receiver is required by a newly live reply capability, and the caller was
blocked until that capability completed. Concurrent transfers therefore
cannot overwrite one another, while a call chain still cannot suppress global
fairness without bound.
*******************************************************************************)

CONSTANTS Tasks, Kinds, MaxHandoffBurst

Blocked == "blocked"
Runnable == "runnable"
Retired == "retired"

NoKind == "none"

VARIABLES taskState, published, kindOf, dispatched, queue, handoffBurst, fairnessTurns

vars == <<taskState, published, kindOf, dispatched, queue, handoffBurst, fairnessTurns>>

QueueSet == {queue[index] : index \in 1..Len(queue)}

Init ==
    /\ Tasks # {}
    /\ Kinds # {}
    /\ MaxHandoffBurst > 0
    /\ taskState = [task \in Tasks |-> Blocked]
    /\ published = {}
    /\ kindOf = [task \in Tasks |-> NoKind]
    /\ dispatched = {}
    /\ queue = <<>>
    /\ handoffBurst = 0
    /\ fairnessTurns = 0

PublishHandoff(task, kind) ==
    /\ task \in Tasks
    /\ kind \in Kinds
    /\ taskState[task] # Retired
    /\ task \notin published
    /\ taskState' = [taskState EXCEPT ![task] = Runnable]
    /\ published' = published \cup {task}
    /\ kindOf' = [kindOf EXCEPT ![task] = kind]
    /\ queue' = Append(queue, task)
    /\ UNCHANGED <<dispatched, handoffBurst, fairnessTurns>>

DispatchHandoff ==
    /\ Len(queue) > 0
    /\ handoffBurst < MaxHandoffBurst
    /\ taskState[Head(queue)] = Runnable
    /\ dispatched' = dispatched \cup {Head(queue)}
    /\ queue' = Tail(queue)
    /\ handoffBurst' = handoffBurst + 1
    /\ UNCHANGED <<taskState, published, kindOf, fairnessTurns>>

FairnessTurn ==
    /\ Len(queue) > 0
    /\ handoffBurst = MaxHandoffBurst
    /\ handoffBurst' = 0
    /\ fairnessTurns' = fairnessTurns + 1
    /\ UNCHANGED <<taskState, published, kindOf, dispatched, queue>>

Retire(task) ==
    /\ task \in Tasks
    /\ taskState[task] # Retired
    /\ taskState' = [taskState EXCEPT ![task] = Retired]
    /\ queue' = SelectSeq(queue, LAMBDA queued: queued # task)
    /\ UNCHANGED <<published, kindOf, dispatched, handoffBurst, fairnessTurns>>

TerminalStutter ==
    /\ Len(queue) = 0
    /\ \A task \in Tasks : taskState[task] = Retired \/ task \in dispatched
    /\ UNCHANGED vars

Next ==
    \/ \E task \in Tasks, kind \in Kinds : PublishHandoff(task, kind)
    \/ DispatchHandoff
    \/ FairnessTurn
    \/ \E task \in Tasks : Retire(task)
    \/ TerminalStutter

Spec == Init /\ [][Next]_vars

TypeOK ==
    /\ taskState \in [Tasks -> {Blocked, Runnable, Retired}]
    /\ published \subseteq Tasks
    /\ kindOf \in [Tasks -> (Kinds \cup {NoKind})]
    /\ dispatched \subseteq Tasks
    /\ queue \in Seq(Tasks)
    /\ handoffBurst \in 0..MaxHandoffBurst
    /\ fairnessTurns \in Nat

QueueBounded == Len(queue) <= Cardinality(Tasks)

QueueHasNoDuplicates == Len(queue) = Cardinality(QueueSet)

PendingHandoffCustody ==
    published \ (dispatched \cup {task \in Tasks : taskState[task] = Retired}) = QueueSet

DispatchRequiresPublication == dispatched \subseteq published

PublishedHandoffHasKind ==
    \A task \in Tasks : (task \in published) = (kindOf[task] \in Kinds)

QueueContainsOnlyRunnableCallers ==
    \A task \in QueueSet : taskState[task] = Runnable

HandoffBurstIsBounded == handoffBurst <= MaxHandoffBurst

=============================================================================
