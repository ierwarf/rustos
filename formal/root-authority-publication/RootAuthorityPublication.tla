------------------------ MODULE RootAuthorityPublication ------------------------
EXTENDS Naturals, FiniteSets

(*******************************************************************************
Models the zero-trust publication root for the service namespace.

Concrete owners:
  kernel/compat/src/user/syscall/linux/ipc_ops.rs
  kernel/ipc-runtime/src/ipc/mod.rs

The first successful rootd publication seals one non-reusable process identity
for the entire boot. A rootd exit or explicit revoke does not reopen that
authority to another process. Every non-root service publication is admitted
only when the endpoint is owned by the publisher and the rootd publication
epoch that authorized it is unchanged at the commit point.
*******************************************************************************)

CONSTANTS Pids, MaxEpoch

NoPid == 0
Running == "running"
Exited == "exited"

VARIABLES processState,
          issuedPids,
          endpointOwned,
          rootOwner,
          rootPublished,
          rootEpoch,
          authorizedPid,
          authorizedEpoch,
          serviceOwner,
          servicePublished,
          serviceGrantEpoch

vars == <<processState, issuedPids, endpointOwned, rootOwner, rootPublished,
          rootEpoch, authorizedPid, authorizedEpoch, serviceOwner,
          servicePublished, serviceGrantEpoch>>

Init ==
    /\ processState = [p \in Pids |-> Exited]
    /\ issuedPids = {}
    /\ endpointOwned = [p \in Pids |-> FALSE]
    /\ rootOwner = NoPid
    /\ rootPublished = FALSE
    /\ rootEpoch = 0
    /\ authorizedPid = NoPid
    /\ authorizedEpoch = 0
    /\ serviceOwner = NoPid
    /\ servicePublished = FALSE
    /\ serviceGrantEpoch = 0

Spawn(p) ==
    /\ p \in Pids \ issuedPids
    /\ processState[p] = Exited
    /\ processState' = [processState EXCEPT ![p] = Running]
    /\ issuedPids' = issuedPids \cup {p}
    /\ endpointOwned' = [endpointOwned EXCEPT ![p] = TRUE]
    /\ UNCHANGED <<rootOwner, rootPublished, rootEpoch, authorizedPid,
                  authorizedEpoch, serviceOwner, servicePublished,
                  serviceGrantEpoch>>

PublishRoot(p) ==
    /\ p \in Pids
    /\ processState[p] = Running
    /\ endpointOwned[p]
    /\ ~rootPublished
    /\ rootOwner = NoPid \/ rootOwner = p
    /\ rootEpoch < MaxEpoch
    /\ rootOwner' = IF rootOwner = NoPid THEN p ELSE rootOwner
    /\ rootPublished' = TRUE
    /\ rootEpoch' = rootEpoch + 1
    /\ UNCHANGED <<processState, issuedPids, endpointOwned, authorizedPid,
                  authorizedEpoch, serviceOwner, servicePublished,
                  serviceGrantEpoch>>

RevokeRoot(p) ==
    /\ p \in Pids
    /\ rootPublished
    /\ rootOwner = p
    /\ rootEpoch < MaxEpoch
    /\ rootPublished' = FALSE
    /\ rootEpoch' = rootEpoch + 1
    /\ UNCHANGED <<processState, issuedPids, endpointOwned, rootOwner,
                  authorizedPid, authorizedEpoch, serviceOwner,
                  servicePublished, serviceGrantEpoch>>

AuthorizeService(p) ==
    /\ p \in Pids
    /\ rootPublished
    /\ processState[p] = Running
    /\ endpointOwned[p]
    /\ authorizedPid' = p
    /\ authorizedEpoch' = rootEpoch
    /\ UNCHANGED <<processState, issuedPids, endpointOwned, rootOwner,
                  rootPublished, rootEpoch, serviceOwner, servicePublished,
                  serviceGrantEpoch>>

PublishService(p) ==
    /\ p \in Pids
    /\ processState[p] = Running
    /\ endpointOwned[p]
    /\ ~servicePublished
    /\ rootPublished
    /\ authorizedPid = p
    /\ authorizedEpoch = rootEpoch
    /\ authorizedEpoch # 0
    /\ serviceOwner' = p
    /\ servicePublished' = TRUE
    /\ serviceGrantEpoch' = authorizedEpoch
    /\ authorizedPid' = NoPid
    /\ authorizedEpoch' = 0
    /\ UNCHANGED <<processState, issuedPids, endpointOwned, rootOwner,
                  rootPublished, rootEpoch>>

RevokeService(p) ==
    /\ p \in Pids
    /\ servicePublished
    /\ serviceOwner = p
    /\ serviceOwner' = NoPid
    /\ servicePublished' = FALSE
    /\ serviceGrantEpoch' = 0
    /\ UNCHANGED <<processState, issuedPids, endpointOwned, rootOwner,
                  rootPublished, rootEpoch, authorizedPid, authorizedEpoch>>

Exit(p) ==
    /\ p \in Pids
    /\ processState[p] = Running
    /\ ~(rootPublished /\ rootOwner = p) \/ rootEpoch < MaxEpoch
    /\ processState' = [processState EXCEPT ![p] = Exited]
    /\ endpointOwned' = [endpointOwned EXCEPT ![p] = FALSE]
    /\ rootPublished' =
        IF rootPublished /\ rootOwner = p THEN FALSE ELSE rootPublished
    /\ rootEpoch' =
        IF rootPublished /\ rootOwner = p THEN rootEpoch + 1 ELSE rootEpoch
    /\ serviceOwner' =
        IF servicePublished /\ serviceOwner = p THEN NoPid ELSE serviceOwner
    /\ servicePublished' =
        IF servicePublished /\ serviceOwner = p THEN FALSE ELSE servicePublished
    /\ serviceGrantEpoch' =
        IF servicePublished /\ serviceOwner = p THEN 0 ELSE serviceGrantEpoch
    /\ authorizedPid' = IF authorizedPid = p THEN NoPid ELSE authorizedPid
    /\ authorizedEpoch' = IF authorizedPid = p THEN 0 ELSE authorizedEpoch
    /\ UNCHANGED <<issuedPids, rootOwner>>

Next ==
    \/ \E p \in Pids : Spawn(p)
    \/ \E p \in Pids : PublishRoot(p)
    \/ \E p \in Pids : RevokeRoot(p)
    \/ \E p \in Pids : AuthorizeService(p)
    \/ \E p \in Pids : PublishService(p)
    \/ \E p \in Pids : RevokeService(p)
    \/ \E p \in Pids : Exit(p)

TypeOK ==
    /\ NoPid \notin Pids
    /\ processState \in [Pids -> {Running, Exited}]
    /\ issuedPids \subseteq Pids
    /\ endpointOwned \in [Pids -> BOOLEAN]
    /\ rootOwner \in Pids \cup {NoPid}
    /\ rootPublished \in BOOLEAN
    /\ rootEpoch \in 0..MaxEpoch
    /\ authorizedPid \in Pids \cup {NoPid}
    /\ authorizedEpoch \in 0..MaxEpoch
    /\ serviceOwner \in Pids \cup {NoPid}
    /\ servicePublished \in BOOLEAN
    /\ serviceGrantEpoch \in 0..MaxEpoch

RootPublicationHasExactLiveEndpointOwner ==
    rootPublished =>
        /\ rootOwner # NoPid
        /\ rootOwner \in issuedPids
        /\ processState[rootOwner] = Running
        /\ endpointOwned[rootOwner]
        /\ rootEpoch # 0

RootAuthorityNeverReopensToUnissuedIdentity ==
    rootOwner # NoPid => rootOwner \in issuedPids

ServicePublicationHasExactLiveEndpointOwner ==
    servicePublished =>
        /\ serviceOwner # NoPid
        /\ serviceOwner \in issuedPids
        /\ processState[serviceOwner] = Running
        /\ endpointOwned[serviceOwner]
        /\ serviceGrantEpoch # 0
        /\ serviceGrantEpoch <= rootEpoch

PendingAuthorizationWasIssuedByOneRootEpoch ==
    authorizedPid # NoPid =>
        /\ authorizedPid \in issuedPids
        /\ authorizedEpoch # 0
        /\ authorizedEpoch <= rootEpoch

ExitedProcessOwnsNoPublishedEndpoint ==
    \A p \in Pids:
        processState[p] = Exited =>
            /\ ~(rootPublished /\ rootOwner = p)
            /\ ~(servicePublished /\ serviceOwner = p)
            /\ ~endpointOwned[p]

=============================================================================
