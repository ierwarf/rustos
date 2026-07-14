------------------------------ MODULE DvmControlRelay -----------------------------
EXTENDS Naturals, FiniteSets

(*******************************************************************************
Models the Linux DVM host-control and input-relay transaction.

Concrete owners and source anchors:
  * L0 control listener: libs/driver-domain-host/src/lib.rs
  * Linux DVM agent: driver-domains/linux/package/rustos-dvm-agent/
    src/rustos-dvm-agent.c
  * RDI2 receiver: kernel/io-manager/src/input/dvm_serial.rs

The host accepts a KVM-vsock connection only from its launch-bound CID. An
exact agent-v1-control HELLO causes L0 to issue one fresh challenge. Only the
matching HMAC proof over that challenge and exact HELLO permits the WELCOME
write; that is the control-plane linearization point. Requests are serial, use
the fixed allowlist and IDs, and time out closed. Only after the three mandatory
probes and the input-stream ready reply may L0 create a fresh RDI2 relay epoch.
The receiver accepts frames
only in that epoch and only at the next sequence number.  Session end is
modeled atomically with its key/button cleanup and clears receiver authority.

This intentionally does not model evdev payload values, UTF-8/frame parsing,
CRC arithmetic, vsock implementation, DMA, or the ivshmem network/display
data planes.  Those have separate source checks and need independent models.
*******************************************************************************)

CONSTANTS ExpectedCid,
          PeerCids,
          SessionIds,
          ChallengeNonces,
          RelayEpochs,
          MaxTime,
          HelloDeadline,
          ProofDeadline,
          ReplyDeadline,
          MaxSequence,
          MaxRejections

NoCid == 0
NoSession == 0
NoChallenge == "none"
NoEpoch == 0
NoOp == "none"
NoRequestId == 0

Idle == "idle"
AwaitHello == "await-hello"
AwaitProof == "await-proof"
ControlReady == "control-ready"
Relaying == "relaying"

None == "none"
PeerRejected == "peer-rejected"
HelloRejected == "hello-rejected"
ProofRejected == "proof-rejected"
TimedOut == "timed-out"
Disconnected == "disconnected"
RelayEnded == "relay-ended"

Ops == {"health", "device-inventory", "driver-inventory", "input-stream"}
ProbeOps == {"health", "device-inventory", "driver-inventory"}
EventSequences == 1..MaxSequence

RequestId(op) ==
    IF op = "health" THEN 1
    ELSE IF op = "device-inventory" THEN 2
    ELSE IF op = "driver-inventory" THEN 3
    ELSE IF op = "input-stream" THEN 4
    ELSE NoRequestId

NextControlRequest(completed) ==
    IF "health" \notin completed THEN "health"
    ELSE IF "device-inventory" \notin completed THEN "device-inventory"
    ELSE IF "driver-inventory" \notin completed THEN "driver-inventory"
    ELSE IF "input-stream" \notin completed THEN "input-stream"
    ELSE NoOp

ControlPrefix(completed) ==
    completed \in {
        {},
        {"health"},
        {"health", "device-inventory"},
        ProbeOps,
        Ops
    }

AcceptedPrefix(last) == {sequence \in EventSequences : sequence <= last}

VARIABLES phase,
          peerCid,
          controlSession,
          issuedSessions,
          helloDeadline,
          activeChallenge,
          issuedChallenges,
          proofDeadline,
          proofAccepted,
          completedOps,
          pendingOp,
          pendingRequestId,
          pendingSession,
          replyDeadline,
          issuedRelayEpochs,
          activeRelayEpoch,
          receiverEpoch,
          receiverSequence,
          acceptedSequences,
          nextSequence,
          cleanupComplete,
          rejectedReplies,
          rejectedFrames,
          lastOutcome,
          now

vars == <<phase, peerCid, controlSession, issuedSessions, helloDeadline,
          activeChallenge, issuedChallenges, proofDeadline, proofAccepted,
          completedOps, pendingOp, pendingRequestId, pendingSession,
          replyDeadline, issuedRelayEpochs, activeRelayEpoch, receiverEpoch,
          receiverSequence, acceptedSequences, nextSequence, cleanupComplete,
          rejectedReplies, rejectedFrames, lastOutcome, now>>

Init ==
    /\ phase = Idle
    /\ peerCid = NoCid
    /\ controlSession = NoSession
    /\ issuedSessions = {}
    /\ helloDeadline = 0
    /\ activeChallenge = NoChallenge
    /\ issuedChallenges = {}
    /\ proofDeadline = 0
    /\ proofAccepted = FALSE
    /\ completedOps = {}
    /\ pendingOp = NoOp
    /\ pendingRequestId = NoRequestId
    /\ pendingSession = NoSession
    /\ replyDeadline = 0
    /\ issuedRelayEpochs = {}
    /\ activeRelayEpoch = NoEpoch
    /\ receiverEpoch = NoEpoch
    /\ receiverSequence = 0
    /\ acceptedSequences = {}
    /\ nextSequence = 1
    /\ cleanupComplete = TRUE
    /\ rejectedReplies = 0
    /\ rejectedFrames = 0
    /\ lastOutcome = None
    /\ now = 0

(*******************************************************************************
ClearSession represents a failed-close socket teardown.  For a relay it also
represents L0's release/key-button cleanup plus RDI2 SESSION_END; the RustOS
decoder cannot retain an epoch or sequence after it.
*******************************************************************************)
ClearSession(outcome) ==
    /\ phase' = Idle
    /\ peerCid' = NoCid
    /\ controlSession' = NoSession
    /\ helloDeadline' = 0
    /\ activeChallenge' = NoChallenge
    /\ proofDeadline' = 0
    /\ proofAccepted' = FALSE
    /\ completedOps' = {}
    /\ pendingOp' = NoOp
    /\ pendingRequestId' = NoRequestId
    /\ pendingSession' = NoSession
    /\ replyDeadline' = 0
    /\ activeRelayEpoch' = NoEpoch
    /\ receiverEpoch' = NoEpoch
    /\ receiverSequence' = 0
    /\ acceptedSequences' = {}
    /\ nextSequence' = 1
    /\ cleanupComplete' = TRUE
    /\ lastOutcome' = outcome

AcceptExpectedPeer(cid, session) ==
    /\ phase = Idle
    /\ cid = ExpectedCid
    /\ cid \in PeerCids
    /\ session \in SessionIds \ issuedSessions
    /\ phase' = AwaitHello
    /\ peerCid' = cid
    /\ controlSession' = session
    /\ issuedSessions' = issuedSessions \cup {session}
    /\ helloDeadline' = now + HelloDeadline
    /\ activeChallenge' = NoChallenge
    /\ proofDeadline' = 0
    /\ proofAccepted' = FALSE
    /\ completedOps' = {}
    /\ pendingOp' = NoOp
    /\ pendingRequestId' = NoRequestId
    /\ pendingSession' = NoSession
    /\ replyDeadline' = 0
    /\ activeRelayEpoch' = NoEpoch
    /\ receiverEpoch' = NoEpoch
    /\ receiverSequence' = 0
    /\ acceptedSequences' = {}
    /\ nextSequence' = 1
    /\ cleanupComplete' = TRUE
    /\ lastOutcome' = None
    /\ UNCHANGED <<issuedChallenges, issuedRelayEpochs, rejectedReplies,
                  rejectedFrames, now>>

RejectForeignPeer(cid) ==
    /\ phase = Idle
    /\ cid \in PeerCids \ {ExpectedCid}
    /\ lastOutcome' = PeerRejected
    /\ UNCHANGED <<phase, peerCid, controlSession, issuedSessions, helloDeadline,
                  activeChallenge, issuedChallenges, proofDeadline, proofAccepted,
                  completedOps, pendingOp, pendingRequestId, pendingSession,
                  replyDeadline, issuedRelayEpochs, activeRelayEpoch, receiverEpoch,
                  receiverSequence, acceptedSequences, nextSequence, cleanupComplete,
                  rejectedReplies, rejectedFrames, now>>

AcceptMatchingHello(challenge) ==
    /\ phase = AwaitHello
    /\ peerCid = ExpectedCid
    /\ controlSession \in issuedSessions
    /\ now < helloDeadline
    /\ challenge \in ChallengeNonces \ issuedChallenges
    /\ phase' = AwaitProof
    /\ activeChallenge' = challenge
    /\ issuedChallenges' = issuedChallenges \cup {challenge}
    /\ proofDeadline' = now + ProofDeadline
    /\ proofAccepted' = FALSE
    /\ lastOutcome' = None
    /\ UNCHANGED <<peerCid, controlSession, issuedSessions, helloDeadline,
                  completedOps, pendingOp, pendingRequestId, pendingSession,
                  replyDeadline, issuedRelayEpochs, activeRelayEpoch, receiverEpoch,
                  receiverSequence, acceptedSequences, nextSequence, cleanupComplete,
                  rejectedReplies, rejectedFrames, now>>

AcceptMatchingProof ==
    /\ phase = AwaitProof
    /\ peerCid = ExpectedCid
    /\ controlSession \in issuedSessions
    /\ activeChallenge \in issuedChallenges
    /\ activeChallenge # NoChallenge
    /\ now < proofDeadline
    /\ phase' = ControlReady
    /\ proofAccepted' = TRUE
    /\ lastOutcome' = None
    /\ UNCHANGED <<peerCid, controlSession, issuedSessions, helloDeadline,
                  activeChallenge, issuedChallenges, proofDeadline, completedOps,
                  pendingOp, pendingRequestId, pendingSession, replyDeadline,
                  issuedRelayEpochs, activeRelayEpoch, receiverEpoch,
                  receiverSequence, acceptedSequences, nextSequence, cleanupComplete,
                  rejectedReplies, rejectedFrames, now>>

RejectMismatchingHello ==
    /\ phase = AwaitHello
    /\ ClearSession(HelloRejected)
    /\ UNCHANGED <<issuedSessions, issuedChallenges, issuedRelayEpochs, rejectedReplies,
                  rejectedFrames, now>>

RejectInvalidProof ==
    /\ phase = AwaitProof
    /\ ClearSession(ProofRejected)
    /\ UNCHANGED <<issuedSessions, issuedChallenges, issuedRelayEpochs,
                  rejectedReplies, rejectedFrames, now>>

SendControlRequest(op) ==
    /\ phase = ControlReady
    /\ pendingOp = NoOp
    /\ op = NextControlRequest(completedOps)
    /\ op \in Ops
    /\ phase' = ControlReady
    /\ pendingOp' = op
    /\ pendingRequestId' = RequestId(op)
    /\ pendingSession' = controlSession
    /\ replyDeadline' = now + ReplyDeadline
    /\ UNCHANGED <<peerCid, controlSession, issuedSessions, helloDeadline,
                  activeChallenge, issuedChallenges, proofDeadline, proofAccepted,
                  completedOps, issuedRelayEpochs, activeRelayEpoch, receiverEpoch,
                  receiverSequence, acceptedSequences, nextSequence, cleanupComplete,
                  rejectedReplies, rejectedFrames, lastOutcome, now>>

AcceptMatchingResponse(op, requestId, session) ==
    /\ phase = ControlReady
    /\ pendingOp = op
    /\ pendingRequestId = requestId
    /\ pendingSession = session
    /\ op \in Ops
    /\ requestId = RequestId(op)
    /\ session = controlSession
    /\ now < replyDeadline
    /\ completedOps' = completedOps \cup {op}
    /\ pendingOp' = NoOp
    /\ pendingRequestId' = NoRequestId
    /\ pendingSession' = NoSession
    /\ replyDeadline' = 0
    /\ UNCHANGED <<phase, peerCid, controlSession, issuedSessions, helloDeadline,
                  activeChallenge, issuedChallenges, proofDeadline, proofAccepted,
                  issuedRelayEpochs, activeRelayEpoch, receiverEpoch, receiverSequence,
                  acceptedSequences, nextSequence, cleanupComplete, rejectedReplies,
                  rejectedFrames, lastOutcome, now>>

RejectMismatchedResponse(op, requestId, session) ==
    /\ rejectedReplies < MaxRejections
    /\ op \in Ops
    /\ requestId \in 1..4
    /\ session \in SessionIds
    /\ <<op, requestId, session>> # <<pendingOp, pendingRequestId, pendingSession>>
    /\ rejectedReplies' = rejectedReplies + 1
    /\ UNCHANGED <<phase, peerCid, controlSession, issuedSessions, helloDeadline,
                  activeChallenge, issuedChallenges, proofDeadline, proofAccepted,
                  completedOps, pendingOp, pendingRequestId, pendingSession,
                  replyDeadline, issuedRelayEpochs, activeRelayEpoch, receiverEpoch,
                  receiverSequence, acceptedSequences, nextSequence, cleanupComplete,
                  rejectedFrames, lastOutcome, now>>

OpenInputRelay(epoch) ==
    /\ phase = ControlReady
    /\ completedOps = Ops
    /\ pendingOp = NoOp
    /\ epoch \in RelayEpochs \ issuedRelayEpochs
    /\ phase' = Relaying
    /\ issuedRelayEpochs' = issuedRelayEpochs \cup {epoch}
    /\ activeRelayEpoch' = epoch
    /\ receiverEpoch' = epoch
    /\ receiverSequence' = 0
    /\ acceptedSequences' = {}
    /\ nextSequence' = 1
    /\ cleanupComplete' = FALSE
    /\ UNCHANGED <<peerCid, controlSession, issuedSessions, helloDeadline,
                  activeChallenge, issuedChallenges, proofDeadline, proofAccepted,
                  completedOps, pendingOp, pendingRequestId, pendingSession,
                  replyDeadline, rejectedReplies, rejectedFrames, lastOutcome, now>>

ForwardValidInput(sequence) ==
    /\ phase = Relaying
    /\ sequence \in EventSequences
    /\ sequence = receiverSequence + 1
    /\ sequence = nextSequence
    /\ receiverSequence' = sequence
    /\ acceptedSequences' = acceptedSequences \cup {sequence}
    /\ nextSequence' = sequence + 1
    /\ UNCHANGED <<phase, peerCid, controlSession, issuedSessions, helloDeadline,
                  activeChallenge, issuedChallenges, proofDeadline, proofAccepted,
                  completedOps, pendingOp, pendingRequestId, pendingSession,
                  replyDeadline, issuedRelayEpochs, activeRelayEpoch, receiverEpoch,
                  cleanupComplete, rejectedReplies, rejectedFrames, lastOutcome, now>>

RejectInvalidInput(epoch, sequence) ==
    /\ rejectedFrames < MaxRejections
    /\ epoch \in RelayEpochs \cup {NoEpoch}
    /\ sequence \in 0..MaxSequence
    /\ ~ (phase = Relaying /\ epoch = activeRelayEpoch /\ sequence = receiverSequence + 1)
    /\ rejectedFrames' = rejectedFrames + 1
    /\ UNCHANGED <<phase, peerCid, controlSession, issuedSessions, helloDeadline,
                  activeChallenge, issuedChallenges, proofDeadline, proofAccepted,
                  completedOps, pendingOp, pendingRequestId, pendingSession,
                  replyDeadline, issuedRelayEpochs, activeRelayEpoch, receiverEpoch,
                  receiverSequence, acceptedSequences, nextSequence, cleanupComplete,
                  rejectedReplies, lastOutcome, now>>

EndInputRelay ==
    /\ phase = Relaying
    /\ ClearSession(RelayEnded)
    /\ UNCHANGED <<issuedSessions, issuedChallenges, issuedRelayEpochs, rejectedReplies,
                  rejectedFrames, now>>

DisconnectControl ==
    /\ phase \in {AwaitHello, AwaitProof, ControlReady}
    /\ ClearSession(Disconnected)
    /\ UNCHANGED <<issuedSessions, issuedChallenges, issuedRelayEpochs, rejectedReplies,
                  rejectedFrames, now>>

AdvanceTime ==
    /\ now < MaxTime
    /\ now' = now + 1
    /\ IF phase = AwaitHello /\ now + 1 >= helloDeadline
       THEN ClearSession(TimedOut)
       ELSE IF phase = AwaitProof /\ now + 1 >= proofDeadline
            THEN ClearSession(TimedOut)
            ELSE IF phase = ControlReady /\ pendingOp # NoOp /\ now + 1 >= replyDeadline
            THEN ClearSession(TimedOut)
            ELSE UNCHANGED <<phase, peerCid, controlSession, helloDeadline,
                            activeChallenge, proofDeadline, proofAccepted,
                            completedOps, pendingOp, pendingRequestId, pendingSession,
                            replyDeadline, activeRelayEpoch, receiverEpoch,
                            receiverSequence, acceptedSequences, nextSequence,
                            cleanupComplete, lastOutcome>>
    /\ UNCHANGED <<issuedSessions, issuedChallenges, issuedRelayEpochs, rejectedReplies,
                  rejectedFrames>>

Next ==
    \/ \E cid \in PeerCids, session \in SessionIds : AcceptExpectedPeer(cid, session)
    \/ \E cid \in PeerCids : RejectForeignPeer(cid)
    \/ \E challenge \in ChallengeNonces : AcceptMatchingHello(challenge)
    \/ AcceptMatchingProof
    \/ RejectMismatchingHello
    \/ RejectInvalidProof
    \/ \E op \in Ops : SendControlRequest(op)
    \/ \E op \in Ops, requestId \in 1..4, session \in SessionIds :
        AcceptMatchingResponse(op, requestId, session)
    \/ \E op \in Ops, requestId \in 1..4, session \in SessionIds :
        RejectMismatchedResponse(op, requestId, session)
    \/ \E epoch \in RelayEpochs : OpenInputRelay(epoch)
    \/ \E sequence \in EventSequences : ForwardValidInput(sequence)
    \/ \E epoch \in RelayEpochs \cup {NoEpoch}, sequence \in 0..MaxSequence :
        RejectInvalidInput(epoch, sequence)
    \/ EndInputRelay
    \/ DisconnectControl
    \/ AdvanceTime

TypeOK ==
    /\ ExpectedCid \in PeerCids
    /\ PeerCids \subseteq Nat
    /\ NoCid \notin PeerCids
    /\ SessionIds \subseteq Nat
    /\ NoSession \notin SessionIds
    /\ ChallengeNonces # {}
    /\ RelayEpochs \subseteq Nat
    /\ NoEpoch \notin RelayEpochs
    /\ MaxTime \in Nat
    /\ HelloDeadline \in Nat \ {0}
    /\ ProofDeadline \in Nat \ {0}
    /\ ReplyDeadline \in Nat \ {0}
    /\ MaxSequence \in Nat \ {0}
    /\ MaxRejections \in Nat
    /\ phase \in {Idle, AwaitHello, AwaitProof, ControlReady, Relaying}
    /\ peerCid \in PeerCids \cup {NoCid}
    /\ controlSession \in SessionIds \cup {NoSession}
    /\ issuedSessions \subseteq SessionIds
    /\ helloDeadline \in Nat
    /\ activeChallenge \in ChallengeNonces \cup {NoChallenge}
    /\ issuedChallenges \subseteq ChallengeNonces
    /\ proofDeadline \in Nat
    /\ proofAccepted \in BOOLEAN
    /\ completedOps \subseteq Ops
    /\ pendingOp \in Ops \cup {NoOp}
    /\ pendingRequestId \in 0..4
    /\ pendingSession \in SessionIds \cup {NoSession}
    /\ replyDeadline \in Nat
    /\ issuedRelayEpochs \subseteq RelayEpochs
    /\ activeRelayEpoch \in RelayEpochs \cup {NoEpoch}
    /\ receiverEpoch \in RelayEpochs \cup {NoEpoch}
    /\ receiverSequence \in 0..MaxSequence
    /\ acceptedSequences \subseteq EventSequences
    /\ nextSequence \in 1..(MaxSequence + 1)
    /\ cleanupComplete \in BOOLEAN
    /\ rejectedReplies \in 0..MaxRejections
    /\ rejectedFrames \in 0..MaxRejections
    /\ lastOutcome \in {None, PeerRejected, HelloRejected, ProofRejected, TimedOut, Disconnected, RelayEnded}
    /\ now \in 0..MaxTime

AuthenticatedChannelHasLaunchBoundIdentity ==
    phase \in {AwaitHello, AwaitProof, ControlReady, Relaying} =>
        /\ peerCid = ExpectedCid
        /\ controlSession \in issuedSessions
        /\ controlSession # NoSession

HandshakeIsDeadlineBounded ==
    /\ phase = AwaitHello =>
        /\ now < helloDeadline
        /\ completedOps = {}
        /\ pendingOp = NoOp
        /\ activeChallenge = NoChallenge
        /\ activeRelayEpoch = NoEpoch
    /\ phase = AwaitProof =>
        /\ now < proofDeadline
        /\ activeChallenge \in issuedChallenges
        /\ activeChallenge # NoChallenge
        /\ ~proofAccepted
        /\ completedOps = {}
        /\ pendingOp = NoOp
        /\ activeRelayEpoch = NoEpoch

ControlAuthorityRequiresFreshProof ==
    phase \in {ControlReady, Relaying} =>
        /\ proofAccepted
        /\ activeChallenge \in issuedChallenges
        /\ activeChallenge # NoChallenge

ControlProtocolIsSerialAndExact ==
    /\ ControlPrefix(completedOps)
    /\ pendingOp # NoOp =>
        /\ phase = ControlReady
        /\ pendingOp = NextControlRequest(completedOps)
        /\ pendingOp \in Ops
        /\ pendingRequestId = RequestId(pendingOp)
        /\ pendingSession = controlSession
        /\ now < replyDeadline

InputRequiresAuthenticatedCompletedControl ==
    phase = Relaying =>
        /\ completedOps = Ops
        /\ pendingOp = NoOp
        /\ peerCid = ExpectedCid
        /\ controlSession \in issuedSessions
        /\ proofAccepted
        /\ activeChallenge \in issuedChallenges
        /\ activeRelayEpoch \in issuedRelayEpochs
        /\ activeRelayEpoch # NoEpoch
        /\ receiverEpoch = activeRelayEpoch
        /\ cleanupComplete = FALSE

InputFramesAreSingleUseAndOrdered ==
    phase = Relaying =>
        /\ acceptedSequences = AcceptedPrefix(receiverSequence)
        /\ nextSequence = receiverSequence + 1
        /\ receiverSequence < MaxSequence + 1

ClosedChannelsRetainNoAuthority ==
    phase = Idle =>
        /\ peerCid = NoCid
        /\ controlSession = NoSession
        /\ helloDeadline = 0
        /\ activeChallenge = NoChallenge
        /\ proofDeadline = 0
        /\ ~proofAccepted
        /\ completedOps = {}
        /\ pendingOp = NoOp
        /\ pendingRequestId = NoRequestId
        /\ pendingSession = NoSession
        /\ replyDeadline = 0
        /\ activeRelayEpoch = NoEpoch
        /\ receiverEpoch = NoEpoch
        /\ receiverSequence = 0
        /\ acceptedSequences = {}
        /\ nextSequence = 1
        /\ cleanupComplete

AllLiveIdentitiesWereIssued ==
    /\ controlSession # NoSession => controlSession \in issuedSessions
    /\ pendingSession # NoSession => pendingSession \in issuedSessions
    /\ activeChallenge # NoChallenge => activeChallenge \in issuedChallenges
    /\ activeRelayEpoch # NoEpoch => activeRelayEpoch \in issuedRelayEpochs
    /\ receiverEpoch # NoEpoch => receiverEpoch \in issuedRelayEpochs

RelayReceiverCannotOutliveItsControlChannel ==
    receiverEpoch # NoEpoch =>
        /\ phase = Relaying
        /\ receiverEpoch = activeRelayEpoch
        /\ peerCid = ExpectedCid

=============================================================================
