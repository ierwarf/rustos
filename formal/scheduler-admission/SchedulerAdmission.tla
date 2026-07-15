--------------------------- MODULE SchedulerAdmission ---------------------------
EXTENDS Naturals

(*******************************************************************************
Models the runtime launch weight admission boundary in
`services/runtimed/src/spawn.rs`.

A launch-catalog record is mutable policy input, not a capability to enter the
kernel's strict System scheduling band. The sole runtime-granted exception is
the exact UI-server path, whose effective weight is pinned in source. Every
other launch is clamped below System admission, including a record altered to
the largest representable request. This preserves the scheduler's ability to
serve input, recovery, and bootstrap rather than allowing a desktop entry to
turn fairness metadata into a denial-of-service primitive.

The model lets an adversary rewrite registry weights before and during an
outstanding request. Admission is weakly fair so an accepted request cannot
remain indefinitely pending behind registry churn.
*******************************************************************************)

CONSTANTS Programs, UiProgram, MinWeight, MaxUserWeight, SystemWeight, MaxRequested

NoProgram == 0
Weights == 0..MaxRequested

VARIABLES registryWeight,
          pending,
          launched,
          admittedWeight

vars == <<registryWeight, pending, launched, admittedWeight>>

AdmittedFor(program) ==
    IF program = UiProgram THEN SystemWeight
    ELSE IF registryWeight[program] < MinWeight THEN MinWeight
    ELSE IF registryWeight[program] > MaxUserWeight THEN MaxUserWeight
    ELSE registryWeight[program]

Init ==
    /\ Programs # {}
    /\ UiProgram \in Programs
    /\ MinWeight > 0
    /\ MinWeight <= MaxUserWeight
    /\ MaxUserWeight < SystemWeight
    /\ SystemWeight <= MaxRequested
    /\ registryWeight = [program \in Programs |-> MinWeight]
    /\ pending = NoProgram
    /\ launched = [program \in Programs |-> FALSE]
    /\ admittedWeight = [program \in Programs |-> 0]

RewriteRegistry(program, weight) ==
    /\ program \in Programs
    /\ weight \in Weights
    /\ registryWeight' = [registryWeight EXCEPT ![program] = weight]
    /\ UNCHANGED <<pending, launched, admittedWeight>>

RequestLaunch(program) ==
    /\ program \in Programs
    /\ pending = NoProgram
    /\ pending' = program
    /\ UNCHANGED <<registryWeight, launched, admittedWeight>>

(*******************************************************************************
The concrete implementation makes this decision while building one loader
request. There is no later scheduler-side reinterpretation of catalog input.
*******************************************************************************)
AdmitPending ==
    /\ pending \in Programs
    /\ admittedWeight' = [admittedWeight EXCEPT ![pending] = AdmittedFor(pending)]
    /\ launched' = [launched EXCEPT ![pending] = TRUE]
    /\ pending' = NoProgram
    /\ UNCHANGED registryWeight

Next ==
    \/ \E program \in Programs, weight \in Weights: RewriteRegistry(program, weight)
    \/ \E program \in Programs: RequestLaunch(program)
    \/ AdmitPending

Spec == Init /\ [][Next]_vars /\ WF_vars(AdmitPending)

TypeOK ==
    /\ registryWeight \in [Programs -> Weights]
    /\ pending \in Programs \cup {NoProgram}
    /\ launched \in [Programs -> BOOLEAN]
    /\ admittedWeight \in [Programs -> 0..SystemWeight]

UntrustedLaunchStaysBelowSystem ==
    \A program \in Programs \ {UiProgram}:
        launched[program] =>
            /\ admittedWeight[program] >= MinWeight
            /\ admittedWeight[program] <= MaxUserWeight

UiWeightIsPinned ==
    launched[UiProgram] => admittedWeight[UiProgram] = SystemWeight

OnlyUiCanReceiveSystemWeight ==
    \A program \in Programs:
        launched[program] /\ admittedWeight[program] = SystemWeight => program = UiProgram

PendingLaunchEventuallySettles ==
    [] (pending \in Programs => <>(pending = NoProgram))

=============================================================================
