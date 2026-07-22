---------------------- MODULE ServiceMutationRecovery ----------------------
EXTENDS Integers, FiniteSets, TLC

(***************************************************************************
Models the common rootd-retained mutation contract used by vfsd wait sets and
the replay ledger used by netd reference retirement.

Concrete refinement:
  - a kernel-minted operation id is stable across retries;
  - the service commits one revision before publishing local policy state;
  - a crash drops only local state and endpoint publication;
  - recovery replays the retained checkpoint before endpoint publication;
  - an exact retry observes the original commit and never increments revision;
  - an operation-id alias with different contents is rejected in source.
***************************************************************************)

CONSTANTS Operations, MaxRevision
ASSUME Operations # {} /\ IsFiniteSet(Operations) /\ MaxRevision \in Nat

States == {"absent", "live", "tombstone"}
Phases == {"idle", "pending", "committed", "replied"}

VARIABLES serviceLive, endpointPublished, checkpoint, localState,
          phase, currentOp, desiredState, applied, revision, pendingReconcile

vars == <<serviceLive, endpointPublished, checkpoint, localState, phase,
          currentOp, desiredState, applied, revision, pendingReconcile>>

Init ==
    /\ serviceLive = TRUE
    /\ endpointPublished = FALSE
    /\ checkpoint = "absent"
    /\ localState = "absent"
    /\ phase = "idle"
    /\ currentOp = CHOOSE op \in Operations: TRUE
    /\ desiredState = "absent"
    /\ applied = {}
    /\ revision = 0
    /\ pendingReconcile = FALSE

PublishEndpoint ==
    /\ serviceLive
    /\ ~endpointPublished
    /\ localState = checkpoint
    /\ endpointPublished' = TRUE
    /\ UNCHANGED <<serviceLive, checkpoint, localState, phase, currentOp,
                   desiredState, applied, revision, pendingReconcile>>

BeginMutation ==
    /\ endpointPublished
    /\ phase \in {"idle", "replied"}
    /\ revision < MaxRevision
    /\ \E op \in Operations \ applied:
          /\ currentOp' = op
          /\ desiredState' \in {"live", "tombstone"}
    /\ phase' = "pending"
    /\ pendingReconcile' = TRUE
    /\ UNCHANGED <<serviceLive, endpointPublished, checkpoint, localState,
                   applied, revision>>

CommitMutation ==
    /\ serviceLive
    /\ phase = "pending"
    /\ currentOp \notin applied
    /\ checkpoint' = desiredState
    /\ localState' = desiredState
    /\ applied' = applied \cup {currentOp}
    /\ revision' = revision + 1
    /\ phase' = "committed"
    /\ UNCHANGED <<serviceLive, endpointPublished, currentOp, desiredState,
                   pendingReconcile>>

ReplyMutation ==
    /\ serviceLive
    /\ endpointPublished
    /\ phase = "committed"
    /\ localState = checkpoint
    /\ phase' = "replied"
    /\ pendingReconcile' = FALSE
    /\ UNCHANGED <<serviceLive, endpointPublished, checkpoint, localState,
                   currentOp, desiredState, applied, revision>>

CrashService ==
    /\ serviceLive
    /\ serviceLive' = FALSE
    /\ endpointPublished' = FALSE
    /\ localState' = "absent"
    /\ UNCHANGED <<checkpoint, phase, currentOp, desiredState, applied,
                   revision, pendingReconcile>>

RecoverCheckpoint ==
    /\ ~serviceLive
    /\ serviceLive' = TRUE
    /\ endpointPublished' = FALSE
    /\ localState' = checkpoint
    /\ UNCHANGED <<checkpoint, phase, currentOp, desiredState, applied,
                   revision, pendingReconcile>>

ReconcileCommitted ==
    /\ serviceLive
    /\ endpointPublished
    /\ phase = "committed"
    /\ currentOp \in applied
    /\ localState = checkpoint
    /\ phase' = "replied"
    /\ pendingReconcile' = FALSE
    /\ UNCHANGED <<serviceLive, endpointPublished, checkpoint, localState,
                   currentOp, desiredState, applied, revision>>

TerminalStutter ==
    /\ revision = MaxRevision
    /\ phase \in {"idle", "replied"}
    /\ UNCHANGED vars

Next ==
    \/ PublishEndpoint
    \/ BeginMutation
    \/ CommitMutation
    \/ ReplyMutation
    \/ CrashService
    \/ RecoverCheckpoint
    \/ ReconcileCommitted
    \/ TerminalStutter

Spec == Init /\ [][Next]_vars

TypeOK ==
    /\ serviceLive \in BOOLEAN
    /\ endpointPublished \in BOOLEAN
    /\ checkpoint \in States
    /\ localState \in States
    /\ phase \in Phases
    /\ currentOp \in Operations
    /\ desiredState \in States
    /\ applied \subseteq Operations
    /\ revision \in 0..MaxRevision
    /\ pendingReconcile \in BOOLEAN

PublishedStateWasReplayed == endpointPublished => serviceLive /\ localState = checkpoint

RevisionCountsUniqueCommits == revision = Cardinality(applied)

CommittedOperationIsRetained == phase = "committed" => currentOp \in applied

UnsettledCommitIsExplicit == phase = "committed" => pendingReconcile

CrashCannotPublishAuthority == ~serviceLive => ~endpointPublished

=============================================================================
