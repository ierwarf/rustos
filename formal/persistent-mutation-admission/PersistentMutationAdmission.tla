----------------- MODULE PersistentMutationAdmission -----------------
EXTENDS Naturals

(***************************************************************************
Owner: vfsd per-mount mutation admission.
Linearization point: operation dispatch before any storage mutation.

Admission is a per-mount property decided when the mount is built: the root
volume of a multi-partition disk is forced read-only, `/data` is writable only
when the block device itself is writable, and a read-only mount answers EROFS
for every write, truncate, create, unlink, rename and attribute change.
Volatile `/run` (tmpfs) policy never mutates a persistent mount.

NOT modeled and NOT claimed: crash consistency. FAT writes have no journal and
no ordering between data, FAT copies and directory entries; a barrier is issued
only by fsync/flush and unmount. Durability is an open gap, not a property of this model.
***************************************************************************)

CONSTANTS Mounts, Operations, VolatileOperations, MaxEpoch
VARIABLES deviceReadOnly, persistentEpoch, denied, admitted, volatileObserved
vars == <<deviceReadOnly, persistentEpoch, denied, admitted, volatileObserved>>

\* The two-partition topology: partition 0 is "root", partition 1 is "data".
MountReadOnly(m) == m = "root" \/ deviceReadOnly

Init ==
    /\ deviceReadOnly \in BOOLEAN
    /\ persistentEpoch = 0 /\ denied = {} /\ admitted = {}
    /\ volatileObserved = {}

Attempt(m, op) ==
    /\ m \in Mounts /\ op \in Operations
    /\ persistentEpoch < MaxEpoch
    /\ IF ~MountReadOnly(m)
          THEN /\ persistentEpoch' = persistentEpoch + 1
               /\ admitted' = admitted \cup {<<m, op>>}
               /\ UNCHANGED denied
          ELSE /\ denied' = denied \cup {<<m, op>>}
               /\ UNCHANGED <<persistentEpoch, admitted>>
    /\ UNCHANGED <<deviceReadOnly, volatileObserved>>

AttemptVolatile(op) ==
    /\ op \in VolatileOperations
    /\ volatileObserved' = volatileObserved \cup {op}
    /\ UNCHANGED <<deviceReadOnly, persistentEpoch, denied, admitted>>

Next == (\E m \in Mounts, op \in Operations: Attempt(m, op))
        \/ (\E volatileOp \in VolatileOperations: AttemptVolatile(volatileOp))
Spec == Init /\ [][Next]_vars

TypeOK ==
    /\ deviceReadOnly \in BOOLEAN
    /\ persistentEpoch \in 0..MaxEpoch
    /\ denied \in SUBSET (Mounts \X Operations)
    /\ admitted \in SUBSET (Mounts \X Operations)
    /\ volatileObserved \in SUBSET VolatileOperations
AdmittedOnlyOnWritableMounts ==
    \A entry \in admitted: ~MountReadOnly(entry[1])
RootIsNeverMutated == \A entry \in admitted: entry[1] # "root"
ReadOnlyDeviceAdmitsNothing == deviceReadOnly => admitted = {}
EpochCountsOnlyAdmittedMutations ==
    (admitted = {}) => persistentEpoch = 0
VolatilePolicyNeverMutatesPersistentEpoch ==
    (volatileObserved # {} /\ deviceReadOnly) => persistentEpoch = 0

=============================================================================
