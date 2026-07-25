----------------------- MODULE UserspaceWaitSetPilot -----------------------

EXTENDS Integers

(***************************************************************************
Typed symbolic refinement of the check-register-recheck wait-set arm window.
The full TLC model retains timeout, close/dup/fork/exec and provider restart.
***************************************************************************)

VARIABLES
    \* @type: Str;
    phase,
    \* @type: Int;
    generation,
    \* @type: Int;
    observed,
    \* @type: Bool;
    waiterInstalled,
    \* @type: Bool;
    providerLive

vars == <<phase, generation, observed, waiterInstalled, providerLive>>

Init ==
    /\ phase = "checked"
    /\ generation = 1
    /\ observed = 1
    /\ waiterInstalled = FALSE
    /\ providerLive = TRUE

Register ==
    /\ phase = "checked"
    /\ providerLive
    /\ phase' = "registered"
    /\ observed' = generation
    /\ waiterInstalled' = TRUE
    /\ UNCHANGED <<generation, providerLive>>

Signal ==
    /\ generation < 3
    /\ generation' = generation + 1
    /\ phase' = IF waiterInstalled THEN "resolved" ELSE phase
    /\ waiterInstalled' = FALSE
    /\ UNCHANGED <<observed, providerLive>>

ArmAfterRecheck ==
    /\ phase = "registered"
    /\ waiterInstalled
    /\ providerLive
    /\ generation = observed
    /\ phase' = "sleeping"
    /\ UNCHANGED <<generation, observed, waiterInstalled, providerLive>>

RejectStaleArm ==
    /\ phase = "registered"
    /\ generation # observed
    /\ phase' = "resolved"
    /\ waiterInstalled' = FALSE
    /\ UNCHANGED <<generation, observed, providerLive>>

Resolve ==
    /\ phase = "sleeping"
    /\ phase' = "resolved"
    /\ waiterInstalled' = FALSE
    /\ UNCHANGED <<generation, observed, providerLive>>

Revoke ==
    /\ providerLive
    /\ providerLive' = FALSE
    /\ phase' = "revoked"
    /\ waiterInstalled' = FALSE
    /\ UNCHANGED <<generation, observed>>

TerminalStutter ==
    /\ phase \in {"resolved", "revoked"}
    /\ UNCHANGED vars

Next ==
    Register \/ Signal \/ ArmAfterRecheck \/ RejectStaleArm \/ Resolve \/
    Revoke \/ TerminalStutter

TypeOK ==
    /\ phase \in {"checked", "registered", "sleeping", "resolved", "revoked"}
    /\ generation \in 1..3
    /\ observed \in 1..3
    /\ waiterInstalled \in BOOLEAN
    /\ providerLive \in BOOLEAN

SleepingHasExactGeneration ==
    phase = "sleeping" => generation = observed

NoSleepWithoutWaiter ==
    phase = "sleeping" => waiterInstalled

RevokedCannotSleep ==
    ~providerLive => phase # "sleeping"

=============================================================================
