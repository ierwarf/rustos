----------------------------- MODULE EndpointPublication --------------------------
EXTENDS Naturals, FiniteSets

(*******************************************************************************
Models the registry mutation protocol below EndpointRegistry's externally
observable wait contract.

Concrete owner: kernel/compat/src/user/syscall/linux/ipc_ops.rs
Lifecycle marker owner: kernel/ps/src/multitask/process_table.rs

Endpoint publication is lock-free for readers, but its three backing fields
(capability, owner, endpoint) are not a single hardware word.  A mutation
critical section serializes registration, revoke, and cleanup.  Process exit
sets an exit marker before attempting cleanup; a registrar that observes that
marker while holding the mutation lock aborts rather than publishing.  Readers
derive authority only from a running owner, so the marker fails lookup and
capability checks closed even before cleanup stores have completed.

The linearization points are: the endpoint store for a successful publication,
the exit-marker store for authority revocation, and the zero-endpoint store for
explicit revoke/cleanup. This model does not cover generic IPC queue delivery
after a caller has already received a handle; endpoint-owner teardown is the
kernel-ipc-runtime contract.
*******************************************************************************)

CONSTANTS Pids, Endpoints

NoPid == 0
NoEndpoint == 0

Running == "running"
Exiting == "exiting"
Exited == "exited"

NoLock == 0

VARIABLES processState,
          issuedPids,
          registryLock,
          preparingPid,
          preparedEndpoint,
          endpointOwner,
          endpointValue,
          capabilityOwner,
          cleanupPid

vars == <<processState, issuedPids, registryLock, preparingPid, preparedEndpoint,
          endpointOwner, endpointValue, capabilityOwner, cleanupPid>>

EffectiveEndpoint ==
    IF endpointValue = NoEndpoint
          \/ endpointOwner = NoPid
          \/ processState[endpointOwner] # Running
    THEN NoEndpoint
    ELSE endpointValue

EffectiveCapability(p) ==
    processState[p] = Running
        /\ endpointOwner = p
        /\ endpointValue # NoEndpoint
        /\ capabilityOwner = p

Init ==
    /\ processState = [p \in Pids |-> Exited]
    /\ issuedPids = {}
    /\ registryLock = NoLock
    /\ preparingPid = NoPid
    /\ preparedEndpoint = NoEndpoint
    /\ endpointOwner = NoPid
    /\ endpointValue = NoEndpoint
    /\ capabilityOwner = NoPid
    /\ cleanupPid = NoPid

Spawn(p) ==
    /\ p \in Pids \ issuedPids
    /\ processState[p] = Exited
    /\ processState' = [processState EXCEPT ![p] = Running]
    /\ issuedPids' = issuedPids \cup {p}
    /\ UNCHANGED <<registryLock, preparingPid, preparedEndpoint, endpointOwner,
                  endpointValue, capabilityOwner, cleanupPid>>

(*******************************************************************************
This action begins after rootd has already validated the exact lease. The
mutex/recheck makes a check-then-publish sequence atomic with respect to other
registry writers while readers still use endpointValue as the commit point.
*******************************************************************************)
BeginPublication(p, endpoint) ==
    /\ p \in Pids
    /\ endpoint \in Endpoints
    /\ processState[p] = Running
    /\ registryLock = NoLock
    /\ preparingPid = NoPid
    /\ endpointValue = NoEndpoint
    /\ registryLock' = p
    /\ preparingPid' = p
    /\ preparedEndpoint' = endpoint
    /\ UNCHANGED <<processState, issuedPids, endpointOwner, endpointValue,
                  capabilityOwner, cleanupPid>>

CommitPublication(p) ==
    /\ p \in Pids
    /\ registryLock = p
    /\ preparingPid = p
    /\ preparedEndpoint \in Endpoints
    /\ processState[p] = Running
    /\ registryLock' = NoLock
    /\ preparingPid' = NoPid
    /\ preparedEndpoint' = NoEndpoint
    /\ endpointOwner' = p
    /\ endpointValue' = preparedEndpoint
    /\ capabilityOwner' = p
    /\ UNCHANGED <<processState, issuedPids, cleanupPid>>

AbortPublicationForExit(p) ==
    /\ p \in Pids
    /\ registryLock = p
    /\ preparingPid = p
    /\ processState[p] = Exiting
    /\ registryLock' = NoLock
    /\ preparingPid' = NoPid
    /\ preparedEndpoint' = NoEndpoint
    /\ UNCHANGED <<processState, issuedPids, endpointOwner, endpointValue,
                  capabilityOwner, cleanupPid>>

MarkProcessExiting(p) ==
    /\ p \in Pids
    /\ processState[p] = Running
    /\ processState' = [processState EXCEPT ![p] = Exiting]
    /\ UNCHANGED <<issuedPids, registryLock, preparingPid, preparedEndpoint,
                  endpointOwner, endpointValue, capabilityOwner, cleanupPid>>

BeginExitCleanup(p) ==
    /\ p \in Pids
    /\ processState[p] = Exiting
    /\ registryLock = NoLock
    /\ cleanupPid = NoPid
    /\ registryLock' = p
    /\ cleanupPid' = p
    /\ UNCHANGED <<processState, issuedPids, preparingPid, preparedEndpoint,
                  endpointOwner, endpointValue, capabilityOwner>>

FinishExitCleanup(p) ==
    /\ p \in Pids
    /\ registryLock = p
    /\ cleanupPid = p
    /\ processState[p] = Exiting
    /\ processState' = [processState EXCEPT ![p] = Exited]
    /\ registryLock' = NoLock
    /\ cleanupPid' = NoPid
    /\ endpointOwner' = IF endpointOwner = p THEN NoPid ELSE endpointOwner
    /\ endpointValue' = IF endpointOwner = p THEN NoEndpoint ELSE endpointValue
    /\ capabilityOwner' = IF endpointOwner = p THEN NoPid ELSE capabilityOwner
    /\ UNCHANGED <<issuedPids, preparingPid, preparedEndpoint>>

ExplicitRevoke(p) ==
    /\ p \in Pids
    /\ processState[p] = Running
    /\ registryLock = NoLock
    /\ endpointOwner = p
    /\ endpointValue # NoEndpoint
    /\ endpointOwner' = NoPid
    /\ endpointValue' = NoEndpoint
    /\ capabilityOwner' = NoPid
    /\ UNCHANGED <<processState, issuedPids, registryLock, preparingPid,
                  preparedEndpoint, cleanupPid>>

Next ==
    \/ \E p \in Pids : Spawn(p)
    \/ \E p \in Pids, endpoint \in Endpoints : BeginPublication(p, endpoint)
    \/ \E p \in Pids : CommitPublication(p)
    \/ \E p \in Pids : AbortPublicationForExit(p)
    \/ \E p \in Pids : MarkProcessExiting(p)
    \/ \E p \in Pids : BeginExitCleanup(p)
    \/ \E p \in Pids : FinishExitCleanup(p)
    \/ \E p \in Pids : ExplicitRevoke(p)

TypeOK ==
    /\ Pids \subseteq Nat
    /\ NoPid \notin Pids
    /\ Endpoints \subseteq Nat
    /\ NoEndpoint \notin Endpoints
    /\ processState \in [Pids -> {Running, Exiting, Exited}]
    /\ issuedPids \subseteq Pids
    /\ registryLock \in Pids \cup {NoLock}
    /\ preparingPid \in Pids \cup {NoPid}
    /\ preparedEndpoint \in Endpoints \cup {NoEndpoint}
    /\ endpointOwner \in Pids \cup {NoPid}
    /\ endpointValue \in Endpoints \cup {NoEndpoint}
    /\ capabilityOwner \in Pids \cup {NoPid}
    /\ cleanupPid \in Pids \cup {NoPid}

PublicationIsCapabilityComplete ==
    endpointValue # NoEndpoint =>
        /\ endpointOwner # NoPid
        /\ capabilityOwner = endpointOwner
        /\ endpointOwner \in issuedPids

ObservableAuthorityHasLiveExactOwner ==
    EffectiveEndpoint # NoEndpoint =>
        /\ processState[endpointOwner] = Running
        /\ EffectiveCapability(endpointOwner)

ExitMarkerRevokesObservableAuthority ==
    \A p \in Pids:
        processState[p] = Exiting =>
            /\ EffectiveEndpoint # NoEndpoint => endpointOwner # p
            /\ ~EffectiveCapability(p)

MutationLockHasOneExclusiveWriter ==
    /\ registryLock = NoLock <=> preparingPid = NoPid /\ cleanupPid = NoPid
    /\ preparingPid # NoPid =>
        /\ registryLock = preparingPid
        /\ cleanupPid = NoPid
        /\ preparedEndpoint \in Endpoints
    /\ cleanupPid # NoPid =>
        /\ registryLock = cleanupPid
        /\ preparingPid = NoPid
        /\ preparedEndpoint = NoEndpoint

ExitMarkedRegistrarCannotCommit ==
    \A p \in Pids:
        /\ processState[p] = Exiting
        /\ preparingPid = p
        => endpointOwner # p

TerminalProcessOwnsNoPublishedAuthority ==
    \A p \in Pids:
        processState[p] = Exited =>
            /\ endpointOwner # p
            /\ capabilityOwner # p
            /\ preparingPid # p
            /\ cleanupPid # p

AllPublishedIdentitiesWereIssued ==
    /\ endpointOwner # NoPid => endpointOwner \in issuedPids
    /\ capabilityOwner # NoPid => capabilityOwner \in issuedPids
    /\ preparingPid # NoPid => preparingPid \in issuedPids
    /\ cleanupPid # NoPid => cleanupPid \in issuedPids

=============================================================================
