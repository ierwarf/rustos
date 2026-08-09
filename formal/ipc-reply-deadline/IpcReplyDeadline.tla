----------------------------- MODULE IpcReplyDeadline ----------------------------
EXTENDS Naturals, FiniteSets

(*******************************************************************************
Models a deadline-bounded IPC call after its endpoint capability has already
been validated.

Concrete owners:
  * kernel/ipc-runtime/src/ipc/mod.rs
  * kernel/compat/src/user/syscall/linux/ipc_ops.rs

`wait_for_reply_with_deadline` first polls, arms the scheduler block state and
reply timer, then polls again before it commits the block.  A reply, endpoint
owner exit, or deadline expiry wakes the exact caller and disarms that wait.
This model intentionally permits two policy services to call one another: a
global acyclic IPC graph is not a RustOS ABI contract.  The contract is that
every kernel-owned control wait has a finite deadline, and a cycle therefore
cannot turn into a permanent blocked state.

Linearization points: enqueue inserts the request/reply identity; reply
completion marks the one-shot reply used; owner exit atomically converts live
calls to PeerClosed; deadline advancement atomically cancels due calls before
the scheduler may select more work.
*******************************************************************************)

CONSTANTS Tasks, Replies, MaxTick

NoTask == 0
NoReply == 0
NoTimer == 0
DeadlineTicks == 2

Runnable == "runnable"
Armed == "armed"
Blocked == "blocked"
Exited == "exited"

Free == "free"
Queued == "queued"
Dequeued == "dequeued"
Replied == "replied"
Taking == "taking"
Cancelled == "cancelled"
PeerClosed == "peer-closed"
Consumed == "consumed"
Abandoned == "abandoned"

NoPublication == "none"
PendingPublication == "pending"
VisiblePublication == "visible"
DroppedPublication == "dropped"

VARIABLES now,
          taskState,
          waitFor,
          timerDeadline,
          replyState,
          callerOf,
          serverOf,
          replyDeadline,
          replyUsed,
          publicationState,
          observedOutcome,
          lastOutcomeReply

vars == <<now, taskState, waitFor, timerDeadline, replyState, callerOf,
          serverOf, replyDeadline, replyUsed, publicationState, observedOutcome,
          lastOutcomeReply>>

NoOutcome == "none"
ReplyReceived == "reply-received"
TimedOut == "timed-out"
PeerClosedOutcome == "peer-closed"

LiveReply(r) == replyState[r] \in {Queued, Dequeued}
OutstandingReply(r) == replyState[r] \in {Queued, Dequeued, Replied, Taking}

WaitedReply(t) == waitFor[t]
WaitingServer(t) ==
    IF WaitedReply(t) = NoReply THEN NoTask ELSE serverOf[WaitedReply(t)]

LiveCallsTo(t) ==
    {r \in Replies : LiveReply(r) /\ serverOf[r] = t}

CallerOutstandingCalls(t) ==
    {r \in Replies : OutstandingReply(r) /\ callerOf[r] = t}

Init ==
    /\ now = 0
    /\ taskState = [t \in Tasks |-> Runnable]
    /\ waitFor = [t \in Tasks |-> NoReply]
    /\ timerDeadline = [t \in Tasks |-> NoTimer]
    /\ replyState = [r \in Replies |-> Free]
    /\ callerOf = [r \in Replies |-> NoTask]
    /\ serverOf = [r \in Replies |-> NoTask]
    /\ replyDeadline = [r \in Replies |-> NoTimer]
    /\ replyUsed = [r \in Replies |-> FALSE]
    /\ publicationState = [r \in Replies |-> NoPublication]
    /\ observedOutcome = [t \in Tasks |-> NoOutcome]
    /\ lastOutcomeReply = [t \in Tasks |-> NoReply]

(*******************************************************************************
The reply identity is allocated together with the endpoint message.  A task is
allowed one in-flight synchronous control call, exactly as the syscall wait
path carries one KernelReplyHandle at a time.
*******************************************************************************)
EnqueueCall(caller, server, reply) ==
    /\ caller \in Tasks
    /\ server \in Tasks
    /\ reply \in Replies
    /\ caller # server
    /\ now <= MaxTick - DeadlineTicks
    /\ taskState[caller] = Runnable
    /\ taskState[server] # Exited
    /\ waitFor[caller] = NoReply
    /\ replyState[reply] = Free
    /\ waitFor' = [waitFor EXCEPT ![caller] = reply]
    /\ replyState' = [replyState EXCEPT ![reply] = Queued]
    /\ callerOf' = [callerOf EXCEPT ![reply] = caller]
    /\ serverOf' = [serverOf EXCEPT ![reply] = server]
    /\ replyDeadline' = [replyDeadline EXCEPT ![reply] = now + DeadlineTicks]
    /\ observedOutcome' = [observedOutcome EXCEPT ![caller] = NoOutcome]
    /\ lastOutcomeReply' = [lastOutcomeReply EXCEPT ![caller] = NoReply]
    /\ UNCHANGED <<now, taskState, timerDeadline, replyUsed,
                  publicationState>>

DequeueCall(server, reply) ==
    /\ server \in Tasks
    /\ reply \in Replies
    /\ taskState[server] = Runnable
    /\ replyState[reply] = Queued
    /\ serverOf[reply] = server
    /\ replyState' = [replyState EXCEPT ![reply] = Dequeued]
    /\ UNCHANGED <<now, taskState, waitFor, timerDeadline, callerOf, serverOf,
                  replyDeadline, replyUsed, publicationState, observedOutcome,
                  lastOutcomeReply>>

(*******************************************************************************
An optional handle publication is prepared only after the server owns the
exact dequeued reply.  It remains invisible while the reply is live.  The
concrete kernel stores the opaque transfer descriptor on that endpoint
message before returning from the prepare broker, so cancellation and server
exit cannot miss the resource.
*******************************************************************************)
PreparePublication(server, reply) ==
    /\ server \in Tasks
    /\ reply \in Replies
    /\ taskState[server] = Runnable
    /\ replyState[reply] = Dequeued
    /\ serverOf[reply] = server
    /\ publicationState[reply] = NoPublication
    /\ publicationState' =
        [publicationState EXCEPT ![reply] = PendingPublication]
    /\ UNCHANGED <<now, taskState, waitFor, timerDeadline, replyState,
                  callerOf, serverOf, replyDeadline, replyUsed,
                  observedOutcome, lastOutcomeReply>>

(*******************************************************************************
This is the response write plus the caller wake.  The concrete reply object is
one-shot (`used`); its caller identity is recovered from the endpoint message,
not supplied by the responder.
*******************************************************************************)
CompleteReply(server, reply) ==
    /\ server \in Tasks
    /\ reply \in Replies
    /\ taskState[server] = Runnable
    /\ replyState[reply] = Dequeued
    /\ serverOf[reply] = server
    /\ taskState[callerOf[reply]] # Exited
    /\ replyState' = [replyState EXCEPT ![reply] = Replied]
    /\ replyUsed' = [replyUsed EXCEPT ![reply] = TRUE]
    /\ taskState' = [taskState EXCEPT ![callerOf[reply]] = Runnable]
    /\ timerDeadline' = [timerDeadline EXCEPT ![callerOf[reply]] = NoTimer]
    /\ UNCHANGED <<now, waitFor, callerOf, serverOf, replyDeadline,
                  publicationState, observedOutcome, lastOutcomeReply>>

(*******************************************************************************
The first poll handles a reply that arrived before arm; the second poll is
represented by the fact that CompleteReply clears Armed before CommitBlock can
fire.  CommitBlock is therefore enabled only for a still-live request with its
matching timer armed.
*******************************************************************************)
ArmBlock(caller) ==
    /\ caller \in Tasks
    /\ taskState[caller] = Runnable
    /\ waitFor[caller] # NoReply
    /\ LiveReply(waitFor[caller])
    /\ timerDeadline[caller] = NoTimer
    /\ taskState' = [taskState EXCEPT ![caller] = Armed]
    /\ UNCHANGED <<now, waitFor, timerDeadline, replyState, callerOf, serverOf,
                  replyDeadline, replyUsed, publicationState, observedOutcome,
                  lastOutcomeReply>>

ArmDeadlineTimer(caller) ==
    /\ caller \in Tasks
    /\ taskState[caller] = Armed
    /\ waitFor[caller] # NoReply
    /\ LiveReply(waitFor[caller])
    /\ timerDeadline[caller] = NoTimer
    /\ replyDeadline[waitFor[caller]] > now
    /\ timerDeadline' =
        [timerDeadline EXCEPT ![caller] = replyDeadline[waitFor[caller]]]
    /\ UNCHANGED <<now, taskState, waitFor, replyState, callerOf, serverOf,
                  replyDeadline, replyUsed, publicationState, observedOutcome,
                  lastOutcomeReply>>

CommitBlock(caller) ==
    /\ caller \in Tasks
    /\ taskState[caller] = Armed
    /\ waitFor[caller] # NoReply
    /\ LiveReply(waitFor[caller])
    /\ timerDeadline[caller] = replyDeadline[waitFor[caller]]
    /\ timerDeadline[caller] > now
    /\ taskState' = [taskState EXCEPT ![caller] = Blocked]
    /\ UNCHANGED <<now, waitFor, timerDeadline, replyState, callerOf, serverOf,
                  replyDeadline, replyUsed, publicationState, observedOutcome,
                  lastOutcomeReply>>

(*******************************************************************************
The concrete caller samples time on both sides of the destructive response
take.  Splitting that operation exposes the only relevant race: a reply and
its pending publication may be removed from the queue, but expiry can still
win before the resource becomes visible in the caller's handle table.
*******************************************************************************)
BeginTakeCompletedReply(caller, reply) ==
    /\ caller \in Tasks
    /\ reply \in Replies
    /\ taskState[caller] = Runnable
    /\ waitFor[caller] = reply
    /\ callerOf[reply] = caller
    /\ replyState[reply] = Replied
    /\ now < replyDeadline[reply]
    /\ replyState' = [replyState EXCEPT ![reply] = Taking]
    /\ UNCHANGED <<now, taskState, waitFor, timerDeadline, callerOf, serverOf,
                  replyDeadline, replyUsed, publicationState,
                  observedOutcome, lastOutcomeReply>>

FinishTakeBeforeDeadline(caller, reply) ==
    /\ caller \in Tasks
    /\ reply \in Replies
    /\ taskState[caller] = Runnable
    /\ waitFor[caller] = reply
    /\ callerOf[reply] = caller
    /\ replyState[reply] = Taking
    /\ now < replyDeadline[reply]
    /\ waitFor' = [waitFor EXCEPT ![caller] = NoReply]
    /\ replyState' = [replyState EXCEPT ![reply] = Consumed]
    /\ publicationState' =
        [publicationState EXCEPT
            ![reply] = IF @ = PendingPublication
                      THEN VisiblePublication
                      ELSE @]
    /\ observedOutcome' = [observedOutcome EXCEPT ![caller] = ReplyReceived]
    /\ lastOutcomeReply' = [lastOutcomeReply EXCEPT ![caller] = reply]
    /\ UNCHANGED <<now, taskState, timerDeadline, callerOf, serverOf,
                  replyDeadline, replyUsed>>

(*******************************************************************************
Process exit must not leave an endpoint call pointing at a dead owner.  Calls
to the exiting endpoint obtain PeerClosed; a reply held by the exiting caller
is discarded, like its task context.  Both cases erase any runnable/block
authority tied to the old reply identity.
*******************************************************************************)
ExitTask(task) ==
    LET ClosedCalls == LiveCallsTo(task) IN
    LET AbandonedCalls == CallerOutstandingCalls(task) IN
    /\ task \in Tasks
    /\ taskState[task] # Exited
    /\ taskState' =
        [t \in Tasks |->
            IF t = task THEN Exited
            ELSE IF waitFor[t] \in ClosedCalls THEN Runnable
            ELSE taskState[t]]
    /\ waitFor' =
        [t \in Tasks |->
            IF t = task \/ waitFor[t] \in ClosedCalls THEN NoReply
            ELSE waitFor[t]]
    /\ timerDeadline' =
        [t \in Tasks |->
            IF t = task \/ waitFor[t] \in ClosedCalls THEN NoTimer
            ELSE timerDeadline[t]]
    /\ replyState' =
        [r \in Replies |->
            IF r \in ClosedCalls THEN PeerClosed
            ELSE IF r \in AbandonedCalls THEN Abandoned
            ELSE replyState[r]]
    /\ replyUsed' =
        [r \in Replies |->
            IF r \in ClosedCalls THEN TRUE ELSE replyUsed[r]]
    /\ publicationState' =
        [r \in Replies |->
            IF r \in (ClosedCalls \cup AbandonedCalls)
               /\ publicationState[r] = PendingPublication
            THEN DroppedPublication
            ELSE publicationState[r]]
    /\ observedOutcome' =
        [t \in Tasks |->
            IF t # task /\ waitFor[t] \in ClosedCalls THEN PeerClosedOutcome
            ELSE observedOutcome[t]]
    /\ lastOutcomeReply' =
        [t \in Tasks |->
            IF t # task /\ waitFor[t] \in ClosedCalls THEN waitFor[t]
            ELSE lastOutcomeReply[t]]
    /\ UNCHANGED <<now, callerOf, serverOf, replyDeadline>>

(*******************************************************************************
The timer interrupt performs cancellation before another scheduling decision.
It processes every due reply in the same transition, so a two-service cycle
has no state in which a control waiter remains blocked at or past its deadline.
*******************************************************************************)
Tick ==
    LET DueCalls ==
        {r \in Replies : OutstandingReply(r) /\ replyDeadline[r] = now + 1} IN
    /\ now < MaxTick
    /\ now' = now + 1
    /\ replyState' =
        [r \in Replies |-> IF r \in DueCalls THEN Cancelled ELSE replyState[r]]
    /\ taskState' =
        [t \in Tasks |->
            IF waitFor[t] \in DueCalls THEN Runnable ELSE taskState[t]]
    /\ waitFor' =
        [t \in Tasks |->
            IF waitFor[t] \in DueCalls THEN NoReply ELSE waitFor[t]]
    /\ timerDeadline' =
        [t \in Tasks |->
            IF waitFor[t] \in DueCalls THEN NoTimer ELSE timerDeadline[t]]
    /\ observedOutcome' =
        [t \in Tasks |->
            IF \E r \in DueCalls : callerOf[r] = t THEN TimedOut
            ELSE observedOutcome[t]]
    /\ lastOutcomeReply' =
        [t \in Tasks |->
            IF \E r \in DueCalls : callerOf[r] = t
            THEN CHOOSE r \in DueCalls : callerOf[r] = t
            ELSE lastOutcomeReply[t]]
    /\ publicationState' =
        [r \in Replies |->
            IF r \in DueCalls /\ publicationState[r] = PendingPublication
            THEN DroppedPublication
            ELSE publicationState[r]]
    /\ UNCHANGED <<callerOf, serverOf, replyDeadline, replyUsed>>

Next ==
    \/ \E caller \in Tasks, server \in Tasks, reply \in Replies :
        EnqueueCall(caller, server, reply)
    \/ \E server \in Tasks, reply \in Replies : DequeueCall(server, reply)
    \/ \E server \in Tasks, reply \in Replies : PreparePublication(server, reply)
    \/ \E server \in Tasks, reply \in Replies : CompleteReply(server, reply)
    \/ \E caller \in Tasks : ArmBlock(caller)
    \/ \E caller \in Tasks : ArmDeadlineTimer(caller)
    \/ \E caller \in Tasks : CommitBlock(caller)
    \/ \E caller \in Tasks, reply \in Replies :
        BeginTakeCompletedReply(caller, reply)
    \/ \E caller \in Tasks, reply \in Replies :
        FinishTakeBeforeDeadline(caller, reply)
    \/ \E task \in Tasks : ExitTask(task)
    \/ Tick

TypeOK ==
    /\ Tasks \subseteq Nat
    /\ NoTask \notin Tasks
    /\ Replies \subseteq Nat
    /\ NoReply \notin Replies
    /\ MaxTick \in Nat
    /\ MaxTick >= DeadlineTicks
    /\ now \in 0..MaxTick
    /\ taskState \in [Tasks -> {Runnable, Armed, Blocked, Exited}]
    /\ waitFor \in [Tasks -> Replies \cup {NoReply}]
    /\ timerDeadline \in [Tasks -> 0..MaxTick]
    /\ replyState \in [Replies -> {Free, Queued, Dequeued, Replied, Taking,
                                   Cancelled, PeerClosed, Consumed, Abandoned}]
    /\ callerOf \in [Replies -> Tasks \cup {NoTask}]
    /\ serverOf \in [Replies -> Tasks \cup {NoTask}]
    /\ replyDeadline \in [Replies -> 0..MaxTick]
    /\ replyUsed \in [Replies -> BOOLEAN]
    /\ publicationState \in
        [Replies -> {NoPublication, PendingPublication, VisiblePublication,
                     DroppedPublication}]
    /\ observedOutcome \in
        [Tasks -> {NoOutcome, ReplyReceived, TimedOut, PeerClosedOutcome}]
    /\ lastOutcomeReply \in [Tasks -> Replies \cup {NoReply}]

ExactCallerOwnsEveryWait ==
    \A t \in Tasks :
        waitFor[t] # NoReply =>
            /\ callerOf[waitFor[t]] = t
            /\ replyState[waitFor[t]] \in {Queued, Dequeued, Replied, Taking}

AtMostOneOutstandingControlCallPerTask ==
    \A t \in Tasks :
        Cardinality({r \in Replies : OutstandingReply(r) /\ callerOf[r] = t}) <= 1

LiveCallHasExactLiveEndpoints ==
    \A r \in Replies :
        LiveReply(r) =>
            /\ callerOf[r] # NoTask
            /\ serverOf[r] # NoTask
            /\ callerOf[r] # serverOf[r]
            /\ taskState[callerOf[r]] # Exited
            /\ taskState[serverOf[r]] # Exited

OneShotReplyCannotBeReopened ==
    \A r \in Replies :
        /\ replyState[r] \in {Replied, Taking, PeerClosed, Consumed} => replyUsed[r]
        /\ replyUsed[r] => replyState[r] \in {Replied, Taking, Cancelled,
                                               PeerClosed, Consumed, Abandoned}

BlockedWaitHasAnArmedExactDeadline ==
    \A t \in Tasks :
        taskState[t] = Blocked =>
            /\ waitFor[t] # NoReply
            /\ LiveReply(waitFor[t])
            /\ timerDeadline[t] = replyDeadline[waitFor[t]]
            /\ now < timerDeadline[t]

TimerBelongsOnlyToItsLiveWait ==
    \A t \in Tasks :
        timerDeadline[t] # NoTimer =>
            /\ taskState[t] \in {Armed, Blocked}
            /\ waitFor[t] # NoReply
            /\ LiveReply(waitFor[t])
            /\ timerDeadline[t] = replyDeadline[waitFor[t]]
            /\ now < timerDeadline[t]

NoExpiredOrResolvedCallerRemainsBlocked ==
    \A t \in Tasks :
        taskState[t] = Blocked =>
            /\ waitFor[t] # NoReply
            /\ replyState[waitFor[t]] \in {Queued, Dequeued}
            /\ replyDeadline[waitFor[t]] > now

EveryBlockedControlCycleHasAFiniteBreak ==
    \A first, second \in Tasks :
        /\ first # second
        /\ taskState[first] = Blocked
        /\ taskState[second] = Blocked
        /\ WaitingServer(first) = second
        /\ WaitingServer(second) = first
        => /\ timerDeadline[first] > now
           /\ timerDeadline[second] > now

TerminalOutcomeRetainsExactCallerIdentity ==
    \A t \in Tasks :
        observedOutcome[t] # NoOutcome =>
            /\ lastOutcomeReply[t] # NoReply
            /\ callerOf[lastOutcomeReply[t]] = t
            /\ observedOutcome[t] = ReplyReceived =>
                replyState[lastOutcomeReply[t]] = Consumed
            /\ observedOutcome[t] = TimedOut =>
                replyState[lastOutcomeReply[t]] = Cancelled
            /\ observedOutcome[t] = PeerClosedOutcome =>
                replyState[lastOutcomeReply[t]] = PeerClosed

ExitedTaskRetainsNoWaitAuthority ==
    \A t \in Tasks :
        taskState[t] = Exited =>
            /\ waitFor[t] = NoReply
            /\ timerDeadline[t] = NoTimer

PendingPublicationIsOwnedByOneLiveReply ==
    \A r \in Replies :
        publicationState[r] = PendingPublication =>
            /\ replyState[r] \in {Dequeued, Replied, Taking}
            /\ callerOf[r] # NoTask
            /\ serverOf[r] # NoTask

VisiblePublicationRequiresConsumedTimelyReply ==
    \A r \in Replies :
        publicationState[r] = VisiblePublication =>
            /\ replyState[r] = Consumed
            /\ replyUsed[r]

TerminalReplyRetainsNoPendingPublication ==
    \A r \in Replies :
        replyState[r] \in {Cancelled, PeerClosed, Consumed, Abandoned} =>
            publicationState[r] # PendingPublication

TakingReplyStillPrecedesDeadline ==
    \A r \in Replies :
        replyState[r] = Taking => now < replyDeadline[r]

DroppedPublicationNeverBecomesVisible ==
    \A r \in Replies :
        publicationState[r] = DroppedPublication =>
            replyState[r] \in {Cancelled, PeerClosed, Abandoned}

BlockedCallerEventuallyUnblocks ==
    \A t \in Tasks:
        taskState[t] = Blocked ~> taskState[t] \in {Runnable, Exited}

\* Timer delivery is the liveness assumption behind the explicit deadline.
\* The model otherwise permits stuttering, which would make a finite numeric
\* deadline look safe while never actually releasing a blocked control call.
Spec == Init /\ [][Next]_vars /\ WF_vars(Tick)

=============================================================================
