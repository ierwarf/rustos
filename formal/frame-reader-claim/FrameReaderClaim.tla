----------------------------- MODULE FrameReaderClaim -----------------------------
EXTENDS Naturals, FiniteSets
CONSTANTS Readers, MaxGeneration
\* Source-refining bounded selection/class/original-funding/resource CAS join.
\* ClaimObserve retains the original physical frame/funding pins. ClaimLock
\* abstracts rq40 -> class45 -> Context48/Domain49 -> claim leaves50..54;
\* the exact owner CAS occurs while ALL admission leaves are still held.
\* ClassPublish abstracts both live base-weight replacement and ledger edges;
\* ContextDebit/DomainDebit/CloseFunding are independent competing producers.
\* This is NOT MM/Current/Transition/register/RSP/settlement/terminal/reuse proof.
VARIABLES owner, generation, queued, readers, leaf, phase, expectedOwner,
          expectedGeneration, admitted, writer, closing, badClaim,
          classEpoch, selectedClassEpoch, contextCredit, domainCredit,
          fundingClosed, jointAdmitted, pinned, badAdmission
joint == <<classEpoch, selectedClassEpoch, contextCredit, domainCredit,
           fundingClosed, jointAdmitted, pinned, badAdmission>>
vars == <<owner, generation, queued, readers, leaf, phase, expectedOwner,
          expectedGeneration, admitted, writer, closing, badClaim, joint>>
Init == /\ owner = "Local" /\ generation = 0 /\ queued = TRUE
        /\ readers = {} /\ leaf = "None" /\ phase = "Idle"
        /\ expectedOwner = "Local" /\ expectedGeneration = 0
        /\ admitted = FALSE /\ writer = FALSE /\ closing = FALSE /\ badClaim = FALSE
        /\ classEpoch = 0 /\ selectedClassEpoch = 0
        /\ contextCredit = TRUE /\ domainCredit = TRUE /\ fundingClosed = FALSE
        /\ jointAdmitted = FALSE /\ pinned = FALSE /\ badAdmission = FALSE
ReaderAcquire(r) ==
    /\ leaf = "None" /\ r \notin readers
    /\ owner \in {"Local", "Direct"} /\ ~writer /\ ~closing
    /\ readers' = readers \cup {r}
    /\ UNCHANGED <<owner, generation, queued, leaf, phase, expectedOwner,
                    expectedGeneration, admitted, writer, closing, badClaim, joint>>
ReaderRelease(r) ==
    /\ leaf = "None" /\ r \in readers
    /\ readers' = readers \ {r}
    /\ UNCHANGED <<owner, generation, queued, leaf, phase, expectedOwner,
                    expectedGeneration, admitted, writer, closing, badClaim, joint>>
ClaimObserve ==
    /\ phase = "Idle" /\ owner \in {"Local", "Direct"}
    /\ phase' = "Seen" /\ expectedOwner' = owner /\ expectedGeneration' = generation
    /\ selectedClassEpoch' = classEpoch /\ pinned' = TRUE
    /\ UNCHANGED <<owner, generation, queued, readers, leaf, admitted, writer, closing,
                    badClaim, classEpoch, contextCredit, domainCredit, fundingClosed,
                    jointAdmitted, badAdmission>>
ClaimLock ==
    /\ phase = "Seen" /\ leaf = "None"
    /\ leaf' = "Claim" /\ phase' = "Locked"
    /\ admitted' = (readers = {} /\ ~writer /\ ~closing)
    /\ jointAdmitted' = (classEpoch = selectedClassEpoch /\ contextCredit
                         /\ domainCredit /\ ~fundingClosed /\ pinned)
    /\ UNCHANGED <<owner, generation, queued, readers, expectedOwner,
                    expectedGeneration, writer, closing, badClaim, classEpoch,
                    selectedClassEpoch, contextCredit, domainCredit, fundingClosed,
                    pinned, badAdmission>>
ClaimCommit ==
    /\ phase = "Locked" /\ leaf = "Claim" /\ admitted /\ jointAdmitted
    /\ owner = expectedOwner /\ generation = expectedGeneration
    /\ generation < MaxGeneration
    /\ badClaim' = (readers # {} \/ writer \/ closing
                    \/ owner # expectedOwner \/ generation # expectedGeneration)
    /\ badAdmission' = (~contextCredit \/ ~domainCredit \/ fundingClosed
                        \/ classEpoch # selectedClassEpoch \/ ~pinned)
    /\ owner' = "Running" /\ generation' = generation + 1 /\ queued' = FALSE
    /\ leaf' = "None" /\ phase' = "Idle"
    /\ UNCHANGED <<readers, expectedOwner, expectedGeneration, admitted, writer, closing,
                    classEpoch, selectedClassEpoch, contextCredit, domainCredit,
                    fundingClosed, jointAdmitted, pinned>>
ClaimRefuse ==
    /\ phase = "Locked" /\ leaf = "Claim"
    /\ ~admitted \/ ~jointAdmitted \/ owner # expectedOwner \/ generation # expectedGeneration
        \/ generation = MaxGeneration
    /\ leaf' = "None" /\ phase' = "Idle" /\ pinned' = FALSE
    /\ UNCHANGED <<owner, generation, queued, readers, expectedOwner,
                    expectedGeneration, admitted, writer, closing, badClaim,
                    classEpoch, selectedClassEpoch, contextCredit, domainCredit,
                    fundingClosed, jointAdmitted, badAdmission>>
\* An independent owner-word writer can republish while the resource leaf is
\* held. The CAS must compare the old exact word, not replay a fresh generation.
Republish ==
    /\ owner \in {"Local", "Direct"} /\ generation < MaxGeneration
    /\ owner' = IF owner = "Local" THEN "Direct" ELSE "Local"
    /\ generation' = generation + 1 /\ queued' = (owner' = "Local")
    /\ UNCHANGED <<readers, leaf, phase, expectedOwner, expectedGeneration,
                    admitted, writer, closing, badClaim, joint>>
WriterStart ==
    /\ leaf = "None" /\ ~writer /\ ~closing /\ readers = {}
    /\ owner # "Running" /\ writer' = TRUE
    /\ UNCHANGED <<owner, generation, queued, readers, leaf, phase, expectedOwner,
                    expectedGeneration, admitted, closing, badClaim, joint>>
WriterFinish ==
    /\ leaf = "None" /\ writer /\ writer' = FALSE
    /\ UNCHANGED <<owner, generation, queued, readers, leaf, phase, expectedOwner,
                    expectedGeneration, admitted, closing, badClaim, joint>>
Close ==
    /\ leaf = "None" /\ ~closing /\ closing' = TRUE
    /\ UNCHANGED <<owner, generation, queued, readers, leaf, phase, expectedOwner,
                    expectedGeneration, admitted, writer, badClaim, joint>>
ClassPublish ==
    /\ leaf = "None" /\ classEpoch < MaxGeneration
    /\ classEpoch' = classEpoch + 1
    /\ UNCHANGED <<owner, generation, queued, readers, leaf, phase, expectedOwner,
                    expectedGeneration, admitted, writer, closing, badClaim,
                    selectedClassEpoch, contextCredit, domainCredit, fundingClosed,
                    jointAdmitted, pinned, badAdmission>>
ContextDebit ==
    /\ leaf = "None" /\ contextCredit /\ contextCredit' = FALSE
    /\ UNCHANGED <<owner, generation, queued, readers, leaf, phase, expectedOwner,
                    expectedGeneration, admitted, writer, closing, badClaim,
                    classEpoch, selectedClassEpoch, domainCredit, fundingClosed,
                    jointAdmitted, pinned, badAdmission>>
DomainDebit ==
    /\ leaf = "None" /\ domainCredit /\ domainCredit' = FALSE
    /\ UNCHANGED <<owner, generation, queued, readers, leaf, phase, expectedOwner,
                    expectedGeneration, admitted, writer, closing, badClaim,
                    classEpoch, selectedClassEpoch, contextCredit, fundingClosed,
                    jointAdmitted, pinned, badAdmission>>
CloseFunding ==
    /\ leaf = "None" /\ ~fundingClosed /\ fundingClosed' = TRUE
    /\ UNCHANGED <<owner, generation, queued, readers, leaf, phase, expectedOwner,
                    expectedGeneration, admitted, writer, closing, badClaim,
                    classEpoch, selectedClassEpoch, contextCredit, domainCredit,
                    jointAdmitted, pinned, badAdmission>>
TerminalStutter == /\ (owner = "Running" \/ closing)
                   /\ phase = "Idle" /\ leaf = "None" /\ UNCHANGED vars
Next == (\E r \in Readers: ReaderAcquire(r) \/ ReaderRelease(r))
        \/ ClaimObserve \/ ClaimLock \/ ClaimCommit \/ ClaimRefuse
        \/ Republish \/ WriterStart \/ WriterFinish \/ Close
        \/ ClassPublish \/ ContextDebit \/ DomainDebit \/ CloseFunding \/ TerminalStutter
TypeOK == /\ owner \in {"Local", "Direct", "Running"}
          /\ generation \in 0..MaxGeneration /\ queued \in BOOLEAN
          /\ readers \subseteq Readers /\ leaf \in {"None", "Claim"}
          /\ phase \in {"Idle", "Seen", "Locked"}
          /\ expectedOwner \in {"Local", "Direct"}
          /\ expectedGeneration \in 0..MaxGeneration
          /\ admitted \in BOOLEAN /\ writer \in BOOLEAN /\ closing \in BOOLEAN
          /\ badClaim \in BOOLEAN
          /\ classEpoch \in 0..MaxGeneration /\ selectedClassEpoch \in 0..MaxGeneration
          /\ contextCredit \in BOOLEAN /\ domainCredit \in BOOLEAN
          /\ fundingClosed \in BOOLEAN /\ jointAdmitted \in BOOLEAN
          /\ pinned \in BOOLEAN /\ badAdmission \in BOOLEAN
NoRunningDuringNonexecutingRead == readers # {} => owner # "Running"
NoWriterDuringRead == writer => readers = {}
ExactClaimHasExclusiveBytes == ~badClaim
MembershipMatchesOwner == queued = (owner = "Local")
LeafMatchesClaimPhase == (leaf = "Claim") = (phase = "Locked")
OriginalPinsSurviveJoin == phase \in {"Seen", "Locked"} => pinned
ExactJointAdmissionAtCAS == ~badAdmission
Spec == Init /\ [][Next]_vars
===================================================================================
