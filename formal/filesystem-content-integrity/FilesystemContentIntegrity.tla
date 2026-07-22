--------------------- MODULE FilesystemContentIntegrity ----------------------
EXTENDS Naturals, FiniteSets

(*******************************************************************************
Owner: vfsd/storaged; kernel boot-volume code is the bounded read substrate.
Linearization point: VerifyRead, where bytes reconstructed from signed extents
must match the staged content digest. The enabled root image is immutable;
mutation requests fail closed rather than creating an unjournaled state.
*******************************************************************************)

CONSTANTS Files, Digests, MaxMutationAttempts

NoFile == "none"
NoDigest == "none"
ExpectedDigest(file) == IF file = "rootd" THEN "good-rootd" ELSE "good-loaderd"
VARIABLES available, pendingFile, observedDigest, result, verifiedFile,
          verifiedDigest, contentEpoch, mutationAttempts
vars == <<available, pendingFile, observedDigest, result, verifiedFile,
          verifiedDigest, contentEpoch, mutationAttempts>>

Results == {"idle", "pending", "verified", "rejected", "io-error"}

Init ==
    /\ available = TRUE
    /\ pendingFile = NoFile
    /\ observedDigest = NoDigest
    /\ result = "idle"
    /\ verifiedFile = NoFile
    /\ verifiedDigest = NoDigest
    /\ contentEpoch = 0
    /\ mutationAttempts = 0

Read(file, digest) ==
    /\ result \in {"idle", "verified", "rejected", "io-error"}
    /\ available
    /\ pendingFile' = file
    /\ observedDigest' = digest
    /\ result' = "pending"
    /\ verifiedFile' = NoFile
    /\ verifiedDigest' = NoDigest
    /\ UNCHANGED <<available, contentEpoch, mutationAttempts>>

VerifyRead ==
    /\ result = "pending"
    /\ result' = IF observedDigest = ExpectedDigest(pendingFile)
                  THEN "verified" ELSE "rejected"
    /\ verifiedFile' = IF result' = "verified" THEN pendingFile ELSE NoFile
    /\ verifiedDigest' = IF result' = "verified" THEN observedDigest ELSE NoDigest
    /\ UNCHANGED <<available, pendingFile, observedDigest, contentEpoch,
                  mutationAttempts>>

RejectMutation ==
    /\ mutationAttempts < MaxMutationAttempts
    /\ mutationAttempts' = mutationAttempts + 1
    /\ UNCHANGED <<available, pendingFile, observedDigest, result,
                  verifiedFile, verifiedDigest, contentEpoch>>

MediaLoss ==
    /\ available
    /\ available' = FALSE
    /\ result' = "io-error"
    /\ pendingFile' = NoFile
    /\ observedDigest' = NoDigest
    /\ verifiedFile' = NoFile
    /\ verifiedDigest' = NoDigest
    /\ UNCHANGED <<contentEpoch, mutationAttempts>>

Recover ==
    /\ ~available
    /\ available' = TRUE
    /\ result' = "idle"
    /\ UNCHANGED <<pendingFile, observedDigest, verifiedFile, verifiedDigest,
                  contentEpoch, mutationAttempts>>

Next ==
    \/ \E file \in Files, digest \in Digests: Read(file, digest)
    \/ VerifyRead
    \/ RejectMutation
    \/ MediaLoss
    \/ Recover

Spec == Init /\ [][Next]_vars /\ WF_vars(VerifyRead)

TypeOK ==
    /\ available \in BOOLEAN
    /\ pendingFile \in Files \cup {NoFile}
    /\ observedDigest \in Digests \cup {NoDigest}
    /\ result \in Results
    /\ verifiedFile \in Files \cup {NoFile}
    /\ verifiedDigest \in Digests \cup {NoDigest}
    /\ contentEpoch \in Nat
    /\ mutationAttempts \in 0..MaxMutationAttempts

VerifiedContentMatchesManifest ==
    result = "verified" =>
        /\ verifiedFile = pendingFile
        /\ verifiedDigest = observedDigest
        /\ verifiedDigest = ExpectedDigest(verifiedFile)
BadContentNeverVerifies ==
    observedDigest # ExpectedDigest(pendingFile) => result # "verified"
RejectedMutationNeverChangesContent ==
    contentEpoch = 0
TerminalReadHasExactEvidenceShape ==
    /\ result = "verified" => verifiedFile # NoFile /\ verifiedDigest # NoDigest
    /\ result \in {"idle", "pending", "rejected", "io-error"} =>
        verifiedFile = NoFile /\ verifiedDigest = NoDigest
PendingEventuallySettles == [] (result = "pending" => <> (result # "pending"))

=============================================================================
