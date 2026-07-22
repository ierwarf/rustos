----------------- MODULE PersistentMutationAdmission -----------------
EXTENDS Naturals

(***************************************************************************
Owner: vfsd persistent-volume mutation admission.
Linearization point: operation dispatch before any storage mutation. Current
runtime persistent write/truncate/create/unlink is read-only. A future writable
mode can become authoritative only after journal and recovery evidence are
both installed; volatile /run policy does not grant persistent authority.
***************************************************************************)

CONSTANTS Operations, VolatileOperations, MaxEpoch, WritableFeatureEnabled
VARIABLES journalReady, recoveryReady, persistentEpoch, denied, admitted,
          volatileObserved
vars == <<journalReady, recoveryReady, persistentEpoch, denied, admitted,
          volatileObserved>>

Init ==
    /\ journalReady = FALSE /\ recoveryReady = FALSE
    /\ persistentEpoch = 0 /\ denied = {} /\ admitted = {}
    /\ volatileObserved = {}

InstallJournal ==
    /\ ~journalReady /\ journalReady' = TRUE
    /\ UNCHANGED <<recoveryReady, persistentEpoch, denied, admitted,
                    volatileObserved>>

InstallRecoveryEvidence ==
    /\ ~recoveryReady /\ recoveryReady' = TRUE
    /\ UNCHANGED <<journalReady, persistentEpoch, denied, admitted,
                    volatileObserved>>

Attempt(op) ==
    /\ op \in Operations
    /\ persistentEpoch < MaxEpoch
    /\ IF WritableFeatureEnabled /\ journalReady /\ recoveryReady
          THEN /\ persistentEpoch' = persistentEpoch + 1
               /\ admitted' = admitted \cup {op}
               /\ UNCHANGED denied
          ELSE /\ denied' = denied \cup {op}
               /\ UNCHANGED <<persistentEpoch, admitted>>
    /\ UNCHANGED <<journalReady, recoveryReady, volatileObserved>>

AttemptVolatile(op) ==
    /\ op \in VolatileOperations
    /\ volatileObserved' = volatileObserved \cup {op}
    /\ UNCHANGED <<journalReady, recoveryReady, persistentEpoch, denied, admitted>>

Next == InstallJournal \/ InstallRecoveryEvidence
        \/ (\E op \in Operations: Attempt(op))
        \/ (\E volatileOp \in VolatileOperations: AttemptVolatile(volatileOp))
Spec == Init /\ [][Next]_vars

TypeOK ==
    /\ journalReady \in BOOLEAN /\ recoveryReady \in BOOLEAN
    /\ persistentEpoch \in 0..MaxEpoch
    /\ denied \in SUBSET Operations /\ admitted \in SUBSET Operations
    /\ volatileObserved \in SUBSET VolatileOperations
AdmissionRequiresCompleteRecoveryContract ==
    admitted # {} => WritableFeatureEnabled /\ journalReady /\ recoveryReady
NoPartialEvidenceMutation ==
    ~(WritableFeatureEnabled /\ journalReady /\ recoveryReady) => persistentEpoch = 0
DisabledWritableFeatureAdmitsNothing == ~WritableFeatureEnabled => admitted = {}
VolatilePolicyNeverMutatesPersistentEpoch ==
    volatileObserved # {} /\ ~WritableFeatureEnabled => persistentEpoch = 0

=============================================================================
