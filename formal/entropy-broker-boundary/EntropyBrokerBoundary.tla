------------------------- MODULE EntropyBrokerBoundary ------------------------
EXTENDS Naturals

(*******************************************************************************
Composes boot entropy admission, the private master CSPRNG, bounded ring0
copyout, and service-owned getrandom/token policy.

An absent deterministic seed cannot initialize the master.  Only an authorized
policy service may request a bounded copy, and child streams are derived from
private master output rather than public PID/TID/counter state.

Concrete owners:
  * boot/boot-protocol and kernel/nucleus-core Multiboot admission
  * libs/boot-random
  * kernel/compat entropy broker
  * services/syscalld and services/netd
*******************************************************************************)

SeedQualities == {"hardware", "absent"}
Derivations == {"master-output", "public-identity"}
TerminalPhases == {"boot-rejected", "served", "denied"}

VARIABLES phase, seedQuality, authorized, bounded, derivation

vars == <<phase, seedQuality, authorized, bounded, derivation>>

Init ==
    /\ phase = "boot"
    /\ seedQuality \in SeedQualities
    /\ authorized \in BOOLEAN
    /\ bounded \in BOOLEAN
    /\ derivation \in Derivations

AdmitBootEntropy ==
    /\ phase = "boot"
    /\ seedQuality = "hardware"
    /\ phase' = "ready"
    /\ UNCHANGED <<seedQuality, authorized, bounded, derivation>>

RejectBootEntropy ==
    /\ phase = "boot"
    /\ seedQuality # "hardware"
    /\ phase' = "boot-rejected"
    /\ UNCHANGED <<seedQuality, authorized, bounded, derivation>>

Serve ==
    /\ phase = "ready"
    /\ authorized
    /\ bounded
    /\ derivation = "master-output"
    /\ phase' = "served"
    /\ UNCHANGED <<seedQuality, authorized, bounded, derivation>>

Deny ==
    /\ phase = "ready"
    /\ (~authorized \/ ~bounded \/ derivation # "master-output")
    /\ phase' = "denied"
    /\ UNCHANGED <<seedQuality, authorized, bounded, derivation>>

Next ==
    \/ AdmitBootEntropy
    \/ RejectBootEntropy
    \/ Serve
    \/ Deny

TypeOK ==
    /\ phase \in {"boot", "ready"} \cup TerminalPhases
    /\ seedQuality \in SeedQualities
    /\ authorized \in BOOLEAN
    /\ bounded \in BOOLEAN
    /\ derivation \in Derivations

NoDeterministicBootSuccess ==
    seedQuality # "hardware" => phase # "served"

EntropyRequiresLeastAuthority ==
    phase = "served" => authorized

EntropyCopyIsBounded ==
    phase = "served" => bounded

EntropyNeverUsesPublicIdentityDerivation ==
    phase = "served" => derivation = "master-output"

Spec == Init /\ [][Next]_vars /\ WF_vars(Next)

EventuallyTerminal == <> (phase \in TerminalPhases)
=============================================================================
