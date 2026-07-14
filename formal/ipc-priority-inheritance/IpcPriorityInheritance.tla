------------------------- MODULE IpcPriorityInheritance -------------------------
EXTENDS Naturals, FiniteSets

(*******************************************************************************
Models the bounded, reply-capability-scoped priority inheritance used by
`kernel/ps/src/multitask/scheduler.rs` and the synchronous IPC transitions in
`kernel/compat/src/user/syscall/linux/ipc_ops.rs`.

The concrete failure this closes is strict System > User scheduling across a
dependency chain such as:

    uiserver (System) -> devmgrd (User) -> sessiond (User)

Without inheritance a ready System poller can defer both servers forever even
though the UI caller is blocked on their reply. A live reply therefore owns a
bounded donation edge from its caller to its receiver. The receiver's
effective class is computed transitively; completion, cancellation, and task
exit revoke the exact edge before the caller resumes.

The model intentionally does not assign a hardcoded service priority or a
time-slice value. It proves the structural contract: only a live reply can
elevate a server, the elevation crosses nested synchronous calls, and a lower
effective class is never chosen while a higher effective class is ready.
*******************************************************************************)

CONSTANTS Tasks, Replies, InteractiveTask, BrokerTasks

NoTask == 0
NoReply == 0

System == "system"
User == "user"

Runnable == "runnable"
Blocked == "blocked"
Exited == "exited"

Free == "free"
Queued == "queued"
Serving == "serving"
Replied == "replied"
Cancelled == "cancelled"

VARIABLES taskState,
          replyState,
          callerOf,
          receiverOf,
          donation,
          lastPick

vars == <<taskState, replyState, callerOf, receiverOf, donation, lastPick>>

LiveReply(reply) == replyState[reply] \in {Queued, Serving}

BaseClass(task) == IF task = InteractiveTask THEN System ELSE User

(*******************************************************************************
Process-owned endpoints do not necessarily have an individual receiver waiter
at enqueue time. `BrokerTasks` represents the live workers of such a process:
a reply bound to any one of its endpoints temporarily promotes the whole owner
process, so a ready worker can reach `IPC_RECV` and claim the request.
*******************************************************************************)
OwnerTasks(receiver) ==
    IF receiver \in BrokerTasks THEN BrokerTasks ELSE {receiver}

RECURSIVE HasSystemDonor(_, _)
HasSystemDonor(task, seen) ==
    IF task \in seen THEN BaseClass(task) = System
    ELSE
        /\ BaseClass(task) = System
        \/ \E reply \in Replies:
              /\ donation[reply]
              /\ LiveReply(reply)
              /\ task \in OwnerTasks(receiverOf[reply])
              /\ HasSystemDonor(callerOf[reply], seen \cup {task})

EffectiveClass(task) ==
    IF HasSystemDonor(task, {}) THEN System ELSE User

SystemReady ==
    \E task \in Tasks:
        /\ taskState[task] = Runnable
        /\ EffectiveClass(task) = System

NoLiveCallFrom(task) ==
    \A reply \in Replies:
        \neg(LiveReply(reply) /\ callerOf[reply] = task)

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

(*******************************************************************************
The sender installs the reply-scoped donation before it wakes or hands off to
the receiver. This is the necessary first-hop rule: waiting for dequeue would
leave a User receiver ineligible whenever some unrelated System task remains
ready.
*******************************************************************************)
StartCall(caller, receiver, reply) ==
    /\ caller \in Tasks
    /\ receiver \in Tasks
    /\ reply \in Replies
    /\ caller # receiver
    /\ taskState[caller] = Runnable
    /\ taskState[receiver] # Exited
    /\ NoLiveCallFrom(caller)
    /\ replyState[reply] = Free
    /\ taskState' = [taskState EXCEPT ![caller] = Blocked]
    /\ replyState' = [replyState EXCEPT ![reply] = Queued]
    /\ callerOf' = [callerOf EXCEPT ![reply] = caller]
    /\ receiverOf' = [receiverOf EXCEPT ![reply] = receiver]
    /\ donation' = [donation EXCEPT ![reply] = TRUE]
    /\ lastPick' = NoTask

DequeueCall(receiver, reply) ==
    /\ receiver \in Tasks
    /\ reply \in Replies
    /\ taskState[receiver] = Runnable
    /\ replyState[reply] = Queued
    /\ receiverOf[reply] = receiver
    /\ replyState' = [replyState EXCEPT ![reply] = Serving]
    /\ UNCHANGED <<taskState, callerOf, receiverOf, donation>>
    /\ lastPick' = NoTask

(*******************************************************************************
A server may synchronously call another server while it still owns an inbound
reply. `EffectiveClass` recursively follows the live donation edges, so this
models UI -> broker -> policy without a special-case service list.
*******************************************************************************)
ForwardCall(caller, receiver, reply) == StartCall(caller, receiver, reply)

CompleteReply(receiver, reply) ==
    /\ receiver \in Tasks
    /\ reply \in Replies
    /\ replyState[reply] = Serving
    /\ receiverOf[reply] = receiver
    /\ taskState[receiver] # Exited
    /\ taskState' = [taskState EXCEPT ![callerOf[reply]] = Runnable]
    /\ replyState' = [replyState EXCEPT ![reply] = Replied]
    /\ donation' = [donation EXCEPT ![reply] = FALSE]
    /\ UNCHANGED <<callerOf, receiverOf>>
    /\ lastPick' = NoTask

CancelReply(caller, reply) ==
    /\ caller \in Tasks
    /\ reply \in Replies
    /\ callerOf[reply] = caller
    /\ LiveReply(reply)
    /\ taskState[caller] = Blocked
    /\ taskState' = [taskState EXCEPT ![caller] = Runnable]
    /\ replyState' = [replyState EXCEPT ![reply] = Cancelled]
    /\ donation' = [donation EXCEPT ![reply] = FALSE]
    /\ UNCHANGED <<callerOf, receiverOf>>
    /\ lastPick' = NoTask

(*******************************************************************************
Task teardown is a second, independent revocation path. It revokes both
outgoing and incoming edges, which prevents a recycled task slot or a failed
endpoint owner from retaining a donated System class.
*******************************************************************************)
ExitTask(task) ==
    /\ task \in Tasks
    /\ taskState[task] # Exited
    /\ taskState' = [taskState EXCEPT ![task] = Exited]
    /\ replyState' = [reply \in Replies |->
          IF LiveReply(reply)
                /\ (callerOf[reply] = task \/ receiverOf[reply] = task)
          THEN Cancelled
          ELSE replyState[reply]]
    /\ donation' = [reply \in Replies |->
          IF callerOf[reply] = task \/ receiverOf[reply] = task
          THEN FALSE
          ELSE donation[reply]]
    /\ UNCHANGED <<callerOf, receiverOf>>
    /\ lastPick' = NoTask

(*******************************************************************************
Strict bands select an effective-System receiver before every effective-User
task. CFS vruntime fairness inside that band is intentionally abstracted; the
invariant captures the boundary that previously admitted inversion.
*******************************************************************************)
Schedule(task) ==
    /\ task \in Tasks
    /\ taskState[task] = Runnable
    /\ ~SystemReady \/ EffectiveClass(task) = System
    /\ lastPick' = task
    /\ UNCHANGED <<taskState, replyState, callerOf, receiverOf, donation>>

Next ==
    \/ \E caller \in Tasks, receiver \in Tasks, reply \in Replies:
          StartCall(caller, receiver, reply)
    \/ \E receiver \in Tasks, reply \in Replies: DequeueCall(receiver, reply)
    \/ \E caller \in Tasks, receiver \in Tasks, reply \in Replies:
          ForwardCall(caller, receiver, reply)
    \/ \E receiver \in Tasks, reply \in Replies: CompleteReply(receiver, reply)
    \/ \E caller \in Tasks, reply \in Replies: CancelReply(caller, reply)
    \/ \E task \in Tasks: ExitTask(task)
    \/ \E task \in Tasks: Schedule(task)

Spec == Init /\ [][Next]_vars

TypeOK ==
    /\ taskState \in [Tasks -> {Runnable, Blocked, Exited}]
    /\ replyState \in [Replies -> {Free, Queued, Serving, Replied, Cancelled}]
    /\ callerOf \in [Replies -> (Tasks \cup {NoTask})]
    /\ receiverOf \in [Replies -> (Tasks \cup {NoTask})]
    /\ donation \in [Replies -> BOOLEAN]
    /\ lastPick \in Tasks \cup {NoTask}

DonationHasLiveReply ==
    \A reply \in Replies:
        donation[reply] =>
            /\ LiveReply(reply)
            /\ callerOf[reply] # NoTask
            /\ receiverOf[reply] # NoTask
            /\ taskState[callerOf[reply]] # Exited
            /\ taskState[receiverOf[reply]] # Exited

TerminalReplyRevokesDonation ==
    \A reply \in Replies:
        replyState[reply] \in {Free, Replied, Cancelled} => ~donation[reply]

OneSynchronousCallPerCaller ==
    \A caller \in Tasks:
        Cardinality({reply \in Replies : LiveReply(reply) /\ callerOf[reply] = caller}) <= 1

TransitiveSystemInheritance ==
    \A reply \in Replies:
        donation[reply] /\ EffectiveClass(callerOf[reply]) = System =>
            \A receiver \in OwnerTasks(receiverOf[reply]):
                EffectiveClass(receiver) = System

BlockedSystemCallerHasSystemReceiver ==
    \A reply \in Replies:
        donation[reply]
          /\ taskState[callerOf[reply]] = Blocked
          /\ EffectiveClass(callerOf[reply]) = System
        => \A receiver \in OwnerTasks(receiverOf[reply]):
               EffectiveClass(receiver) = System

StrictPrioritySelection ==
    lastPick # NoTask /\ SystemReady => EffectiveClass(lastPick) = System

NoPromotionAfterTerminalReply ==
    \A reply \in Replies:
        ~LiveReply(reply) => ~donation[reply]

=============================================================================
