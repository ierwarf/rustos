--------------------- MODULE FilesystemContentIntegrity ----------------------
EXTENDS Naturals, FiniteSets

(*******************************************************************************
Owner: vfsd/storaged; kernel boot-volume code is the bounded read substrate.
Linearization point: VerifyRead, where bytes reconstructed from signed extents
must match the staged content digest. The enabled root image is immutable;
mutation requests fail closed rather than creating an unjournaled state.
*******************************************************************************)

CONSTANTS Files, Digests

NoFile == "none"
NoDigest == "none"
ExpectedDigest(file) == IF file = "rootd" THEN "good-rootd" ELSE "good-loaderd"
VARIABLES available, pendingFile, observedDigest, result, mutationRejected
vars == <<available, pendingFile, observedDigest, result, mutationRejected>>

Results == {"idle", "pending", "verified", "rejected", "io-error"}

Init ==
    /\ available = TRUE
    /\ pendingFile = NoFile
    /\ observedDigest = NoDigest
    /\ result = "idle"
    /\ mutationRejected = FALSE

Read(file, digest) ==
    /\ result \in {"idle", "verified", "rejected", "io-error"}
    /\ available
    /\ pendingFile' = file
    /\ observedDigest' = digest
    /\ result' = "pending"
    /\ UNCHANGED <<available, mutationRejected>>

VerifyRead ==
    /\ result = "pending"
    /\ result' = IF observedDigest = ExpectedDigest(pendingFile)
                  THEN "verified" ELSE "rejected"
    /\ UNCHANGED <<available, pendingFile, observedDigest, mutationRejected>>

RejectMutation ==
    /\ mutationRejected' = TRUE
    /\ UNCHANGED <<available, pendingFile, observedDigest, result>>

MediaLoss ==
    /\ available
    /\ available' = FALSE
    /\ result' = "io-error"
    /\ pendingFile' = NoFile
    /\ observedDigest' = NoDigest
    /\ UNCHANGED mutationRejected

Recover ==
    /\ ~available
    /\ available' = TRUE
    /\ result' = "idle"
    /\ UNCHANGED <<pendingFile, observedDigest, mutationRejected>>

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
    /\ mutationRejected \in BOOLEAN

VerifiedContentMatchesManifest ==
    result = "verified" => observedDigest = ExpectedDigest(pendingFile)
BadContentNeverVerifies ==
    result = "pending" /\ observedDigest # ExpectedDigest(pendingFile)
        => result # "verified"
PendingEventuallySettles == [] (result = "pending" => <> (result # "pending"))

=============================================================================
