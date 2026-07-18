------------------------- MODULE DvmAmdgpuEvidence -------------------------
EXTENDS Naturals

(***************************************************************************
Authenticated physical AMD display evidence admitted by tools/hostd and the
Linux DVM relay.  The finite two-sample abstraction represents the signed
production policy's five consecutive one-second samples.  A sample is usable
only when it advances the relay-owned sequence and proves the exact AMDGPU
identity, direct DMA-BUF scanout with no CPU copy, minimum page-flip rate, and
bounded page-flip/atomic-commit latency.  A stale or failing monitored sample
revokes readiness rather than retaining the last successful measurement.
***************************************************************************)

ExactAmd == "amdgpu-1002-1900"
RequiredSamples == 2
MaxSequence == 3

VARIABLES policySigned,
          hostIdentity,
          dvmIdentity,
          relayReady,
          sampleSequence,
          lastAcceptedSequence,
          sampleFresh,
          directScanout,
          cpuCopyZero,
          frameRatePassed,
          pageflipLatencyPassed,
          atomicCommitPassed,
          consecutiveSamples,
          admitted,
          revoked

vars == <<policySigned, hostIdentity, dvmIdentity, relayReady,
          sampleSequence, lastAcceptedSequence, sampleFresh, directScanout,
          cpuCopyZero, frameRatePassed, pageflipLatencyPassed,
          atomicCommitPassed, consecutiveSamples, admitted, revoked>>

Init ==
    /\ policySigned = FALSE
    /\ hostIdentity = "none"
    /\ dvmIdentity = "none"
    /\ relayReady = FALSE
    /\ sampleSequence = 0
    /\ lastAcceptedSequence = 0
    /\ sampleFresh = FALSE
    /\ directScanout = FALSE
    /\ cpuCopyZero = FALSE
    /\ frameRatePassed = FALSE
    /\ pageflipLatencyPassed = FALSE
    /\ atomicCommitPassed = FALSE
    /\ consecutiveSamples = 0
    /\ admitted = FALSE
    /\ revoked = FALSE

SignPolicy ==
    /\ ~policySigned
    /\ policySigned' = TRUE
    /\ UNCHANGED <<hostIdentity, dvmIdentity, relayReady, sampleSequence,
                  lastAcceptedSequence, sampleFresh, directScanout,
                  cpuCopyZero, frameRatePassed, pageflipLatencyPassed,
                  atomicCommitPassed, consecutiveSamples, admitted, revoked>>

ObserveHost(identity) ==
    /\ hostIdentity = "none"
    /\ identity \in {ExactAmd, "other"}
    /\ hostIdentity' = identity
    /\ UNCHANGED <<policySigned, dvmIdentity, relayReady, sampleSequence,
                  lastAcceptedSequence, sampleFresh, directScanout,
                  cpuCopyZero, frameRatePassed, pageflipLatencyPassed,
                  atomicCommitPassed, consecutiveSamples, admitted, revoked>>

StartRelay(identity) ==
    /\ policySigned
    /\ hostIdentity = ExactAmd
    /\ ~relayReady
    /\ ~revoked
    /\ identity \in {ExactAmd, "other"}
    /\ dvmIdentity' = identity
    /\ relayReady' = TRUE
    /\ UNCHANGED <<policySigned, hostIdentity, sampleSequence,
                  lastAcceptedSequence, sampleFresh, directScanout,
                  cpuCopyZero, frameRatePassed, pageflipLatencyPassed,
                  atomicCommitPassed, consecutiveSamples, admitted, revoked>>

PublishPassingSample ==
    /\ relayReady
    /\ sampleSequence < MaxSequence
    /\ sampleSequence' = sampleSequence + 1
    /\ sampleFresh' = TRUE
    /\ directScanout' = TRUE
    /\ cpuCopyZero' = TRUE
    /\ frameRatePassed' = TRUE
    /\ pageflipLatencyPassed' = TRUE
    /\ atomicCommitPassed' = TRUE
    /\ UNCHANGED <<policySigned, hostIdentity, dvmIdentity, relayReady,
                  lastAcceptedSequence, consecutiveSamples, admitted, revoked>>

PublishFailingSample ==
    /\ relayReady
    /\ sampleSequence < MaxSequence
    /\ sampleSequence' = sampleSequence + 1
    /\ sampleFresh' = FALSE
    /\ directScanout' = FALSE
    /\ cpuCopyZero' = FALSE
    /\ frameRatePassed' = FALSE
    /\ pageflipLatencyPassed' = FALSE
    /\ atomicCommitPassed' = FALSE
    /\ relayReady' = FALSE
    /\ consecutiveSamples' = 0
    /\ admitted' = FALSE
    /\ revoked' = TRUE
    /\ UNCHANGED <<policySigned, hostIdentity, dvmIdentity,
                  lastAcceptedSequence>>

AcceptPassingSample ==
    /\ relayReady
    /\ dvmIdentity = ExactAmd
    /\ sampleSequence > lastAcceptedSequence
    /\ sampleFresh /\ directScanout /\ cpuCopyZero
    /\ frameRatePassed /\ pageflipLatencyPassed /\ atomicCommitPassed
    /\ lastAcceptedSequence' = sampleSequence
    /\ consecutiveSamples' =
        IF consecutiveSamples + 1 >= RequiredSamples
        THEN RequiredSamples
        ELSE consecutiveSamples + 1
    /\ admitted' = (consecutiveSamples + 1 >= RequiredSamples)
    /\ UNCHANGED <<policySigned, hostIdentity, dvmIdentity, relayReady,
                  sampleSequence, sampleFresh, directScanout, cpuCopyZero,
                  frameRatePassed, pageflipLatencyPassed, atomicCommitPassed,
                  revoked>>

RejectWrongIdentity ==
    /\ relayReady
    /\ dvmIdentity # ExactAmd
    /\ relayReady' = FALSE
    /\ consecutiveSamples' = 0
    /\ admitted' = FALSE
    /\ revoked' = TRUE
    /\ UNCHANGED <<policySigned, hostIdentity, dvmIdentity, sampleSequence,
                  lastAcceptedSequence, sampleFresh, directScanout,
                  cpuCopyZero, frameRatePassed, pageflipLatencyPassed,
                  atomicCommitPassed>>

RevokeStaleEvidence ==
    /\ relayReady
    /\ admitted
    /\ sampleSequence = lastAcceptedSequence
    /\ relayReady' = FALSE
    /\ consecutiveSamples' = 0
    /\ admitted' = FALSE
    /\ revoked' = TRUE
    /\ UNCHANGED <<policySigned, hostIdentity, dvmIdentity, sampleSequence,
                  lastAcceptedSequence, sampleFresh, directScanout,
                  cpuCopyZero, frameRatePassed, pageflipLatencyPassed,
                  atomicCommitPassed>>

Next ==
    \/ SignPolicy
    \/ \E identity \in {ExactAmd, "other"}: ObserveHost(identity)
    \/ \E identity \in {ExactAmd, "other"}: StartRelay(identity)
    \/ PublishPassingSample
    \/ PublishFailingSample
    \/ AcceptPassingSample
    \/ RejectWrongIdentity
    \/ RevokeStaleEvidence

TypeOK ==
    /\ policySigned \in BOOLEAN
    /\ hostIdentity \in {"none", ExactAmd, "other"}
    /\ dvmIdentity \in {"none", ExactAmd, "other"}
    /\ relayReady \in BOOLEAN
    /\ sampleSequence \in 0..MaxSequence
    /\ lastAcceptedSequence \in 0..MaxSequence
    /\ sampleFresh \in BOOLEAN
    /\ directScanout \in BOOLEAN
    /\ cpuCopyZero \in BOOLEAN
    /\ frameRatePassed \in BOOLEAN
    /\ pageflipLatencyPassed \in BOOLEAN
    /\ atomicCommitPassed \in BOOLEAN
    /\ consecutiveSamples \in 0..RequiredSamples
    /\ admitted \in BOOLEAN
    /\ revoked \in BOOLEAN

AdmittedRequiresExactAmd ==
    admitted => policySigned /\ hostIdentity = ExactAmd /\ dvmIdentity = ExactAmd

AdmittedRequiresPassingEvidence ==
    admitted => sampleFresh /\ directScanout /\ cpuCopyZero /\
                frameRatePassed /\ pageflipLatencyPassed /\ atomicCommitPassed

AdmittedRequiresConsecutiveFreshSamples ==
    admitted => consecutiveSamples = RequiredSamples /\
                lastAcceptedSequence <= sampleSequence

AcceptedSequenceNeverLeadsRelay == lastAcceptedSequence <= sampleSequence
RevokedEvidenceIsOffline == revoked => ~relayReady /\ ~admitted

Spec == Init /\ [][Next]_vars

=============================================================================
