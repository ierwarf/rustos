-------------------------- MODULE ServiceCallAuthority --------------------------
EXTENDS Naturals, FiniteSets

(*******************************************************************************
Models call authority for user-visible IPC endpoints.

Concrete owner: kernel/compat/src/user/syscall/linux/ipc_ops.rs

Numeric endpoint IDs are routing identifiers, not ambient bearer authority.
Service lookup mints a process-local grant for one exact service publication
epoch. A caller may invoke a published endpoint only when it owns the endpoint
or holds that exact grant. Revoke/republication advances the epoch, process
exit clears its grants, and an unpublished generic endpoint is callable only
by its process owner.
*******************************************************************************)

CONSTANTS Pids, MaxEpoch

NoPid == 0
Running == "running"
Exited == "exited"

VARIABLES processState,
          issuedPids,
          endpointOwner,
          endpointPublished,
          endpointEpoch,
          grantedPid,
          grantedEpoch,
          admittedPid,
          admittedEpoch

vars == <<processState, issuedPids, endpointOwner, endpointPublished,
          endpointEpoch, grantedPid, grantedEpoch, admittedPid, admittedEpoch>>

Init ==
    /\ processState = [p \in Pids |-> Exited]
    /\ issuedPids = {}
    /\ endpointOwner = NoPid
    /\ endpointPublished = FALSE
    /\ endpointEpoch = 0
    /\ grantedPid = NoPid
    /\ grantedEpoch = 0
    /\ admittedPid = NoPid
    /\ admittedEpoch = 0

Spawn(p) ==
    /\ p \in Pids \ issuedPids
    /\ processState[p] = Exited
    /\ processState' = [processState EXCEPT ![p] = Running]
    /\ issuedPids' = issuedPids \cup {p}
    /\ UNCHANGED <<endpointOwner, endpointPublished, endpointEpoch,
                  grantedPid, grantedEpoch, admittedPid, admittedEpoch>>

Publish(p) ==
    /\ p \in Pids
    /\ processState[p] = Running
    /\ ~endpointPublished
    /\ endpointEpoch < MaxEpoch
    /\ endpointOwner' = p
    /\ endpointPublished' = TRUE
    /\ endpointEpoch' = endpointEpoch + 1
    /\ admittedPid' = NoPid
    /\ admittedEpoch' = 0
    /\ UNCHANGED <<processState, issuedPids, grantedPid, grantedEpoch>>

LookupGrant(p) ==
    /\ p \in Pids
    /\ processState[p] = Running
    /\ endpointPublished
    /\ endpointOwner # NoPid
    /\ processState[endpointOwner] = Running
    /\ grantedPid' = p
    /\ grantedEpoch' = endpointEpoch
    /\ UNCHANGED <<processState, issuedPids, endpointOwner,
                  endpointPublished, endpointEpoch, admittedPid, admittedEpoch>>

CallAsOwner(p) ==
    /\ p \in Pids
    /\ endpointPublished
    /\ endpointOwner = p
    /\ processState[p] = Running
    /\ admittedPid' = p
    /\ admittedEpoch' = endpointEpoch
    /\ UNCHANGED <<processState, issuedPids, endpointOwner,
                  endpointPublished, endpointEpoch, grantedPid, grantedEpoch>>

CallWithGrant(p) ==
    /\ p \in Pids
    /\ endpointPublished
    /\ processState[p] = Running
    /\ grantedPid = p
    /\ grantedEpoch = endpointEpoch
    /\ grantedEpoch # 0
    /\ admittedPid' = p
    /\ admittedEpoch' = endpointEpoch
    /\ UNCHANGED <<processState, issuedPids, endpointOwner,
                  endpointPublished, endpointEpoch, grantedPid, grantedEpoch>>

SettleCall ==
    /\ admittedPid # NoPid
    /\ admittedPid' = NoPid
    /\ admittedEpoch' = 0
    /\ UNCHANGED <<processState, issuedPids, endpointOwner,
                  endpointPublished, endpointEpoch, grantedPid, grantedEpoch>>

Revoke ==
    /\ endpointPublished
    /\ endpointEpoch < MaxEpoch
    /\ endpointOwner' = NoPid
    /\ endpointPublished' = FALSE
    /\ endpointEpoch' = endpointEpoch + 1
    /\ admittedPid' = NoPid
    /\ admittedEpoch' = 0
    /\ UNCHANGED <<processState, issuedPids, grantedPid, grantedEpoch>>

Exit(p) ==
    /\ p \in Pids
    /\ processState[p] = Running
    /\ ~(endpointPublished /\ endpointOwner = p) \/ endpointEpoch < MaxEpoch
    /\ processState' = [processState EXCEPT ![p] = Exited]
    /\ endpointOwner' =
        IF endpointPublished /\ endpointOwner = p THEN NoPid ELSE endpointOwner
    /\ endpointPublished' =
        IF endpointPublished /\ endpointOwner = p THEN FALSE ELSE endpointPublished
    /\ endpointEpoch' =
        IF endpointPublished /\ endpointOwner = p THEN endpointEpoch + 1
        ELSE endpointEpoch
    /\ grantedPid' = IF grantedPid = p THEN NoPid ELSE grantedPid
    /\ grantedEpoch' = IF grantedPid = p THEN 0 ELSE grantedEpoch
    /\ admittedPid' =
        IF admittedPid = p \/ (endpointPublished /\ endpointOwner = p)
        THEN NoPid ELSE admittedPid
    /\ admittedEpoch' =
        IF admittedPid = p \/ (endpointPublished /\ endpointOwner = p)
        THEN 0 ELSE admittedEpoch
    /\ UNCHANGED issuedPids

Next ==
    \/ \E p \in Pids : Spawn(p)
    \/ \E p \in Pids : Publish(p)
    \/ \E p \in Pids : LookupGrant(p)
    \/ \E p \in Pids : CallAsOwner(p)
    \/ \E p \in Pids : CallWithGrant(p)
    \/ SettleCall
    \/ Revoke
    \/ \E p \in Pids : Exit(p)

TypeOK ==
    /\ NoPid \notin Pids
    /\ processState \in [Pids -> {Running, Exited}]
    /\ issuedPids \subseteq Pids
    /\ endpointOwner \in Pids \cup {NoPid}
    /\ endpointPublished \in BOOLEAN
    /\ endpointEpoch \in 0..MaxEpoch
    /\ grantedPid \in Pids \cup {NoPid}
    /\ grantedEpoch \in 0..MaxEpoch
    /\ admittedPid \in Pids \cup {NoPid}
    /\ admittedEpoch \in 0..MaxEpoch

PublishedEndpointHasLiveIssuedOwner ==
    endpointPublished =>
        /\ endpointOwner # NoPid
        /\ endpointOwner \in issuedPids
        /\ processState[endpointOwner] = Running
        /\ endpointEpoch # 0

GrantBelongsToLiveIssuedProcess ==
    grantedPid # NoPid =>
        /\ grantedPid \in issuedPids
        /\ processState[grantedPid] = Running
        /\ grantedEpoch # 0
        /\ grantedEpoch <= endpointEpoch

AdmittedCallHasCurrentAuthority ==
    admittedPid # NoPid =>
        /\ endpointPublished
        /\ processState[admittedPid] = Running
        /\ admittedEpoch = endpointEpoch
        /\ admittedEpoch # 0

ExitedProcessHasNoGrantOrCall ==
    \A p \in Pids:
        processState[p] = Exited =>
            /\ grantedPid # p
            /\ admittedPid # p
            /\ ~(endpointPublished /\ endpointOwner = p)

=============================================================================
