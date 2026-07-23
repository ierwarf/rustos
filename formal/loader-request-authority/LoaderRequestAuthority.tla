------------------------ MODULE LoaderRequestAuthority ------------------------
EXTENDS Naturals

(*******************************************************************************
Models the privileged loader request from service ingress through the terminal
ring0 commit. A PID is not authority: SPAWN_EXEC is admitted only for the
current rootd, initd, or sessiond endpoint owner, while EXEC_TARGET is admitted
only for the current procd owner. The exact role is revalidated after the
potentially long image-load window, so service restart or revoke fails closed.
*******************************************************************************)

Roles == {"none", "rootd", "initd", "sessiond", "procd"}
Ops == {"spawn", "exec"}
Phases == {"received", "loading", "committed", "denied"}

VARIABLES op, role, roleLive, admitted, phase, loaderEpoch

vars == <<op, role, roleLive, admitted, phase, loaderEpoch>>

RoleAllows(requestOp, requesterRole) ==
    \/ /\ requestOp = "spawn"
       /\ requesterRole \in {"rootd", "initd", "sessiond"}
    \/ /\ requestOp = "exec"
       /\ requesterRole = "procd"

Init ==
    /\ op \in Ops
    /\ role \in Roles
    /\ roleLive = TRUE
    /\ admitted = FALSE
    /\ phase = "received"
    /\ loaderEpoch = 0

Admit ==
    /\ phase = "received"
    /\ roleLive
    /\ RoleAllows(op, role)
    /\ admitted' = TRUE
    /\ phase' = "loading"
    /\ UNCHANGED <<op, role, roleLive, loaderEpoch>>

RejectIngress ==
    /\ phase = "received"
    /\ ~RoleAllows(op, role)
    /\ phase' = "denied"
    /\ UNCHANGED <<op, role, roleLive, admitted, loaderEpoch>>

RestartLoader ==
    /\ phase = "loading"
    /\ loaderEpoch = 0
    /\ loaderEpoch' = 1
    /\ UNCHANGED <<op, role, roleLive, admitted, phase>>

RevokeRoleDuringLoad ==
    /\ phase = "loading"
    /\ roleLive
    /\ roleLive' = FALSE
    /\ UNCHANGED <<op, role, admitted, phase, loaderEpoch>>

Commit ==
    /\ phase = "loading"
    /\ admitted
    /\ roleLive
    /\ RoleAllows(op, role)
    /\ phase' = "committed"
    /\ UNCHANGED <<op, role, roleLive, admitted, loaderEpoch>>

RejectCommit ==
    /\ phase = "loading"
    /\ ~roleLive
    /\ phase' = "denied"
    /\ UNCHANGED <<op, role, roleLive, admitted, loaderEpoch>>

Next ==
    Admit
    \/ RejectIngress
    \/ RestartLoader
    \/ RevokeRoleDuringLoad
    \/ Commit
    \/ RejectCommit

TypeOK ==
    /\ op \in Ops
    /\ role \in Roles
    /\ roleLive \in BOOLEAN
    /\ admitted \in BOOLEAN
    /\ phase \in Phases
    /\ loaderEpoch \in 0..1

CommittedHasExactLiveRole ==
    phase = "committed" => admitted /\ roleLive /\ RoleAllows(op, role)

LoadingWasAuthorized ==
    phase = "loading" => admitted /\ RoleAllows(op, role)

ForeignNeverCommits ==
    ~RoleAllows(op, role) => phase # "committed"

RevokedRoleNeverCommits ==
    ~roleLive => phase # "committed"

Spec == Init /\ [][Next]_vars
=============================================================================
