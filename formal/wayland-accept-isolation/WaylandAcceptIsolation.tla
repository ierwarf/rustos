------------------------ MODULE WaylandAcceptIsolation ------------------------
EXTENDS Naturals, Sequences, FiniteSets

(*******************************************************************************
Models the Wayland listener boundary in uiserver.

Concrete owner:
  * services/uiserver/src/wayland_accept.rs `start_wayland_acceptor`
  * services/uiserver/src/wayland.rs `WaylandCompositor::tick`

The netd-backed accept4 call can be delayed even when the listener is
nonblocking, because nonblocking describes socket state, not the latency of
the cross-service RPC that observes that state.  The accept worker is therefore
the only owner of that RPC, and it may enter accept4 only after the listener
wait-set publishes a readiness edge.  It passes completed streams through a
bounded channel and publishes one coalescing UI wake; the UI thread performs
only bounded `try_recv` operations.  Queue overload rejects and logs the newly
accepted client instead of blocking the UI.  Periodic accept probing is not an
admitted transition.

`AcceptCallStalls` deliberately leaves the UI actions enabled.  Weak fairness
then checks that a stalled/recovering netd call cannot suppress UI frame
progress.  `ResolveAcceptCall` represents the kernel IPC deadline or a normal
reply; the model does not assume that netd replies successfully.
*******************************************************************************)

CONSTANTS StreamCount, QueueCapacity, MaxAcceptWait

Streams == 1..StreamCount
NoStream == 0
Idle == "idle"
Waiting == "waiting"

VARIABLES listenerReady,
          acceptStartedWithoutReady,
          acceptCall,
          acceptWait,
          nextStream,
          acceptQueue,
          inserted,
          rejected,
          uiEpoch,
          frameEpoch,
          stallObserved,
          uiAdvancedAfterStall,
          overloadLogged

vars == <<listenerReady, acceptStartedWithoutReady, acceptCall, acceptWait,
          nextStream, acceptQueue, inserted, rejected, uiEpoch, frameEpoch,
          stallObserved, uiAdvancedAfterStall, overloadLogged>>

SeqSet(sequence) == {sequence[index] : index \in 1..Len(sequence)}

Distinct(sequence) ==
    \A left, right \in 1..Len(sequence):
        left # right => sequence[left] # sequence[right]

Init ==
    /\ listenerReady = FALSE
    /\ acceptStartedWithoutReady = FALSE
    /\ acceptCall = Idle
    /\ acceptWait = 0
    /\ nextStream = 1
    /\ acceptQueue = <<>>
    /\ inserted = <<>>
    /\ rejected = <<>>
    /\ uiEpoch = 0
    /\ frameEpoch = 0
    /\ stallObserved = FALSE
    /\ uiAdvancedAfterStall = FALSE
    /\ overloadLogged = [stream \in Streams |-> FALSE]

PublishListenerReady ==
    /\ acceptCall = Idle
    /\ ~listenerReady
    /\ listenerReady' = TRUE
    /\ UNCHANGED <<acceptStartedWithoutReady, acceptCall, acceptWait,
                  nextStream, acceptQueue, inserted, rejected, uiEpoch,
                  frameEpoch, stallObserved, uiAdvancedAfterStall,
                  overloadLogged>>

BeginAcceptCall ==
    /\ acceptCall = Idle
    /\ listenerReady
    /\ listenerReady' = FALSE
    /\ acceptStartedWithoutReady' = ~listenerReady
    /\ acceptCall' = Waiting
    /\ acceptWait' = 0
    /\ UNCHANGED <<nextStream, acceptQueue, inserted, rejected, uiEpoch,
                  frameEpoch, stallObserved, uiAdvancedAfterStall,
                  overloadLogged>>

AcceptCallStalls ==
    /\ acceptCall = Waiting
    /\ acceptWait < MaxAcceptWait
    /\ acceptWait' = acceptWait + 1
    /\ stallObserved' = TRUE
    /\ UNCHANGED <<listenerReady, acceptStartedWithoutReady, acceptCall,
                  nextStream, acceptQueue, inserted, rejected, uiEpoch,
                  frameEpoch, uiAdvancedAfterStall, overloadLogged>>

AcceptWouldBlock ==
    /\ acceptCall = Waiting
    /\ acceptCall' = Idle
    /\ acceptWait' = 0
    /\ UNCHANGED <<listenerReady, acceptStartedWithoutReady, nextStream,
                  acceptQueue, inserted, rejected, uiEpoch, frameEpoch,
                  stallObserved, uiAdvancedAfterStall, overloadLogged>>

AcceptReturnsStream ==
    /\ acceptCall = Waiting
    /\ nextStream \in Streams
    /\ acceptCall' = Idle
    /\ acceptWait' = 0
    /\ nextStream' = nextStream + 1
    /\ IF Len(acceptQueue) < QueueCapacity
          THEN /\ acceptQueue' = Append(acceptQueue, nextStream)
               /\ rejected' = rejected
               /\ overloadLogged' = overloadLogged
          ELSE /\ acceptQueue' = acceptQueue
               /\ rejected' = Append(rejected, nextStream)
               /\ overloadLogged' = [overloadLogged EXCEPT ![nextStream] = TRUE]
    /\ UNCHANGED <<listenerReady, acceptStartedWithoutReady, inserted, uiEpoch,
                  frameEpoch, stallObserved, uiAdvancedAfterStall>>

UiTick ==
    /\ uiEpoch' = (uiEpoch + 1) % 3
    /\ frameEpoch' = (frameEpoch + 1) % 3
    /\ uiAdvancedAfterStall' = (uiAdvancedAfterStall \/ stallObserved)
    /\ IF Len(acceptQueue) = 0
          THEN /\ acceptQueue' = acceptQueue
               /\ inserted' = inserted
          ELSE /\ acceptQueue' = Tail(acceptQueue)
               /\ inserted' = Append(inserted, Head(acceptQueue))
    /\ UNCHANGED <<listenerReady, acceptStartedWithoutReady, acceptCall,
                  acceptWait, nextStream, rejected, stallObserved,
                  overloadLogged>>

ResolveAcceptCall == AcceptWouldBlock \/ AcceptReturnsStream

Next ==
    \/ PublishListenerReady
    \/ BeginAcceptCall
    \/ AcceptCallStalls
    \/ AcceptWouldBlock
    \/ AcceptReturnsStream
    \/ UiTick

Spec ==
    Init /\ [][Next]_vars
         /\ WF_vars(UiTick)
         /\ WF_vars(BeginAcceptCall)
         /\ WF_vars(ResolveAcceptCall)

TypeOK ==
    /\ listenerReady \in BOOLEAN
    /\ acceptStartedWithoutReady \in BOOLEAN
    /\ acceptCall \in {Idle, Waiting}
    /\ acceptWait \in 0..MaxAcceptWait
    /\ nextStream \in 1..(StreamCount + 1)
    /\ acceptQueue \in Seq(Streams)
    /\ inserted \in Seq(Streams)
    /\ rejected \in Seq(Streams)
    /\ uiEpoch \in 0..2
    /\ frameEpoch \in 0..2
    /\ stallObserved \in BOOLEAN
    /\ uiAdvancedAfterStall \in BOOLEAN
    /\ overloadLogged \in [Streams -> BOOLEAN]

BoundedAcceptQueue == Len(acceptQueue) <= QueueCapacity

ExactStreamOwnership ==
    /\ Distinct(acceptQueue \o inserted \o rejected)
    /\ SeqSet(acceptQueue \o inserted \o rejected) = 1..(nextStream - 1)

RejectedOverloadIsObservable ==
    /\ \A stream \in SeqSet(rejected): overloadLogged[stream]
    /\ \A stream \in Streams:
          overloadLogged[stream] => stream \in SeqSet(rejected)

UiNeverOwnsAcceptWait ==
    /\ uiEpoch \in 0..2
    /\ frameEpoch \in 0..2

AcceptWaitIsBounded ==
    acceptCall = Waiting => acceptWait <= MaxAcceptWait

AcceptRequiresPublishedReadiness ==
    ~acceptStartedWithoutReady

StalledAcceptCannotStopFrames ==
    stallObserved ~> uiAdvancedAfterStall

QueuedClientEventuallyInserted ==
    \A stream \in Streams:
        stream \in SeqSet(acceptQueue) ~> stream \in SeqSet(inserted)

=============================================================================
