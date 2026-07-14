--------------------------- MODULE DvmControlEndpoint ---------------------------
EXTENDS Naturals, FiniteSets

(*******************************************************************************
Models the launch-private KVM-vsock endpoint capability for the Linux DVM
control agent.

Concrete owners and source anchors:
  * L0 derivation and bind: libs/driver-domain-host/src/lib.rs
    ControlSecret::control_port and HostControlListener::bind
  * launch validation: tools/hostd/src/main.rs and tools/xtask/src/kvm.rs
  * DVM derivation: driver-domains/linux/package/rustos-dvm-agent/
    src/rustos-dvm-agent.c control_port_from_secret and connect_host

The endpoint is derived from the root-only per-launch secret and is therefore
known only to the launch agent. A same-CID unprivileged process may attempt to
connect, but cannot reserve the listener setup slot. Reaching a secret endpoint
is still not authority: the separate DvmControlRelay model checks the fresh
challenge/proof transaction before control is ready.
*******************************************************************************)

CONSTANTS MaxAttempts, MaxTime, SetupDeadline

Agent == "launch-agent"
SameCidUntrusted == "same-cid-untrusted"
NoOwner == "none"

Idle == "idle"
AwaitProof == "await-proof"
Ready == "ready"

VARIABLES phase,
          portReaders,
          setupOwner,
          authenticated,
          deadline,
          rejectedAttempts,
          now

vars == <<phase, portReaders, setupOwner, authenticated, deadline,
          rejectedAttempts, now>>

Init ==
    /\ phase = Idle
    /\ portReaders = {Agent}
    /\ setupOwner = NoOwner
    /\ authenticated = FALSE
    /\ deadline = 0
    /\ rejectedAttempts = 0
    /\ now = 0

\* An ordinary process shares the guest CID but has no read access to fw_cfg
\* raw, so it cannot derive the per-launch listener port or occupy its setup
\* slot. This action deliberately leaves every authority state unchanged.
UntrustedEndpointAttempt ==
    /\ phase = Idle
    /\ SameCidUntrusted \notin portReaders
    /\ rejectedAttempts < MaxAttempts
    /\ rejectedAttempts' = rejectedAttempts + 1
    /\ UNCHANGED <<phase, portReaders, setupOwner, authenticated, deadline, now>>

AgentConnect ==
    /\ phase = Idle
    /\ Agent \in portReaders
    \* A proof transaction must fit in the finite TLC clock, otherwise the
    \* final clock state could retain setup authority without a timeout step.
    /\ now <= MaxTime - SetupDeadline
    /\ phase' = AwaitProof
    /\ setupOwner' = Agent
    /\ authenticated' = FALSE
    /\ deadline' = now + SetupDeadline
    /\ UNCHANGED <<portReaders, rejectedAttempts, now>>

AcceptValidProof ==
    /\ phase = AwaitProof
    /\ setupOwner = Agent
    /\ now < deadline
    /\ phase' = Ready
    /\ authenticated' = TRUE
    /\ UNCHANGED <<portReaders, setupOwner, deadline, rejectedAttempts, now>>

RejectOrDisconnect ==
    /\ phase \in {AwaitProof, Ready}
    /\ phase' = Idle
    /\ setupOwner' = NoOwner
    /\ authenticated' = FALSE
    /\ deadline' = 0
    /\ UNCHANGED <<portReaders, rejectedAttempts, now>>

AdvanceTime ==
    /\ now < MaxTime
    /\ now' = now + 1
    /\ IF phase = AwaitProof /\ now + 1 >= deadline
       THEN /\ phase' = Idle
            /\ setupOwner' = NoOwner
            /\ authenticated' = FALSE
            /\ deadline' = 0
       ELSE UNCHANGED <<phase, setupOwner, authenticated, deadline>>
    /\ UNCHANGED <<portReaders, rejectedAttempts>>

Next ==
    \/ UntrustedEndpointAttempt
    \/ AgentConnect
    \/ AcceptValidProof
    \/ RejectOrDisconnect
    \/ AdvanceTime

TypeOK ==
    /\ MaxAttempts \in Nat
    /\ MaxTime \in Nat
    /\ SetupDeadline \in Nat \ {0}
    /\ phase \in {Idle, AwaitProof, Ready}
    /\ portReaders \subseteq {Agent}
    /\ setupOwner \in {NoOwner, Agent}
    /\ authenticated \in BOOLEAN
    /\ deadline \in 0..MaxTime
    /\ rejectedAttempts \in 0..MaxAttempts
    /\ now \in 0..MaxTime

EndpointCapabilityNeverLeaks ==
    portReaders = {Agent}

OnlyLaunchAgentCanOccupySetup ==
    phase \in {AwaitProof, Ready} => setupOwner = Agent

SameCidUntrustedCannotCreateAuthority ==
    SameCidUntrusted \notin portReaders /\ setupOwner \in {NoOwner, Agent}

ControlAuthorityRequiresProof ==
    phase = Ready =>
        /\ setupOwner = Agent
        /\ authenticated

SetupIsDeadlineBounded ==
    phase = AwaitProof =>
        /\ setupOwner = Agent
        /\ ~authenticated
        /\ now < deadline

SetupEventuallyResolves ==
    phase = AwaitProof ~> phase # AwaitProof

ClosedEndpointRetainsNoAuthority ==
    phase = Idle =>
        /\ setupOwner = NoOwner
        /\ ~authenticated
        /\ deadline = 0

Spec == Init /\ [][Next]_vars /\ WF_vars(AdvanceTime)

=============================================================================
