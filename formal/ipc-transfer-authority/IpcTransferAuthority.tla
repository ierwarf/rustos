---------------------- MODULE IpcTransferAuthority ----------------------
EXTENDS Naturals

(***************************************************************************
Models the v2 AF_UNIX ancillary transfer batch. The kernel, not netd, binds
source process generation, service epoch, channel generation/direction, exact
stream position, and intended receiver. FD reservations remain invisible
until every user data/control/header copy has completed. Every queue discard,
peer close, copyout fault, and service restart is one terminal release edge.
***************************************************************************)

CONSTANTS Senders, Receivers, Services, Channels, Positions, NoIdentity

Source == "source"
Exported == "exported"
Enqueued == "enqueued"
Claimed == "claimed"
Installed == "installed"
Released == "released"

VARIABLES state, source, service, channel, streamStart, streamEnd,
          intendedReceiver, claimedReceiver, observedService, observedChannel,
          observedPosition, queueOwns, copyoutComplete, fdVisible, terminalCount

vars == <<state, source, service, channel, streamStart, streamEnd,
          intendedReceiver, claimedReceiver, observedService, observedChannel,
          observedPosition, queueOwns, copyoutComplete, fdVisible, terminalCount>>

Init ==
    /\ state = Source
    /\ source = NoIdentity
    /\ service = NoIdentity
    /\ channel = NoIdentity
    /\ streamStart = 0
    /\ streamEnd = 0
    /\ intendedReceiver = NoIdentity
    /\ claimedReceiver = NoIdentity
    /\ observedService = NoIdentity
    /\ observedChannel = NoIdentity
    /\ observedPosition = 0
    /\ queueOwns = FALSE
    /\ copyoutComplete = FALSE
    /\ fdVisible = FALSE
    /\ terminalCount = 0

Export(sender, svc, chan, start, receiver) ==
    /\ state = Source
    /\ sender \in Senders
    /\ svc \in Services
    /\ chan \in Channels
    /\ start \in Positions
    /\ receiver \in Receivers
    /\ state' = Exported
    /\ source' = sender
    /\ service' = svc
    /\ channel' = chan
    /\ streamStart' = start
    /\ streamEnd' = start + 1
    /\ intendedReceiver' = receiver
    /\ UNCHANGED <<claimedReceiver, observedService, observedChannel,
                   observedPosition, queueOwns, copyoutComplete, fdVisible,
                   terminalCount>>

ExportAny ==
    \E sender \in Senders, svc \in Services, chan \in Channels,
       start \in Positions, receiver \in Receivers:
        Export(sender, svc, chan, start, receiver)

Enqueue ==
    /\ state = Exported
    /\ state' = Enqueued
    /\ queueOwns' = TRUE
    /\ UNCHANGED <<source, service, channel, streamStart, streamEnd,
                   intendedReceiver, claimedReceiver, observedService,
                   observedChannel, observedPosition, copyoutComplete,
                   fdVisible, terminalCount>>

Claim(receiver, svc, chan, position) ==
    /\ state = Enqueued
    /\ receiver = intendedReceiver
    /\ svc = service
    /\ chan = channel
    /\ position = streamStart
    /\ state' = Claimed
    /\ claimedReceiver' = receiver
    /\ observedService' = svc
    /\ observedChannel' = chan
    /\ observedPosition' = position
    /\ queueOwns' = FALSE
    /\ UNCHANGED <<source, service, channel, streamStart, streamEnd,
                   intendedReceiver, copyoutComplete, fdVisible, terminalCount>>

ClaimAny ==
    \E receiver \in Receivers, svc \in Services, chan \in Channels,
       position \in Positions: Claim(receiver, svc, chan, position)

RejectMisdirected(receiver, svc, chan, position) ==
    /\ state = Enqueued
    /\ receiver \in Receivers
    /\ svc \in Services
    /\ chan \in Channels
    /\ position \in Positions
    /\ receiver # intendedReceiver \/ svc # service \/ chan # channel \/ position # streamStart
    /\ state' = Released
    /\ queueOwns' = FALSE
    /\ terminalCount' = 1
    /\ UNCHANGED <<source, service, channel, streamStart, streamEnd,
                   intendedReceiver, claimedReceiver, observedService,
                   observedChannel, observedPosition, copyoutComplete, fdVisible>>

RejectMisdirectedAny ==
    \E receiver \in Receivers, svc \in Services, chan \in Channels,
       position \in Positions: RejectMisdirected(receiver, svc, chan, position)

CompleteCopyout ==
    /\ state = Claimed
    /\ ~copyoutComplete
    /\ copyoutComplete' = TRUE
    /\ UNCHANGED <<state, source, service, channel, streamStart, streamEnd,
                   intendedReceiver, claimedReceiver, observedService,
                   observedChannel, observedPosition, queueOwns, fdVisible,
                   terminalCount>>

CommitInstall ==
    /\ state = Claimed
    /\ copyoutComplete
    /\ state' = Installed
    /\ fdVisible' = TRUE
    /\ terminalCount' = 1
    /\ UNCHANGED <<source, service, channel, streamStart, streamEnd,
                   intendedReceiver, claimedReceiver, observedService,
                   observedChannel, observedPosition, queueOwns, copyoutComplete>>

Release ==
    /\ state \in {Exported, Enqueued, Claimed}
    /\ state' = Released
    /\ queueOwns' = FALSE
    /\ terminalCount' = 1
    /\ UNCHANGED <<source, service, channel, streamStart, streamEnd,
                   intendedReceiver, claimedReceiver, observedService,
                   observedChannel, observedPosition, copyoutComplete, fdVisible>>

Terminate == ClaimAny \/ RejectMisdirectedAny \/ Release \/ CompleteCopyout \/ CommitInstall

Terminal ==
    /\ state \in {Installed, Released}
    /\ UNCHANGED vars

Next == ExportAny \/ Enqueue \/ Terminate \/ Terminal

Spec == Init /\ [][Next]_vars /\ SF_vars(Terminate)

TypeOK ==
    /\ state \in {Source, Exported, Enqueued, Claimed, Installed, Released}
    /\ source \in Senders \union {NoIdentity}
    /\ service \in Services \union {NoIdentity}
    /\ channel \in Channels \union {NoIdentity}
    /\ streamStart \in Nat
    /\ streamEnd \in Nat
    /\ intendedReceiver \in Receivers \union {NoIdentity}
    /\ claimedReceiver \in Receivers \union {NoIdentity}
    /\ observedService \in Services \union {NoIdentity}
    /\ observedChannel \in Channels \union {NoIdentity}
    /\ observedPosition \in Nat
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

AncillaryMatchesStreamPosition ==
    state \in {Claimed, Installed} => observedPosition = streamStart

FdVisibilityRequiresCompleteCopyout == fdVisible => copyoutComplete

QueueOwnershipIsExact == queueOwns <=> state = Enqueued

TerminalOutcomeIsExactlyOnce ==
    /\ state \in {Installed, Released} => terminalCount = 1
    /\ terminalCount = 1 => state \in {Installed, Released}

PublishedBatchEventuallyTerminates ==
    state \in {Exported, Enqueued, Claimed} ~> state \in {Installed, Released}

=============================================================================
