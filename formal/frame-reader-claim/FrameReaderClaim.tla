----------------------------- MODULE FrameReaderClaim -----------------------------
EXTENDS Naturals, FiniteSets
CONSTANTS Readers, MaxGeneration
\* The distinguished Selection reader is a non-Copy original validated loan.
\* ClaimObserve captures its physical/reader custody; ClaimLock abstracts
\* rq40 -> class section -> original Context48/Domain49 -> leaves50..54.
\* Only that SAME reader may be promoted at the exact owner CAS. Independent
\* readers still refuse. Promotion ends byte reads; ClaimRelease abstracts its
\* outside-leaf conversion into the Prepared allocation pin (not hardware run).
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
    /\ phase = "Idle" /\ leaf = "None" /\ owner \in {"Local", "Direct"}
    /\ ~writer /\ ~closing
    /\ phase' = "Seen" /\ expectedOwner' = owner /\ expectedGeneration' = generation
    /\ readers' = readers \cup {"Selection"}
    /\ selectedClassEpoch' = classEpoch /\ pinned' = TRUE
    /\ UNCHANGED <<owner, generation, queued, leaf, admitted, writer, closing,
                    badClaim, classEpoch, contextCredit, domainCredit, fundingClosed,
                    jointAdmitted, badAdmission>>
ClaimLock ==
    /\ phase = "Seen" /\ leaf = "None"
    /\ leaf' = "Claim" /\ phase' = "Locked"
    /\ admitted' = (readers = {"Selection"} /\ ~writer /\ ~closing)
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
    /\ badClaim' = (readers # {"Selection"} \/ writer \/ closing
                    \/ owner # expectedOwner \/ generation # expectedGeneration)
    /\ badAdmission' = (~contextCredit \/ ~domainCredit \/ fundingClosed
                        \/ classEpoch # selectedClassEpoch \/ ~pinned)
    /\ owner' = "Running" /\ generation' = generation + 1 /\ queued' = FALSE
    /\ leaf' = "None" /\ phase' = "Promoted"
    /\ UNCHANGED <<readers, expectedOwner, expectedGeneration, admitted, writer, closing,
                    classEpoch, selectedClassEpoch, contextCredit, domainCredit,
                    fundingClosed, jointAdmitted, pinned>>
ClaimRelease ==
    /\ phase = "Promoted" /\ leaf = "None"
    /\ readers' = readers \ {"Selection"} /\ phase' = "Idle"
    /\ UNCHANGED <<owner, generation, queued, leaf, expectedOwner, expectedGeneration,
                    admitted, writer, closing, badClaim, joint>>
ClaimRefuse ==
    /\ phase = "Locked" /\ leaf = "Claim"
    /\ ~admitted \/ ~jointAdmitted \/ owner # expectedOwner \/ generation # expectedGeneration
        \/ generation = MaxGeneration
    /\ leaf' = "None" /\ phase' = "Idle" /\ pinned' = FALSE
    /\ readers' = readers \ {"Selection"}
    /\ UNCHANGED <<owner, generation, queued, expectedOwner,
                    expectedGeneration, admitted, writer, closing, badClaim,
                    classEpoch, selectedClassEpoch, contextCredit, domainCredit,
                    fundingClosed, jointAdmitted, badAdmission>>
\* Owner-word producers may republish while the resource leaf is held. Only
\* the original expected generation may be spent; do not reacquire a new one.
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
        \/ ClaimObserve \/ ClaimLock \/ ClaimCommit \/ ClaimRelease \/ ClaimRefuse
        \/ Republish \/ WriterStart \/ WriterFinish \/ Close
        \/ ClassPublish \/ ContextDebit \/ DomainDebit \/ CloseFunding \/ TerminalStutter
TypeOK == /\ owner \in {"Local", "Direct", "Running"}
          /\ generation \in 0..MaxGeneration /\ queued \in BOOLEAN
          /\ readers \subseteq Readers \cup {"Selection"} /\ leaf \in {"None", "Claim"}
          /\ phase \in {"Idle", "Seen", "Locked", "Promoted"}
          /\ expectedOwner \in {"Local", "Direct"}
          /\ expectedGeneration \in 0..MaxGeneration
          /\ admitted \in BOOLEAN /\ writer \in BOOLEAN /\ closing \in BOOLEAN
          /\ badClaim \in BOOLEAN
          /\ classEpoch \in 0..MaxGeneration /\ selectedClassEpoch \in 0..MaxGeneration
          /\ contextCredit \in BOOLEAN /\ domainCredit \in BOOLEAN
          /\ fundingClosed \in BOOLEAN /\ jointAdmitted \in BOOLEAN
          /\ pinned \in BOOLEAN /\ badAdmission \in BOOLEAN
NoRunningDuringNonexecutingRead == owner = "Running" => readers \subseteq {"Selection"}
PromotedReaderCannotBeObserved == owner = "Running" /\ "Selection" \in readers => phase = "Promoted"
SelectionSpansJoin == phase \in {"Seen", "Locked", "Promoted"} => "Selection" \in readers
NoWriterDuringRead == writer => readers = {}
ExactClaimHasExclusiveBytes == ~badClaim
MembershipMatchesOwner == queued = (owner = "Local")
LeafMatchesClaimPhase == (leaf = "Claim") = (phase = "Locked")
OriginalPinsSurviveJoin == phase \in {"Seen", "Locked", "Promoted"} => pinned
ExactJointAdmissionAtCAS == ~badAdmission
Spec == Init /\ [][Next]_vars
===================================================================================
