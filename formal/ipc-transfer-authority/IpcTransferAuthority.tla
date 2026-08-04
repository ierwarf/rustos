---------------------- MODULE IpcTransferAuthority ----------------------
EXTENDS Naturals

(***************************************************************************
Models the v2 AF_UNIX ancillary transfer batch. The kernel, not netd, binds
source process generation, service epoch, channel generation/direction, exact
stream position, and intended receiver. FD reservations remain invisible
until every user data/control/header copy has completed. Every queue discard,
peer close, copyout fault, and service restart is one terminal release edge.
***************************************************************************)

CONSTANTS Senders, Receivers, Services, Channels, Positions, Lengths,
          ReceiverEpochs, NoIdentity

Source == "source"
Exported == "exported"
Enqueued == "enqueued"
Prebound == "prebound"
Claimed == "claimed"
Installed == "installed"
Released == "released"

VARIABLES state, source, service, channel, streamStart, streamEnd,
          intendedReceiver, receiverSetEpoch, claimedReceiver,
          observedReceiverSetEpoch, observedService, observedChannel,
          observedPosition, committedPosition, lastRequestedEnd,
          lastAcceptedEnd, queueOwns, copyoutComplete, fdVisible, terminalCount

vars == <<state, source, service, channel, streamStart, streamEnd,
          intendedReceiver, receiverSetEpoch, claimedReceiver,
          observedReceiverSetEpoch, observedService, observedChannel,
          observedPosition, committedPosition, lastRequestedEnd,
          lastAcceptedEnd, queueOwns, copyoutComplete, fdVisible, terminalCount>>

Init ==
    /\ state = Source
    /\ source = NoIdentity
    /\ service = NoIdentity
    /\ channel = NoIdentity
    /\ streamStart = 0
    /\ streamEnd = 0
    /\ intendedReceiver = NoIdentity
    /\ receiverSetEpoch = 0
    /\ claimedReceiver = NoIdentity
    /\ observedReceiverSetEpoch = 0
    /\ observedService = NoIdentity
    /\ observedChannel = NoIdentity
    /\ observedPosition = 0
    /\ committedPosition = 0
    /\ lastRequestedEnd = 0
    /\ lastAcceptedEnd = 0
    /\ queueOwns = FALSE
    /\ copyoutComplete = FALSE
    /\ fdVisible = FALSE
    /\ terminalCount = 0

PartialOrdinarySend(requested, accepted) ==
    /\ state = Source
    /\ requested \in Lengths
    /\ accepted \in 1..requested
    /\ committedPosition \in Positions
    /\ committedPosition + accepted \in Positions
    /\ committedPosition' = committedPosition + accepted
    /\ lastRequestedEnd' = committedPosition + requested
    /\ lastAcceptedEnd' = committedPosition + accepted
    /\ UNCHANGED <<state, source, service, channel, streamStart, streamEnd,
                    intendedReceiver, receiverSetEpoch, claimedReceiver,
                    observedReceiverSetEpoch, observedService, observedChannel,
                    observedPosition, queueOwns, copyoutComplete, fdVisible,
                    terminalCount>>

PartialOrdinarySendAny ==
    \E requested \in Lengths:
        \E accepted \in 1..requested: PartialOrdinarySend(requested, accepted)

Export(sender, svc, chan, start) ==
    /\ state = Source
    /\ sender \in Senders
    /\ svc \in Services
    /\ chan \in Channels
    /\ start \in Positions
    /\ start = committedPosition
    /\ state' = Exported
    /\ source' = sender
    /\ service' = svc
    /\ channel' = chan
    /\ streamStart' = start
    /\ streamEnd' = start + 1
    /\ intendedReceiver' = NoIdentity
    /\ receiverSetEpoch' = 0
    /\ UNCHANGED <<claimedReceiver, observedReceiverSetEpoch,
                   observedService, observedChannel, observedPosition,
                   committedPosition, lastRequestedEnd, lastAcceptedEnd,
                   queueOwns, copyoutComplete, fdVisible, terminalCount>>

ExportAny ==
    \E sender \in Senders, svc \in Services, chan \in Channels,
       start \in Positions:
        Export(sender, svc, chan, start)

Enqueue ==
    /\ state = Exported
    /\ state' = Enqueued
    /\ queueOwns' = TRUE
    /\ UNCHANGED <<source, service, channel, streamStart, streamEnd,
                   intendedReceiver, receiverSetEpoch, claimedReceiver,
                   observedReceiverSetEpoch, observedService, observedChannel,
                   observedPosition, committedPosition, lastRequestedEnd,
                   lastAcceptedEnd, copyoutComplete, fdVisible, terminalCount>>

BindReceiver(receiver, receiverEpoch, svc, chan, position) ==
    /\ state = Enqueued
    /\ receiver \in Receivers
    /\ receiverEpoch \in ReceiverEpochs
    /\ svc = service
    /\ chan = channel
    /\ position = streamStart
    /\ state' = Prebound
    /\ intendedReceiver' = receiver
    /\ receiverSetEpoch' = receiverEpoch
    /\ UNCHANGED <<source, service, channel, streamStart, streamEnd,
                   claimedReceiver, observedReceiverSetEpoch,
                   observedService, observedChannel, observedPosition,
                   committedPosition, lastRequestedEnd, lastAcceptedEnd,
                   queueOwns, copyoutComplete, fdVisible, terminalCount>>

BindReceiverAny ==
    \E receiver \in Receivers, receiverEpoch \in ReceiverEpochs,
       svc \in Services, chan \in Channels, position \in Positions:
        BindReceiver(receiver, receiverEpoch, svc, chan, position)

Claim(receiver, receiverEpoch, svc, chan, position) ==
    /\ state = Prebound
    /\ receiver = intendedReceiver
    /\ receiverEpoch = receiverSetEpoch
    /\ svc = service
    /\ chan = channel
    /\ position = streamStart
    /\ state' = Claimed
    /\ claimedReceiver' = receiver
    /\ observedReceiverSetEpoch' = receiverEpoch
    /\ observedService' = svc
    /\ observedChannel' = chan
    /\ observedPosition' = position
    /\ queueOwns' = FALSE
    /\ UNCHANGED <<source, service, channel, streamStart, streamEnd,
                   intendedReceiver, receiverSetEpoch, committedPosition,
                   lastRequestedEnd, lastAcceptedEnd, copyoutComplete,
                   fdVisible, terminalCount>>

ClaimAny ==
    \E receiver \in Receivers, receiverEpoch \in ReceiverEpochs,
       svc \in Services, chan \in Channels, position \in Positions:
        Claim(receiver, receiverEpoch, svc, chan, position)

RejectMisdirected(receiver, receiverEpoch, svc, chan, position) ==
    /\ state = Prebound
    /\ receiver \in Receivers
    /\ receiverEpoch \in ReceiverEpochs
    /\ svc \in Services
    /\ chan \in Channels
    /\ position \in Positions
    /\ receiver # intendedReceiver \/ receiverEpoch # receiverSetEpoch
       \/ svc # service \/ chan # channel \/ position # streamStart
    /\ state' = Released
    /\ queueOwns' = FALSE
    /\ terminalCount' = 1
    /\ UNCHANGED <<source, service, channel, streamStart, streamEnd,
                   intendedReceiver, receiverSetEpoch, claimedReceiver,
                   observedReceiverSetEpoch, observedService, observedChannel,
                   observedPosition, committedPosition, lastRequestedEnd,
                   lastAcceptedEnd, copyoutComplete, fdVisible>>

RejectMisdirectedAny ==
    \E receiver \in Receivers, receiverEpoch \in ReceiverEpochs,
       svc \in Services, chan \in Channels, position \in Positions:
        RejectMisdirected(receiver, receiverEpoch, svc, chan, position)

CompleteCopyout ==
    /\ state = Claimed
    /\ ~copyoutComplete
    /\ copyoutComplete' = TRUE
    /\ UNCHANGED <<state, source, service, channel, streamStart, streamEnd,
                   intendedReceiver, receiverSetEpoch, claimedReceiver,
                   observedReceiverSetEpoch, observedService, observedChannel,
                   observedPosition, committedPosition, lastRequestedEnd,
                   lastAcceptedEnd, queueOwns, fdVisible, terminalCount>>

CommitInstall ==
    /\ state = Claimed
    /\ copyoutComplete
    /\ state' = Installed
    /\ fdVisible' = TRUE
    /\ terminalCount' = 1
    /\ UNCHANGED <<source, service, channel, streamStart, streamEnd,
                   intendedReceiver, receiverSetEpoch, claimedReceiver,
                   observedReceiverSetEpoch, observedService, observedChannel,
                   observedPosition, committedPosition, lastRequestedEnd,
                   lastAcceptedEnd, queueOwns, copyoutComplete>>

Release ==
    /\ state \in {Exported, Enqueued, Prebound, Claimed}
    /\ state' = Released
    /\ queueOwns' = FALSE
    /\ terminalCount' = 1
    /\ UNCHANGED <<source, service, channel, streamStart, streamEnd,
                   intendedReceiver, receiverSetEpoch, claimedReceiver,
                   observedReceiverSetEpoch, observedService, observedChannel,
                   observedPosition, committedPosition, lastRequestedEnd,
                   lastAcceptedEnd, copyoutComplete, fdVisible>>

Terminate == ClaimAny \/ RejectMisdirectedAny \/ Release \/ CompleteCopyout \/ CommitInstall

Terminal ==
    /\ state \in {Installed, Released}
    /\ UNCHANGED vars

Next == PartialOrdinarySendAny \/ ExportAny \/ Enqueue \/ BindReceiverAny
        \/ Terminate \/ Terminal

Spec == Init /\ [][Next]_vars /\ SF_vars(Terminate)

TypeOK ==
    /\ state \in {Source, Exported, Enqueued, Prebound, Claimed, Installed, Released}
    /\ source \in Senders \union {NoIdentity}
    /\ service \in Services \union {NoIdentity}
    /\ channel \in Channels \union {NoIdentity}
    /\ streamStart \in Nat
    /\ streamEnd \in Nat
    /\ intendedReceiver \in Receivers \union {NoIdentity}
    /\ receiverSetEpoch \in ReceiverEpochs \union {0}
    /\ claimedReceiver \in Receivers \union {NoIdentity}
    /\ observedReceiverSetEpoch \in ReceiverEpochs \union {0}
    /\ observedService \in Services \union {NoIdentity}
    /\ observedChannel \in Channels \union {NoIdentity}
    /\ observedPosition \in Nat
    /\ committedPosition \in Nat
    /\ lastRequestedEnd \in Nat
    /\ lastAcceptedEnd \in Nat
    /\ queueOwns \in BOOLEAN
    /\ copyoutComplete \in BOOLEAN
    /\ fdVisible \in BOOLEAN
    /\ terminalCount \in 0..1
    /\ NoIdentity \notin Senders \union Receivers \union Services \union Channels

OnlyIntendedRecipientInstalls ==
    state = Installed => claimedReceiver = intendedReceiver

ServiceAndChannelCannotRedirect ==
    state \in {Claimed, Installed} =>
        /\ observedService = service
        /\ observedChannel = channel

ReceiverSetCannotRedirect ==
    state \in {Claimed, Installed} =>
        observedReceiverSetEpoch = receiverSetEpoch

AncillaryMatchesStreamPosition ==
    state \in {Claimed, Installed} => observedPosition = streamStart

AcceptedBytesOwnTheFrontier ==
    /\ lastAcceptedEnd <= lastRequestedEnd
    /\ state = Source => committedPosition = lastAcceptedEnd
    /\ state \in {Exported, Enqueued, Prebound, Claimed, Installed, Released}
          => streamStart = committedPosition

FdVisibilityRequiresCompleteCopyout == fdVisible => copyoutComplete

QueueOwnershipIsExact == queueOwns <=> state \in {Enqueued, Prebound}

TerminalOutcomeIsExactlyOnce ==
    /\ state \in {Installed, Released} => terminalCount = 1
    /\ terminalCount = 1 => state \in {Installed, Released}

PublishedBatchEventuallyTerminates ==
    state \in {Exported, Enqueued, Prebound, Claimed} ~> state \in {Installed, Released}

=============================================================================
