------------------------- MODULE IpcPriorityInheritance -------------------------
EXTENDS Naturals, FiniteSets

CONSTANTS Tasks, Replies, InteractiveTask, BrokerTasks, MaxSystemBurst,
          MaxDonations

NoTask == 0
System == "system"
User == "user"
Runnable == "runnable"
Blocked == "blocked"
Exited == "exited"
Free == "free"
Reserved == "reserved"
Queued == "queued"
Serving == "serving"
Replied == "replied"
Cancelled == "cancelled"

VARIABLES taskState, replyState, callerOf, receiverOf, donation,
          lastPick, systemDispatchStreak

vars == <<taskState, replyState, callerOf, receiverOf, donation,
          lastPick, systemDispatchStreak>>

LiveReply(reply) == replyState[reply] \in {Queued, Serving}
OwnsDonationSlot(reply) == replyState[reply] \in {Reserved, Queued, Serving}
BaseClass(task) == IF task = InteractiveTask THEN System ELSE User

RECURSIVE HasSystemDonor(_, _)
HasSystemDonor(task, seen) ==
    IF task \in seen THEN BaseClass(task) = System
    ELSE
        \/ BaseClass(task) = System
        \/ \E reply \in Replies:
              /\ donation[reply]
              /\ LiveReply(reply)
              /\ receiverOf[reply] = task
              /\ HasSystemDonor(callerOf[reply], seen \cup {task})

EffectiveClass(task) == IF HasSystemDonor(task, {}) THEN System ELSE User

SystemReady ==
    \E task \in Tasks:
        taskState[task] = Runnable /\ EffectiveClass(task) = System

UserReady ==
    \E task \in Tasks:
        taskState[task] = Runnable /\ EffectiveClass(task) = User

UserReservationDue ==
    SystemReady /\ UserReady /\ systemDispatchStreak = MaxSystemBurst

NoLiveCallFrom(task) ==
    \A reply \in Replies:
        \neg(OwnsDonationSlot(reply) /\ callerOf[reply] = task)

DonationSlotsUsed == Cardinality({reply \in Replies : OwnsDonationSlot(reply)})

Init ==
    /\ Tasks # {}
    /\ InteractiveTask \in Tasks
    /\ BrokerTasks \subseteq Tasks
    /\ taskState = [task \in Tasks |-> Runnable]
    /\ replyState = [reply \in Replies |-> Free]
    /\ callerOf = [reply \in Replies |-> NoTask]
    /\ receiverOf = [reply \in Replies |-> NoTask]
    /\ donation = [reply \in Replies |-> FALSE]
    /\ lastPick = NoTask
    /\ systemDispatchStreak = 0

(***************************************************************************
Capacity is reserved while the caller is still runnable. Exhaustion disables
this action, so there is no state in which a System caller is blocked without
a donation slot. Reply publication then binds one exact worker; BrokerTasks is
only the eligible set and never the donation target as a group.
***************************************************************************)
ReserveCall(caller, reply) ==
    /\ caller \in Tasks
    /\ reply \in Replies
    /\ taskState[caller] = Runnable
    /\ NoLiveCallFrom(caller)
    /\ replyState[reply] = Free
    /\ DonationSlotsUsed < MaxDonations
    /\ replyState' = [replyState EXCEPT ![reply] = Reserved]
    /\ callerOf' = [callerOf EXCEPT ![reply] = caller]
    /\ receiverOf' = [receiverOf EXCEPT ![reply] = NoTask]
    /\ donation' = [donation EXCEPT ![reply] = FALSE]
    /\ UNCHANGED taskState
    /\ lastPick' = NoTask
    /\ UNCHANGED systemDispatchStreak

CancelReservation(caller, reply) ==
    /\ replyState[reply] = Reserved
    /\ callerOf[reply] = caller
    /\ replyState' = [replyState EXCEPT ![reply] = Cancelled]
    /\ UNCHANGED <<taskState, callerOf, receiverOf, donation>>
    /\ lastPick' = NoTask
    /\ UNCHANGED systemDispatchStreak

PublishCall(caller, receiver, reply) ==
    /\ caller \in Tasks /\ receiver \in Tasks /\ reply \in Replies
    /\ caller # receiver
    /\ replyState[reply] = Reserved
    /\ callerOf[reply] = caller
    /\ taskState[caller] = Runnable
    /\ taskState[receiver] # Exited
    /\ taskState' = [taskState EXCEPT ![caller] = Blocked]
    /\ replyState' = [replyState EXCEPT ![reply] = Queued]
    /\ receiverOf' = [receiverOf EXCEPT ![reply] = receiver]
    /\ donation' = [donation EXCEPT ![reply] = TRUE]
    /\ UNCHANGED callerOf
    /\ lastPick' = NoTask
    /\ UNCHANGED systemDispatchStreak

(***************************************************************************
An endpoint may have no parked receiver while its single owner is between
reply and the next receive. Publication still transfers the finite reservation
to the exact reply; it does not invent a process-wide target. The caller may
block, and the worker that actually dequeues performs the only target bind.
***************************************************************************)
PublishCallUnbound(caller, reply) ==
    /\ caller \in Tasks /\ reply \in Replies
    /\ replyState[reply] = Reserved
    /\ callerOf[reply] = caller
    /\ taskState[caller] = Runnable
    /\ taskState' = [taskState EXCEPT ![caller] = Blocked]
    /\ replyState' = [replyState EXCEPT ![reply] = Queued]
    /\ receiverOf' = [receiverOf EXCEPT ![reply] = NoTask]
    /\ donation' = [donation EXCEPT ![reply] = FALSE]
    /\ UNCHANGED callerOf
    /\ lastPick' = NoTask
    /\ UNCHANGED systemDispatchStreak

DequeueCall(receiver, reply) ==
    /\ receiver \in Tasks /\ reply \in Replies
    /\ taskState[receiver] = Runnable
    /\ replyState[reply] = Queued
    /\ receiverOf[reply] \in {NoTask, receiver}
    /\ receiver # callerOf[reply]
    /\ replyState' = [replyState EXCEPT ![reply] = Serving]
    /\ receiverOf' = [receiverOf EXCEPT ![reply] = receiver]
    /\ donation' = [donation EXCEPT ![reply] = TRUE]
    /\ UNCHANGED <<taskState, callerOf>>
    /\ lastPick' = NoTask
    /\ UNCHANGED systemDispatchStreak

RebindWorker(oldWorker, newWorker, reply) ==
    /\ replyState[reply] \in {Queued, Serving}
    /\ receiverOf[reply] = oldWorker
    /\ newWorker \in Tasks
    /\ newWorker # callerOf[reply]
    /\ taskState[newWorker] # Exited
    /\ receiverOf' = [receiverOf EXCEPT ![reply] = newWorker]
    /\ lastPick' = NoTask
    /\ UNCHANGED <<taskState, replyState, callerOf, donation,
                    systemDispatchStreak>>

CompleteReply(receiver, reply) ==
    /\ replyState[reply] = Serving
    /\ receiverOf[reply] = receiver
    /\ taskState[receiver] # Exited
    /\ taskState' = [taskState EXCEPT ![callerOf[reply]] = Runnable]
    /\ replyState' = [replyState EXCEPT ![reply] = Replied]
    /\ donation' = [donation EXCEPT ![reply] = FALSE]
    /\ UNCHANGED <<callerOf, receiverOf>>
    /\ lastPick' = NoTask
    /\ UNCHANGED systemDispatchStreak

CancelReply(caller, reply) ==
    /\ callerOf[reply] = caller
    /\ LiveReply(reply)
    /\ taskState[caller] = Blocked
    /\ taskState' = [taskState EXCEPT ![caller] = Runnable]
    /\ replyState' = [replyState EXCEPT ![reply] = Cancelled]
    /\ donation' = [donation EXCEPT ![reply] = FALSE]
    /\ UNCHANGED <<callerOf, receiverOf>>
    /\ lastPick' = NoTask
    /\ UNCHANGED systemDispatchStreak

ExitTask(task) ==
    /\ task \in Tasks /\ taskState[task] # Exited
    /\ taskState' = [taskState EXCEPT ![task] = Exited]
    /\ replyState' = [reply \in Replies |->
          IF OwnsDonationSlot(reply)
                /\ (callerOf[reply] = task \/ receiverOf[reply] = task)
          THEN Cancelled ELSE replyState[reply]]
    /\ donation' = [reply \in Replies |->
          IF callerOf[reply] = task \/ receiverOf[reply] = task
          THEN FALSE ELSE donation[reply]]
    /\ UNCHANGED <<callerOf, receiverOf>>
    /\ lastPick' = NoTask
    /\ UNCHANGED systemDispatchStreak

Schedule(task) ==
    /\ task \in Tasks /\ taskState[task] = Runnable
    /\ IF UserReservationDue
          THEN EffectiveClass(task) = User
          ELSE ~SystemReady \/ EffectiveClass(task) = System
    /\ lastPick' = task
    /\ UNCHANGED <<taskState, replyState, callerOf, receiverOf, donation>>
    /\ systemDispatchStreak' =
          IF EffectiveClass(task) = System
          THEN IF systemDispatchStreak < MaxSystemBurst
               THEN systemDispatchStreak + 1 ELSE MaxSystemBurst
          ELSE 0

Next ==
    \/ \E caller \in Tasks, reply \in Replies: ReserveCall(caller, reply)
    \/ \E caller \in Tasks, reply \in Replies: CancelReservation(caller, reply)
    \/ \E caller, receiver \in Tasks, reply \in Replies:
          PublishCall(caller, receiver, reply)
    \/ \E caller \in Tasks, reply \in Replies: PublishCallUnbound(caller, reply)
    \/ \E receiver \in Tasks, reply \in Replies: DequeueCall(receiver, reply)
    \/ \E oldWorker, newWorker \in Tasks, reply \in Replies:
          RebindWorker(oldWorker, newWorker, reply)
    \/ \E receiver \in Tasks, reply \in Replies: CompleteReply(receiver, reply)
    \/ \E caller \in Tasks, reply \in Replies: CancelReply(caller, reply)
    \/ \E task \in Tasks: ExitTask(task)
    \/ \E task \in Tasks: Schedule(task)

Spec == Init /\ [][Next]_vars

TypeOK ==
    /\ taskState \in [Tasks -> {Runnable, Blocked, Exited}]
    /\ replyState \in [Replies -> {Free, Reserved, Queued, Serving, Replied, Cancelled}]
    /\ callerOf \in [Replies -> (Tasks \cup {NoTask})]
    /\ receiverOf \in [Replies -> (Tasks \cup {NoTask})]
    /\ donation \in [Replies -> BOOLEAN]
    /\ lastPick \in Tasks \cup {NoTask}
    /\ systemDispatchStreak \in 0..MaxSystemBurst
    /\ DonationSlotsUsed <= MaxDonations

ReservedCallerIsNotBlocked ==
    \A reply \in Replies:
        replyState[reply] = Reserved => taskState[callerOf[reply]] = Runnable

DonationHasExactLiveReply ==
    \A reply \in Replies:
        donation[reply] =>
            /\ LiveReply(reply)
            /\ callerOf[reply] # NoTask
            /\ receiverOf[reply] \in Tasks
            /\ taskState[callerOf[reply]] # Exited
            /\ taskState[receiverOf[reply]] # Exited

TerminalReplyRevokesDonation ==
    \A reply \in Replies:
        replyState[reply] \in {Free, Reserved, Replied, Cancelled} => ~donation[reply]

OneSynchronousCallPerCaller ==
    \A caller \in Tasks:
        Cardinality({reply \in Replies : OwnsDonationSlot(reply)
                                          /\ callerOf[reply] = caller}) <= 1

BlockedSystemCallerHasExactSystemReceiver ==
    \A reply \in Replies:
        donation[reply]
          /\ taskState[callerOf[reply]] = Blocked
          /\ EffectiveClass(callerOf[reply]) = System
        => /\ receiverOf[reply] \in Tasks
           /\ EffectiveClass(receiverOf[reply]) = System

UnselectedBrokerIsNotPromoted ==
    \A reply \in Replies:
        donation[reply] /\ receiverOf[reply] \in BrokerTasks =>
            \A worker \in BrokerTasks \ {receiverOf[reply]}:
                (~\E other \in Replies:
                    donation[other] /\ receiverOf[other] = worker)
                => EffectiveClass(worker) = BaseClass(worker)

BoundedSystemBurst == systemDispatchStreak <= MaxSystemBurst

UserPickRequiresReservation ==
    lastPick # NoTask /\ SystemReady /\ EffectiveClass(lastPick) = User =>
        UserReady /\ systemDispatchStreak = 0

=============================================================================
