-------------------- MODULE ApplicationJobCustody --------------------
EXTENDS Naturals, FiniteSets

(***************************************************************************
Two launches in one explicitly provisioned, non-service application realm.
Loader owns child descriptors; rootd retains ancestor authority through lost
provisioning replies, loader death and bounded retirement. Loader's service
loop is serial; realm revocation can race it through kernel job operations.
Job generation, descriptor copyout and advisory page-limit slack are supplied
by JobLifecycle, JobHandlePublication and MemoryChargeConservation, not proven
again here. This is a bounded protocol projection, not production IPC/code.
***************************************************************************)
Children == {1, 2}
Phases == {"unused", "created", "limited", "published", "acked", "retiring", "closed"}
VARIABLES rootdOwn, loaderAlive, configured, revoked, authorized,
          phase, own, limited, exact, remaining
vars == <<rootdOwn, loaderAlive, configured, revoked, authorized,
          phase, own, limited, exact, remaining>>

Init ==
    /\ rootdOwn = TRUE
    /\ loaderAlive = TRUE
    /\ configured = FALSE
    /\ revoked = FALSE
    /\ authorized \in BOOLEAN
    /\ phase = [c \in Children |-> "unused"]
    /\ own = [c \in Children |-> FALSE]
    /\ limited = [c \in Children |-> FALSE]
    /\ exact = [c \in Children |-> FALSE]
    /\ remaining = [c \in Children |-> 0]

Install ==
    /\ loaderAlive /\ ~revoked /\ rootdOwn
    /\ authorized \* exact sender, live rootd owner and exact typed rights
    /\ configured' = TRUE
    /\ UNCHANGED <<rootdOwn, loaderAlive, revoked, authorized, phase, own, limited, exact, remaining>>

Create(c) ==
    /\ configured /\ loaderAlive /\ ~revoked
    /\ phase[c] = "unused"
    /\ phase' = [phase EXCEPT ![c] = "created"]
    /\ own' = [own EXCEPT ![c] = TRUE] \* retain before fallible limit setting
    /\ UNCHANGED <<rootdOwn, loaderAlive, configured, revoked, authorized, limited, exact, remaining>>

SetLimits(c) ==
    /\ loaderAlive /\ ~revoked /\ phase[c] = "created"
    /\ limited' = [limited EXCEPT ![c] = TRUE] \* all three limits before prepare
    /\ phase' = [phase EXCEPT ![c] = "limited"]
    /\ UNCHANGED <<rootdOwn, loaderAlive, configured, revoked, authorized, own, exact, remaining>>

Publish(c, matching) ==
    /\ loaderAlive /\ ~revoked /\ phase[c] = "limited"
    /\ limited[c] /\ own[c]
    /\ phase' = [phase EXCEPT ![c] = "published"]
    /\ remaining' = [remaining EXCEPT ![c] = 2]
    /\ exact' = [exact EXCEPT ![c] = matching]
    /\ UNCHANGED <<rootdOwn, loaderAlive, configured, revoked, authorized, own, limited>>

Ack(c) ==
    /\ loaderAlive /\ phase[c] = "published"
    /\ exact[c] \* PROCESS_JOB must match before exposing success
    /\ phase' = [phase EXCEPT ![c] = "acked"]
    /\ UNCHANGED <<rootdOwn, loaderAlive, configured, revoked, authorized, own, limited, exact, remaining>>

Abort(c) ==
    /\ loaderAlive /\ phase[c] \in {"created", "limited", "published", "acked"}
    /\ phase' = [phase EXCEPT ![c] = "retiring"]
    /\ UNCHANGED <<rootdOwn, loaderAlive, configured, revoked, authorized, own, limited, exact, remaining>>

Drain(c) ==
    /\ ((loaderAlive /\ phase[c] = "retiring") \/ (rootdOwn /\ revoked))
    /\ remaining[c] > 0
    /\ remaining' = [remaining EXCEPT ![c] = @ - 1]
    /\ UNCHANGED <<rootdOwn, loaderAlive, configured, revoked, authorized, phase, own, limited, exact>>

Close(c) ==
    /\ loaderAlive /\ phase[c] = "retiring"
    /\ remaining[c] = 0 \* partial or denied kill cannot release custody
    /\ own' = [own EXCEPT ![c] = FALSE]
    /\ phase' = [phase EXCEPT ![c] = "closed"]
    /\ UNCHANGED <<rootdOwn, loaderAlive, configured, revoked, authorized, limited, exact, remaining>>

RevokeRealm ==
    /\ rootdOwn /\ ~revoked
    /\ revoked' = TRUE
    /\ UNCHANGED <<rootdOwn, loaderAlive, configured, authorized, phase, own, limited, exact, remaining>>

LoaderDies ==
    /\ loaderAlive
    /\ loaderAlive' = FALSE
    /\ own' = [c \in Children |-> FALSE]
    /\ UNCHANGED <<rootdOwn, configured, revoked, authorized, phase, limited, exact, remaining>>

ReleaseRealm ==
    /\ rootdOwn /\ revoked
    /\ \A c \in Children: remaining[c] = 0 \* realm must drain before authority closes
    /\ rootdOwn' = FALSE
    /\ UNCHANGED <<loaderAlive, configured, revoked, authorized, phase, own, limited, exact, remaining>>

Next == Install \/ RevokeRealm \/ LoaderDies \/ ReleaseRealm
    \/ (\E c \in Children: Create(c) \/ SetLimits(c) \/ Ack(c) \/ Abort(c) \/ Drain(c) \/ Close(c)
        \/ (\E matching \in BOOLEAN: Publish(c, matching)))
    \/ UNCHANGED vars
Spec == Init /\ [][Next]_vars

TypeOK ==
    /\ rootdOwn \in BOOLEAN /\ loaderAlive \in BOOLEAN /\ configured \in BOOLEAN
    /\ revoked \in BOOLEAN /\ authorized \in BOOLEAN
    /\ phase \in [Children -> Phases] /\ own \in [Children -> BOOLEAN]
    /\ limited \in [Children -> BOOLEAN] /\ exact \in [Children -> BOOLEAN]
    /\ remaining \in [Children -> 0..2]
ProvisionedOnlyByAuthority == configured => authorized
LimitedBeforePublication == \A c \in Children: remaining[c] > 0 => limited[c]
AcknowledgedExactPlacement == \A c \in Children: phase[c] = "acked" => exact[c]
NoAbandonedMembers == \A c \in Children: remaining[c] > 0 => rootdOwn \/ own[c]
ClosedOnlyWhenEmpty == \A c \in Children: phase[c] = "closed" => remaining[c] = 0
=============================================================================
