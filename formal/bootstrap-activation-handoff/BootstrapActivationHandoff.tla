---------------- MODULE BootstrapActivationHandoff ----------------
EXTENDS Naturals, FiniteSets, Sequences

(***************************************************************************
Models the scheduler's first-turn handoff for supervisor-committed children.

Every exact activation is retained in one allocation-free FIFO. Later
activations and unrelated IPC hints cannot overwrite older children. A task
that retires before its first turn is removed without disturbing the order of
the remaining queue. An absolute ready-age dispatch may interrupt the handoff
sequence. An unrelated overdue turn cannot consume pending authority; when
another qualified priority arm actually claims THAT pending task, its exact
first-turn promise is satisfied. Other entries retain their original FIFO order.

Concrete owner:
  * scheduler/activation/publication.rs and ordering_publication.rs
  * scheduler/dispatcher.rs (hint consumption is NOT an execution grant)
***************************************************************************)

CONSTANT Tasks

Absent == "absent"
Pending == "pending"
Dispatched == "dispatched"
Retired == "retired"

VARIABLES status, queue, activationRank, nextRank,
          lastDispatchedRank, overdue, ordinaryTurnObserved

baseVars ==
    <<status, queue, activationRank, nextRank,
      lastDispatchedRank, overdue, ordinaryTurnObserved>>

QueueSet == {queue[index] : index \in 1..Len(queue)}

BaseInit ==
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
    /\ UNCHANGED baseVars

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

\* Abstract one retained successful claim plus its bounded Policy completion.
\* Tasks name exact incarnations; scalar Running observations are not this action.
DispatchPendingOverdueTurn(task) ==
    /\ overdue
    /\ status[task] = Pending
    /\ status' = [status EXCEPT ![task] = Dispatched]
    /\ queue' = SelectSeq(queue, LAMBDA candidate: candidate # task)
    /\ overdue' = FALSE
    /\ ordinaryTurnObserved' = TRUE
    /\ UNCHANGED <<activationRank, nextRank, lastDispatchedRank>>

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
    /\ UNCHANGED baseVars

BaseNext ==
    \/ \E task \in Tasks:
        Activate(task)
        \/ IgnoreDuplicateActivation(task)
        \/ RetirePending(task)
    \/ PublishOverdueTurn
    \/ DispatchOverdueTurn
    \/ \E task \in Tasks: DispatchPendingOverdueTurn(task)
    \/ DispatchActivationHead
    \/ TerminalStutter



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


(***************************************************************************
Non-authoritative ordering projection. Capture means the ORIGINAL BindingPin,
not a scalar generation lookup. It spans qualification and final publication.
Reuse may occur after drop, so the consumer must match exact UID again. Native
born-resource witnesses refine these steps; this does NOT model queue claim,
Current/Transition, MM/RSP, interval settlement or full joint hardware return.
***************************************************************************)
VARIABLES targetUid, capturedUid, hintUid, hintPin, orderingPhase,
          badHintPublish, badHintConsume
hintVars == <<targetUid, capturedUid, hintUid, hintPin, orderingPhase,
              badHintPublish, badHintConsume>>
vars == <<baseVars, hintVars>>
Init == /\ BaseInit
        /\ targetUid = 1 /\ capturedUid = 0 /\ hintUid = 0
        /\ hintPin = FALSE /\ orderingPhase = "Absent"
        /\ badHintPublish = FALSE /\ badHintConsume = FALSE
CaptureOrdering == /\ orderingPhase = "Absent"
                   /\ capturedUid' = targetUid /\ hintPin' = TRUE
                   /\ orderingPhase' = "Captured"
                   /\ UNCHANGED <<baseVars,targetUid,hintUid,badHintPublish,badHintConsume>>
PublishOrdering == /\ orderingPhase = "Captured"
                   /\ hintUid' = capturedUid /\ orderingPhase' = "Published"
                   /\ badHintPublish' = (~hintPin \/ capturedUid # targetUid)
                   /\ UNCHANGED <<baseVars,targetUid,capturedUid,hintPin,badHintConsume>>
DropOrdering == /\ orderingPhase = "Published"
                /\ hintPin' = FALSE /\ orderingPhase' = "Finished"
                /\ UNCHANGED <<baseVars,targetUid,capturedUid,hintUid,badHintPublish,badHintConsume>>
ReuseOrderingTarget == /\ ~hintPin /\ targetUid = 1
                       /\ targetUid' = 2
                       /\ UNCHANGED <<baseVars,capturedUid,hintUid,hintPin,orderingPhase,badHintPublish,badHintConsume>>
ConsumeOrdering == /\ hintUid # 0 /\ hintUid = targetUid
                   /\ badHintConsume' = (badHintConsume \/ hintUid # targetUid)
                   /\ hintUid' = 0
                   /\ UNCHANGED <<baseVars,targetUid,capturedUid,hintPin,orderingPhase,badHintPublish>>
DiscardStaleOrdering == /\ hintUid # 0 /\ hintUid # targetUid
                        /\ hintUid' = 0
                        /\ UNCHANGED <<baseVars,targetUid,capturedUid,hintPin,orderingPhase,badHintPublish,badHintConsume>>
Next == \/ (BaseNext /\ UNCHANGED hintVars)
        \/ CaptureOrdering \/ PublishOrdering \/ DropOrdering
        \/ ReuseOrderingTarget \/ ConsumeOrdering \/ DiscardStaleOrdering
Spec == Init /\ [][Next]_vars
HintTypeOK == /\ targetUid \in {1,2} /\ capturedUid \in {0,1,2} /\ hintUid \in {0,1,2}
              /\ hintPin \in BOOLEAN /\ badHintPublish \in BOOLEAN /\ badHintConsume \in BOOLEAN
              /\ orderingPhase \in {"Absent","Captured","Published","Finished"}
OriginalOrderingTarget == hintPin => capturedUid = targetUid
HintPublishRequiresOriginal == ~badHintPublish
HintConsumeExactUID == ~badHintConsume

=============================================================================
