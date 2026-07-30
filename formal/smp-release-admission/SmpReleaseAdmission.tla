---------------------- MODULE SmpReleaseAdmission ----------------------
EXTENDS Naturals, FiniteSets

(***************************************************************************
The commercial launcher may start more than one RustOS vCPU only when every
high-risk prerequisite is covered by a fresh, versioned seal bound to the
exact source tree. The launched topology becomes release-admitted only after
every requested logical CPU supplies online, idle/user-dispatch, timer, and
IPI evidence from that run.

Concrete owner:
  * tools/xtask/src/kvm/guest.rs

Checked-in booleans are not evidence. A stale or source-mismatched seal and a
partial per-CPU runtime matrix both fail closed.
***************************************************************************)

CONSTANTS CpuCounts, Prerequisites, ExpectedEvidenceVersion

VARIABLES requested, completed, evidenceVersion, sourceBound, evidenceFresh,
          runtimeCpuEvidence, phase
vars == <<requested, completed, evidenceVersion, sourceBound, evidenceFresh,
          runtimeCpuEvidence, phase>>

Init ==
    /\ requested \in CpuCounts
    /\ completed = {}
    /\ evidenceVersion = ExpectedEvidenceVersion
    /\ sourceBound = TRUE
    /\ evidenceFresh = TRUE
    /\ runtimeCpuEvidence = {}
    /\ phase = "assessing"

CompletePrerequisite(prerequisite) ==
    /\ phase = "assessing"
    /\ prerequisite \in Prerequisites \ completed
    /\ completed' = completed \cup {prerequisite}
    /\ UNCHANGED <<requested, evidenceVersion, sourceBound, evidenceFresh,
                   runtimeCpuEvidence, phase>>

InvalidateEvidenceVersion ==
    /\ phase = "assessing"
    /\ evidenceVersion = ExpectedEvidenceVersion
    /\ evidenceVersion' = 0
    /\ UNCHANGED <<requested, completed, sourceBound, evidenceFresh,
                   runtimeCpuEvidence, phase>>

InvalidateSourceBinding ==
    /\ phase = "assessing"
    /\ sourceBound
    /\ sourceBound' = FALSE
    /\ UNCHANGED <<requested, completed, evidenceVersion, evidenceFresh,
                   runtimeCpuEvidence, phase>>

ExpireEvidence ==
    /\ phase = "assessing"
    /\ evidenceFresh
    /\ evidenceFresh' = FALSE
    /\ UNCHANGED <<requested, completed, evidenceVersion, sourceBound,
                   runtimeCpuEvidence, phase>>

EvidenceValid ==
    /\ evidenceVersion = ExpectedEvidenceVersion
    /\ sourceBound
    /\ evidenceFresh

RequestedCpuSet == 0..(requested - 1)

AdmitUniprocessor ==
    /\ phase = "assessing"
    /\ requested = 1
    /\ phase' = "launch-admitted"
    /\ UNCHANGED <<requested, completed, evidenceVersion, sourceBound,
                   evidenceFresh, runtimeCpuEvidence>>

AdmitSmpLaunch ==
    /\ phase = "assessing"
    /\ requested > 1
    /\ completed = Prerequisites
    /\ EvidenceValid
    /\ phase' = "launch-admitted"
    /\ UNCHANGED <<requested, completed, evidenceVersion, sourceBound,
                   evidenceFresh, runtimeCpuEvidence>>

RejectSmpLaunch ==
    /\ phase = "assessing"
    /\ requested > 1
    /\ (completed # Prerequisites \/ ~EvidenceValid)
    /\ phase' = "rejected"
    /\ UNCHANGED <<requested, completed, evidenceVersion, sourceBound,
                   evidenceFresh, runtimeCpuEvidence>>

RecordCpuRuntime(cpu) ==
    /\ phase = "launch-admitted"
    /\ cpu \in RequestedCpuSet \ runtimeCpuEvidence
    /\ runtimeCpuEvidence' = runtimeCpuEvidence \cup {cpu}
    /\ UNCHANGED <<requested, completed, evidenceVersion, sourceBound,
                   evidenceFresh, phase>>

AdmitRuntime ==
    /\ phase = "launch-admitted"
    /\ runtimeCpuEvidence = RequestedCpuSet
    /\ phase' = "admitted"
    /\ UNCHANGED <<requested, completed, evidenceVersion, sourceBound,
                   evidenceFresh, runtimeCpuEvidence>>

Terminal ==
    /\ phase \in {"admitted", "rejected"}
    /\ UNCHANGED vars

Next ==
    \/ AdmitUniprocessor
    \/ AdmitSmpLaunch
    \/ RejectSmpLaunch
    \/ AdmitRuntime
    \/ InvalidateEvidenceVersion
    \/ InvalidateSourceBinding
    \/ ExpireEvidence
    \/ Terminal
    \/ \E prerequisite \in Prerequisites:
        CompletePrerequisite(prerequisite)
    \/ \E cpu \in RequestedCpuSet:
        RecordCpuRuntime(cpu)

Spec == Init /\ [][Next]_vars

TypeOK ==
    /\ requested \in CpuCounts
    /\ requested >= 1
    /\ completed \subseteq Prerequisites
    /\ evidenceVersion \in {0, ExpectedEvidenceVersion}
    /\ sourceBound \in BOOLEAN
    /\ evidenceFresh \in BOOLEAN
    /\ runtimeCpuEvidence \subseteq RequestedCpuSet
    /\ phase \in {"assessing", "launch-admitted", "admitted", "rejected"}

NoPartialSmpLaunchAdmission ==
    phase \in {"launch-admitted", "admitted"} /\ requested > 1 =>
        /\ completed = Prerequisites
        /\ EvidenceValid

UniprocessorNeedsNoFalseSmpClaim ==
    phase \in {"launch-admitted", "admitted"} /\ requested = 1 =>
        completed = {} \/ completed \subseteq Prerequisites

RejectionPublishesNoSmp ==
    phase = "rejected" =>
        /\ requested > 1
        /\ (completed # Prerequisites \/ ~EvidenceValid)

RuntimeAdmissionRequiresEveryCpu ==
    phase = "admitted" => runtimeCpuEvidence = RequestedCpuSet

=============================================================================
