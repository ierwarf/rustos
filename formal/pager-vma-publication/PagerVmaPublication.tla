-------------------------- MODULE PagerVmaPublication --------------------------
EXTENDS Naturals

(*******************************************************************************
Owner: kernel-ps pager VMA publication.
Writers are serialized; exception readers observe all-atomic sequence snapshots
and admit only an exact live process/MM/VMA identity with allowed access.
*******************************************************************************)

CONSTANTS Empty, Writing, Published, Revoked, Ordinary, Inherited

VARIABLES state, sequence, vmaGeneration, overlap, writable, executable,
          committed, lookupAdmitted, admittedIdentityExact,
          admittedPermissionAllowed, admittedCommitted, denyAll,
          slotGeneration, currentGeneration, publicationMode, residentLeaves
vars == <<state, sequence, vmaGeneration, overlap, writable, executable,
          committed, lookupAdmitted, admittedIdentityExact,
          admittedPermissionAllowed, admittedCommitted, denyAll,
          slotGeneration, currentGeneration, publicationMode, residentLeaves>>

Init ==
    /\ state = Empty
    /\ sequence = 0
    /\ vmaGeneration = 0
    /\ overlap = FALSE
    /\ writable = FALSE
    /\ executable = FALSE
    /\ committed = FALSE
    /\ lookupAdmitted = FALSE
    /\ admittedIdentityExact = TRUE
    /\ admittedPermissionAllowed = TRUE
    /\ admittedCommitted = TRUE
    /\ denyAll = FALSE
    /\ slotGeneration = 0
    /\ currentGeneration = 1
    /\ publicationMode = Ordinary
    /\ residentLeaves = FALSE

BeginPublish(mayWrite, mayExecute, noRights, hasOverlap, isCommitted,
             mode, hasResidentLeaves) ==
    /\ \/ state \in {Empty, Revoked}
       \/ slotGeneration # currentGeneration
    /\ sequence < 6
    /\ sequence % 2 = 0
    /\ ~(mayWrite /\ mayExecute)
    /\ ~(noRights /\ (mayWrite \/ mayExecute))
    /\ ~hasOverlap
    /\ mode \in {Ordinary, Inherited}
    /\ mode = Ordinary => ~hasResidentLeaves
    /\ state' = Writing
    /\ sequence' = sequence + 1
    /\ vmaGeneration' = vmaGeneration + 1
    /\ overlap' = hasOverlap
    /\ writable' = mayWrite
    /\ executable' = mayExecute
    /\ committed' = isCommitted
    /\ denyAll' = noRights
    /\ slotGeneration' = currentGeneration
    /\ publicationMode' = mode
    /\ residentLeaves' = hasResidentLeaves
    /\ lookupAdmitted' = FALSE
    /\ admittedIdentityExact' = TRUE
    /\ admittedPermissionAllowed' = TRUE
    /\ admittedCommitted' = TRUE
    /\ UNCHANGED currentGeneration

BeginCommitUpdate(nextCommitted) ==
    /\ state = Published
    /\ sequence < 6
    /\ sequence % 2 = 0
    /\ state' = Writing
    /\ sequence' = sequence + 1
    /\ committed' = nextCommitted
    /\ lookupAdmitted' = FALSE
    /\ admittedCommitted' = TRUE
    /\ UNCHANGED <<vmaGeneration, overlap, writable, executable, denyAll,
                    slotGeneration, currentGeneration, publicationMode,
                    residentLeaves, admittedIdentityExact,
                    admittedPermissionAllowed>>

Commit ==
    /\ state = Writing
    /\ sequence % 2 = 1
    /\ state' = Published
    /\ sequence' = sequence + 1
    /\ UNCHANGED <<vmaGeneration, overlap, writable, executable, committed,
                    denyAll, slotGeneration, currentGeneration,
                    publicationMode, residentLeaves, lookupAdmitted,
                    admittedIdentityExact, admittedPermissionAllowed,
                    admittedCommitted>>

Lookup(identityExact, permissionAllowed) ==
    /\ state = Published
    /\ sequence % 2 = 0
    /\ identityExact
    /\ permissionAllowed
    /\ committed
    /\ ~denyAll
    /\ slotGeneration = currentGeneration
    /\ lookupAdmitted' = TRUE
    /\ admittedIdentityExact' = identityExact
    /\ admittedPermissionAllowed' = permissionAllowed
    /\ admittedCommitted' = committed
    /\ UNCHANGED <<state, sequence, vmaGeneration, overlap, writable,
                    executable, committed, denyAll, slotGeneration,
                    currentGeneration, publicationMode, residentLeaves>>

RejectLookup(identityExact, permissionAllowed) ==
    /\ state \in {Writing, Published, Revoked}
    /\ \/ state # Published
       \/ sequence % 2 = 1
       \/ ~identityExact
       \/ ~permissionAllowed
       \/ ~committed
       \/ denyAll
       \/ slotGeneration # currentGeneration
    /\ UNCHANGED vars

Revoke ==
    /\ state = Published
    /\ sequence < 7
    /\ sequence % 2 = 0
    /\ state' = Revoked
    /\ sequence' = sequence + 2
    /\ lookupAdmitted' = FALSE
    /\ UNCHANGED <<vmaGeneration, overlap, writable, executable, committed,
                    denyAll, slotGeneration, currentGeneration,
                    publicationMode, residentLeaves, admittedIdentityExact,
                    admittedPermissionAllowed, admittedCommitted>>

ReuseProcessGeneration ==
    /\ state \in {Published, Revoked}
    /\ currentGeneration < 2
    /\ currentGeneration' = currentGeneration + 1
    /\ lookupAdmitted' = FALSE
    /\ UNCHANGED <<state, sequence, vmaGeneration, overlap, writable,
                    executable, committed, denyAll, slotGeneration,
                    publicationMode, residentLeaves, admittedIdentityExact,
                    admittedPermissionAllowed, admittedCommitted>>

ObserveTerminal ==
    /\ \/ state = Published
       \/ /\ state = Revoked
          /\ sequence >= 4
    /\ UNCHANGED vars

Next ==
    \/ \E mayWrite \in BOOLEAN, mayExecute \in BOOLEAN,
          noRights \in BOOLEAN, hasOverlap \in BOOLEAN,
          isCommitted \in BOOLEAN, mode \in {Ordinary, Inherited},
          hasResidentLeaves \in BOOLEAN:
         BeginPublish(mayWrite, mayExecute, noRights, hasOverlap, isCommitted,
                      mode, hasResidentLeaves)
    \/ \E nextCommitted \in BOOLEAN: BeginCommitUpdate(nextCommitted)
    \/ Commit
    \/ \E identityExact \in BOOLEAN, permissionAllowed \in BOOLEAN:
         Lookup(identityExact, permissionAllowed)
    \/ \E identityExact \in BOOLEAN, permissionAllowed \in BOOLEAN:
         RejectLookup(identityExact, permissionAllowed)
    \/ Revoke
    \/ ReuseProcessGeneration
    \/ ObserveTerminal

Spec == Init /\ [][Next]_vars

TypeOK ==
    /\ state \in {Empty, Writing, Published, Revoked}
    /\ sequence \in 0..8
    /\ vmaGeneration \in 0..3
    /\ overlap \in BOOLEAN
    /\ writable \in BOOLEAN
    /\ executable \in BOOLEAN
    /\ committed \in BOOLEAN
    /\ lookupAdmitted \in BOOLEAN
    /\ admittedIdentityExact \in BOOLEAN
    /\ admittedPermissionAllowed \in BOOLEAN
    /\ admittedCommitted \in BOOLEAN
    /\ denyAll \in BOOLEAN
    /\ slotGeneration \in 0..2
    /\ currentGeneration \in 1..2
    /\ publicationMode \in {Ordinary, Inherited}
    /\ residentLeaves \in BOOLEAN

PublishedSequenceIsStable == (state = Published) => sequence % 2 = 0
PublishedVmaIsNonoverlapping == (state = Published) => ~overlap
PublishedRightsAreWxSafe == (state = Published) => ~(writable /\ executable)
PublishedVmaGenerationIsNonzero == (state = Published) => vmaGeneration > 0
LookupRequiresExactIdentity == lookupAdmitted => admittedIdentityExact
LookupRequiresAllowedAccess == lookupAdmitted => admittedPermissionAllowed
LookupRequiresCommitted == lookupAdmitted => admittedCommitted
ReservedVmaIsNotAdmitted == (state = Published /\ ~committed) => ~lookupAdmitted
DenyAllVmaIsNotAdmitted == (state = Published /\ denyAll) => ~lookupAdmitted
StaleProcessGenerationIsNotAdmitted ==
    (state = Published /\ slotGeneration # currentGeneration) => ~lookupAdmitted
LookupRequiresCurrentProcessGeneration ==
    lookupAdmitted => slotGeneration = currentGeneration
OrdinaryPublicationRequiresEmptyResidentSet ==
    (state = Published /\ slotGeneration = currentGeneration
     /\ publicationMode = Ordinary) => ~residentLeaves
RevokedVmaIsNotAdmitted == (state = Revoked) => ~lookupAdmitted

=============================================================================
