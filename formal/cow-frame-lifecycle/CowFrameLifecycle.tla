-------------------------- MODULE CowFrameLifecycle --------------------------
EXTENDS FiniteSets, Naturals, TLC

Roots == {"parent", "child"}
NoActor == "none"
Frames == {"none", "old", "new-parent", "new-child"}
Perms == {"none", "cow", "writable", "readonly"}
Roles == {"free", "exclusive", "shared", "locked"}
Kinds == {"anonymous", "private-file"}
ForkPhases == {"exclusive", "prepared", "waiting-ack", "ready", "shared"}
Ops == {"none", "copy", "promote", "unmap"}

VARIABLES kind, frame, perm, ledger, oldRole, forkPhase,
          childRunnable, parentWritableTlb, logicalWrite, op, actor, stale

vars == <<kind, frame, perm, ledger, oldRole, forkPhase,
          childRunnable, parentWritableTlb, logicalWrite, op, actor, stale>>

OldPtes == {r \in Roots : frame[r] = "old"}
FreshFrame(r) == IF r = "parent" THEN "new-parent" ELSE "new-child"

AnonymousInit ==
    /\ kind = "anonymous"
    /\ frame = [r \in Roots |-> IF r = "parent" THEN "old" ELSE "none"]
    /\ perm = [r \in Roots |-> IF r = "parent" THEN "writable" ELSE "none"]
    /\ ledger = {"parent"}
    /\ oldRole = "exclusive"
    /\ forkPhase = "exclusive"
    /\ childRunnable = FALSE
    /\ parentWritableTlb = FALSE
    /\ logicalWrite = [r \in Roots |-> TRUE]
    /\ op = "none"
    /\ actor = NoActor
    /\ stale = {}

PrivateFileInit ==
    /\ kind = "private-file"
    /\ frame = [r \in Roots |-> "old"]
    /\ perm = [r \in Roots |-> "cow"]
    /\ ledger = Roots
    /\ oldRole = "shared"
    /\ forkPhase = "shared"
    /\ childRunnable = TRUE
    /\ parentWritableTlb = FALSE
    /\ logicalWrite = [r \in Roots |-> TRUE]
    /\ op = "none"
    /\ actor = NoActor
    /\ stale = {}

Init == AnonymousInit \/ PrivateFileInit

BeginFork ==
    /\ kind = "anonymous"
    /\ forkPhase = "exclusive"
    /\ op = "none"
    /\ frame["parent"] = "old"
    /\ perm["parent"] = "writable"
    /\ ledger = {"parent"}
    /\ oldRole = "exclusive"
    /\ frame' = [frame EXCEPT !["child"] = "old"]
    /\ perm' = [perm EXCEPT !["child"] = "cow"]
    /\ ledger' = Roots
    /\ oldRole' = "shared"
    /\ forkPhase' = "prepared"
    /\ UNCHANGED <<kind, childRunnable, parentWritableTlb, logicalWrite, op, actor, stale>>

DowngradeParent ==
    /\ forkPhase = "prepared"
    /\ perm' = [perm EXCEPT !["parent"] = "cow"]
    /\ parentWritableTlb' = TRUE
    /\ forkPhase' = "waiting-ack"
    /\ UNCHANGED <<kind, frame, ledger, oldRole, childRunnable, logicalWrite, op, actor, stale>>

AckParentDowngrade ==
    /\ forkPhase = "waiting-ack"
    /\ parentWritableTlb
    /\ parentWritableTlb' = FALSE
    /\ forkPhase' = "ready"
    /\ UNCHANGED <<kind, frame, perm, ledger, oldRole, childRunnable, logicalWrite, op, actor, stale>>

ActivateChild ==
    /\ forkPhase = "ready"
    /\ ~parentWritableTlb
    /\ childRunnable' = TRUE
    /\ forkPhase' = "shared"
    /\ UNCHANGED <<kind, frame, perm, ledger, oldRole, parentWritableTlb, logicalWrite, op, actor, stale>>

CancelChildBeforeActivation ==
    /\ kind = "anonymous"
    /\ forkPhase = "ready"
    /\ ~childRunnable
    /\ op = "none"
    /\ frame["child"] = "old"
    /\ "child" \in ledger
    /\ frame' = [frame EXCEPT !["child"] = "none"]
    /\ perm' = [perm EXCEPT !["child"] = "none"]
    /\ ledger' = {"parent"}
    /\ logicalWrite' = [logicalWrite EXCEPT !["child"] = FALSE]
    /\ forkPhase' = "shared"
    /\ UNCHANGED <<kind, oldRole, childRunnable, parentWritableTlb, op, actor, stale>>

RollbackFork ==
    /\ kind = "anonymous"
    /\ forkPhase \in {"prepared", "waiting-ack", "ready"}
    /\ ~childRunnable
    /\ op = "none"
    /\ actor = NoActor
    /\ stale = {}
    /\ frame' = [r \in Roots |-> IF r = "parent" THEN "old" ELSE "none"]
    /\ perm' = [r \in Roots |-> IF r = "parent" THEN "writable" ELSE "none"]
    /\ ledger' = {"parent"}
    /\ oldRole' = "exclusive"
    /\ forkPhase' = "exclusive"
    /\ parentWritableTlb' = FALSE
    /\ logicalWrite' = [r \in Roots |-> IF r = "parent" THEN TRUE ELSE FALSE]
    /\ UNCHANGED <<kind, childRunnable, op, actor, stale>>

StartCow(r) ==
    /\ op = "none"
    /\ forkPhase \in {"exclusive", "shared"}
    /\ frame[r] = "old"
    /\ perm[r] = "cow"
    /\ logicalWrite[r]
    /\ r \in ledger
    /\ oldRole = "shared"
    /\ actor' = r
    /\ op' = IF kind = "anonymous" /\ Cardinality(ledger) = 1
              THEN "promote" ELSE "copy"
    /\ oldRole' = "locked"
    /\ UNCHANGED <<kind, frame, perm, ledger, forkPhase, childRunnable, logicalWrite,
                    parentWritableTlb, stale>>

InstallCowCopy ==
    /\ op = "copy"
    /\ actor \in Roots
    /\ frame' = [frame EXCEPT ![actor] = FreshFrame(actor)]
    /\ perm' = [perm EXCEPT ![actor] = "writable"]
    /\ stale' = {actor}
    /\ UNCHANGED <<kind, ledger, oldRole, forkPhase, childRunnable, logicalWrite,
                    parentWritableTlb, op, actor>>

AckCowCopy ==
    /\ op = "copy"
    /\ actor \in stale
    /\ ledger' = ledger \ {actor}
    /\ stale' = {}
    /\ oldRole' = IF Cardinality(ledger \ {actor}) = 0 THEN "free" ELSE "shared"
    /\ op' = "none"
    /\ actor' = NoActor
    /\ UNCHANGED <<kind, frame, perm, forkPhase, childRunnable, parentWritableTlb, logicalWrite>>

InstallPromotion ==
    /\ op = "promote"
    /\ kind = "anonymous"
    /\ actor \in Roots
    /\ Cardinality(ledger) = 1
    /\ perm' = [perm EXCEPT ![actor] = "writable"]
    /\ stale' = {actor}
    /\ UNCHANGED <<kind, frame, ledger, oldRole, forkPhase, childRunnable, logicalWrite,
                    parentWritableTlb, op, actor>>

AckPromotion ==
    /\ op = "promote"
    /\ actor \in stale
    /\ oldRole' = "exclusive"
    /\ stale' = {}
    /\ op' = "none"
    /\ actor' = NoActor
    /\ UNCHANGED <<kind, frame, perm, ledger, forkPhase, childRunnable, logicalWrite,
                    parentWritableTlb>>

StartUnmap(r) ==
    /\ op = "none"
    /\ forkPhase \in {"exclusive", "shared"}
    /\ r \in ledger
    /\ frame[r] = "old"
    /\ actor' = r
    /\ op' = "unmap"
    /\ oldRole' = "locked"
    /\ frame' = [frame EXCEPT ![r] = "none"]
    /\ perm' = [perm EXCEPT ![r] = "none"]
    /\ stale' = {r}
    /\ logicalWrite' = [logicalWrite EXCEPT ![r] = FALSE]
    /\ UNCHANGED <<kind, ledger, forkPhase, childRunnable, parentWritableTlb>>

AckUnmap ==
    /\ op = "unmap"
    /\ actor \in stale
    /\ ledger' = ledger \ {actor}
    /\ stale' = {}
    /\ oldRole' = IF Cardinality(ledger \ {actor}) = 0 THEN "free" ELSE "shared"
    /\ op' = "none"
    /\ actor' = NoActor
    /\ childRunnable' = IF actor = "child" THEN FALSE ELSE childRunnable
    /\ UNCHANGED <<kind, frame, perm, forkPhase, parentWritableTlb, logicalWrite>>

Protect(r) ==
    /\ op = "none"
    /\ forkPhase \in {"exclusive", "shared"}
    /\ frame[r] # "none"
    /\ perm' = [perm EXCEPT ![r] = IF @ = "cow" THEN "cow" ELSE "readonly"]
    /\ logicalWrite' = [logicalWrite EXCEPT ![r] = FALSE]
    /\ UNCHANGED <<kind, frame, ledger, oldRole, forkPhase, childRunnable,
                    parentWritableTlb, op, actor, stale>>

TerminalStutter ==
    /\ frame["parent"] = "none"
    /\ frame["child"] = "none"
    /\ UNCHANGED vars

Next ==
    BeginFork \/ DowngradeParent \/ AckParentDowngrade \/ ActivateChild
    \/ CancelChildBeforeActivation \/ RollbackFork
    \/ (\E r \in Roots : StartCow(r))
    \/ InstallCowCopy \/ AckCowCopy \/ InstallPromotion \/ AckPromotion
    \/ (\E r \in Roots : StartUnmap(r)) \/ AckUnmap
    \/ (\E r \in Roots : Protect(r)) \/ TerminalStutter

Spec == Init /\ [][Next]_vars

TypeOK ==
    /\ kind \in Kinds
    /\ frame \in [Roots -> Frames]
    /\ perm \in [Roots -> Perms]
    /\ ledger \subseteq Roots
    /\ oldRole \in Roles
    /\ forkPhase \in ForkPhases
    /\ childRunnable \in BOOLEAN
    /\ parentWritableTlb \in BOOLEAN
    /\ logicalWrite \in [Roots -> BOOLEAN]
    /\ op \in Ops
    /\ actor \in Roots \cup {NoActor}
    /\ stale \subseteq Roots

ExactMappingLedger == ledger = OldPtes \cup stale

ChildRunsOnlyAfterDowngradeAck ==
    childRunnable /\ kind = "anonymous" => forkPhase = "shared" /\ ~parentWritableTlb

NoFrameReuseBeforeTranslationAck ==
    oldRole = "free" => ledger = {} /\ stale = {} /\ op = "none"

SharedMappingsStayWriteProtected ==
    \A r \in OldPtes :
        oldRole \in {"shared", "locked"}
        /\ ~(forkPhase = "prepared" /\ r = "parent")
        /\ ~(op = "promote" /\ actor = r)
        => perm[r] = "cow"

FreshCopiesArePrivate ==
    /\ \A r \in Roots :
        frame[r] = FreshFrame(r)
        => perm[r] \in {"writable", "readonly"}
           /\ (r \notin ledger \/ r \in stale)
    /\ ~(frame["parent"] = "new-child")
    /\ ~(frame["child"] = "new-parent")

PrivateFileNeverPromotesInPlace ==
    kind = "private-file" => op # "promote" /\ oldRole # "exclusive"

CowResolutionRequiresLogicalWrite ==
    op \in {"copy", "promote"} => actor \in Roots /\ logicalWrite[actor]

InvisibleCancelledChildHasNoMapping ==
    kind = "anonymous" /\ forkPhase = "shared" /\ ~childRunnable
    => frame["child"] = "none" /\ "child" \notin ledger

=============================================================================
