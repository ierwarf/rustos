----------------------- MODULE DevmgrdSessiondIsolation ------------------------
EXTENDS Naturals, Sequences, FiniteSets

(*******************************************************************************
Models the devmgrd admission boundary for console ioctls that must synchronously
call sessiond, alongside unrelated device work such as input ingress.

Concrete owner:
  * services/devmgrd/src/main.rs `serve`, `start_sessiond_ioctl_workers`, and
    `reply_device_ioctl`

The receive loop must never execute a sessiond call itself. It admits that work
to a bounded FIFO worker queue or returns EAGAIN. A worker may remain blocked
on sessiond indefinitely, but a pending unrelated device request remains
replyable by the main loop.

Linearization points:
  * SubmitSession admits a sessiond ioctl to the worker queue or records EAGAIN.
  * StartSession assigns only the FIFO head to one free worker.
  * FinishSession terminally replies to that exact request.
  * ReplyDevice completes unrelated device work without worker availability.
*******************************************************************************)

CONSTANTS SessionCount, WorkerCount, QueueCapacity, MaxTick

Sessions == 1..SessionCount
Workers == 1..WorkerCount
NoSession == 0

Source == "source"
Queue == "queue"
Worker == "worker"
Done == "done"
Rejected == "rejected"

NoResult == "none"
Replied == "replied"
Eagain == "eagain"

DeviceIdle == "idle"
DevicePending == "pending"
DeviceReplied == "replied"

MainReceive == "receive"
MainReply == "reply"

VARIABLES now,
          nextSession,
          sessionOwner,
          sessionQueue,
          workerItem,
          sessionResult,
          rejectionLogged,
          deviceState,
          mainPhase

vars == <<now, nextSession, sessionOwner, sessionQueue, workerItem,
          sessionResult, rejectionLogged, deviceState, mainPhase>>

SeqSet(sequence) == {sequence[index] : index \in 1..Len(sequence)}

Init ==
    /\ now = 0
    /\ nextSession = 1
    /\ sessionOwner = [session \in Sessions |-> Source]
    /\ sessionQueue = <<>>
    /\ workerItem = [worker \in Workers |-> NoSession]
    /\ sessionResult = [session \in Sessions |-> NoResult]
    /\ rejectionLogged = [session \in Sessions |-> FALSE]
    /\ deviceState = DeviceIdle
    /\ mainPhase = MainReceive

SubmitSession ==
    /\ nextSession \in Sessions
    /\ sessionOwner[nextSession] = Source
    /\ IF Len(sessionQueue) < QueueCapacity
          THEN /\ sessionOwner' = [sessionOwner EXCEPT ![nextSession] = Queue]
               /\ sessionQueue' = Append(sessionQueue, nextSession)
               /\ rejectionLogged' = rejectionLogged
          ELSE /\ sessionOwner' = [sessionOwner EXCEPT ![nextSession] = Rejected]
               /\ sessionQueue' = sessionQueue
               /\ rejectionLogged' = [rejectionLogged EXCEPT ![nextSession] = TRUE]
    /\ nextSession' = nextSession + 1
    /\ mainPhase' = MainReceive
    /\ UNCHANGED <<now, workerItem, sessionResult, deviceState>>

StartSession ==
    \E worker \in Workers:
        /\ workerItem[worker] = NoSession
        /\ Len(sessionQueue) > 0
        /\ LET session == Head(sessionQueue) IN
           /\ sessionOwner[session] = Queue
           /\ sessionOwner' = [sessionOwner EXCEPT ![session] = Worker]
           /\ sessionQueue' = Tail(sessionQueue)
           /\ workerItem' = [workerItem EXCEPT ![worker] = session]
        /\ mainPhase' = MainReceive
        /\ UNCHANGED <<now, nextSession, sessionResult, rejectionLogged,
                      deviceState>>

FinishSession ==
    \E worker \in Workers:
        /\ workerItem[worker] \in Sessions
        /\ LET session == workerItem[worker] IN
           /\ sessionOwner[session] = Worker
           /\ sessionOwner' = [sessionOwner EXCEPT ![session] = Done]
           /\ sessionResult' = [sessionResult EXCEPT ![session] = Replied]
           /\ workerItem' = [workerItem EXCEPT ![worker] = NoSession]
        /\ mainPhase' = MainReceive
        /\ UNCHANGED <<now, nextSession, sessionQueue, rejectionLogged,
                      deviceState>>

WorkerStalls ==
    \E worker \in Workers:
        /\ workerItem[worker] \in Sessions
        /\ now < MaxTick
        /\ now' = now + 1
        /\ UNCHANGED <<nextSession, sessionOwner, sessionQueue, workerItem,
                      sessionResult, rejectionLogged, deviceState, mainPhase>>

SubmitDevice ==
    /\ deviceState = DeviceIdle
    /\ deviceState' = DevicePending
    /\ mainPhase' = MainReceive
    /\ UNCHANGED <<now, nextSession, sessionOwner, sessionQueue, workerItem,
                  sessionResult, rejectionLogged>>

(*******************************************************************************
No guard refers to workerItem or sessionQueue: this is the explicit
head-of-line isolation contract for unrelated device traffic.
*******************************************************************************)
ReplyDevice ==
    /\ deviceState = DevicePending
    /\ deviceState' = DeviceReplied
    /\ mainPhase' = MainReply
    /\ UNCHANGED <<now, nextSession, sessionOwner, sessionQueue, workerItem,
                  sessionResult, rejectionLogged>>

ReturnMainReceive ==
    /\ mainPhase = MainReply
    /\ mainPhase' = MainReceive
    /\ UNCHANGED <<now, nextSession, sessionOwner, sessionQueue, workerItem,
                  sessionResult, rejectionLogged, deviceState>>

Next ==
    \/ SubmitSession
    \/ StartSession
    \/ FinishSession
    \/ WorkerStalls
    \/ SubmitDevice
    \/ ReplyDevice
    \/ ReturnMainReceive

Spec == Init /\ [][Next]_vars /\ WF_vars(ReplyDevice)

TypeOK ==
    /\ now \in 0..MaxTick
    /\ nextSession \in 1..(SessionCount + 1)
    /\ sessionOwner \in [Sessions -> {Source, Queue, Worker, Done, Rejected}]
    /\ sessionQueue \in Seq(Sessions)
    /\ workerItem \in [Workers -> Sessions \cup {NoSession}]
    /\ sessionResult \in [Sessions -> {NoResult, Replied}]
    /\ rejectionLogged \in [Sessions -> BOOLEAN]
    /\ deviceState \in {DeviceIdle, DevicePending, DeviceReplied}
    /\ mainPhase \in {MainReceive, MainReply}

BoundedSessionAdmission == Len(sessionQueue) <= QueueCapacity

SessionQueueOwnership ==
    /\ SeqSet(sessionQueue) = {session \in Sessions : sessionOwner[session] = Queue}
    /\ Cardinality(SeqSet(sessionQueue)) = Len(sessionQueue)

WorkerOwnership ==
    /\ \A worker \in Workers:
          workerItem[worker] = NoSession
              \/ sessionOwner[workerItem[worker]] = Worker
    /\ \A first \in Workers:
          \A second \in Workers:
              first # second /\ workerItem[first] # NoSession
                  => workerItem[first] # workerItem[second]

FifoSessionDelivery ==
    \A first \in 1..Len(sessionQueue):
        \A second \in 1..Len(sessionQueue):
            first < second => sessionQueue[first] < sessionQueue[second]

TerminalAndRejectionAccounting ==
    \A session \in Sessions:
        /\ sessionOwner[session] = Rejected => rejectionLogged[session]
        /\ sessionOwner[session] = Done => sessionResult[session] = Replied
        /\ sessionResult[session] = Replied => sessionOwner[session] = Done

NoMainSessiondWait == mainPhase \in {MainReceive, MainReply}

DevicePendingEventuallyReplied ==
    deviceState = DevicePending ~> deviceState = DeviceReplied

=============================================================================
